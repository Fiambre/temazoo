//! Optional video for songs that have one (music videos).
//!
//! yt-dlp saves a small H.264 mp4; symphonia demuxes it and the H.264 decoder that ships with
//! Windows (Media Foundation) decodes it. Nothing here runs unless a video is being shown: the
//! decoder thread, its buffers and Media Foundation itself only exist while a `Video` is alive.

use std::{
    fs::File,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
use symphonia::core::{
    formats::{probe::Hint, FormatOptions, SeekMode, SeekTo, TrackType},
    io::MediaSourceStream,
    meta::MetadataOptions,
    units::Time,
};
use windows::{
    core::Result as WinResult,
    Win32::{Media::MediaFoundation::*, System::Com::*},
};

pub struct Frame {
    pub w: usize,
    pub h: usize,
    pub rgba: Vec<u8>,
}

struct Shared {
    stop: AtomicBool,
    /// Audio position in ms, written by the UI; the decoder paces itself against it.
    pos_ms: AtomicU64,
    /// Whether the audio is advancing; the clock is then interpolated between position updates.
    running: AtomicBool,
    /// Free-running clock: (position in ms, when it was valid). Audio reports only nudge it.
    anchor: Mutex<(f64, Instant)>,
    /// Height in pixels of the video being decoded (0 until it is open).
    height: AtomicU64,
    // Counters for the debug panel.
    shown: AtomicU64,
    dropped: AtomicU64,
    decode_us: AtomicU64,
    convert_us: AtomicU64,
    sync_ms: std::sync::atomic::AtomicI64,
    frame: Mutex<Option<Frame>>,
}

pub struct Video {
    shared: Arc<Shared>,
}

impl Video {
    /// `pos` is where the song is now: the decoder jumps there instead of starting from the beginning.
    pub fn start(path: PathBuf, pos: Duration, running: bool, repaint: impl Fn() + Send + 'static) -> Video {
        let shared = Arc::new(Shared {
            stop: AtomicBool::new(false),
            pos_ms: AtomicU64::new(pos.as_millis() as u64),
            running: AtomicBool::new(running),
            anchor: Mutex::new((pos.as_millis() as f64, Instant::now())),
            height: AtomicU64::new(0),
            shown: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            decode_us: AtomicU64::new(0),
            convert_us: AtomicU64::new(0),
            sync_ms: std::sync::atomic::AtomicI64::new(0),
            frame: Mutex::new(None),
        });
        let s = shared.clone();
        std::thread::Builder::new()
            .name("video".into())
            .spawn(move || {
                // Media Foundation is only touched from this thread.
                unsafe {
                    if CoInitializeEx(None, COINIT_MULTITHREADED).is_err() || MFStartup(MF_VERSION, MFSTARTUP_FULL).is_err() {
                        return;
                    }
                }
                // Windows sleeps in ~15 ms steps by default, which is a third of a frame: ask for 1 ms.
                unsafe {
                    windows::Win32::Media::timeBeginPeriod(1);
                }
                let _ = run(&path, &s, &repaint);
                unsafe {
                    windows::Win32::Media::timeEndPeriod(1);
                }
                unsafe {
                    let _ = MFShutdown();
                    CoUninitialize();
                }
                *s.frame.lock().unwrap() = None;
            })
            .ok();
        Video { shared }
    }

    /// `pos` is the audio position and `at` the instant it was measured (the audio thread only samples
    /// it every few tens of ms), so the real position now is `pos` plus the time since.
    pub fn set_position(&self, pos: Duration, at: Option<Instant>, running: bool) {
        let sampled_ms = pos.as_millis() as u64;
        let lag = if running { at.map_or(0.0, |t| t.elapsed().as_secs_f64() * 1000.0) } else { 0.0 };

        let was_running = self.shared.running.swap(running, Ordering::Relaxed);
        // The audio position only updates every ~200 ms and reaches us a little late each time, so
        // the video keeps its own smooth clock and the audio only corrects it gently (10 % of the
        // error per report). A big error means a seek or a pause/resume: follow it at once.
        if self.shared.pos_ms.swap(sampled_ms, Ordering::Relaxed) != sampled_ms || running != was_running {
            let now = Instant::now();
            let mut a = self.shared.anchor.lock().unwrap();
            let est = a.0 + if was_running { a.1.elapsed().as_secs_f64() * 1000.0 } else { 0.0 };
            let target = sampled_ms as f64 + lag;
            let err = target - est;
            *a = if !running || !was_running || err.abs() > 300.0 { (target, now) } else { (est + err * 0.1, now) };
        }
    }

    /// Height in pixels of the video being decoded (0 until the file has been opened).
    pub fn height(&self) -> u32 {
        self.shared.height.load(Ordering::Relaxed) as u32
    }

    pub fn stats(&self) -> Stats {
        let s = &self.shared;
        Stats {
            shown: s.shown.load(Ordering::Relaxed),
            dropped: s.dropped.load(Ordering::Relaxed),
            decode_ms: s.decode_us.load(Ordering::Relaxed) as f32 / 1000.0,
            convert_ms: s.convert_us.load(Ordering::Relaxed) as f32 / 1000.0,
            sync_ms: s.sync_ms.load(Ordering::Relaxed),
        }
    }

    pub fn take_frame(&self) -> Option<Frame> {
        self.shared.frame.lock().unwrap().take()
    }
}

impl Drop for Video {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
    }
}

/// avcC → (Annex B parameter sets, NAL length-field size).
fn annexb_header(avcc: &[u8]) -> Option<(Vec<u8>, usize)> {
    let nal_len = (*avcc.get(4)? & 3) as usize + 1;
    let mut out = Vec::new();
    let mut i = 5;
    let n_sps = (*avcc.get(i)? & 0x1f) as usize;
    i += 1;
    let take = |count: usize, i: &mut usize, out: &mut Vec<u8>| -> Option<()> {
        for _ in 0..count {
            let l = u16::from_be_bytes([*avcc.get(*i)?, *avcc.get(*i + 1)?]) as usize;
            *i += 2;
            out.extend([0, 0, 0, 1]);
            out.extend(avcc.get(*i..*i + l)?);
            *i += l;
        }
        Some(())
    };
    take(n_sps, &mut i, &mut out)?;
    let n_pps = *avcc.get(i)? as usize;
    i += 1;
    take(n_pps, &mut i, &mut out)?;
    Some((out, nal_len))
}

fn to_annexb(pkt: &[u8], nal_len: usize, out: &mut Vec<u8>) {
    let mut i = 0;
    while i + nal_len <= pkt.len() {
        let l = pkt[i..i + nal_len].iter().fold(0usize, |a, b| a << 8 | *b as usize);
        i += nal_len;
        if i + l > pkt.len() {
            break;
        }
        out.extend([0, 0, 0, 1]);
        out.extend(&pkt[i..i + l]);
        i += l;
    }
}

unsafe fn mem_sample(data: &[u8], ts100: i64) -> WinResult<IMFSample> {
    let buf = MFCreateMemoryBuffer(data.len() as u32)?;
    let (mut p, mut max, mut len) = (std::ptr::null_mut(), 0u32, 0u32);
    buf.Lock(&mut p, Some(&mut max), Some(&mut len))?;
    std::ptr::copy_nonoverlapping(data.as_ptr(), p, data.len());
    buf.Unlock()?;
    buf.SetCurrentLength(data.len() as u32)?;
    let s = MFCreateSample()?;
    s.AddBuffer(&buf)?;
    s.SetSampleTime(ts100)?;
    Ok(s)
}

/// Selects NV12 output; returns (width, height, stride) of the decoded surface.
unsafe fn pick_nv12(t: &IMFTransform) -> WinResult<(usize, usize, usize)> {
    let mut i = 0;
    loop {
        let mt = t.GetOutputAvailableType(0, i)?;
        i += 1;
        if mt.GetGUID(&MF_MT_SUBTYPE)? == MFVideoFormat_NV12 {
            t.SetOutputType(0, &mt, 0)?;
            let s = mt.GetUINT64(&MF_MT_FRAME_SIZE)?;
            let (w, h) = ((s >> 32) as usize, s as u32 as usize);
            let stride = mt.GetUINT32(&MF_MT_DEFAULT_STRIDE).map(|s| s as usize).unwrap_or(w);
            return Ok((w, h, stride));
        }
    }
}

fn nv12_to_rgba(d: &[u8], h: usize, stride: usize, cw: usize, ch: usize) -> Vec<u8> {
    // Fixed-point BT.601; this runs on every frame, so keep it cheap for large pictures.
    let mut out = vec![255u8; cw * ch * 4];
    for y in 0..ch {
        let yrow = &d[y * stride..y * stride + cw];
        let uv_row = &d[h * stride + (y / 2) * stride..];
        for (x, (px, &yv)) in out[y * cw * 4..(y + 1) * cw * 4].chunks_exact_mut(4).zip(yrow).enumerate() {
            let c = 298 * (yv as i32 - 16);
            let u = uv_row[(x / 2) * 2] as i32 - 128;
            let v = uv_row[(x / 2) * 2 + 1] as i32 - 128;
            px[0] = ((c + 409 * v + 128) >> 8).clamp(0, 255) as u8;
            px[1] = ((c - 100 * u - 208 * v + 128) >> 8).clamp(0, 255) as u8;
            px[2] = ((c + 516 * u + 128) >> 8).clamp(0, 255) as u8;
        }
    }
    out
}


/// How a pass over the file ended.
enum End {
    Stop,
    /// The position moved back: decode again from the start.
    Rewind,
}

fn run(path: &PathBuf, sh: &Shared, repaint: &dyn Fn()) -> Result<(), String> {
    loop {
        match unsafe { play(path, sh, repaint) }? {
            End::Stop => return Ok(()),
            End::Rewind => {}
        }
    }
}

unsafe fn play(path: &PathBuf, sh: &Shared, repaint: &dyn Fn()) -> Result<End, String> {
    let e = |e: windows::core::Error| e.to_string();
    let file = File::open(path).map_err(|e| e.to_string())?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    hint.with_extension("mp4");
    let mut fmt = symphonia::default::get_probe()
        .probe(&hint, mss, FormatOptions::default(), MetadataOptions::default())
        .map_err(|e| e.to_string())?;
    let track = fmt.default_track(TrackType::Video).ok_or("sin pista de video")?;
    let vp = track.codec_params.as_ref().and_then(|p| p.video()).ok_or("sin parámetros de video")?.clone();
    let (tid, tb) = (track.id, track.time_base.ok_or("sin base de tiempo")?);
    let (cw, ch) = (vp.width.ok_or("sin ancho")? as usize, vp.height.ok_or("sin alto")? as usize);
    sh.height.store(ch as u64, Ordering::Relaxed);
    let (hdr, nal_len) = annexb_header(&vp.extra_data.first().ok_or("sin avcC")?.data).ok_or("avcC inválido")?;
    // Start near the current position (at a keyframe) instead of decoding everything before it.
    let start = clock(sh).saturating_sub(1000);
    if start > 3000 {
        if let Some(time) = Time::try_from_secs_f64(start as f64 / 1000.0) {
            let _ = fmt.seek(SeekMode::Coarse, SeekTo::Time { time, track_id: Some(tid) });
        }
    }

    let mft: IMFTransform = CoCreateInstance(&CLSID_MSH264DecoderMFT, None, CLSCTX_INPROC_SERVER).map_err(e)?;
    // Low-latency mode: the decoder hands frames out as soon as it can instead of holding a pile of them
    // back. Same picture and order, about a third of the memory at 1080p (131 -> 38 MB measured).
    if let Ok(attrs) = mft.GetAttributes() {
        let _ = attrs.SetUINT32(&MF_LOW_LATENCY, 1);
    }
    let it = MFCreateMediaType().map_err(e)?;
    it.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).map_err(e)?;
    it.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264).map_err(e)?;
    it.SetUINT64(&MF_MT_FRAME_SIZE, (cw as u64) << 32 | ch as u64).map_err(e)?;
    it.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32).map_err(e)?;
    mft.SetInputType(0, &it, 0).map_err(e)?;
    let (mut w, mut h, mut stride) = pick_nv12(&mft).map_err(e)?;
    mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0).map_err(e)?;
    mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0).map_err(e)?;

    let mut first = true;
    let mut last_pts = 0u64;
    let mut data = Vec::new();
    // Input times not yet matched to an output frame (smallest first).
    let mut ts_queue: std::collections::BinaryHeap<std::cmp::Reverse<i64>> = std::collections::BinaryHeap::new();
    let mut out_sample: Option<(IMFSample, u32)> = None;
    // One decoded frame at a time: wait for its moment, then publish it.
    let handle = |s: &IMFSample, pts: u64, last_pts: &mut u64, w: usize, h: usize, stride: usize| -> Result<Option<End>, String> {
        loop {
            if sh.stop.load(Ordering::Relaxed) {
                return Ok(Some(End::Stop));
            }
            let pos = clock(sh);
            if pos + 1500 < *last_pts {
                return Ok(Some(End::Rewind));
            }
            if pts + 120 < pos {
                *last_pts = pts;
                sh.dropped.fetch_add(1, Ordering::Relaxed);
                return Ok(None); // late: drop it and catch up
            }
            if pts <= pos + 10 {
                break;
            }
            std::thread::sleep(Duration::from_millis((pts - pos).min(40)));
        }
        *last_pts = pts;
        let b = s.ConvertToContiguousBuffer().map_err(|x| x.to_string())?;
        let (mut p, mut max, mut len) = (std::ptr::null_mut(), 0u32, 0u32);
        b.Lock(&mut p, Some(&mut max), Some(&mut len)).map_err(|x| x.to_string())?;
        let d = std::slice::from_raw_parts(p, len as usize);
        let ok = d.len() >= stride * h * 3 / 2 && w >= cw && h >= ch;
        let t = Instant::now();
        let rgba = if ok { Some(nv12_to_rgba(d, h, stride, cw, ch)) } else { None };
        ewma(&sh.convert_us, t.elapsed().as_micros() as u64);
        let _ = b.Unlock();
        if let Some(rgba) = rgba {
            sh.shown.fetch_add(1, Ordering::Relaxed);
            sh.sync_ms.store(clock(sh) as i64 - pts as i64, Ordering::Relaxed);
            *sh.frame.lock().unwrap() = Some(Frame { w: cw, h: ch, rgba });
            repaint();
        }
        Ok(None)
    };

    while let Ok(Some(p)) = fmt.next_packet() {
        if p.track_id != tid {
            continue;
        }
        data.clear();
        if first {
            data.extend(&hdr);
            first = false;
        }
        to_annexb(&p.data, nal_len, &mut data);
        let ts = (p.pts.get() as f64 * tb.numer.get() as f64 / tb.denom.get() as f64 * 1e7) as i64;
        let t_in = Instant::now();
        ts_queue.push(std::cmp::Reverse(ts));
        mft.ProcessInput(0, &mem_sample(&data, ts).map_err(e)?, 0).map_err(e)?;
        let in_us = t_in.elapsed().as_micros() as u64;
        loop {
            let t_out = Instant::now();
            let info = mft.GetOutputStreamInfo(0).map_err(e)?;
            // One output sample reused for every frame (it is copied out right away), instead of a new buffer each time.
            let need = info.cbSize.max((w * h * 3 / 2) as u32);
            if out_sample.as_ref().is_none_or(|(_, size)| *size < need) {
                let buf = MFCreateMemoryBuffer(need).map_err(e)?;
                let smp = MFCreateSample().map_err(e)?;
                smp.AddBuffer(&buf).map_err(e)?;
                out_sample = Some((smp, need));
            }
            let smp = out_sample.as_ref().map(|(s, _)| s.clone());
            // Empty it so the decoder can write a fresh frame into it.
            if let Some(b) = smp.as_ref().and_then(|s| s.GetBufferByIndex(0).ok()) {
                let _ = b.SetCurrentLength(0);
            }
            let mut out = [MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: std::mem::ManuallyDrop::new(smp),
                dwStatus: 0,
                pEvents: std::mem::ManuallyDrop::new(None),
            }];
            let mut status = 0u32;
            match mft.ProcessOutput(0, &mut out, &mut status) {
                Ok(()) => {
                    ewma(&sh.decode_us, in_us + t_out.elapsed().as_micros() as u64);
                    let s = std::mem::ManuallyDrop::take(&mut out[0].pSample);
                    let _ = std::mem::ManuallyDrop::take(&mut out[0].pEvents);
                    if let Some(s) = s {
                        // The decoder returns frames in display order but stamps them with the time of the
                        // packet that fed it (decode order), which is wrong with B-frames. Hand out the
                        // input times in increasing order instead: the k-th frame out is the k-th earliest.
                        let pts = ts_queue.pop().map_or_else(|| s.GetSampleTime().unwrap_or(0), |r| r.0);
                        if let Some(end) = handle(&s, (pts.max(0) / 10_000) as u64, &mut last_pts, w, h, stride)? {
                            return Ok(end);
                        }
                    }
                }
                Err(x) if x.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => {
                    let _ = std::mem::ManuallyDrop::take(&mut out[0].pSample);
                    break;
                }
                Err(x) if x.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    let _ = std::mem::ManuallyDrop::take(&mut out[0].pSample);
                    (w, h, stride) = pick_nv12(&mft).map_err(e)?;
                }
                Err(x) => return Err(x.to_string()),
            }
        }
    }
    // End of the file: keep the last frame up and wait (or start over if the song restarts).
    loop {
        if sh.stop.load(Ordering::Relaxed) {
            return Ok(End::Stop);
        }
        if clock(sh) + 1500 < last_pts {
            return Ok(End::Rewind);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Current playback time: the last reported position plus the time elapsed since it was reported.
fn clock(sh: &Shared) -> u64 {
    let (pos, at) = *sh.anchor.lock().unwrap();
    // Cap how far it can run ahead of the last report, in case the UI stops reporting (window minimized).
    let ahead = if sh.running.load(Ordering::Relaxed) { (at.elapsed().as_secs_f64() * 1000.0).min(1500.0) } else { 0.0 };
    (pos + ahead).max(0.0) as u64
}

/// Moving average (7/8 old + 1/8 new) so the numbers shown don't jitter.
fn ewma(slot: &AtomicU64, us: u64) {
    let old = slot.load(Ordering::Relaxed);
    slot.store(if old == 0 { us } else { (old * 7 + us) / 8 }, Ordering::Relaxed);
}

/// Numbers for the debug panel.
pub struct Stats {
    pub shown: u64,
    pub dropped: u64,
    /// ms the decoder takes per frame, and ms the colour conversion takes.
    pub decode_ms: f32,
    pub convert_ms: f32,
    /// Video clock minus audio clock when the last frame was shown (ms).
    pub sync_ms: i64,
}

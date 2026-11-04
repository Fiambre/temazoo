//! Symphonia-backed rodio source that decodes both AAC (m4a) and Opus (webm).

use rodio::{source::SeekError, ChannelCount, SampleRate, Source};
use std::{fs::File, num::NonZero, path::Path, sync::Arc, time::Duration};
use symphonia::core::{
    codecs::{
        audio::{AudioDecoder, AudioDecoderOptions},
        registry::CodecRegistry,
    },
    formats::{probe::Hint, FormatOptions, FormatReader, SeekMode, SeekTo, TrackType},
    io::MediaSourceStream,
    meta::MetadataOptions,
    units::Time,
};
use symphonia_adapter_opus_rs::OpusAdapter;

pub struct SymSource {
    format: Box<dyn FormatReader>,
    decoder: Box<dyn AudioDecoder>,
    track_id: u32,
    buf: Vec<f32>,
    idx: usize,
    rate: u32,
    channels: u16,
    total: Option<Duration>,
}

fn probe(path: &Path) -> Result<Box<dyn FormatReader>, String> {
    let file = File::open(path).map_err(|e| e.to_string())?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    symphonia::default::get_probe()
        .probe(&hint, mss, FormatOptions::default(), MetadataOptions::default())
        .map_err(|e| e.to_string())
}

/// Duration of containers that don't declare it (webm), read from the packets without decoding.
fn scan_total(path: &Path) -> Option<Duration> {
    let mut format = probe(path).ok()?;
    let track = format.default_track(TrackType::Audio)?;
    let (id, tb) = (track.id, track.time_base?);
    // webm packets usually carry no duration, so use the end of the last packet (pts + dur).
    let mut end = 0i64;
    while let Ok(Some(p)) = format.next_packet() {
        if p.track_id == id {
            end = end.max(p.pts.get() + p.dur.get() as i64);
        }
    }
    let secs = end as f64 * tb.numer.get() as f64 / tb.denom.get() as f64;
    (secs > 0.0).then(|| Duration::from_secs_f64(secs))
}

impl SymSource {
    pub fn open(path: &Path) -> Result<Self, String> {
        let format = probe(path)?;
        let track = format.default_track(TrackType::Audio).ok_or("sin pista de audio")?;
        let params = track
            .codec_params
            .as_ref()
            .and_then(|p| p.audio())
            .ok_or("sin parámetros de audio")?
            .clone();
        let (track_id, num_frames) = (track.id, track.num_frames);

        let mut reg = CodecRegistry::new();
        symphonia::default::register_enabled_codecs(&mut reg);
        reg.register_audio_decoder::<OpusAdapter>();
        let decoder = reg
            .make_audio_decoder(&params, &AudioDecoderOptions::default())
            .map_err(|e| e.to_string())?;

        let mut s = SymSource {
            format,
            decoder,
            track_id,
            buf: Vec::new(),
            idx: 0,
            rate: 0,
            channels: 0,
            total: None,
        };
        // Decode the first packet eagerly so rate/channels are known.
        s.refill().ok_or("no se pudo decodificar")?;
        let rate = s.rate;
        s.total = match num_frames {
            Some(n) if rate > 0 => Some(Duration::from_secs_f64(n as f64 / rate as f64)),
            _ => scan_total(path),
        };
        Ok(s)
    }

    /// Decodes the next packet into `buf`. Returns `None` at end of stream.
    fn refill(&mut self) -> Option<()> {
        loop {
            let p = self.format.next_packet().ok()??;
            if p.track_id != self.track_id {
                continue;
            }
            if let Ok(b) = self.decoder.decode(&p) {
                let spec = b.spec();
                self.rate = spec.rate();
                self.channels = spec.channels().count() as u16;
                self.buf.resize(b.samples_interleaved(), 0.0);
                b.copy_to_slice_interleaved(&mut self.buf);
                self.idx = 0;
                return Some(());
            }
        }
    }
}

impl Iterator for SymSource {
    type Item = f32;

    fn next(&mut self) -> Option<f32> {
        if self.idx >= self.buf.len() {
            self.refill()?;
        }
        let s = self.buf.get(self.idx).copied();
        self.idx += 1;
        s
    }
}

impl Source for SymSource {
    fn current_span_len(&self) -> Option<usize> {
        None
    }

    fn channels(&self) -> ChannelCount {
        NonZero::new(self.channels).unwrap_or(NonZero::<u16>::MIN)
    }

    fn sample_rate(&self) -> SampleRate {
        NonZero::new(self.rate).unwrap_or(NonZero::<u32>::MIN)
    }

    fn total_duration(&self) -> Option<Duration> {
        self.total
    }

    fn try_seek(&mut self, pos: Duration) -> Result<(), SeekError> {
        let other = |m: String| SeekError::Other(Arc::new(std::io::Error::other(m)));
        let time = Time::try_from_secs_f64(pos.as_secs_f64()).ok_or_else(|| other("tiempo inválido".into()))?;
        self.format
            .seek(SeekMode::Coarse, SeekTo::Time { time, track_id: Some(self.track_id) })
            .map_err(|e| other(e.to_string()))?;
        self.decoder.reset();
        self.buf.clear();
        self.idx = 0;
        Ok(())
    }
}

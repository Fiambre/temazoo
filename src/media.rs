//! Windows media controls (media keys, volume overlay, lock screen) through souvlaki.

use souvlaki::{MediaControlEvent, MediaControls, MediaMetadata, MediaPlayback, PlatformConfig};
use std::{ffi::c_void, time::Duration};

pub struct Media(MediaControls);

impl Media {
    /// `on_event` runs on another thread whenever the user presses a media key.
    pub fn new(hwnd: *mut c_void, on_event: impl Fn(MediaControlEvent) + Send + 'static) -> Option<Self> {
        let mut controls =
            MediaControls::new(PlatformConfig { display_name: "Temazoo", dbus_name: "temazoo", hwnd: Some(hwnd) }).ok()?;
        controls.attach(on_event).ok()?;
        Some(Self(controls))
    }

    pub fn set_track(&mut self, title: &str, artist: &str, cover_url: &str, secs: u32) {
        let _ = self.0.set_metadata(MediaMetadata {
            title: Some(title),
            artist: Some(artist),
            album: None,
            cover_url: (!cover_url.is_empty()).then_some(cover_url),
            duration: (secs > 0).then(|| Duration::from_secs(secs as u64)),
        });
    }

    pub fn set_state(&mut self, playing: bool, paused: bool) {
        let state = if !playing {
            MediaPlayback::Stopped
        } else if paused {
            MediaPlayback::Paused { progress: None }
        } else {
            MediaPlayback::Playing { progress: None }
        };
        let _ = self.0.set_playback(state);
    }
}

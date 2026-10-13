//! MPRIS (D-Bus media player interface) so desktop widgets, media keys and
//! `playerctl` can see and control tuber.

use souvlaki::{MediaControlEvent, MediaControls, MediaMetadata, MediaPlayback, MediaPosition, PlatformConfig};
use std::time::{Duration, Instant};

use crate::yt::Track;

/// Clients interpolate position between updates; this just keeps them honest.
const POSITION_REFRESH: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, PartialEq)]
enum Status {
    Stopped,
    Paused,
    Playing,
}

pub struct Mpris {
    controls: MediaControls,
    track_id: Option<String>,
    status: Status,
    volume: f32,
    last_update: Instant,
}

impl Mpris {
    /// `None` when there's no session bus; tuber works fine without it.
    pub fn new(on_event: impl Fn(MediaControlEvent) + Send + 'static) -> Option<Self> {
        // souvlaki's service thread unwraps on startup failure, so don't even
        // try without a bus. A per-process name avoids clashes between instances.
        let has_bus = std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_some()
            || std::env::var_os("XDG_RUNTIME_DIR").is_some_and(|d| std::path::Path::new(&d).join("bus").exists());
        if !has_bus {
            return None;
        }
        let dbus_name = format!("tuber.instance{}", std::process::id());
        let mut controls =
            MediaControls::new(PlatformConfig { display_name: "tuber", dbus_name: &dbus_name, hwnd: None }).ok()?;
        controls.attach(on_event).ok()?;
        Some(Self { controls, track_id: None, status: Status::Stopped, volume: -1.0, last_update: Instant::now() })
    }

    /// Push current state; cheap to call every frame.
    pub fn sync(&mut self, track: Option<&Track>, active: bool, paused: bool, position: f64, volume: f32) {
        if track.map(|t| &t.id) != self.track_id.as_ref() {
            self.track_id = track.map(|t| t.id.clone());
            let meta = match track {
                Some(t) => MediaMetadata {
                    title: Some(&t.title),
                    artist: Some(&t.artist),
                    album: (!t.album.is_empty()).then_some(t.album.as_str()),
                    cover_url: t.art.as_deref(),
                    duration: t.duration.map(Duration::from_secs),
                },
                None => MediaMetadata::default(),
            };
            let _ = self.controls.set_metadata(meta);
            self.last_update = Instant::now() - POSITION_REFRESH; // force a status push
        }

        let status = match (track.is_some() && active, paused) {
            (false, _) => Status::Stopped,
            (true, true) => Status::Paused,
            (true, false) => Status::Playing,
        };
        if status != self.status || self.last_update.elapsed() >= POSITION_REFRESH {
            let progress = Some(MediaPosition(Duration::from_secs_f64(position.max(0.0))));
            let playback = match status {
                Status::Stopped => MediaPlayback::Stopped,
                Status::Paused => MediaPlayback::Paused { progress },
                Status::Playing => MediaPlayback::Playing { progress },
            };
            let _ = self.controls.set_playback(playback);
            self.status = status;
            self.last_update = Instant::now();
        }

        if (volume - self.volume).abs() > f32::EPSILON {
            let _ = self.controls.set_volume(volume as f64);
            self.volume = volume;
        }
    }
}

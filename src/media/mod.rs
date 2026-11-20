use std::time::Duration;

use crate::core::clock::PlaybackStatus;

pub mod backend;
pub mod fake;
#[cfg(any(
    target_os = "linux",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly"
))]
pub mod mpris;

pub const MPRIS_BUS_PREFIX: &str = "org.mpris.MediaPlayer2.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendKind {
    Mpris,
    Unsupported,
}

impl BackendKind {
    pub const fn current() -> Self {
        if cfg!(any(
            target_os = "linux",
            target_os = "freebsd",
            target_os = "openbsd",
            target_os = "netbsd",
            target_os = "dragonfly"
        )) {
            Self::Mpris
        } else {
            Self::Unsupported
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Mpris => "mpris",
            Self::Unsupported => "unsupported",
        }
    }

    pub const fn is_supported(self) -> bool {
        matches!(self, Self::Mpris)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MediaAvailability {
    #[default]
    Available,
    Unsupported,
    Unavailable,
}

#[derive(Debug, thiserror::Error)]
pub enum MediaError {
    #[error("media backend is not available on this platform")]
    Unsupported,
    #[error("media session unavailable: {0}")]
    SessionUnavailable(String),
}

impl MediaError {
    pub fn availability(&self) -> MediaAvailability {
        match self {
            Self::Unsupported => MediaAvailability::Unsupported,
            Self::SessionUnavailable(_) => MediaAvailability::Unavailable,
        }
    }
}

pub fn player_short_name(id: &str) -> &str {
    let short = id.strip_prefix(MPRIS_BUS_PREFIX).unwrap_or(id);
    short.split('.').next().unwrap_or(short)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayerKind {
    Native,
    Browser,
    Unknown,
}

impl PlayerKind {
    pub fn from_id(id: &str) -> Self {
        let raw_name = player_short_name(id).to_ascii_lowercase();
        let name = raw_name
            .strip_suffix("-app")
            .or_else(|| raw_name.strip_suffix("-bin"))
            .unwrap_or(&raw_name);

        if matches!(
            name,
            "firefox"
                | "chromium"
                | "chrome"
                | "google-chrome"
                | "brave"
                | "plasma-browser-integration"
                | "vivaldi"
                | "opera"
                | "microsoft-edge"
                | "edge"
                | "zen"
                | "floorp"
                | "librewolf"
                | "waterfox"
                | "thorium"
        ) {
            Self::Browser
        } else if matches!(
            name,
            "mpv"
                | "limusic"
                | "ytmusic"
                | "vlc"
                | "spotify"
                | "spotifyd"
                | "mpd"
                | "mpdris2"
                | "rhythmbox"
                | "audacious"
                | "strawberry"
                | "elisa"
                | "cider"
                | "ncspot"
                | "cmus"
                | "amberol"
                | "lollypop"
                | "deadbeef"
                | "qmmp"
                | "feishin"
                | "tauon"
                | "termusic"
                | "gapless"
                | "harmonoid"
                | "supersonic"
        ) {
            Self::Native
        } else {
            Self::Unknown
        }
    }

    pub fn is_browser(self) -> bool {
        matches!(self, Self::Browser)
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Native => "native",
            Self::Browser => "browser",
            Self::Unknown => "unknown",
        }
    }

    pub fn priority_rank(self) -> u8 {
        match self {
            Self::Native => 0,
            Self::Unknown => 1,
            Self::Browser => 2,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackMeta {
    pub title: String,
    pub artist: String,
    pub album: Option<String>,
    pub duration_ms: Option<u64>,
    pub track_id: Option<String>,
    pub url: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum MediaEvent {
    PlayerChanged { id: String, kind: PlayerKind },
    NoPlayer,
    TrackChanged(TrackMeta),
    NoTrack,
    Status(PlaybackStatus),
    Rate(f64),
    Seeked { pos_ms: u64 },
    PositionPoll { pos_ms: u64, rtt: Duration },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlayerSnapshot {
    pub id: String,
    pub kind: PlayerKind,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MediaSnapshot {
    pub player: Option<PlayerSnapshot>,
    pub track: Option<TrackMeta>,
    pub playback: PlaybackStatus,
    pub position_ms: u64,
}

impl Default for MediaSnapshot {
    fn default() -> Self {
        Self {
            player: None,
            track: None,
            playback: PlaybackStatus::Stopped,
            position_ms: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_the_mpris_prefix_and_instance_suffix() {
        assert_eq!(
            player_short_name("org.mpris.MediaPlayer2.firefox.instance1"),
            "firefox"
        );
        assert_eq!(player_short_name("org.mpris.MediaPlayer2.mpv"), "mpv");
        assert_eq!(player_short_name("custom-player"), "custom-player");
    }

    #[test]
    fn backend_kind_reports_platform_support() {
        let backend = BackendKind::current();
        assert_eq!(backend.is_supported(), backend == BackendKind::Mpris);
        assert_eq!(
            backend.as_str(),
            if backend.is_supported() {
                "mpris"
            } else {
                "unsupported"
            }
        );
    }

    #[test]
    fn media_errors_map_to_availability() {
        assert_eq!(
            MediaError::Unsupported.availability(),
            MediaAvailability::Unsupported
        );
        assert_eq!(
            MediaError::SessionUnavailable("no session".to_string()).availability(),
            MediaAvailability::Unavailable
        );
    }
}

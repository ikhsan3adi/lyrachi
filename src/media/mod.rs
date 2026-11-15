use std::time::Duration;

use crate::core::clock::PlaybackStatus;

pub mod mpris;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayerKind {
    Native,
    Browser,
    Unknown,
}

impl PlayerKind {
    pub fn from_bus(bus: &str) -> Self {
        let short = bus.strip_prefix("org.mpris.MediaPlayer2.").unwrap_or(bus);
        let raw_name = short
            .split('.')
            .next()
            .unwrap_or(short)
            .to_ascii_lowercase();
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
    PlayerChanged { bus: String, kind: PlayerKind },
    NoPlayer,
    TrackChanged(TrackMeta),
    NoTrack,
    Status(PlaybackStatus),
    Rate(f64),
    Seeked { pos_ms: u64 },
    PositionPoll { pos_ms: u64, rtt: Duration },
}

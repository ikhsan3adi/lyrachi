use std::time::Duration;

use crate::core::clock::PlaybackStatus;

pub mod fake;
pub mod mpris;

pub const MPRIS_BUS_PREFIX: &str = "org.mpris.MediaPlayer2.";

pub fn player_short_name(bus: &str) -> &str {
    let short = bus.strip_prefix(MPRIS_BUS_PREFIX).unwrap_or(bus);
    short.split('.').next().unwrap_or(short)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayerKind {
    Native,
    Browser,
    Unknown,
}

impl PlayerKind {
    pub fn from_bus(bus: &str) -> Self {
        let raw_name = player_short_name(bus).to_ascii_lowercase();
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
    PlayerChanged { bus: String, kind: PlayerKind },
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
    pub bus: String,
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
}

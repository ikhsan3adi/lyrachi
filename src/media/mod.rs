use std::time::Duration;

use crate::core::clock::PlaybackStatus;

pub mod backend;
pub mod composite;
pub mod config;
pub mod diff;
pub mod fake;
#[cfg(any(target_os = "macos", test))]
pub mod macos;
#[cfg(any(target_os = "macos", test))]
pub mod mediaremote;
pub mod mpd;
#[cfg(any(
    target_os = "linux",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly"
))]
pub mod mpris;
#[cfg(any(target_os = "windows", test))]
pub mod smtc;
pub mod webnowplaying;

pub use backend::{BoxFuture, MediaBackend};
pub use composite::{CompositeBackend, ExponentialBackoff};
pub use config::{MediaConfig, MpdConfig, WebNowPlayingConfig};

pub const MPRIS_BUS_PREFIX: &str = "org.mpris.MediaPlayer2.";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackendKind {
    Mpris,
    Mpd,
    WebNowPlaying,
    Smtc,
    AppleScript,
    MediaRemote,
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
        } else if cfg!(target_os = "windows") {
            Self::Smtc
        } else if cfg!(target_os = "macos") {
            Self::AppleScript
        } else {
            Self::Unsupported
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Mpris => "mpris",
            Self::Mpd => "mpd",
            Self::WebNowPlaying => "webnowplaying",
            Self::Smtc => "smtc",
            Self::AppleScript => "applescript",
            Self::MediaRemote => "mediaremote",
            Self::Unsupported => "unsupported",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        match name.to_ascii_lowercase().as_str() {
            "mpris" => Some(Self::Mpris),
            "mpd" => Some(Self::Mpd),
            "webnowplaying" => Some(Self::WebNowPlaying),
            "smtc" => Some(Self::Smtc),
            "applescript" => Some(Self::AppleScript),
            "mediaremote" => Some(Self::MediaRemote),
            "unsupported" => Some(Self::Unsupported),
            _ => None,
        }
    }

    pub const fn is_supported(self) -> bool {
        match self {
            Self::Mpris => cfg!(any(
                target_os = "linux",
                target_os = "freebsd",
                target_os = "openbsd",
                target_os = "netbsd",
                target_os = "dragonfly"
            )),
            Self::Mpd | Self::WebNowPlaying => true,
            Self::Smtc => cfg!(target_os = "windows"),
            Self::AppleScript | Self::MediaRemote => cfg!(target_os = "macos"),
            Self::Unsupported => false,
        }
    }

    pub fn priority_rank(self) -> u8 {
        match self {
            Self::Mpris | Self::Smtc | Self::MediaRemote => 0,
            Self::AppleScript => 1,
            Self::Mpd => 2,
            Self::WebNowPlaying => 3,
            Self::Unsupported => 255,
        }
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

// Only verified AUMIDs; collect more with `lyrachi debug smtc` before adding entries.
fn aumid_friendly_name(aumid: &str) -> Option<&'static str> {
    const FRIENDLY: &[(&str, &str)] = &[
        ("Microsoft.ZuneMusic", "media-player"),
        ("SpotifyMusic", "spotify"),
    ];
    let (package, app_id) = aumid.split_once('!')?;
    // Package family is "Name_PublisherHash"; the PRAID after '!' may differ from the name.
    let package_name = package.rsplit_once('_').map_or(package, |(name, _)| name);
    FRIENDLY
        .iter()
        .find(|(key, _)| app_id.eq_ignore_ascii_case(key) || package_name.eq_ignore_ascii_case(key))
        .map(|(_, name)| *name)
}

pub fn player_short_name(id: &str) -> &str {
    if let Some((_, app_id)) = id.split_once('!') {
        // Packaged AppId may contain dots (Microsoft.ZuneMusic), so no dot split here.
        return aumid_friendly_name(id).unwrap_or(app_id);
    }
    let short = id.strip_prefix(MPRIS_BUS_PREFIX).unwrap_or(id);
    short.split('.').next().unwrap_or(short)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlayerKind {
    Native,
    Browser,
    Unknown,
}

impl PlayerKind {
    pub fn from_id(id: &str) -> Self {
        let raw_name = player_short_name(id).to_ascii_lowercase();
        let name = raw_name.strip_suffix(".exe").unwrap_or(&raw_name);
        let name = name
            .strip_suffix("-app")
            .or_else(|| name.strip_suffix("-bin"))
            .unwrap_or(name);

        if matches!(
            name,
            "firefox"
                | "chromium"
                | "chrome"
                | "google-chrome"
                | "brave"
                | "brave-browser"
                | "plasma-browser-integration"
                | "vivaldi"
                | "opera"
                | "microsoft-edge"
                | "edge"
                | "msedge"
                | "zen"
                | "floorp"
                | "librewolf"
                | "waterfox"
                | "thorium"
                | "safari"
                | "arc"
                | "yandex"
                | "webnowplaying"
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
                | "music"
                | "media-player"
                | "apple-music"
                | "foobar2000"
                | "cantata"
                | "pragha"
                | "quodlibet"
                | "sayonara"
                | "kodi"
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
    PlayerChanged {
        id: String,
        kind: PlayerKind,
        backend: BackendKind,
    },
    NoPlayer,
    TrackChanged(TrackMeta),
    NoTrack,
    Status(PlaybackStatus),
    Rate(f64),
    Seeked {
        pos_ms: u64,
    },
    PositionPoll {
        pos_ms: u64,
        rtt: Duration,
        sample_age: Option<Duration>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PlayerSnapshot {
    pub id: String,
    pub kind: PlayerKind,
    pub backend: BackendKind,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MediaSnapshot {
    pub player: Option<PlayerSnapshot>,
    pub track: Option<TrackMeta>,
    pub playback: PlaybackStatus,
    pub position_ms: u64,
    pub backend: BackendKind,
}

impl Default for MediaSnapshot {
    fn default() -> Self {
        Self {
            player: None,
            track: None,
            playback: PlaybackStatus::Stopped,
            position_ms: 0,
            backend: BackendKind::current(),
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
    fn packaged_aumids_map_to_friendly_names() {
        assert_eq!(
            player_short_name("SpotifyAB.SpotifyMusic_zpdnkdrmxy60w!SpotifyMusic"),
            "spotify"
        );
        assert_eq!(
            player_short_name("Microsoft.ZuneMusic_8wekyb3d8bbwe!Microsoft.ZuneMusic"),
            "media-player"
        );
        assert_eq!(
            PlayerKind::from_id("Microsoft.ZuneMusic_8wekyb3d8bbwe!Microsoft.ZuneMusic"),
            PlayerKind::Native
        );
        // Unmapped packaged apps keep their PRAID without a dot split.
        assert_eq!(
            player_short_name("SomeApp_abc123!SomeApp.Player"),
            "SomeApp.Player"
        );
    }

    #[test]
    fn backend_kind_reports_platform_support() {
        let backend = BackendKind::current();
        assert!(backend.is_supported(), "{}", backend.as_str());
        assert_ne!(backend.as_str(), "unsupported");
        assert_eq!(backend, BackendKind::from_name(backend.as_str()).unwrap());
        assert_eq!(BackendKind::from_name("mpris"), Some(BackendKind::Mpris));
        assert_eq!(BackendKind::from_name("mpd"), Some(BackendKind::Mpd));
        assert_eq!(
            BackendKind::from_name("webnowplaying"),
            Some(BackendKind::WebNowPlaying)
        );
        assert_eq!(BackendKind::from_name("smtc"), Some(BackendKind::Smtc));
        assert_eq!(
            BackendKind::from_name("applescript"),
            Some(BackendKind::AppleScript)
        );
        assert_eq!(
            BackendKind::from_name("mediaremote"),
            Some(BackendKind::MediaRemote)
        );
        assert_eq!(
            BackendKind::from_name("unsupported"),
            Some(BackendKind::Unsupported)
        );
        assert_eq!(BackendKind::from_name("unknown"), None);

        assert_eq!(BackendKind::Mpris.priority_rank(), 0);
        assert_eq!(BackendKind::Smtc.priority_rank(), 0);
        assert_eq!(BackendKind::MediaRemote.priority_rank(), 0);
        assert_eq!(BackendKind::AppleScript.priority_rank(), 1);
        assert_eq!(BackendKind::Mpd.priority_rank(), 2);
        assert_eq!(BackendKind::WebNowPlaying.priority_rank(), 3);
    }

    #[test]
    fn player_kind_normalization_handles_windows_and_browsers() {
        assert_eq!(PlayerKind::from_id("Spotify.exe"), PlayerKind::Native);
        assert_eq!(PlayerKind::from_id("msedge.exe"), PlayerKind::Browser);
        assert_eq!(PlayerKind::from_id("chrome.exe"), PlayerKind::Browser);
        assert_eq!(PlayerKind::from_id("foobar2000.exe"), PlayerKind::Native);
        assert_eq!(PlayerKind::from_id("mpd"), PlayerKind::Native);
        assert_eq!(PlayerKind::from_id("webnowplaying"), PlayerKind::Browser);
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

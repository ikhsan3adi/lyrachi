use std::collections::HashMap;

use zbus::OwnedVariant;
pub use zbus::fdo::{DBusProxy, PropertiesProxy};

use crate::core::clock::PlaybackStatus;

#[zbus::proxy(
    default_service = "org.mpris.MediaPlayer2",
    default_path = "/org/mpris/MediaPlayer2",
    interface = "org.mpris.MediaPlayer2.Player"
)]
pub trait Player {
    #[zbus(property)]
    fn playback_status(&self) -> zbus::Result<String>;

    #[zbus(property)]
    fn metadata(&self) -> zbus::Result<HashMap<String, OwnedVariant>>;

    #[zbus(property)]
    fn rate(&self) -> zbus::Result<f64>;

    #[zbus(property(emits_changed_signal = "false"))]
    fn position(&self) -> zbus::Result<i64>;

    #[zbus(signal)]
    fn seeked(&self, position: i64) -> zbus::Result<()>;
}

pub fn parse_playback_status(value: &str) -> PlaybackStatus {
    match value {
        "Playing" => PlaybackStatus::Playing,
        "Paused" => PlaybackStatus::Paused,
        _ => PlaybackStatus::Stopped,
    }
}

pub fn micros_to_ms(value: i64) -> u64 {
    if value <= 0 {
        0
    } else {
        (value as u64) / 1_000
    }
}

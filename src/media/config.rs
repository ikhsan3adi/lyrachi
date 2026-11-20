use std::fmt;
use std::net::{IpAddr, Ipv4Addr};
use std::time::Duration;

use super::BackendKind;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MediaConfig {
    pub backends: Vec<BackendKind>,
    pub mpd: MpdConfig,
    pub webnowplaying: WebNowPlayingConfig,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MpdConfig {
    pub enabled: bool,
    pub host: String,
    pub port: u16,
    pub timeout: Duration,
}

impl Default for MpdConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            host: "127.0.0.1".to_string(),
            port: 6600,
            timeout: Duration::from_secs(2),
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct WebNowPlayingConfig {
    pub enabled: bool,
    pub bind_address: IpAddr,
    pub port: u16,
    pub allowed_origins: Vec<String>,
    pub token: Option<String>,
}

impl Default for WebNowPlayingConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            bind_address: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 8974,
            allowed_origins: Vec::new(),
            token: None,
        }
    }
}

pub fn is_valid_extension_origin(origin: &str) -> bool {
    let Some(rest) = origin
        .strip_prefix("chrome-extension://")
        .or_else(|| origin.strip_prefix("moz-extension://"))
    else {
        return false;
    };
    !rest.is_empty()
        && !rest.contains(['*', '/', '?', '#', ' '])
        && rest
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
}

impl fmt::Debug for WebNowPlayingConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WebNowPlayingConfig")
            .field("enabled", &self.enabled)
            .field("bind_address", &self.bind_address)
            .field("port", &self.port)
            .field("allowed_origins", &self.allowed_origins)
            .field("token", &self.token.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

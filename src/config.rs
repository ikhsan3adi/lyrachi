use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::app::resolve::L1_CACHE_CAPACITY;
use crate::cli::Args;
use crate::media::config::is_valid_extension_origin;
use crate::media::{BackendKind, MediaConfig};
use crate::providers::Provider;
use crate::style::{
    DEFAULT_STYLE_NAME,
    color::{ColorMode, parse_rgb_hint},
};
use crate::ui::DisplayMode;

pub const DEFAULT_GLOBAL_OFFSET_MS: i64 = -300;
const MIN_OFFSET_MS: i64 = -30_000;
const MAX_OFFSET_MS: i64 = 30_000;
const MAX_LOOKAHEAD_MS: u64 = 5_000;
const MIN_L1_ENTRIES: usize = 1;
const MAX_L1_ENTRIES: usize = 1_024;
const MIN_MEDIA_TIMEOUT_MS: u64 = 100;
const MAX_MEDIA_TIMEOUT_MS: u64 = 30_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeConfig {
    pub player_filter: Option<String>,
    pub global_offset_ms: i64,
    pub lookahead_ms: u64,
    pub provider_order: Vec<Provider>,
    pub network: bool,
    pub local_dirs: Vec<PathBuf>,
    pub media: MediaConfig,
    pub l1_entries: usize,
    pub fullscreen: bool,
    pub show_header: bool,
    pub show_header_override: Option<bool>,
    pub status_bar: StatusBarMode,
    pub display_mode: DisplayMode,
    pub word_highlight: bool,
    pub fps_active: u16,
    pub style: String,
    pub color_mode: ColorMode,
    pub bg_blend_hint: Option<(u8, u8, u8)>,
    pub ambiguous_width: u8,
    pub unicode: TerminalUnicode,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            player_filter: None,
            global_offset_ms: DEFAULT_GLOBAL_OFFSET_MS,
            lookahead_ms: 0,
            provider_order: Provider::all().to_vec(),
            network: true,
            local_dirs: Vec::new(),
            media: MediaConfig::default(),
            l1_entries: L1_CACHE_CAPACITY,
            fullscreen: false,
            show_header: true,
            show_header_override: None,
            status_bar: StatusBarMode::default(),
            display_mode: DisplayMode::Normal,
            word_highlight: true,
            fps_active: 60,
            style: DEFAULT_STYLE_NAME.to_string(),
            color_mode: ColorMode::Auto,
            bg_blend_hint: None,
            ambiguous_width: 1,
            unicode: TerminalUnicode::Full,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigLoadOutcome {
    pub config: RuntimeConfig,
    pub warnings: Vec<String>,
}

impl RuntimeConfig {
    pub fn resolve(
        config_path: &Path,
        default_music_dirs: Vec<PathBuf>,
        args: &Args,
    ) -> ConfigLoadOutcome {
        let mut config = Self {
            local_dirs: default_music_dirs,
            ..Self::default()
        };
        let mut warnings = Vec::new();

        match load_file(config_path) {
            Ok(Some(file)) => apply_file(&mut config, file, &mut warnings),
            Ok(None) => {}
            Err(warning) => warnings.push(warning),
        }

        apply_args(&mut config, args, &mut warnings);

        ConfigLoadOutcome { config, warnings }
    }
}

fn load_file(path: &Path) -> Result<Option<FileConfig>, String> {
    let source = match fs::read_to_string(path) {
        Ok(source) => source,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(format!("could not read config {}: {error}", path.display()));
        }
    };

    toml::from_str::<FileConfig>(&source)
        .map(Some)
        .map_err(|error| format_toml_error(&source, &error))
}

fn format_toml_error(source: &str, error: &toml::de::Error) -> String {
    let mut message = format!("invalid config: {}", error.message());
    if let Some(span) = error.span() {
        let line = source
            .as_bytes()
            .get(..span.start.min(source.len()))
            .map_or(1, |prefix| {
                prefix.iter().filter(|byte| **byte == b'\n').count() + 1
            });
        message.push_str(&format!(" at line {line}"));
    }
    message
}

fn apply_file(config: &mut RuntimeConfig, file: FileConfig, warnings: &mut Vec<String>) {
    if let Some(version) = file.schema_version
        && version != 1
    {
        warnings.push(format!(
            "config: unsupported schema_version {version}; expected 1"
        ));
    }

    if let Some(filter) = file.player.filter {
        config.player_filter = non_empty(filter);
    }
    if let Some(offset_ms) = file.sync.global_offset_ms {
        config.global_offset_ms = clamp_offset(offset_ms, "sync.global_offset_ms", warnings);
    }
    if let Some(lookahead_ms) = file.sync.lookahead_ms {
        config.lookahead_ms = clamp_lookahead(lookahead_ms, "sync.lookahead_ms", warnings);
    }
    if let Some(order) = file.providers.order
        && let Some(order) = parse_provider_order(&order, warnings)
    {
        config.provider_order = order;
    }
    if let Some(network) = file.providers.network {
        config.network = network;
    }
    if let Some(local_dirs) = file.providers.local_dirs {
        config.local_dirs = local_dirs;
    }
    apply_media(&mut config.media, file.media, warnings);
    if let Some(l1_entries) = file.cache.l1_entries {
        config.l1_entries = clamp_l1_entries(l1_entries, "cache.l1_entries", warnings);
    }
    if let Some(fullscreen) = file.ui.fullscreen {
        config.fullscreen = fullscreen;
    }
    if let Some(show_header) = file.ui.show_header {
        config.show_header = show_header;
        config.show_header_override = Some(show_header);
    }
    if let Some(name) = file.ui.display_mode {
        match DisplayMode::from_config_name(&name) {
            Some(mode) => config.display_mode = mode,
            None => warnings.push(format!(
                "config: unknown ui.display_mode `{name}`; valid values: normal, block, kinetic"
            )),
        }
    }
    if let Some(word_highlight) = file.ui.word_highlight {
        config.word_highlight = word_highlight;
    }
    if let Some(status_bar) = file.ui.status_bar {
        config.status_bar = status_bar;
    }
    if let Some(fps_active) = file.ui.fps_active {
        config.fps_active = clamp_fps_active(fps_active, "ui.fps_active", warnings);
    }
    if let Some(style) = file.ui.style {
        if let Some(style) = non_empty(style) {
            config.style = style;
        } else {
            warnings.push(format!(
                "config: ui.style cannot be empty; using {DEFAULT_STYLE_NAME}"
            ));
        }
    }
    if let Some(color_mode) = file.terminal.color_mode {
        config.color_mode = color_mode;
    }
    if let Some(hint) = file.terminal.bg_blend_hint {
        match parse_rgb_hint(&hint) {
            Ok(rgb) => config.bg_blend_hint = Some(rgb),
            Err(error) => warnings.push(format!("config: terminal.bg_blend_hint invalid: {error}")),
        }
    }
    if let Some(ambiguous_width) = file.terminal.ambiguous_width {
        config.ambiguous_width =
            clamp_ambiguous_width(ambiguous_width, "terminal.ambiguous_width", warnings);
    }
    if let Some(unicode) = file.terminal.unicode {
        config.unicode = unicode;
    }
}

fn apply_media(config: &mut MediaConfig, section: MediaSection, warnings: &mut Vec<String>) {
    if let Some(backends) = section.backends {
        let mut parsed = Vec::new();
        for name in backends {
            let Some(kind) = BackendKind::from_name(name.trim()) else {
                warnings.push(format!(
                    "config: unknown media backend `{name}`; valid values: mpris, mpd, webnowplaying, smtc, applescript"
                ));
                continue;
            };
            if kind == BackendKind::Unsupported {
                warnings.push("config: media backend `unsupported` cannot be selected".to_string());
                continue;
            }
            if !kind.is_supported() {
                warnings.push(format!(
                    "config: media backend `{}` is not available on this platform",
                    kind.as_str()
                ));
                continue;
            }
            if !parsed.contains(&kind) {
                parsed.push(kind);
            }
        }
        config.backends = parsed;
    }

    if let Some(enabled) = section.mpd.enabled {
        config.mpd.enabled = enabled;
    }
    if let Some(host) = section.mpd.host {
        let host = host.trim();
        if host.is_empty() {
            warnings.push("config: media.mpd.host cannot be empty; using 127.0.0.1".to_string());
        } else {
            config.mpd.host = host.to_string();
        }
    }
    if let Some(port) = section.mpd.port {
        config.mpd.port = clamp_media_port(port, "media.mpd.port", warnings);
    }
    if let Some(timeout_ms) = section.mpd.timeout_ms {
        config.mpd.timeout = clamp_media_timeout(timeout_ms, "media.mpd.timeout_ms", warnings);
    }

    if let Some(enabled) = section.webnowplaying.enabled {
        config.webnowplaying.enabled = enabled;
    }
    if let Some(bind_address) = section.webnowplaying.bind_address {
        match bind_address.trim().parse::<std::net::IpAddr>() {
            Ok(address) if address.is_loopback() => config.webnowplaying.bind_address = address,
            Ok(_) => warnings.push(
                "config: media.webnowplaying.bind_address must be loopback; using 127.0.0.1"
                    .to_string(),
            ),
            Err(error) => warnings.push(format!(
                "config: media.webnowplaying.bind_address invalid ({error}); using 127.0.0.1"
            )),
        }
    }
    if let Some(port) = section.webnowplaying.port {
        config.webnowplaying.port = clamp_media_port(port, "media.webnowplaying.port", warnings);
    }
    if let Some(origins) = section.webnowplaying.allowed_origins {
        let mut allowed = Vec::new();
        for origin in origins {
            let origin = origin.trim();
            if is_valid_extension_origin(origin) {
                if !allowed.iter().any(|value: &String| value == origin) {
                    allowed.push(origin.to_string());
                }
            } else {
                warnings.push(format!(
                    "config: media.webnowplaying.allowed_origins entry `{origin}` must be a chrome-extension:// or moz-extension:// origin"
                ));
            }
        }
        config.webnowplaying.allowed_origins = allowed;
    }
    if let Some(token) = section.webnowplaying.token {
        config.webnowplaying.token = non_empty(token);
    }

    if !config.backends.is_empty() {
        for (kind, enabled, section_name) in [
            (BackendKind::Mpd, config.mpd.enabled, "mpd"),
            (
                BackendKind::WebNowPlaying,
                config.webnowplaying.enabled,
                "webnowplaying",
            ),
        ] {
            let selected = config.backends.contains(&kind);
            if selected && !enabled {
                warnings.push(format!(
                    "config: media.backends includes `{section_name}` but media.{section_name}.enabled is false"
                ));
            } else if !selected && enabled {
                warnings.push(format!(
                    "config: media.{section_name}.enabled is true but `{section_name}` is not listed in media.backends"
                ));
            }
        }
    }
}

fn clamp_media_port(value: u32, field: &str, warnings: &mut Vec<String>) -> u16 {
    let clamped = value.clamp(1, u32::from(u16::MAX)) as u16;
    if u32::from(clamped) != value {
        warnings.push(format!(
            "config: {field} must be 1..=65535; using {clamped}"
        ));
    }
    clamped
}

fn clamp_media_timeout(value: u64, field: &str, warnings: &mut Vec<String>) -> std::time::Duration {
    let clamped = value.clamp(MIN_MEDIA_TIMEOUT_MS, MAX_MEDIA_TIMEOUT_MS);
    if clamped != value {
        warnings.push(format!(
            "config: {field} {value} is outside {MIN_MEDIA_TIMEOUT_MS}..={MAX_MEDIA_TIMEOUT_MS}; using {clamped}"
        ));
    }
    std::time::Duration::from_millis(clamped)
}

fn apply_args(config: &mut RuntimeConfig, args: &Args, warnings: &mut Vec<String>) {
    if let Some(player) = &args.player {
        config.player_filter = non_empty(player.clone());
    }
    if !args.local_dir.is_empty() {
        config.local_dirs.clone_from(&args.local_dir);
    }
    if let Some(offset_ms) = args.offset_ms {
        config.global_offset_ms = clamp_offset(offset_ms, "--offset-ms", warnings);
    }
    if let Some(lookahead_ms) = args.lookahead_ms {
        config.lookahead_ms = clamp_lookahead(lookahead_ms, "--lookahead-ms", warnings);
    }
    if args.offline {
        config.network = false;
    }
}

fn non_empty(value: String) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

fn parse_provider_order(values: &[String], warnings: &mut Vec<String>) -> Option<Vec<Provider>> {
    if values.is_empty() {
        warnings.push("config: providers.order must contain at least one provider".to_string());
        return None;
    }

    let mut order = Vec::new();
    for name in values {
        let Some(provider) = Provider::from_config_name(name) else {
            warnings.push(format!(
                "config: unknown provider `{name}` in providers.order; valid values: {}",
                Provider::valid_config_names()
            ));
            return None;
        };
        if !order.contains(&provider) {
            order.push(provider);
        }
    }

    Some(order)
}

fn clamp_offset(value: i64, field: &str, warnings: &mut Vec<String>) -> i64 {
    let clamped = value.clamp(MIN_OFFSET_MS, MAX_OFFSET_MS);
    if clamped != value {
        warnings.push(format!(
            "config: {field} {value} is outside {MIN_OFFSET_MS}..={MAX_OFFSET_MS}; using {clamped}"
        ));
    }
    clamped
}

fn clamp_lookahead(value: u64, field: &str, warnings: &mut Vec<String>) -> u64 {
    let clamped = value.min(MAX_LOOKAHEAD_MS);
    if clamped != value {
        warnings.push(format!(
            "config: {field} {value} is outside 0..={MAX_LOOKAHEAD_MS}; using {clamped}"
        ));
    }
    clamped
}

fn clamp_l1_entries(value: usize, field: &str, warnings: &mut Vec<String>) -> usize {
    let clamped = value.clamp(MIN_L1_ENTRIES, MAX_L1_ENTRIES);
    if clamped != value {
        warnings.push(format!(
            "config: {field} {value} is outside {MIN_L1_ENTRIES}..={MAX_L1_ENTRIES}; using {clamped}"
        ));
    }
    clamped
}

fn clamp_fps_active(value: u16, field: &str, warnings: &mut Vec<String>) -> u16 {
    let clamped = value.clamp(15, 120);
    if clamped != value {
        warnings.push(format!(
            "config: {field} {value} is outside 15..=120; using {clamped}"
        ));
    }
    clamped
}

fn clamp_ambiguous_width(value: u8, field: &str, warnings: &mut Vec<String>) -> u8 {
    let clamped = if value == 2 { 2 } else { 1 };
    if clamped != value {
        warnings.push(format!(
            "config: {field} {value} is outside 1..=2; using {clamped}"
        ));
    }
    clamped
}

#[derive(Debug, Clone, Default, Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct FileConfig {
    pub schema_version: Option<u32>,
    pub player: PlayerSection,
    pub sync: SyncSection,
    pub providers: ProvidersSection,
    pub media: MediaSection,
    pub cache: CacheSection,
    pub ui: UiSection,
    pub terminal: TerminalSection,
}

#[derive(Debug, Clone, Default, Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct PlayerSection {
    pub filter: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct SyncSection {
    pub global_offset_ms: Option<i64>,
    pub lookahead_ms: Option<u64>,
}

#[derive(Debug, Clone, Default, Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ProvidersSection {
    pub order: Option<Vec<String>>,
    pub network: Option<bool>,
    pub local_dirs: Option<Vec<PathBuf>>,
}

#[derive(Debug, Clone, Default, Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct MediaSection {
    pub backends: Option<Vec<String>>,
    pub mpd: MpdSection,
    pub webnowplaying: WebNowPlayingSection,
}

#[derive(Debug, Clone, Default, Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct MpdSection {
    pub enabled: Option<bool>,
    pub host: Option<String>,
    pub port: Option<u32>,
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Default, Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct WebNowPlayingSection {
    pub enabled: Option<bool>,
    pub bind_address: Option<String>,
    pub port: Option<u32>,
    pub allowed_origins: Option<Vec<String>>,
    pub token: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct CacheSection {
    pub l1_entries: Option<usize>,
}

#[derive(Debug, Clone, Default, Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct UiSection {
    pub fullscreen: Option<bool>,
    pub show_header: Option<bool>,
    pub status_bar: Option<StatusBarMode>,
    pub display_mode: Option<String>,
    pub word_highlight: Option<bool>,
    pub fps_active: Option<u16>,
    pub style: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum StatusBarMode {
    #[default]
    Bottom,
    Top,
    Off,
    Auto,
}

impl StatusBarMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Bottom => "bottom",
            Self::Top => "top",
            Self::Off => "off",
            Self::Auto => "auto",
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct TerminalSection {
    pub ambiguous_width: Option<u8>,
    pub unicode: Option<TerminalUnicode>,
    pub color_mode: Option<ColorMode>,
    pub bg_blend_hint: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum TerminalUnicode {
    #[default]
    Full,
    Ascii,
}

impl TerminalUnicode {
    pub fn is_full(self) -> bool {
        self == Self::Full
    }
}

pub const DEFAULT_CONFIG_TEMPLATE: &str = r##"# lyrachi default configuration (schema version 1)
schema_version = 1

[player]
# Only consider this media player (for example: mpv, spotify, firefox)
# filter = "mpv"

[sync]
# Manual lyric delay in milliseconds (positive values delay lyrics)
global_offset_ms = -300
# Lookahead in milliseconds (render transitions earlier)
lookahead_ms = 0

[media]
# Optional backend allowlist. Empty uses the platform native backend plus any
# enabled network backend below.
backends = []

[media.mpd]
# Connect to a local Music Player Daemon.
enabled = false
host = "127.0.0.1"
port = 6600
timeout_ms = 2000

[media.webnowplaying]
# Local WebSocket bridge for the WebNowPlaying browser extension.
enabled = false
bind_address = "127.0.0.1"
port = 8974
# Exact extension origins. Wildcards and http(s) origins are rejected.
allowed_origins = []
# token = ""

[providers]
# Provider resolution order
order = ["local", "betterlyrics", "unison", "lrclib", "lyricsplus", "ytmusic"]
# Enable or disable network lyric providers
network = true
# Additional local lyric directories
local_dirs = []

[cache]
# Maximum number of in-memory L1 cache entries
l1_entries = 64

[ui]
# Start in fullscreen mode
fullscreen = false
# Display track title and artist header
show_header = true
# Status bar placement: "bottom", "top", "off", or "auto"
status_bar = "bottom"
# Display mode: "normal", "block", or "kinetic"
display_mode = "normal"
# Enable word-level karaoke highlighting
word_highlight = true
# Frame rate while wipe/effects/countdown animations are active (15..=120)
fps_active = 60
# Active style name or preset (e.g. "builtin:default", "builtin:dark", "builtin:aurora", "builtin:minimal")
style = "builtin:default"

[terminal]
# Ambiguous-width Unicode character width: 1 or 2
ambiguous_width = 1
# Unicode glyph support: "full" or "ascii"
unicode = "full"
# Terminal color capability mode: "auto", "truecolor", "256", or "16"
color_mode = "auto"
# Optional fallback background RGB color hint (e.g. "#1a1b26")
# bg_blend_hint = ""
"##;

pub fn check_config(
    config_path: &Path,
    styles_dir: Option<&Path>,
) -> Result<(String, Vec<String>), String> {
    let mut warnings = Vec::new();
    let mut style_name = DEFAULT_STYLE_NAME.to_string();

    if config_path.exists() {
        let source = fs::read_to_string(config_path).map_err(|error| {
            format!(
                "could not read config file {}: {error}",
                config_path.display()
            )
        })?;

        let file: FileConfig =
            toml::from_str(&source).map_err(|error| format_toml_error(&source, &error))?;

        if let Some(style) = file.ui.style.as_ref()
            && !style.trim().is_empty()
        {
            style_name = style.clone();
        }

        let mut runtime = RuntimeConfig::default();
        apply_file(&mut runtime, file, &mut warnings);
    } else {
        warnings.push(format!(
            "config file not found at {}; using default configuration",
            config_path.display()
        ));
    }

    let style_outcome = crate::style::loader::load_style(&style_name, styles_dir, 80)
        .map_err(|error| format!("error in style `{style_name}`: {error}"))?;

    warnings.extend(style_outcome.warnings);

    Ok((style_name, warnings))
}

pub fn run_config_action(
    action: crate::cli::ConfigAction,
    config_path: &Path,
    styles_dir: Option<&Path>,
) -> Result<(), String> {
    match action {
        crate::cli::ConfigAction::PrintDefault => {
            print!("{DEFAULT_CONFIG_TEMPLATE}");
            Ok(())
        }
        crate::cli::ConfigAction::Schema { kind } => {
            match kind {
                crate::cli::SchemaKind::Config => {
                    let schema = schemars::schema_for!(FileConfig);
                    let json = serde_json::to_string_pretty(&schema)
                        .map_err(|error| format!("failed to serialize config schema: {error}"))?;
                    println!("{json}");
                }
                crate::cli::SchemaKind::Style => {
                    let schema = schemars::schema_for!(crate::style::StyleProfile);
                    let json = serde_json::to_string_pretty(&schema)
                        .map_err(|error| format!("failed to serialize style schema: {error}"))?;
                    println!("{json}");
                }
            }
            Ok(())
        }
        crate::cli::ConfigAction::Check => {
            let (style_name, warnings) = check_config(config_path, styles_dir)?;
            if warnings.is_empty() {
                println!("OK: configuration and active style `{style_name}` are valid.");
            } else {
                println!(
                    "OK: configuration and active style `{style_name}` passed with {} warning(s):",
                    warnings.len()
                );
                for warning in &warnings {
                    println!("  warning: {warning}");
                }
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use clap::Parser;

    use super::*;

    fn temp_file(name: &str, contents: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "lyrachi-{name}-{}-{unique}.toml",
            std::process::id()
        ));
        fs::write(&path, contents).unwrap();
        path
    }

    fn args(values: &[&str]) -> Args {
        let mut argv = vec!["lyrachi"];
        argv.extend_from_slice(values);
        Args::try_parse_from(argv).unwrap()
    }

    #[test]
    fn missing_config_uses_defaults_without_warnings() {
        let path = std::env::temp_dir().join("lyrachi-does-not-exist.toml");
        let outcome = RuntimeConfig::resolve(&path, vec![PathBuf::from("/music")], &args(&[]));

        assert!(outcome.warnings.is_empty());
        assert_eq!(outcome.config.global_offset_ms, DEFAULT_GLOBAL_OFFSET_MS);
        assert_eq!(outcome.config.local_dirs, vec![PathBuf::from("/music")]);
        assert_eq!(outcome.config.provider_order, Provider::all());
    }

    #[test]
    fn config_overrides_defaults_and_cli_overrides_config() {
        let path = temp_file(
            "precedence",
            r#"
[player]
filter = "mpv"
[sync]
global_offset_ms = 200
lookahead_ms = 120
[providers]
network = false
local_dirs = ["/from-config"]
[cache]
l1_entries = 8
"#,
        );

        let file_only = RuntimeConfig::resolve(&path, Vec::new(), &args(&[]));
        assert_eq!(file_only.config.player_filter.as_deref(), Some("mpv"));
        assert_eq!(file_only.config.global_offset_ms, 200);
        assert_eq!(file_only.config.lookahead_ms, 120);
        assert!(!file_only.config.network);
        assert_eq!(
            file_only.config.local_dirs,
            vec![PathBuf::from("/from-config")]
        );
        assert_eq!(file_only.config.l1_entries, 8);

        let cli = RuntimeConfig::resolve(
            &path,
            Vec::new(),
            &args(&[
                "--player",
                "vlc",
                "--offset-ms",
                "-300",
                "--lookahead-ms",
                "40",
                "--local-dir",
                "/from-cli",
                "--offline",
            ]),
        );
        assert_eq!(cli.config.player_filter.as_deref(), Some("vlc"));
        assert_eq!(cli.config.global_offset_ms, -300);
        assert_eq!(cli.config.lookahead_ms, 40);
        assert_eq!(cli.config.local_dirs, vec![PathBuf::from("/from-cli")]);
        assert!(!cli.config.network);

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn loads_media_backend_configuration() {
        let path = temp_file(
            "media-backends",
            r#"
[media]
backends = ["mpd", "webnowplaying"]

[media.mpd]
enabled = true
host = "127.0.0.1"
port = 6601
timeout_ms = 1500

[media.webnowplaying]
enabled = true
bind_address = "::1"
port = 9000
allowed_origins = ["chrome-extension://abcdefghijklmnop", "moz-extension://1234-abcd"]
token = "secret"
"#,
        );
        let outcome = RuntimeConfig::resolve(&path, Vec::new(), &args(&[]));

        assert!(outcome.warnings.is_empty(), "{:?}", outcome.warnings);
        assert_eq!(
            outcome.config.media.backends,
            vec![BackendKind::Mpd, BackendKind::WebNowPlaying]
        );
        assert!(outcome.config.media.mpd.enabled);
        assert_eq!(outcome.config.media.mpd.port, 6601);
        assert_eq!(
            outcome.config.media.mpd.timeout,
            std::time::Duration::from_millis(1500)
        );
        assert!(outcome.config.media.webnowplaying.enabled);
        assert!(
            outcome
                .config
                .media
                .webnowplaying
                .bind_address
                .is_loopback()
        );
        assert_eq!(outcome.config.media.webnowplaying.port, 9000);
        assert_eq!(
            outcome.config.media.webnowplaying.allowed_origins,
            vec![
                "chrome-extension://abcdefghijklmnop".to_string(),
                "moz-extension://1234-abcd".to_string()
            ]
        );
        assert_eq!(
            outcome.config.media.webnowplaying.token.as_deref(),
            Some("secret")
        );

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn rejects_unsafe_media_network_configuration() {
        let path = temp_file(
            "media-unsafe",
            r#"
[media]
backends = ["mpd", "webnowplaying", "bogus"]

[media.mpd]
enabled = true
port = 0
timeout_ms = 10

[media.webnowplaying]
enabled = true
bind_address = "0.0.0.0"
port = 70000
allowed_origins = ["http://localhost", "chrome-extension://*", "moz-extension://valid-id"]
"#,
        );
        let outcome = RuntimeConfig::resolve(&path, Vec::new(), &args(&[]));

        assert_eq!(outcome.config.media.mpd.port, 1);
        assert_eq!(
            outcome.config.media.mpd.timeout,
            std::time::Duration::from_millis(MIN_MEDIA_TIMEOUT_MS)
        );
        assert!(
            outcome
                .config
                .media
                .webnowplaying
                .bind_address
                .is_loopback()
        );
        assert_eq!(
            outcome.config.media.webnowplaying.allowed_origins,
            vec!["moz-extension://valid-id".to_string()]
        );
        assert!(
            outcome
                .warnings
                .iter()
                .any(|warning| warning.contains("bogus"))
        );
        assert!(
            outcome
                .warnings
                .iter()
                .any(|warning| warning.contains("http://localhost"))
        );
        assert!(
            outcome
                .warnings
                .iter()
                .any(|warning| warning.contains("must be loopback"))
        );

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn loads_ui_layout_preferences() {
        let path = temp_file(
            "ui-layout",
            "[ui]\nfullscreen = true\nshow_header = false\nstatus_bar = \"off\"\n",
        );
        let outcome = RuntimeConfig::resolve(&path, Vec::new(), &args(&[]));

        assert!(outcome.config.fullscreen);
        assert!(!outcome.config.show_header);
        assert_eq!(outcome.config.status_bar, StatusBarMode::Off);

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn loads_display_mode_and_word_highlight() {
        let path = temp_file(
            "ui-display",
            "[ui]\ndisplay_mode = \"block\"\nword_highlight = false\n",
        );
        let outcome = RuntimeConfig::resolve(&path, Vec::new(), &args(&[]));

        assert_eq!(outcome.config.display_mode, DisplayMode::Block);
        assert!(!outcome.config.word_highlight);
        assert!(outcome.warnings.is_empty());

        fs::remove_file(&path).unwrap();

        let kinetic_path = temp_file("ui-display-kinetic", "[ui]\ndisplay_mode = \"kinetic\"\n");
        let kinetic_outcome = RuntimeConfig::resolve(&kinetic_path, Vec::new(), &args(&[]));

        assert_eq!(kinetic_outcome.config.display_mode, DisplayMode::Kinetic);
        assert!(kinetic_outcome.warnings.is_empty());

        fs::remove_file(kinetic_path).unwrap();
    }

    #[test]
    fn warns_on_unknown_display_mode() {
        let path = temp_file("ui-display-invalid", "[ui]\ndisplay_mode = \"marquee\"\n");
        let outcome = RuntimeConfig::resolve(&path, Vec::new(), &args(&[]));

        assert_eq!(outcome.config.display_mode, DisplayMode::Normal);
        assert!(
            outcome
                .warnings
                .iter()
                .any(|warning| warning.contains("display_mode") && warning.contains("marquee"))
        );

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn loads_and_clamps_active_frame_rate() {
        let path = temp_file("ui-fps", "[ui]\nfps_active = 30\n");
        let outcome = RuntimeConfig::resolve(&path, Vec::new(), &args(&[]));
        assert_eq!(outcome.config.fps_active, 30);
        assert!(outcome.warnings.is_empty());
        fs::remove_file(path).unwrap();

        let path = temp_file("ui-fps-low", "[ui]\nfps_active = 5\n");
        let outcome = RuntimeConfig::resolve(&path, Vec::new(), &args(&[]));
        assert_eq!(outcome.config.fps_active, 15);
        assert!(
            outcome
                .warnings
                .iter()
                .any(|warning| warning.contains("fps_active"))
        );
        fs::remove_file(path).unwrap();

        let path = temp_file("ui-fps-high", "[ui]\nfps_active = 240\n");
        let outcome = RuntimeConfig::resolve(&path, Vec::new(), &args(&[]));
        assert_eq!(outcome.config.fps_active, 120);

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn loads_terminal_options() {
        let path = temp_file(
            "terminal-options",
            "[terminal]\nambiguous_width = 2\nunicode = \"ascii\"\n",
        );
        let outcome = RuntimeConfig::resolve(&path, Vec::new(), &args(&[]));

        assert_eq!(outcome.config.ambiguous_width, 2);
        assert_eq!(outcome.config.unicode, TerminalUnicode::Ascii);
        assert!(outcome.warnings.is_empty());

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn clamps_invalid_ambiguous_width() {
        let path = temp_file("ambiguous-width", "[terminal]\nambiguous_width = 3\n");
        let outcome = RuntimeConfig::resolve(&path, Vec::new(), &args(&[]));

        assert_eq!(outcome.config.ambiguous_width, 1);
        assert!(
            outcome
                .warnings
                .iter()
                .any(|warning| warning.contains("ambiguous_width"))
        );

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn unknown_key_warns_and_falls_back_to_defaults() {
        let path = temp_file(
            "unknown-key",
            "fulscreen = true\n[sync]\nlookahead_ms = 200\n",
        );
        let outcome = RuntimeConfig::resolve(&path, Vec::new(), &args(&[]));

        assert_eq!(outcome.config, RuntimeConfig::default());
        assert!(
            outcome
                .warnings
                .iter()
                .any(|warning| warning.contains("fulscreen") && warning.contains("line 1"))
        );

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn clamps_numeric_config_values() {
        let path = temp_file(
            "clamping",
            "[sync]\nglobal_offset_ms = 40000\nlookahead_ms = 9000\n[cache]\nl1_entries = 0\n",
        );
        let outcome = RuntimeConfig::resolve(&path, Vec::new(), &args(&[]));

        assert_eq!(outcome.config.global_offset_ms, 30_000);
        assert_eq!(outcome.config.lookahead_ms, 5_000);
        assert_eq!(outcome.config.l1_entries, 1);
        assert_eq!(outcome.warnings.len(), 3);
        assert!(
            outcome
                .warnings
                .iter()
                .any(|warning| warning.contains("global_offset_ms"))
        );

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn rejects_unknown_provider_with_valid_names() {
        let path = temp_file(
            "provider-order",
            "[providers]\norder = [\"local\", \"bogus\"]\n",
        );
        let outcome = RuntimeConfig::resolve(&path, Vec::new(), &args(&[]));

        assert_eq!(outcome.config.provider_order, Provider::all());
        assert!(outcome.warnings.iter().any(|warning| {
            warning.contains("bogus")
                && warning.contains("local")
                && warning.contains("betterlyrics")
                && warning.contains("unison")
                && warning.contains("lrclib")
                && warning.contains("lyricsplus")
        }));

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn accepts_open_providers_in_order() {
        let path = temp_file(
            "open-provider-order",
            "[providers]\norder = [\"local\", \"betterlyrics\", \"unison\", \"lrclib\", \"lyricsplus\"]\n",
        );
        let outcome = RuntimeConfig::resolve(&path, Vec::new(), &args(&[]));

        assert!(outcome.warnings.is_empty());
        assert_eq!(
            outcome.config.provider_order,
            vec![
                Provider::Local,
                Provider::BetterLyrics,
                Provider::Unison,
                Provider::Lrclib,
                Provider::LyricsPlus
            ]
        );

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn cli_offsets_are_clamped_too() {
        let path = temp_file("cli-clamp", "");
        let outcome = RuntimeConfig::resolve(&path, Vec::new(), &args(&["--offset-ms", "999999"]));

        assert_eq!(outcome.config.global_offset_ms, 30_000);
        assert!(!outcome.warnings.is_empty());

        fs::remove_file(path).unwrap();
    }

    #[test]
    #[cfg(feature = "ytmusic")]
    fn opt_in_ytmusic_in_provider_order_is_accepted() {
        let path = temp_file(
            "ytmusic-order",
            "[providers]\norder = [\"local\", \"lrclib\", \"lyricsplus\", \"ytmusic\"]\n",
        );
        let outcome = RuntimeConfig::resolve(&path, Vec::new(), &args(&[]));

        assert!(outcome.warnings.is_empty());
        assert_eq!(
            outcome.config.provider_order,
            vec![
                Provider::Local,
                Provider::Lrclib,
                Provider::LyricsPlus,
                Provider::YtMusic
            ]
        );

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn loads_ui_status_bar_modes() {
        for (raw, expected) in [
            ("auto", StatusBarMode::Auto),
            ("top", StatusBarMode::Top),
            ("bottom", StatusBarMode::Bottom),
            ("off", StatusBarMode::Off),
        ] {
            let path = temp_file(
                &format!("status-bar-{raw}"),
                &format!("[ui]\nstatus_bar = \"{raw}\"\n"),
            );
            let outcome = RuntimeConfig::resolve(&path, Vec::new(), &args(&[]));
            assert!(outcome.warnings.is_empty());
            assert_eq!(outcome.config.status_bar, expected);
            fs::remove_file(path).unwrap();
        }
    }

    #[test]
    fn warns_on_invalid_status_bar_mode() {
        let path = temp_file("status-bar-invalid", "[ui]\nstatus_bar = \"invalid\"\n");
        let outcome = RuntimeConfig::resolve(&path, Vec::new(), &args(&[]));
        assert_eq!(outcome.config.status_bar, StatusBarMode::Bottom);
        assert!(!outcome.warnings.is_empty());
        assert!(outcome.warnings[0].contains("unknown variant"));
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn loads_ui_style_and_terminal_color_settings() {
        let path = temp_file(
            "style-terminal",
            "[ui]\nstyle = \"builtin:aurora\"\nshow_header = false\n[terminal]\ncolor_mode = \"256\"\nbg_blend_hint = \"#1a1b26\"\n",
        );
        let outcome = RuntimeConfig::resolve(&path, Vec::new(), &args(&[]));

        assert!(outcome.warnings.is_empty(), "{:?}", outcome.warnings);
        assert_eq!(outcome.config.style, "builtin:aurora");
        assert_eq!(outcome.config.show_header_override, Some(false));
        assert_eq!(outcome.config.color_mode, ColorMode::Ansi256);
        assert_eq!(outcome.config.bg_blend_hint, Some((0x1a, 0x1b, 0x26)));

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn warns_on_invalid_bg_blend_hint() {
        let path = temp_file(
            "blend-invalid",
            "[terminal]\ncolor_mode = \"16\"\nbg_blend_hint = \"reset\"\n",
        );
        let outcome = RuntimeConfig::resolve(&path, Vec::new(), &args(&[]));

        assert_eq!(outcome.config.color_mode, ColorMode::Ansi16);
        assert_eq!(outcome.config.bg_blend_hint, None);
        assert!(
            outcome
                .warnings
                .iter()
                .any(|warning| warning.contains("bg_blend_hint"))
        );

        fs::remove_file(path).unwrap();
    }
}

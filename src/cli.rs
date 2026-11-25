use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

#[derive(Debug, Parser)]
#[command(
    name = "lyrachi",
    version,
    about = "Synchronized lyrics with animated visual stages in your terminal",
    after_help = "\nExamples:\n  lyrachi                        start the TUI with the resolved config\n  lyrachi --player spotify       follow only Spotify\n  lyrachi status --json          one JSON object for scripts\n  lyrachi pipe --format json     stream lyric lines to a status bar\n  lyrachi config check           validate config.toml and the active style\n\nEnvironment:\n  LYRACHI_HOME  put config, data, cache, state, and Music under one tree\n  LYRACHI_LOG   tracing filter, for example: info or lyrachi=debug"
)]
pub struct Args {
    /// Only consider this media player (for example: mpv, spotify, firefox).
    #[arg(long, value_name = "NAME")]
    pub player: Option<String>,

    /// Directory to scan for local .lrc and .elrc files. May be repeated.
    #[arg(long = "local-dir", value_name = "PATH")]
    pub local_dir: Vec<PathBuf>,

    /// Manual lyric delay in milliseconds. Positive values delay the lyrics.
    #[arg(long, value_name = "MS", allow_hyphen_values = true)]
    pub offset_ms: Option<i64>,

    /// Show lyric transitions this many milliseconds earlier.
    #[arg(long, value_name = "MS")]
    pub lookahead_ms: Option<u64>,

    /// Disable all network providers.
    #[arg(long, global = true)]
    pub offline: bool,

    /// Disable reading and writing the persistent lyric cache.
    #[arg(long, global = true)]
    pub no_cache: bool,

    /// Read configuration from this file.
    #[arg(long, global = true, value_name = "PATH")]
    pub config: Option<PathBuf>,

    /// Write logs to this file instead of the default state directory.
    #[arg(long, global = true, value_name = "PATH")]
    pub log_file: Option<PathBuf>,

    /// Print media events to stdout without starting the TUI.
    #[arg(long, global = true)]
    pub dump_events: bool,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum Command {
    /// Print resolved application paths (config, styles, cache, log).
    Paths,
    /// Print the active media snapshot and lyric resolution status.
    Status {
        /// Print the status as a single JSON object.
        #[arg(long)]
        json: bool,
    },
    /// Print active lyric lines without starting the TUI.
    Pipe {
        /// Output format for status bar integrations.
        #[arg(long, value_enum, default_value = "plain")]
        format: PipeFormat,
        /// Truncate output to this display width.
        #[arg(long, value_name = "N")]
        max_width: Option<u16>,
    },
    /// Inspect, validate, or generate configuration and style schemas.
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
    /// Inspect live media sessions for debugging.
    Debug {
        #[command(subcommand)]
        target: DebugTarget,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Subcommand)]
pub enum DebugTarget {
    /// Dump every System Media Transport Controls session (Windows).
    Smtc,
    /// Dump raw per-player AppleScript output for debugging (macOS).
    Applescript,
    /// Dump `media-control` get output and raw stream frames (macOS).
    Mediaremote,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Subcommand)]
pub enum ConfigAction {
    /// Validate config.toml and the active style file, reporting any warnings.
    Check,
    /// Print default config.toml template.
    PrintDefault,
    /// Print JSON Schema Draft 2020-12 for config or style.
    Schema {
        /// Target schema kind (config or style).
        #[arg(long, value_enum, default_value = "config")]
        kind: SchemaKind,
    },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum SchemaKind {
    #[default]
    Config,
    Style,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum PipeFormat {
    #[default]
    Plain,
    Json,
}

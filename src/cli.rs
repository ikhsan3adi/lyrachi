use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "lyrachi",
    version,
    about = "Synchronized terminal lyrics for active media players"
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

    /// Read configuration from this file.
    #[arg(long, global = true, value_name = "PATH")]
    pub config: Option<PathBuf>,

    /// Write logs to this file instead of the XDG state directory.
    #[arg(long, global = true, value_name = "PATH")]
    pub log_file: Option<PathBuf>,

    /// Print media events to stdout without starting the TUI.
    #[arg(long, global = true)]
    pub dump_events: bool,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Subcommand)]
pub enum Command {
    /// Print resolved application paths.
    Paths,
    /// Print the active media snapshot and lyric resolution status.
    Status {
        /// Print the status as a single JSON object.
        #[arg(long)]
        json: bool,
    },
}

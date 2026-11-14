use std::path::PathBuf;

use clap::Parser;

/// Command line arguments for the MVP.
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
    #[arg(
        long,
        default_value_t = 0,
        value_name = "MS",
        allow_hyphen_values = true
    )]
    pub offset_ms: i64,

    /// Show lyric transitions this many milliseconds earlier.
    #[arg(long, default_value_t = 0, value_name = "MS")]
    pub lookahead_ms: u64,

    /// Write logs to this file instead of the XDG state directory.
    #[arg(long, value_name = "PATH")]
    pub log_file: Option<PathBuf>,

    /// Print media events to stdout without starting the TUI.
    #[arg(long)]
    pub dump_events: bool,
}

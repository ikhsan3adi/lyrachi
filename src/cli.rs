use std::path::PathBuf;

use clap::Parser;

/// Command line arguments for the MVP.
#[derive(Debug, Parser)]
#[command(name = "lyrachi", version, about = "Terminal lyrics for MPRIS players")]
pub struct Args {
    /// Only consider this MPRIS player bus (for example: mpv, spotify, firefox).
    #[arg(long, value_name = "NAME")]
    pub player: Option<String>,

    /// Directory to scan for local .lrc and .elrc files. May be repeated.
    #[arg(long = "local-dir", value_name = "PATH")]
    pub local_dir: Vec<PathBuf>,

    /// Manual lyric offset in milliseconds added to the playback position.
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

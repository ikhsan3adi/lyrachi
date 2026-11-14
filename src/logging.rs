use std::{fs, path::PathBuf};

use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{EnvFilter, fmt};

use crate::cli::Args;

/// Initialize file logging and return the guard that must live until exit.
pub fn init(args: &Args) -> color_eyre::Result<WorkerGuard> {
    let path = args.log_file.clone().unwrap_or_else(default_log_path);

    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)?;
    }

    let file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    let (writer, guard) = tracing_appender::non_blocking(file);

    let filter = EnvFilter::try_from_env("LYRACHI_LOG")
        .or_else(|_| EnvFilter::try_from_default_env())
        .unwrap_or_else(|_| EnvFilter::new("lyrachi=info,zbus=warn"));

    fmt()
        .with_writer(writer)
        .with_env_filter(filter)
        .with_ansi(false)
        .init();

    tracing::info!(path = %path.display(), "logging initialized");
    Ok(guard)
}

fn default_log_path() -> PathBuf {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))
        .unwrap_or_else(|| PathBuf::from("."));

    base.join("lyrachi").join("lyrachi.log")
}

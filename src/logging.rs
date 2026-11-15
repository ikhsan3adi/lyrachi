use std::{fs, path::Path};

use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{EnvFilter, fmt};

pub fn init(path: &Path) -> color_eyre::Result<WorkerGuard> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)?;
    }

    let file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
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

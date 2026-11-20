use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::media::{MediaError, MediaEvent, MediaSnapshot};

#[cfg(any(
    target_os = "linux",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly"
))]
pub async fn start_monitor(
    player_filter: Option<String>,
    tx: mpsc::Sender<MediaEvent>,
    cancel: CancellationToken,
) -> Result<JoinHandle<()>, MediaError> {
    let connection = zbus::Connection::session()
        .await
        .map_err(|error| MediaError::SessionUnavailable(error.to_string()))?;
    Ok(tokio::spawn(async move {
        if let Err(error) =
            crate::media::mpris::arbiter::run(player_filter, connection, tx, cancel).await
        {
            tracing::warn!(%error, "media monitor stopped");
        }
    }))
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly"
)))]
pub async fn start_monitor(
    player_filter: Option<String>,
    tx: mpsc::Sender<MediaEvent>,
    cancel: CancellationToken,
) -> Result<JoinHandle<()>, MediaError> {
    let _ = (player_filter, tx, cancel);
    Err(MediaError::Unsupported)
}

#[cfg(any(
    target_os = "linux",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly"
))]
pub async fn probe(player_filter: Option<&str>) -> Result<MediaSnapshot, MediaError> {
    crate::media::mpris::probe::probe(player_filter).await
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly"
)))]
pub async fn probe(player_filter: Option<&str>) -> Result<MediaSnapshot, MediaError> {
    let _ = player_filter;
    Err(MediaError::Unsupported)
}

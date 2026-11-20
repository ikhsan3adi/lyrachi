use std::time::Duration;

use tracing::debug;

use super::arbiter::{discover_candidates, select_player};
use super::metadata::extract_track_meta;
use super::proxy::{PlayerProxy, micros_to_ms};
use crate::media::{BackendKind, MediaError, MediaSnapshot, PlayerSnapshot, TrackMeta};

const CALL_TIMEOUT: Duration = Duration::from_millis(500);

pub async fn probe(player_filter: Option<&str>) -> Result<MediaSnapshot, MediaError> {
    let connection = zbus::Connection::session()
        .await
        .map_err(|error| MediaError::SessionUnavailable(error.to_string()))?;
    let candidates = discover_candidates(&connection, player_filter).await;
    let Some(selected) = select_player(&candidates, player_filter).cloned() else {
        return Ok(MediaSnapshot::default());
    };
    let player = PlayerSnapshot {
        id: selected.bus,
        kind: selected.kind,
        backend: BackendKind::Mpris,
    };
    let (track, position_ms) = read_player(&connection, &player).await;

    Ok(MediaSnapshot {
        player: Some(player),
        track,
        playback: selected.status,
        position_ms,
        backend: BackendKind::Mpris,
    })
}

async fn read_player(
    connection: &zbus::Connection,
    player: &PlayerSnapshot,
) -> (Option<TrackMeta>, u64) {
    let proxy = match tokio::time::timeout(
        CALL_TIMEOUT,
        PlayerProxy::builder(connection)
            .destination(player.id.as_str())
            .build(),
    )
    .await
    {
        Ok(Ok(proxy)) => proxy,
        Ok(Err(error)) => {
            debug!(%error, id = %player.id, "failed to build MPRIS proxy");
            return (None, 0);
        }
        Err(_) => {
            debug!(id = %player.id, "building MPRIS proxy timed out");
            return (None, 0);
        }
    };

    let track = match tokio::time::timeout(CALL_TIMEOUT, proxy.metadata()).await {
        Ok(Ok(metadata)) => extract_track_meta(&metadata, player.kind),
        Ok(Err(error)) => {
            debug!(%error, id = %player.id, "failed to read metadata");
            None
        }
        Err(_) => {
            debug!(id = %player.id, "reading metadata timed out");
            None
        }
    };
    let position_ms = match tokio::time::timeout(CALL_TIMEOUT, proxy.position()).await {
        Ok(Ok(position)) => micros_to_ms(position),
        Ok(Err(error)) => {
            debug!(%error, id = %player.id, "failed to read position");
            0
        }
        Err(_) => {
            debug!(id = %player.id, "reading position timed out");
            0
        }
    };

    (track, position_ms)
}

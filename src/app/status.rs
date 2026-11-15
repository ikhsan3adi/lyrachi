use std::path::{Path, PathBuf};
use std::time::Duration;

use color_eyre::Result;
use serde::Serialize;
use tracing::debug;

use crate::config::RuntimeConfig;
use crate::core::clock::PlaybackStatus;
use crate::core::model::{SyncGranularity, SyncedTrack};
use crate::media::mpris::arbiter::{discover_candidates, select_player};
use crate::media::mpris::metadata::extract_track_meta;
use crate::media::mpris::proxy::{PlayerProxy, micros_to_ms};
use crate::media::{MediaSnapshot, PlayerKind, PlayerSnapshot, TrackMeta};
use crate::offsets::OffsetStore;
use crate::providers::sanitize::encode_offset_key;
use crate::providers::{ProviderAnswer, ProviderResolver, TrackQuery};

const CALL_TIMEOUT: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StatusPlayer {
    pub bus: String,
    pub name: String,
    pub kind: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StatusTrack {
    pub title: String,
    pub artist: String,
    pub album: Option<String>,
    pub duration_ms: Option<u64>,
    pub url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StatusLyrics {
    pub source: String,
    pub granularity: String,
    pub confidence: f32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StatusOffset {
    pub global_ms: i64,
    pub track_ms: i64,
    pub effective_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StatusReport {
    pub player: Option<StatusPlayer>,
    pub playback: String,
    pub track: Option<StatusTrack>,
    pub position_ms: u64,
    pub lyrics: Option<StatusLyrics>,
    pub lyrics_error: Option<String>,
    pub canonical_key: Option<String>,
    pub offset: StatusOffset,
}

pub async fn run(config: RuntimeConfig, offsets_file: PathBuf, json: bool) -> Result<()> {
    let snapshot = probe_live(&config).await;
    let report = collect_report(snapshot, &config, &offsets_file).await;

    if json {
        println!("{}", render_json(&report)?);
    } else {
        print!("{}", render_text(&report));
    }

    Ok(())
}

pub async fn collect_report(
    snapshot: MediaSnapshot,
    config: &RuntimeConfig,
    offsets_file: &Path,
) -> StatusReport {
    let player = snapshot.player.as_ref().map(|player| StatusPlayer {
        bus: player.bus.clone(),
        name: player_name(&player.bus),
        kind: player_kind(player.kind),
    });
    let track = snapshot.track.as_ref().map(|track| StatusTrack {
        title: track.title.clone(),
        artist: track.artist.clone(),
        album: track.album.clone(),
        duration_ms: track.duration_ms,
        url: track.url.clone(),
    });
    let query = snapshot
        .track
        .as_ref()
        .map(|track| TrackQuery::from_meta(track, 0))
        .filter(TrackQuery::is_usable);
    let canonical_key = query
        .as_ref()
        .map(|query| encode_offset_key(&query.canonical));
    let offsets = OffsetStore::load(offsets_file);
    let track_ms = query
        .as_ref()
        .map_or(0, |query| offsets.offset_for(&query.canonical));
    let (lyrics, lyrics_error) = match snapshot.track.as_ref() {
        Some(track) if query.is_some() => resolve_lyrics(track, track_ms, config).await,
        _ => (None, None),
    };
    let effective_ms = config.global_offset_ms.saturating_add(track_ms);

    StatusReport {
        player,
        playback: playback_name(snapshot.playback).to_string(),
        track,
        position_ms: snapshot.position_ms,
        lyrics,
        lyrics_error,
        canonical_key,
        offset: StatusOffset {
            global_ms: config.global_offset_ms,
            track_ms,
            effective_ms,
        },
    }
}

async fn resolve_lyrics(
    track: &TrackMeta,
    track_ms: i64,
    config: &RuntimeConfig,
) -> (Option<StatusLyrics>, Option<String>) {
    let resolver = match ProviderResolver::with_config(
        config.local_dirs.clone(),
        config.provider_order.clone(),
        config.network,
    ) {
        Ok(resolver) => resolver,
        Err(error) => return (None, Some(error.to_string())),
    };

    match resolver.resolve(track, track_ms).await {
        Ok(Some(track)) => (Some(StatusLyrics::from(&track)), None),
        Ok(None) => (None, None),
        Err(error) => (None, Some(error.to_string())),
    }
}

async fn probe_live(config: &RuntimeConfig) -> MediaSnapshot {
    let connection = match zbus::Connection::session().await {
        Ok(connection) => connection,
        Err(error) => {
            debug!(%error, "session bus unavailable");
            return MediaSnapshot::default();
        }
    };
    let candidates = discover_candidates(&connection, config.player_filter.as_deref()).await;
    let Some(selected) = select_player(&candidates, config.player_filter.as_deref()).cloned()
    else {
        return MediaSnapshot::default();
    };
    let player = PlayerSnapshot {
        bus: selected.bus,
        kind: selected.kind,
    };
    let (track, position_ms) = read_player(&connection, &player).await;

    MediaSnapshot {
        player: Some(player),
        track,
        playback: selected.status,
        position_ms,
    }
}

async fn read_player(
    connection: &zbus::Connection,
    player: &PlayerSnapshot,
) -> (Option<TrackMeta>, u64) {
    let proxy = match tokio::time::timeout(
        CALL_TIMEOUT,
        PlayerProxy::builder(connection)
            .destination(player.bus.as_str())
            .build(),
    )
    .await
    {
        Ok(Ok(proxy)) => proxy,
        Ok(Err(error)) => {
            debug!(%error, bus = %player.bus, "failed to build MPRIS proxy");
            return (None, 0);
        }
        Err(_) => {
            debug!(bus = %player.bus, "building MPRIS proxy timed out");
            return (None, 0);
        }
    };

    let track = match tokio::time::timeout(CALL_TIMEOUT, proxy.metadata()).await {
        Ok(Ok(metadata)) => extract_track_meta(&metadata, player.kind),
        Ok(Err(error)) => {
            debug!(%error, bus = %player.bus, "failed to read metadata");
            None
        }
        Err(_) => {
            debug!(bus = %player.bus, "reading metadata timed out");
            None
        }
    };
    let position_ms = match tokio::time::timeout(CALL_TIMEOUT, proxy.position()).await {
        Ok(Ok(position)) => micros_to_ms(position),
        Ok(Err(error)) => {
            debug!(%error, bus = %player.bus, "failed to read position");
            0
        }
        Err(_) => {
            debug!(bus = %player.bus, "reading position timed out");
            0
        }
    };

    (track, position_ms)
}

pub fn render_text(report: &StatusReport) -> String {
    let mut output = String::new();
    match &report.player {
        Some(player) => push_line(
            &mut output,
            "player",
            format!("{} ({})", player.name, player.bus),
        ),
        None => push_line(&mut output, "player", "none"),
    }
    push_line(&mut output, "playback", &report.playback);

    let track = report.track.as_ref();
    push_line(
        &mut output,
        "title",
        track.map_or("none", |track| track.title.as_str()),
    );
    push_line(
        &mut output,
        "artist",
        track.map_or("none", |track| track.artist.as_str()),
    );
    push_line(
        &mut output,
        "album",
        track
            .and_then(|track| track.album.as_deref())
            .unwrap_or("none"),
    );
    let duration = track
        .and_then(|track| track.duration_ms)
        .map(|duration| duration.to_string())
        .unwrap_or_else(|| "none".to_string());
    push_line(&mut output, "duration_ms", duration);
    push_line(
        &mut output,
        "url",
        track
            .and_then(|track| track.url.as_deref())
            .unwrap_or("none"),
    );
    push_line(&mut output, "position_ms", report.position_ms);

    let lyrics = report.lyrics.as_ref();
    push_line(
        &mut output,
        "lyrics_source",
        lyrics.map_or("none", |lyrics| lyrics.source.as_str()),
    );
    push_line(
        &mut output,
        "lyrics_granularity",
        lyrics.map_or("none", |lyrics| lyrics.granularity.as_str()),
    );
    match lyrics {
        Some(lyrics) => push_line(
            &mut output,
            "lyrics_confidence",
            format!("{:.3}", lyrics.confidence),
        ),
        None => push_line(&mut output, "lyrics_confidence", "none"),
    }
    if let Some(error) = &report.lyrics_error {
        push_line(&mut output, "lyrics_error", error);
    }
    push_line(
        &mut output,
        "canonical_key",
        report.canonical_key.as_deref().unwrap_or("none"),
    );
    push_line(&mut output, "offset_global_ms", report.offset.global_ms);
    push_line(&mut output, "offset_track_ms", report.offset.track_ms);
    push_line(
        &mut output,
        "offset_effective_ms",
        report.offset.effective_ms,
    );

    output
}

pub fn render_json(report: &StatusReport) -> Result<String> {
    Ok(serde_json::to_string_pretty(report)?)
}

fn push_line(output: &mut String, label: &str, value: impl std::fmt::Display) {
    use std::fmt::Write as _;

    let _ = writeln!(output, "{label}: {value}");
}

fn player_name(bus: &str) -> String {
    let short = bus.strip_prefix("org.mpris.MediaPlayer2.").unwrap_or(bus);
    short.split('.').next().unwrap_or(short).to_string()
}

fn player_kind(kind: PlayerKind) -> String {
    match kind {
        PlayerKind::Native => "native",
        PlayerKind::Browser => "browser",
        PlayerKind::Unknown => "unknown",
    }
    .to_string()
}

fn playback_name(status: PlaybackStatus) -> &'static str {
    match status {
        PlaybackStatus::Playing => "playing",
        PlaybackStatus::Paused => "paused",
        PlaybackStatus::Stopped => "stopped",
    }
}

fn granularity_name(granularity: SyncGranularity) -> &'static str {
    match granularity {
        SyncGranularity::Syllable => "syllable",
        SyncGranularity::Word => "word",
        SyncGranularity::Line => "line",
        SyncGranularity::Unsynced => "unsynced",
    }
}

impl From<&SyncedTrack> for StatusLyrics {
    fn from(track: &SyncedTrack) -> Self {
        Self {
            source: track.source_provider.clone(),
            granularity: granularity_name(track.granularity).to_string(),
            confidence: if track.match_confidence.is_finite() {
                track.match_confidence
            } else {
                0.0
            },
        }
    }
}

impl From<&ProviderAnswer> for StatusLyrics {
    fn from(answer: &ProviderAnswer) -> Self {
        Self::from(&answer.track)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_syllable_granularity_for_status_output() {
        assert_eq!(granularity_name(SyncGranularity::Syllable), "syllable");
    }
}

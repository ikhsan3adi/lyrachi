use std::path::{Path, PathBuf};

use color_eyre::Result;
use serde::Serialize;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::cache::CacheHandle;
use crate::config::RuntimeConfig;
use crate::core::model::SyncedTrack;
use crate::media::{BackendKind, MediaSnapshot, TrackMeta, backend, player_short_name};
use crate::offsets::OffsetStore;
use crate::providers::sanitize::encode_offset_key;
use crate::providers::{ProviderAnswer, ProviderResolver, TrackQuery};

use super::resolve::resolve_with_cache;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StatusPlayer {
    pub id: String,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StatusCircuitBreaker {
    pub provider: String,
    pub status: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StatusReport {
    pub backend: String,
    pub backend_available: bool,
    pub player: Option<StatusPlayer>,
    pub playback: String,
    pub track: Option<StatusTrack>,
    pub position_ms: u64,
    pub lyrics: Option<StatusLyrics>,
    pub lyrics_error: Option<String>,
    pub canonical_key: Option<String>,
    pub offset: StatusOffset,
    pub circuit_breakers: Vec<StatusCircuitBreaker>,
}

pub async fn run(
    config: RuntimeConfig,
    offsets_file: PathBuf,
    json: bool,
    cache: Option<CacheHandle>,
) -> Result<()> {
    let backend = BackendKind::current();
    let (snapshot, backend_available) = probe_live(&config).await;
    let report = collect_report_with_cache(
        snapshot,
        backend,
        backend_available,
        &config,
        &offsets_file,
        cache.as_ref(),
    )
    .await;

    if json {
        println!("{}", render_json(&report)?);
    } else {
        print!("{}", render_text(&report));
    }

    if let Some(cache) = cache
        && let Err(error) = cache.shutdown().await
    {
        warn!(%error, "failed to shut down lyric cache");
    }

    Ok(())
}

pub async fn collect_report(
    snapshot: MediaSnapshot,
    config: &RuntimeConfig,
    offsets_file: &Path,
) -> StatusReport {
    let backend = BackendKind::current();
    collect_report_with_cache(
        snapshot,
        backend,
        backend.is_supported(),
        config,
        offsets_file,
        None,
    )
    .await
}

pub async fn collect_report_with_cache(
    snapshot: MediaSnapshot,
    backend: BackendKind,
    backend_available: bool,
    config: &RuntimeConfig,
    offsets_file: &Path,
    cache: Option<&CacheHandle>,
) -> StatusReport {
    let player = snapshot.player.as_ref().map(|player| StatusPlayer {
        id: player.id.clone(),
        name: player_short_name(&player.id).to_string(),
        kind: player.kind.as_str().to_string(),
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
    let resolver = ProviderResolver::with_config(
        config.local_dirs.clone(),
        config.provider_order.clone(),
        config.network,
    )
    .ok();
    let circuit_breakers = resolver
        .as_ref()
        .map(|resolver| {
            resolver
                .circuit_breaker_statuses()
                .into_iter()
                .map(|(provider, status)| StatusCircuitBreaker {
                    provider: provider.as_str().to_string(),
                    status: status.to_string(),
                })
                .collect()
        })
        .unwrap_or_default();
    let (lyrics, lyrics_error) = match snapshot.track.as_ref() {
        Some(track) if query.is_some() => {
            resolve_lyrics(track, track_ms, resolver.as_ref(), cache).await
        }
        _ => (None, None),
    };
    let effective_ms = config.global_offset_ms.saturating_add(track_ms);

    StatusReport {
        backend: backend.as_str().to_string(),
        backend_available,
        player,
        playback: snapshot.playback.as_str().to_string(),
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
        circuit_breakers,
    }
}

async fn resolve_lyrics(
    track: &TrackMeta,
    track_ms: i64,
    resolver: Option<&ProviderResolver>,
    cache: Option<&CacheHandle>,
) -> (Option<StatusLyrics>, Option<String>) {
    let Some(resolver) = resolver else {
        return (
            None,
            Some("failed to initialize provider resolver".to_string()),
        );
    };

    let query = TrackQuery::from_meta(track, track_ms);
    match resolve_with_cache(resolver, cache, &query, &CancellationToken::new()).await {
        Ok(Some(answer)) => (Some(StatusLyrics::from(&answer)), None),
        Ok(None) => (None, None),
        Err(error) => (None, Some(error.to_string())),
    }
}

async fn probe_live(config: &RuntimeConfig) -> (MediaSnapshot, bool) {
    match backend::probe(config.player_filter.as_deref()).await {
        Ok(snapshot) => (snapshot, true),
        Err(error) => {
            debug!(%error, "media backend unavailable");
            (MediaSnapshot::default(), false)
        }
    }
}

pub fn render_text(report: &StatusReport) -> String {
    let mut output = String::new();
    match &report.player {
        Some(player) => push_line(
            &mut output,
            "player",
            format!("{} ({})", player.name, player.id),
        ),
        None => push_line(&mut output, "player", "none"),
    }
    push_line(
        &mut output,
        "backend",
        format!(
            "{} ({})",
            report.backend,
            if report.backend_available {
                "available"
            } else {
                "unavailable"
            }
        ),
    );
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
    for cb in &report.circuit_breakers {
        push_line(
            &mut output,
            &format!("circuit_breaker_{}", cb.provider),
            &cb.status,
        );
    }

    output
}

pub fn render_json(report: &StatusReport) -> Result<String> {
    Ok(serde_json::to_string_pretty(report)?)
}

fn push_line(output: &mut String, label: &str, value: impl std::fmt::Display) {
    use std::fmt::Write as _;

    let _ = writeln!(output, "{label}: {value}");
}

impl From<&SyncedTrack> for StatusLyrics {
    fn from(track: &SyncedTrack) -> Self {
        Self {
            source: track.source_provider.clone(),
            granularity: track.granularity.as_str().to_string(),
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
    #[test]
    fn names_syllable_granularity_for_status_output() {
        assert_eq!(
            crate::core::model::SyncGranularity::Syllable.as_str(),
            "syllable"
        );
    }
}

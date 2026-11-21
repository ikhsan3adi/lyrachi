use std::time::Duration;

use reqwest::StatusCode;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;
use tracing::warn;

use super::http;
use super::score;
use super::{
    CandidateSummary, LyricProvider, Provider, ProviderAnswer, ProviderCapabilities, ProviderError,
    Tier, TrackQuery,
};
use crate::core::normalize::{normalize_plain, normalize_track_checked};
use crate::core::parse::lrc::parse_lrc;
use crate::core::parse::ttml::parse_ttml;

const BASE_URL: &str = "https://unison.betterlyrics.org";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(8);
const MAX_CANDIDATES: usize = 20;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct UnisonEnvelope {
    data: Option<UnisonLyric>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct UnisonLyric {
    id: Option<u64>,
    video_id: Option<String>,
    song: Option<String>,
    artist: Option<String>,
    album: Option<String>,
    duration: Option<f64>,
    lyrics: Option<String>,
    format: Option<String>,
    sync_type: Option<String>,
    match_score: Option<f32>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct UnisonSearchEnvelope {
    data: Vec<UnisonLyric>,
}

pub struct UnisonProvider {
    client: reqwest::Client,
    base_url: String,
}

impl UnisonProvider {
    pub fn new() -> Result<Self, ProviderError> {
        Self::with_base_url(BASE_URL)
    }

    pub fn with_base_url(base_url: impl Into<String>) -> Result<Self, ProviderError> {
        let client = http::client_builder().timeout(REQUEST_TIMEOUT).build()?;
        let base_url = base_url.into().trim_end_matches('/').to_string();
        Ok(Self { client, base_url })
    }

    pub async fn resolve(
        &self,
        query: &TrackQuery,
    ) -> Result<Option<ProviderAnswer>, ProviderError> {
        self.resolve_with_cancel(query, &CancellationToken::new())
            .await
    }

    pub async fn resolve_with_cancel(
        &self,
        query: &TrackQuery,
        cancel: &CancellationToken,
    ) -> Result<Option<ProviderAnswer>, ProviderError> {
        if cancel.is_cancelled() {
            return Err(ProviderError::Cancelled);
        }
        if !query.is_usable() {
            return Ok(None);
        }

        let mut params = vec![("song", query.title.clone())];
        if !query.artist.is_empty() {
            params.push(("artist", query.artist.clone()));
        }
        if let Some(album) = query.album.as_ref() {
            params.push(("album", album.clone()));
        }
        if let Some(duration) = query
            .duration_sec()
            .filter(|duration| duration.is_finite() && *duration > 0.0)
        {
            params.push(("duration", format!("{:.0}", duration.round())));
        }

        let Some(body) = self.get_json("/lyrics", &params, cancel).await? else {
            return Ok(None);
        };
        let envelope: UnisonEnvelope = serde_json::from_str(&body).map_err(invalid_json)?;
        let Some(lyric) = envelope.data else {
            return Ok(None);
        };

        let confidence = lyric_confidence(query, &lyric);
        if !score::is_confident(confidence) {
            warn!(confidence, "Unison response did not match the track");
            return Ok(None);
        }

        let source_track_id = lyric
            .video_id
            .clone()
            .or_else(|| lyric.id.map(|id| id.to_string()));
        let answer = parse_lyric(&lyric, confidence, query.user_offset_ms, source_track_id)?;
        Ok(answer.map(|answer| ProviderAnswer::new(Provider::Unison, answer)))
    }

    async fn get_json(
        &self,
        path: &str,
        params: &[(&str, String)],
        cancel: &CancellationToken,
    ) -> Result<Option<String>, ProviderError> {
        let send_fut = self
            .client
            .get(format!("{}{path}", self.base_url))
            .query(params)
            .send();
        let response = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(ProviderError::Cancelled),
            result = send_fut => result?,
        };

        match response.status() {
            StatusCode::NOT_FOUND => Ok(None),
            StatusCode::TOO_MANY_REQUESTS => Err(ProviderError::RateLimited {
                retry_after_ms: http::retry_after_ms(&response),
            }),
            StatusCode::SERVICE_UNAVAILABLE => Err(ProviderError::Unavailable(
                "Unison lookup is pending".to_string(),
            )),
            status if status.is_success() => {
                let body = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => return Err(ProviderError::Cancelled),
                    result = http::read_limited_text(response) => result?,
                };
                Ok(Some(body))
            }
            status if status.is_client_error() => Err(ProviderError::Invalid(format!(
                "Unison returned HTTP {status}"
            ))),
            status => Err(ProviderError::Unavailable(format!(
                "Unison returned HTTP {status}"
            ))),
        }
    }
}

fn parse_lyric(
    lyric: &UnisonLyric,
    confidence: f32,
    user_offset_ms: i64,
    source_track_id: Option<String>,
) -> Result<Option<crate::core::model::SyncedTrack>, ProviderError> {
    let Some(text) = lyric
        .lyrics
        .as_deref()
        .filter(|lyrics| !lyrics.trim().is_empty())
    else {
        return Ok(None);
    };
    let format = lyric
        .format
        .as_deref()
        .map(str::trim)
        .filter(|format| !format.is_empty());
    let provider = Provider::Unison.as_str();

    let sniffed = text.trim_start();
    let is_ttml = match format {
        Some("ttml") => true,
        Some("lrc" | "plain") => false,
        _ => sniffed.starts_with('<'),
    };
    let is_lrc = match format {
        Some("lrc") => true,
        Some("ttml" | "plain") => false,
        _ => !is_ttml && sniffed.starts_with('['),
    };

    let mut track = if is_ttml {
        parse_ttml(text, provider)?
    } else if is_lrc {
        let parsed = parse_lrc(
            text,
            lyric.duration.map(|duration| (duration * 1_000.0) as u64),
        )
        .map_err(|error| ProviderError::Invalid(error.to_string()))?;
        normalize_track_checked(
            parsed,
            provider,
            None,
            confidence,
            lyric.duration.map(|duration| (duration * 1_000.0) as u64),
            user_offset_ms,
        )
        .map_err(|error| ProviderError::Invalid(error.to_string()))?
    } else {
        normalize_plain(text, provider, None, confidence, user_offset_ms)
    };

    if track.lines.is_empty() {
        return Ok(None);
    }
    track.source_track_id = source_track_id;
    track.match_confidence = confidence;
    track.user_offset_ms = user_offset_ms;
    Ok(Some(track))
}

fn lyric_confidence(query: &TrackQuery, lyric: &UnisonLyric) -> f32 {
    let metadata = score::score_metadata(
        query,
        lyric.song.as_deref().unwrap_or_default(),
        lyric.artist.as_deref().unwrap_or_default(),
        lyric.duration,
    );
    metadata.max(lyric.match_score.unwrap_or(0.0).clamp(0.0, 1.0))
}

fn summary_tier(lyric: &UnisonLyric) -> Tier {
    match lyric.sync_type.as_deref() {
        Some("richsync") => Tier::Word,
        Some("linesync") => Tier::Line,
        _ => match lyric.format.as_deref() {
            Some("ttml") => Tier::Word,
            Some("lrc") => Tier::Line,
            _ => Tier::Plain,
        },
    }
}

fn invalid_json(error: serde_json::Error) -> ProviderError {
    ProviderError::Invalid(format!("invalid Unison response: {error}"))
}

impl LyricProvider for UnisonProvider {
    fn id(&self) -> Provider {
        Provider::Unison
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            supports_search: true,
            needs_hydration: true,
            max_candidates: MAX_CANDIDATES,
        }
    }

    fn rate_limited(&self) -> bool {
        true
    }

    fn resolve<'a>(
        &'a self,
        query: &'a TrackQuery,
        cancel: &'a CancellationToken,
    ) -> crate::media::BoxFuture<'a, Result<Option<ProviderAnswer>, ProviderError>> {
        Box::pin(self.resolve_with_cancel(query, cancel))
    }

    fn search_candidates<'a>(
        &'a self,
        query: &'a TrackQuery,
        cancel: &'a CancellationToken,
    ) -> crate::media::BoxFuture<'a, Result<Vec<CandidateSummary>, ProviderError>> {
        Box::pin(self.search_candidates_with_cancel(query, cancel))
    }

    fn hydrate_candidate<'a>(
        &'a self,
        summary: &'a CandidateSummary,
        limiter: Option<&'a crate::providers::engine::TokenBucketRateLimiter>,
        cancel: &'a CancellationToken,
    ) -> crate::media::BoxFuture<'a, Result<Option<ProviderAnswer>, ProviderError>> {
        Box::pin(self.hydrate_candidate_with_cancel(summary, limiter, cancel))
    }
}

impl UnisonProvider {
    async fn search_candidates_with_cancel(
        &self,
        query: &TrackQuery,
        cancel: &CancellationToken,
    ) -> Result<Vec<CandidateSummary>, ProviderError> {
        if cancel.is_cancelled() {
            return Err(ProviderError::Cancelled);
        }
        if !query.is_usable() {
            return Ok(Vec::new());
        }

        let text = format!("{} {}", query.artist, query.title);
        let params = [
            ("q", text.trim().to_string()),
            ("limit", MAX_CANDIDATES.to_string()),
        ];
        let Some(body) = self.get_json("/lyrics/search", &params, cancel).await? else {
            return Ok(Vec::new());
        };
        let envelope: UnisonSearchEnvelope = serde_json::from_str(&body).map_err(invalid_json)?;

        let mut items = envelope
            .data
            .into_iter()
            .filter_map(|lyric| {
                let confidence = lyric_confidence(query, &lyric);
                let id = lyric.id?;
                Some(CandidateSummary {
                    provider: Provider::Unison,
                    source_track_id: Some(id.to_string()),
                    title: lyric.song.clone().unwrap_or_else(|| query.title.clone()),
                    artist: lyric.artist.clone(),
                    album: lyric.album.clone(),
                    duration_ms: lyric
                        .duration
                        .filter(|duration| duration.is_finite() && *duration > 0.0)
                        .map(|duration| (duration * 1_000.0).round() as u64),
                    tier: summary_tier(&lyric),
                    confidence,
                    local_path: None,
                    browse_hint: lyric.video_id.clone(),
                    query_duration_ms: query.duration_ms,
                    query_user_offset_ms: query.user_offset_ms,
                    hydrated: None,
                })
            })
            .collect::<Vec<_>>();
        items.sort_by(|left, right| right.confidence.total_cmp(&left.confidence));
        items.truncate(MAX_CANDIDATES);
        Ok(items)
    }

    async fn hydrate_candidate_with_cancel(
        &self,
        summary: &CandidateSummary,
        limiter: Option<&crate::providers::engine::TokenBucketRateLimiter>,
        cancel: &CancellationToken,
    ) -> Result<Option<ProviderAnswer>, ProviderError> {
        if cancel.is_cancelled() {
            return Err(ProviderError::Cancelled);
        }
        if let Some(answer) = &summary.hydrated {
            return Ok(Some(answer.clone()));
        }
        if let Some(limiter) = limiter {
            limiter.acquire(cancel).await?;
        }

        let body = if let Some(id) = summary
            .source_track_id
            .as_deref()
            .filter(|id| !id.trim().is_empty())
        {
            self.get_json(&format!("/lyrics/{id}"), &[], cancel).await?
        } else if let Some(video_id) = summary.browse_hint() {
            self.get_json("/lyrics", &[("v", video_id.to_string())], cancel)
                .await?
        } else {
            return Ok(None);
        };
        let Some(body) = body else {
            return Ok(None);
        };
        let envelope: UnisonEnvelope = serde_json::from_str(&body).map_err(invalid_json)?;
        let Some(lyric) = envelope.data else {
            return Ok(None);
        };
        let confidence = summary.confidence.max(0.0);
        let track = parse_lyric(
            &lyric,
            confidence,
            summary.query_user_offset_ms,
            summary.source_track_id.clone(),
        )?;
        Ok(track.map(|track| ProviderAnswer::new(Provider::Unison, track)))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::*;
    use crate::media::TrackMeta;

    fn query() -> TrackQuery {
        TrackQuery::from_meta(
            &TrackMeta {
                title: "Explorer".to_string(),
                artist: "Justice, Connan Mockasin".to_string(),
                album: Some("Hyperdrama".to_string()),
                duration_ms: Some(249_000),
                track_id: None,
                url: None,
            },
            0,
        )
    }

    async fn spawn_mock(
        responses: Vec<(String, String)>,
    ) -> (
        String,
        Arc<std::sync::Mutex<Vec<String>>>,
        tokio::task::JoinHandle<()>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorded = requests.clone();
        let handle = tokio::spawn(async move {
            for (status, body) in responses {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = [0_u8; 8192];
                let read = socket.read(&mut request).await.unwrap();
                recorded
                    .lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&request[..read]).to_string());
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(response.as_bytes()).await.unwrap();
            }
        });
        (format!("http://{address}"), requests, handle)
    }

    #[tokio::test]
    async fn resolves_ttml_fixture() {
        let body = include_str!("../../tests/fixtures/unison_lyrics.json").to_string();
        let (base_url, requests, handle) = spawn_mock(vec![("200 OK".to_string(), body)]).await;
        let provider = UnisonProvider::with_base_url(base_url).unwrap();

        let answer = provider.resolve(&query()).await.unwrap().unwrap();
        assert_eq!(answer.provider, Provider::Unison);
        assert_eq!(answer.track.source_track_id.as_deref(), Some("Ugr4KKK_SqY"));
        assert!(!answer.track.lines.is_empty());
        assert!(answer.track.lines[0].words.len() > 1);

        {
            let recorded = requests.lock().unwrap();
            assert!(recorded[0].contains("song=Explorer"));
            assert!(recorded[0].contains("artist=Justice%2C+Connan+Mockasin"));
            assert!(recorded[0].contains("album=Hyperdrama"));
            assert!(recorded[0].contains("duration=249"));
        }
        handle.await.unwrap();
    }

    #[test]
    fn parses_lrc_fixture() {
        let lyric = UnisonLyric {
            id: Some(1),
            format: Some("lrc".to_string()),
            lyrics: Some(include_str!("../../tests/fixtures/unison_lyrics.lrc").to_string()),
            duration: Some(10.0),
            ..UnisonLyric::default()
        };
        let track = parse_lyric(&lyric, 0.9, 0, Some("1".to_string()))
            .unwrap()
            .unwrap();
        assert_eq!(track.granularity, crate::core::model::SyncGranularity::Line);
        assert_eq!(track.lines[1].start_ms, 2_500);
    }

    #[test]
    fn plain_format_keeps_section_headers_as_plain_lyrics() {
        let lyric = UnisonLyric {
            id: Some(1),
            format: Some("plain".to_string()),
            lyrics: Some("[Verse 1]\nFirst line\nSecond line\n".to_string()),
            ..UnisonLyric::default()
        };
        let track = parse_lyric(&lyric, 0.9, 0, None).unwrap().unwrap();
        assert_eq!(
            track.granularity,
            crate::core::model::SyncGranularity::Unsynced
        );
        assert!(!track.lines.is_empty());
    }

    #[test]
    fn summary_tier_maps_lrc_and_ttml_formats() {
        let lrc = UnisonLyric {
            format: Some("lrc".to_string()),
            ..UnisonLyric::default()
        };
        let ttml = UnisonLyric {
            format: Some("ttml".to_string()),
            ..UnisonLyric::default()
        };
        let plain = UnisonLyric::default();
        assert_eq!(summary_tier(&lrc), Tier::Line);
        assert_eq!(summary_tier(&ttml), Tier::Word);
        assert_eq!(summary_tier(&plain), Tier::Plain);
    }

    #[tokio::test]
    async fn search_returns_summaries_and_hydrates_by_id() {
        let search = include_str!("../../tests/fixtures/unison_search.json").to_string();
        let detail = include_str!("../../tests/fixtures/unison_lyrics.json").to_string();
        let (base_url, requests, handle) = spawn_mock(vec![
            ("200 OK".to_string(), search),
            ("200 OK".to_string(), detail),
        ])
        .await;
        let provider = UnisonProvider::with_base_url(base_url).unwrap();

        let items = provider
            .search_candidates_with_cancel(&query(), &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].source_track_id.as_deref(), Some("13529"));
        assert!(!items[0].is_hydrated());

        let answer = provider
            .hydrate_candidate_with_cancel(&items[0], None, &CancellationToken::new())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(answer.provider, Provider::Unison);
        assert!(!answer.track.lines.is_empty());

        {
            let recorded = requests.lock().unwrap();
            assert!(recorded[0].contains("GET /lyrics/search?"));
            assert!(recorded[1].contains("GET /lyrics/13529"));
        }
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn hydrate_candidate_acquires_the_rate_limiter() {
        let search = include_str!("../../tests/fixtures/unison_search.json").to_string();
        let detail = include_str!("../../tests/fixtures/unison_lyrics.json").to_string();
        let (base_url, _, handle) = spawn_mock(vec![
            ("200 OK".to_string(), search),
            ("200 OK".to_string(), detail),
        ])
        .await;
        let provider = UnisonProvider::with_base_url(base_url).unwrap();

        let items = provider
            .search_candidates_with_cancel(&query(), &CancellationToken::new())
            .await
            .unwrap();
        let limiter = crate::providers::engine::TokenBucketRateLimiter::new(1.0, 1.0);
        let answer = provider
            .hydrate_candidate_with_cancel(&items[0], Some(&limiter), &CancellationToken::new())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(answer.provider, Provider::Unison);
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn maps_rate_limit_and_pending() {
        let (base_url, _, first) = spawn_mock(vec![(
            "429 Too Many Requests".to_string(),
            "{}".to_string(),
        )])
        .await;
        let provider = UnisonProvider::with_base_url(base_url).unwrap();
        assert!(matches!(
            provider.resolve(&query()).await,
            Err(ProviderError::RateLimited { .. })
        ));
        first.await.unwrap();

        let (base_url, _, second) = spawn_mock(vec![(
            "503 Service Unavailable".to_string(),
            "{}".to_string(),
        )])
        .await;
        let provider = UnisonProvider::with_base_url(base_url).unwrap();
        assert!(matches!(
            provider.resolve(&query()).await,
            Err(ProviderError::Unavailable(_))
        ));
        second.await.unwrap();
    }
}

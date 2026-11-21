use std::time::Duration;

use reqwest::StatusCode;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use super::http;
use super::{
    CandidateSummary, LyricProvider, Provider, ProviderAnswer, ProviderCapabilities, ProviderError,
    TrackQuery,
};
use crate::core::model::SyncedTrack;
use crate::core::parse::ttml::parse_ttml;

const BASE_URL: &str = "https://lyrics-api.boidu.dev";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(8);
const MATCH_CONFIDENCE: f32 = 0.95;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct BetterLyricsResponse {
    ttml: Option<String>,
}

pub struct BetterLyricsProvider {
    client: reqwest::Client,
    base_url: String,
}

impl BetterLyricsProvider {
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

    async fn resolve_with_cancel(
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
        let Some(duration) = query
            .duration_sec()
            .filter(|duration| duration.is_finite() && *duration >= 1.0)
        else {
            return Ok(None);
        };

        let mut base = vec![
            ("s", query.title.clone()),
            ("d", format!("{:.0}", duration.round())),
        ];
        if !query.artist.is_empty() {
            base.push(("a", query.artist.clone()));
        }
        let album_forms = if query.album.is_some() { 2 } else { 1 };

        for album in [query.album.as_ref(), None].into_iter().take(album_forms) {
            if cancel.is_cancelled() {
                return Err(ProviderError::Cancelled);
            }
            let mut params = base.clone();
            if let Some(album) = album {
                params.push(("al", album.clone()));
            }

            let send_fut = self
                .client
                .get(format!("{}/getLyrics", self.base_url))
                .query(&params)
                .send();
            let response = tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(ProviderError::Cancelled),
                result = send_fut => result?,
            };

            match response.status() {
                StatusCode::UNAUTHORIZED | StatusCode::NOT_FOUND => continue,
                StatusCode::TOO_MANY_REQUESTS => {
                    return Err(ProviderError::RateLimited {
                        retry_after_ms: http::retry_after_ms(&response),
                    });
                }
                status if status.is_success() => {
                    let body = tokio::select! {
                        biased;
                        _ = cancel.cancelled() => return Err(ProviderError::Cancelled),
                        result = http::read_limited_text(response) => result?,
                    };
                    let body: BetterLyricsResponse =
                        serde_json::from_str(&body).map_err(invalid_json)?;
                    return parse_response(&body, query).map(|track| {
                        track.map(|track| ProviderAnswer::new(Provider::BetterLyrics, track))
                    });
                }
                status if status.is_client_error() => {
                    return Err(ProviderError::Invalid(format!(
                        "BetterLyrics returned HTTP {status}"
                    )));
                }
                status => {
                    return Err(ProviderError::Unavailable(format!(
                        "BetterLyrics returned HTTP {status}"
                    )));
                }
            }
        }

        Ok(None)
    }
}

fn parse_response(
    body: &BetterLyricsResponse,
    query: &TrackQuery,
) -> Result<Option<SyncedTrack>, ProviderError> {
    let Some(ttml) = body
        .ttml
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    else {
        return Ok(None);
    };

    let mut track = parse_ttml(ttml, Provider::BetterLyrics.as_str())?;
    if track.lines.is_empty() {
        return Ok(None);
    }
    track.match_confidence = MATCH_CONFIDENCE;
    track.user_offset_ms = query.user_offset_ms;
    Ok(Some(track))
}

fn invalid_json(error: serde_json::Error) -> ProviderError {
    ProviderError::Invalid(format!("invalid BetterLyrics response: {error}"))
}

impl LyricProvider for BetterLyricsProvider {
    fn id(&self) -> Provider {
        Provider::BetterLyrics
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            supports_search: false,
            needs_hydration: false,
            max_candidates: 1,
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
        _query: &'a TrackQuery,
        _cancel: &'a CancellationToken,
    ) -> crate::media::BoxFuture<'a, Result<Vec<CandidateSummary>, ProviderError>> {
        Box::pin(async { Ok(Vec::new()) })
    }

    fn hydrate_candidate<'a>(
        &'a self,
        summary: &'a CandidateSummary,
        _limiter: Option<&'a crate::providers::engine::TokenBucketRateLimiter>,
        _cancel: &'a CancellationToken,
    ) -> crate::media::BoxFuture<'a, Result<Option<ProviderAnswer>, ProviderError>> {
        Box::pin(async move { Ok(summary.hydrated.clone()) })
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
                title: "Yellow".to_string(),
                artist: "Coldplay".to_string(),
                album: Some("Parachutes".to_string()),
                duration_ms: Some(267_000),
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
    async fn resolves_cached_ttml_and_sends_metadata() {
        let body = include_str!("../../tests/fixtures/betterlyrics_ttml.json").to_string();
        let (base_url, requests, handle) = spawn_mock(vec![("200 OK".to_string(), body)]).await;
        let provider = BetterLyricsProvider::with_base_url(base_url).unwrap();

        let answer = provider.resolve(&query()).await.unwrap().unwrap();
        assert_eq!(answer.provider, Provider::BetterLyrics);
        assert!(answer.track.lines[0].words.len() > 1);

        {
            let recorded = requests.lock().unwrap();
            assert!(recorded[0].contains("s=Yellow"));
            assert!(recorded[0].contains("a=Coldplay"));
            assert!(recorded[0].contains("d=267"));
            assert!(recorded[0].contains("al=Parachutes"));
        }
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn retries_without_album_after_unauthorized() {
        let body = include_str!("../../tests/fixtures/betterlyrics_ttml.json").to_string();
        let (base_url, requests, handle) = spawn_mock(vec![
            ("401 Unauthorized".to_string(), "{}".to_string()),
            ("200 OK".to_string(), body),
        ])
        .await;
        let provider = BetterLyricsProvider::with_base_url(base_url).unwrap();

        assert!(provider.resolve(&query()).await.unwrap().is_some());
        {
            let recorded = requests.lock().unwrap();
            assert!(recorded[0].contains("al=Parachutes"));
            assert!(!recorded[1].contains("al="));
        }
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn retries_without_album_after_not_found() {
        let body = include_str!("../../tests/fixtures/betterlyrics_ttml.json").to_string();
        let (base_url, requests, handle) = spawn_mock(vec![
            ("404 Not Found".to_string(), "{}".to_string()),
            ("200 OK".to_string(), body),
        ])
        .await;
        let provider = BetterLyricsProvider::with_base_url(base_url).unwrap();

        assert!(provider.resolve(&query()).await.unwrap().is_some());
        {
            let recorded = requests.lock().unwrap();
            assert!(recorded[0].contains("al=Parachutes"));
            assert!(!recorded[1].contains("al="));
        }
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn skips_without_duration() {
        let provider = BetterLyricsProvider::with_base_url("http://127.0.0.1:1").unwrap();
        let query = TrackQuery::from_meta(
            &TrackMeta {
                title: "Yellow".to_string(),
                artist: "Coldplay".to_string(),
                album: None,
                duration_ms: None,
                track_id: None,
                url: None,
            },
            0,
        );
        assert!(provider.resolve(&query).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn maps_miss_rate_limit_and_empty_payload() {
        let (base_url, _, first) = spawn_mock(vec![
            ("404 Not Found".to_string(), "{}".to_string()),
            ("404 Not Found".to_string(), "{}".to_string()),
        ])
        .await;
        let provider = BetterLyricsProvider::with_base_url(base_url).unwrap();
        assert!(provider.resolve(&query()).await.unwrap().is_none());
        first.await.unwrap();

        let (base_url, _, second) = spawn_mock(vec![(
            "429 Too Many Requests".to_string(),
            "{}".to_string(),
        )])
        .await;
        let provider = BetterLyricsProvider::with_base_url(base_url).unwrap();
        assert!(matches!(
            provider.resolve(&query()).await,
            Err(ProviderError::RateLimited { .. })
        ));
        second.await.unwrap();

        let (base_url, _, third) =
            spawn_mock(vec![("200 OK".to_string(), r#"{"ttml":""}"#.to_string())]).await;
        let provider = BetterLyricsProvider::with_base_url(base_url).unwrap();
        assert!(provider.resolve(&query()).await.unwrap().is_none());
        third.await.unwrap();
    }
}

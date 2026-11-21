use std::time::Duration;

use reqwest::StatusCode;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use super::{decode_base64_bounded, decode_utf8, inflate_bounded, track_from_parsed};
use crate::core::parse::krc::parse_krc;
use crate::core::parse::lrc::parse_lrc;
use crate::providers::http;
use crate::providers::score;
use crate::providers::{
    CandidateSummary, LyricProvider, Provider, ProviderAnswer, ProviderCapabilities, ProviderError,
    Tier, TrackQuery,
};

const BASE_URL: &str = "https://lyrics.kugou.com";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(8);
const MAX_CANDIDATES: usize = 10;
const KRC_KEY: [u8; 16] = [
    0x40, 0x47, 0x61, 0x77, 0x5e, 0x32, 0x74, 0x47, 0x51, 0x36, 0x31, 0x2d, 0xce, 0xd2, 0x6e, 0x69,
];

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct SearchEnvelope {
    status: i64,
    candidates: Vec<SearchCandidate>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct SearchCandidate {
    id: String,
    accesskey: String,
    song: String,
    singer: String,
    duration: i64,
    krctype: i64,
    contenttype: i64,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct DownloadEnvelope {
    status: i64,
    fmt: String,
    content: String,
}

pub struct KugouProvider {
    client: reqwest::Client,
    base_url: String,
}

impl KugouProvider {
    pub fn new() -> Result<Self, ProviderError> {
        Self::with_base_url(BASE_URL)
    }

    pub fn with_base_url(base_url: impl Into<String>) -> Result<Self, ProviderError> {
        let client = http::client_builder().timeout(REQUEST_TIMEOUT).build()?;
        Ok(Self {
            client,
            base_url: base_url.into().trim_end_matches('/').to_string(),
        })
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
        let candidates = self.search_candidates_with_cancel(query, cancel).await?;
        for candidate in candidates.iter().take(3) {
            if let Some(answer) = self.hydrate_summary(candidate, None, cancel).await? {
                return Ok(Some(answer));
            }
        }
        Ok(None)
    }

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

        let keyword = format!("{} {}", query.artist, query.title);
        let mut params = vec![
            ("ver", "1".to_string()),
            ("man", "yes".to_string()),
            ("client", "pc".to_string()),
            ("keyword", keyword.trim().to_string()),
        ];
        if let Some(duration_ms) = query.duration_ms {
            params.push(("timelength", duration_ms.to_string()));
        }

        let send_fut = self
            .client
            .get(format!("{}/search", self.base_url))
            .query(&params)
            .header("User-Agent", "KuGou2012-9020-ExpandSearchManager")
            .send();
        let response = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(ProviderError::Cancelled),
            result = send_fut => result?,
        };

        if response.status() == StatusCode::TOO_MANY_REQUESTS {
            return Err(ProviderError::RateLimited {
                retry_after_ms: http::retry_after_ms(&response),
            });
        }
        if !response.status().is_success() {
            return Ok(Vec::new());
        }

        let body = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(ProviderError::Cancelled),
            result = http::read_limited_text(response) => result?,
        };
        let envelope: SearchEnvelope = serde_json::from_str(&body).map_err(invalid_json)?;
        if envelope.status != 1 {
            return Ok(Vec::new());
        }

        let mut candidates = envelope
            .candidates
            .into_iter()
            .filter_map(|candidate| {
                if candidate.id.is_empty()
                    || candidate.accesskey.is_empty()
                    || candidate.song.trim().is_empty()
                {
                    return None;
                }
                let duration_ms = (candidate.duration > 0).then_some(candidate.duration as u64);
                let confidence = score::score_metadata(
                    query,
                    &candidate.song,
                    &candidate.singer,
                    duration_ms.map(|duration| duration as f64 / 1_000.0),
                );
                if !score::is_confident(confidence) {
                    return None;
                }
                let tier = if candidate.krctype > 0 {
                    Tier::Word
                } else {
                    Tier::Line
                };
                Some(CandidateSummary {
                    provider: Provider::Kugou,
                    source_track_id: Some(candidate.id),
                    title: candidate.song,
                    artist: non_empty(candidate.singer),
                    album: None,
                    duration_ms,
                    tier,
                    confidence,
                    local_path: None,
                    browse_hint: Some(candidate.accesskey),
                    query_duration_ms: query.duration_ms,
                    query_user_offset_ms: query.user_offset_ms,
                    hydrated: None,
                })
            })
            .collect::<Vec<_>>();

        candidates.sort_by(|left, right| right.confidence.total_cmp(&left.confidence));
        candidates.truncate(MAX_CANDIDATES);
        Ok(candidates)
    }

    async fn hydrate_summary(
        &self,
        summary: &CandidateSummary,
        limiter: Option<&crate::providers::engine::TokenBucketRateLimiter>,
        cancel: &CancellationToken,
    ) -> Result<Option<ProviderAnswer>, ProviderError> {
        if let Some(answer) = summary.hydrated.clone() {
            return Ok(Some(answer));
        }
        let Some(id) = summary.source_track_id.as_deref() else {
            return Ok(None);
        };
        let Some(access_key) = summary.browse_hint.as_deref() else {
            return Ok(None);
        };
        if let Some(limiter) = limiter {
            limiter.acquire(cancel).await?;
        }

        if let Some(content) = self.download(id, access_key, "krc", cancel).await?
            && let Ok(text) = decode_krc(&content)
            && let Ok(parsed) = parse_krc(&text, summary.query_duration_ms)
            && let Some(track) = track_from_parsed(
                parsed,
                Provider::Kugou.as_str(),
                Some(id.to_string()),
                summary.confidence,
                summary.query_duration_ms,
                summary.query_user_offset_ms,
            )?
        {
            return Ok(Some(ProviderAnswer::new(Provider::Kugou, track)));
        }

        if let Some(content) = self.download(id, access_key, "lrc", cancel).await? {
            let bytes = decode_base64_bounded(&content)?;
            let text = decode_utf8(bytes)?;
            if let Ok(parsed) = parse_lrc(&text, summary.query_duration_ms)
                && let Some(track) = track_from_parsed(
                    parsed,
                    Provider::Kugou.as_str(),
                    Some(id.to_string()),
                    summary.confidence,
                    summary.query_duration_ms,
                    summary.query_user_offset_ms,
                )?
            {
                return Ok(Some(ProviderAnswer::new(Provider::Kugou, track)));
            }
        }

        Ok(None)
    }

    async fn download(
        &self,
        id: &str,
        access_key: &str,
        format: &str,
        cancel: &CancellationToken,
    ) -> Result<Option<String>, ProviderError> {
        let send_fut = self
            .client
            .get(format!("{}/download", self.base_url))
            .query(&[
                ("ver", "1"),
                ("client", "pc"),
                ("id", id),
                ("accesskey", access_key),
                ("fmt", format),
                ("charset", "utf8"),
            ])
            .header("User-Agent", "KuGou2012-9020-ExpandSearchManager")
            .send();
        let response = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(ProviderError::Cancelled),
            result = send_fut => result?,
        };

        if response.status() == StatusCode::TOO_MANY_REQUESTS {
            return Err(ProviderError::RateLimited {
                retry_after_ms: http::retry_after_ms(&response),
            });
        }
        if !response.status().is_success() {
            return Ok(None);
        }

        let body = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(ProviderError::Cancelled),
            result = http::read_limited_text(response) => result?,
        };
        let envelope: DownloadEnvelope = serde_json::from_str(&body).map_err(invalid_json)?;
        if envelope.status != 1 || envelope.content.trim().is_empty() {
            return Ok(None);
        }
        Ok(Some(envelope.content))
    }
}

impl LyricProvider for KugouProvider {
    fn id(&self) -> Provider {
        Provider::Kugou
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
        Box::pin(self.hydrate_summary(summary, limiter, cancel))
    }
}

fn decode_krc(content: &str) -> Result<String, ProviderError> {
    let data = decode_base64_bounded(content)?;
    if data.len() <= 4 || &data[..4] != b"krc1" {
        return Err(ProviderError::Invalid(
            "KuGou KRC payload has an invalid header".to_string(),
        ));
    }
    let mut body = data[4..].to_vec();
    for (index, byte) in body.iter_mut().enumerate() {
        *byte ^= KRC_KEY[index % KRC_KEY.len()];
    }
    decode_utf8(inflate_bounded(&body)?)
}

fn non_empty(value: String) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

fn invalid_json(error: serde_json::Error) -> ProviderError {
    ProviderError::Invalid(format!("invalid KuGou response: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::TrackMeta;

    #[test]
    fn rejects_invalid_krc_header() {
        let error = decode_krc("a3JjMg==").unwrap_err();
        assert!(matches!(error, ProviderError::Invalid(_)));
    }

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

    #[tokio::test]
    async fn resolves_search_result_and_decodes_krc() {
        let search = r#"{"status":1,"candidates":[{"id":"123","accesskey":"key","song":"Yellow","singer":"Coldplay","duration":267000,"krctype":1,"contenttype":0}]}"#;
        let download = r#"{"status":1,"fmt":"krc","content":"a3JjMTjb6kFqAkSXYAQBHUZnX7lxclGnb4KHD5yHMJzubvIVQNWQfxE="}"#;
        let (base_url, server) =
            super::super::mock_server(vec![(200, search.to_string()), (200, download.to_string())])
                .await;

        let provider = KugouProvider::with_base_url(base_url).unwrap();
        let answer = provider.resolve(&query()).await.unwrap().unwrap();

        assert_eq!(answer.provider, Provider::Kugou);
        assert_eq!(answer.track.lines[0].text, "Hello");
        assert_eq!(answer.tier, Tier::Syllable);
        server.await.unwrap();
    }
}

use std::time::Duration;

use reqwest::StatusCode;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use super::track_from_parsed;
use crate::core::parse::lrc::parse_lrc;
use crate::core::parse::yrc::parse_yrc;
use crate::providers::http;
use crate::providers::score;
use crate::providers::{
    CandidateSummary, LyricProvider, Provider, ProviderAnswer, ProviderCapabilities, ProviderError,
    Tier, TrackQuery,
};

const BASE_URL: &str = "https://music.163.com";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(8);
const MAX_CANDIDATES: usize = 10;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct SearchEnvelope {
    result: Option<SearchResult>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct SearchResult {
    songs: Vec<Song>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct Song {
    id: u64,
    name: String,
    ar: Vec<Artist>,
    al: Option<Album>,
    artists: Vec<Artist>,
    album: Option<Album>,
    dt: Option<u64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct Artist {
    name: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct Album {
    name: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct LyricEnvelope {
    yrc: Option<LyricField>,
    lrc: Option<LyricField>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct LyricField {
    lyric: Option<String>,
}

pub struct NeteaseProvider {
    client: reqwest::Client,
    base_url: String,
}

impl NeteaseProvider {
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

        let text = format!("{} {}", query.artist, query.title);
        let params = [
            ("s", text.trim().to_string()),
            ("type", "1".to_string()),
            ("limit", MAX_CANDIDATES.to_string()),
            ("offset", "0".to_string()),
        ];
        let send_fut = self
            .client
            .get(format!("{}/api/cloudsearch/pc", self.base_url))
            .query(&params)
            .header("Referer", "https://music.163.com/")
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

        let mut candidates = envelope
            .result
            .map(|result| result.songs)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|song| {
                if song.id == 0 || song.name.trim().is_empty() {
                    return None;
                }
                let artist = song_artist(&song);
                let album = song_album(&song);
                let duration_ms = song.dt;
                let confidence = score::score_metadata(
                    query,
                    &song.name,
                    &artist,
                    duration_ms.map(|duration| duration as f64 / 1_000.0),
                );
                if !score::is_confident(confidence) {
                    return None;
                }
                Some(CandidateSummary {
                    provider: Provider::Netease,
                    source_track_id: Some(song.id.to_string()),
                    title: song.name,
                    artist: (!artist.is_empty()).then_some(artist),
                    album,
                    duration_ms,
                    tier: Tier::Line,
                    confidence,
                    local_path: None,
                    browse_hint: None,
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
        if let Some(limiter) = limiter {
            limiter.acquire(cancel).await?;
        }

        let send_fut = self
            .client
            .get(format!("{}/api/song/lyric", self.base_url))
            .query(&[
                ("id", id),
                ("lv", "-1"),
                ("kv", "-1"),
                ("tv", "-1"),
                ("yv", "-1"),
            ])
            .header("Referer", "https://music.163.com/")
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
        let envelope: LyricEnvelope = serde_json::from_str(&body).map_err(invalid_json)?;

        if let Some(text) = envelope.yrc.and_then(|field| field.lyric)
            && let Ok(parsed) = parse_yrc(&text, summary.query_duration_ms)
            && let Some(track) = track_from_parsed(
                parsed,
                Provider::Netease.as_str(),
                Some(id.to_string()),
                summary.confidence,
                summary.query_duration_ms,
                summary.query_user_offset_ms,
            )?
        {
            return Ok(Some(ProviderAnswer::new(Provider::Netease, track)));
        }

        if let Some(text) = envelope.lrc.and_then(|field| field.lyric)
            && let Ok(parsed) = parse_lrc(&text, summary.query_duration_ms)
            && let Some(track) = track_from_parsed(
                parsed,
                Provider::Netease.as_str(),
                Some(id.to_string()),
                summary.confidence,
                summary.query_duration_ms,
                summary.query_user_offset_ms,
            )?
        {
            return Ok(Some(ProviderAnswer::new(Provider::Netease, track)));
        }

        Ok(None)
    }
}

impl LyricProvider for NeteaseProvider {
    fn id(&self) -> Provider {
        Provider::Netease
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

fn song_artist(song: &Song) -> String {
    let names = song
        .ar
        .iter()
        .chain(song.artists.iter())
        .map(|artist| artist.name.trim())
        .filter(|name| !name.is_empty())
        .collect::<Vec<_>>();
    names.join(" / ")
}

fn song_album(song: &Song) -> Option<String> {
    song.al
        .as_ref()
        .or(song.album.as_ref())
        .map(|album| album.name.trim())
        .filter(|name| !name.is_empty())
        .map(str::to_string)
}

fn invalid_json(error: serde_json::Error) -> ProviderError {
    ProviderError::Invalid(format!("invalid NetEase response: {error}"))
}

#[cfg(test)]
mod tests {
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

    #[tokio::test]
    async fn resolves_cloudsearch_result_and_hydrates_yrc() {
        let search = r#"{"result":{"songs":[{"id":123,"name":"Yellow","ar":[{"name":"Coldplay"}],"al":{"name":"Parachutes"},"dt":267000}]}}"#;
        let lyric = r#"{"yrc":{"lyric":"[1000,1000](1000,400,0)Look (1400,600,0)at stars"},"lrc":{"lyric":"[00:01.00]Look at stars"}}"#;
        let (base_url, server) =
            super::super::mock_server(vec![(200, search.to_string()), (200, lyric.to_string())])
                .await;

        let provider = NeteaseProvider::with_base_url(base_url).unwrap();
        let answer = provider.resolve(&query()).await.unwrap().unwrap();

        assert_eq!(answer.provider, Provider::Netease);
        assert_eq!(answer.track.lines[0].text, "Look at stars");
        assert_eq!(answer.tier, Tier::Word);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn falls_back_to_lrc_when_yrc_is_absent() {
        let search = r#"{"result":{"songs":[{"id":123,"name":"Yellow","ar":[{"name":"Coldplay"}],"al":{"name":"Parachutes"},"dt":267000}]}}"#;
        let lyric = r#"{"lrc":{"lyric":"[00:01.00]Line fallback"}}"#;
        let (base_url, server) =
            super::super::mock_server(vec![(200, search.to_string()), (200, lyric.to_string())])
                .await;

        let provider = NeteaseProvider::with_base_url(base_url).unwrap();
        let answer = provider.resolve(&query()).await.unwrap().unwrap();

        assert_eq!(answer.track.lines[0].text, "Line fallback");
        assert_eq!(answer.tier, Tier::Line);
        server.await.unwrap();
    }
}

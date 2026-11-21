use std::time::Duration;

use reqwest::StatusCode;
use serde::Deserialize;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use super::qq_crypto::decode_qrc;
use super::{decode_base64_bounded, decode_utf8, track_from_parsed};
use crate::core::parse::lrc::parse_lrc;
use crate::core::parse::qrc::parse_qrc;
use crate::providers::http;
use crate::providers::score;
use crate::providers::{
    CandidateSummary, LyricProvider, Provider, ProviderAnswer, ProviderCapabilities, ProviderError,
    Tier, TrackQuery,
};

const SEARCH_BASE_URL: &str = "https://c.y.qq.com";
const LYRIC_BASE_URL: &str = "https://u.y.qq.com";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(8);
const MAX_CANDIDATES: usize = 10;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct SearchEnvelope {
    data: Option<SearchData>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct SearchData {
    song: Option<SearchSong>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct SearchSong {
    list: Vec<SearchItem>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct SearchItem {
    songid: Option<u64>,
    songmid: String,
    songname: String,
    albumname: String,
    interval: Option<u64>,
    singer: Vec<Singer>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct Singer {
    name: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct MusicuEnvelope {
    req: Option<MusicuRequest>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct MusicuRequest {
    code: i64,
    data: Option<MusicuData>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct MusicuData {
    lyric: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct LegacyLyricEnvelope {
    lyric: Option<String>,
}

pub struct QqMusicProvider {
    client: reqwest::Client,
    search_base_url: String,
    lyric_base_url: String,
}

impl QqMusicProvider {
    pub fn new() -> Result<Self, ProviderError> {
        Self::with_urls(SEARCH_BASE_URL, LYRIC_BASE_URL)
    }

    pub fn with_base_url(base_url: impl Into<String>) -> Result<Self, ProviderError> {
        let base_url = base_url.into();
        Self::with_urls(&base_url, &base_url)
    }

    fn with_urls(
        search_base_url: impl Into<String>,
        lyric_base_url: impl Into<String>,
    ) -> Result<Self, ProviderError> {
        let client = http::client_builder().timeout(REQUEST_TIMEOUT).build()?;
        Ok(Self {
            client,
            search_base_url: search_base_url.into().trim_end_matches('/').to_string(),
            lyric_base_url: lyric_base_url.into().trim_end_matches('/').to_string(),
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

        let text = format!("{} {}", query.artist, query.title);
        let params = [
            ("p", "1".to_string()),
            ("n", MAX_CANDIDATES.to_string()),
            ("w", text.trim().to_string()),
            ("format", "json".to_string()),
        ];
        let send_fut = self
            .client
            .get(format!(
                "{}/soso/fcgi-bin/client_search_cp",
                self.search_base_url
            ))
            .query(&params)
            .header("Referer", "https://y.qq.com/")
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
            .data
            .and_then(|data| data.song)
            .map(|song| song.list)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|item| {
                let song_id = item.songid?;
                if song_id == 0 || item.songname.trim().is_empty() {
                    return None;
                }
                let artist = item
                    .singer
                    .iter()
                    .map(|singer| singer.name.trim())
                    .filter(|name| !name.is_empty())
                    .collect::<Vec<_>>()
                    .join(" / ");
                let duration_ms = item.interval.map(|seconds| seconds * 1_000);
                let confidence = score::score_metadata(
                    query,
                    &item.songname,
                    &artist,
                    item.interval.map(|seconds| seconds as f64),
                );
                if !score::is_confident(confidence) {
                    return None;
                }
                Some(CandidateSummary {
                    provider: Provider::QqMusic,
                    source_track_id: Some(song_id.to_string()),
                    title: item.songname,
                    artist: (!artist.is_empty()).then_some(artist),
                    album: (!item.albumname.trim().is_empty()).then(|| item.albumname.clone()),
                    duration_ms,
                    tier: Tier::Line,
                    confidence,
                    local_path: None,
                    browse_hint: (!item.songmid.is_empty()).then_some(item.songmid),
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
        let Some(song_id) = summary.source_track_id.as_deref() else {
            return Ok(None);
        };
        let Some(song_mid) = summary.browse_hint.as_deref() else {
            return Ok(None);
        };
        if let Some(limiter) = limiter {
            limiter.acquire(cancel).await?;
        }

        if let Some(text) = self.qrc(song_id, song_mid, cancel).await?
            && let Ok(decoded) = decode_qrc(&text)
            && let Ok(parsed) = parse_qrc(&decoded, summary.query_duration_ms)
            && let Some(track) = track_from_parsed(
                parsed,
                Provider::QqMusic.as_str(),
                Some(song_id.to_string()),
                summary.confidence,
                summary.query_duration_ms,
                summary.query_user_offset_ms,
            )?
        {
            return Ok(Some(ProviderAnswer::new(Provider::QqMusic, track)));
        }

        if let Some(text) = self.legacy_lrc(song_mid, cancel).await?
            && let Ok(parsed) = parse_lrc(&text, summary.query_duration_ms)
            && let Some(track) = track_from_parsed(
                parsed,
                Provider::QqMusic.as_str(),
                Some(song_id.to_string()),
                summary.confidence,
                summary.query_duration_ms,
                summary.query_user_offset_ms,
            )?
        {
            return Ok(Some(ProviderAnswer::new(Provider::QqMusic, track)));
        }

        Ok(None)
    }

    async fn qrc(
        &self,
        song_id: &str,
        song_mid: &str,
        cancel: &CancellationToken,
    ) -> Result<Option<String>, ProviderError> {
        let body = json!({
            "comm": {
                "ct": "19",
                "cv": "1859",
                "uin": "0"
            },
            "req": {
                "method": "GetPlayLyricInfo",
                "module": "music.musichallSong.PlayLyricInfo",
                "param": {
                    "format": "json",
                    "crypt": 1,
                    "ct": 19,
                    "cv": 1873,
                    "interval": 0,
                    "lrc_t": 0,
                    "qrc": 1,
                    "qrc_t": 0,
                    "roma": 0,
                    "roma_t": 0,
                    "songID": song_id.parse::<u64>().unwrap_or(0),
                    "songMID": song_mid,
                    "trans": 0,
                    "trans_t": 0,
                    "type": -1
                }
            }
        });
        let send_fut = self
            .client
            .post(format!("{}/cgi-bin/musicu.fcg", self.lyric_base_url))
            .header("Referer", "https://y.qq.com/")
            .json(&body)
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
        let envelope: MusicuEnvelope = serde_json::from_str(&body).map_err(invalid_json)?;
        let Some(request) = envelope.req.filter(|request| request.code == 0) else {
            return Ok(None);
        };
        Ok(request
            .data
            .and_then(|data| data.lyric)
            .filter(|lyric| !lyric.trim().is_empty()))
    }

    async fn legacy_lrc(
        &self,
        song_mid: &str,
        cancel: &CancellationToken,
    ) -> Result<Option<String>, ProviderError> {
        let send_fut = self
            .client
            .get(format!(
                "{}/lyric/fcgi-bin/fcg_query_lyric_new.fcg",
                self.search_base_url
            ))
            .query(&[
                ("songmid", song_mid),
                ("format", "json"),
                ("nobase64", "1"),
                ("g_tk", "5381"),
                ("loginUin", "0"),
                ("hostUin", "0"),
                ("platform", "yqq"),
                ("inCharset", "utf8"),
                ("outCharset", "utf-8"),
            ])
            .header("Referer", "https://y.qq.com/")
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
        let envelope: LegacyLyricEnvelope = serde_json::from_str(&body).map_err(invalid_json)?;
        let Some(lyric) = envelope.lyric.filter(|lyric| !lyric.trim().is_empty()) else {
            return Ok(None);
        };
        if lyric.contains('[') {
            return Ok(Some(decode_entities(&lyric)));
        }

        let bytes = decode_base64_bounded(&lyric)?;
        decode_utf8(bytes).map(|text| Some(decode_entities(&text)))
    }
}

impl LyricProvider for QqMusicProvider {
    fn id(&self) -> Provider {
        Provider::QqMusic
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

fn decode_entities(input: &str) -> String {
    input
        .replace("&#10;", "\n")
        .replace("&#58;", ":")
        .replace("&#39;", "'")
        .replace("&#32;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
}

fn invalid_json(error: serde_json::Error) -> ProviderError {
    ProviderError::Invalid(format!("invalid QQ Music response: {error}"))
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
    async fn resolves_search_result_and_decrypts_qrc() {
        let search = r#"{"data":{"song":{"list":[{"songid":123,"songmid":"mid123","songname":"Yellow","albumname":"Parachutes","interval":267,"singer":[{"name":"Coldplay"}]}]}}}"#;
        let lyric = r#"{"req":{"code":0,"data":{"lyric":"EE7BEFB746F93831D42DCDF2E21F0D5D977DE00C85449A7C535DA3BCF1BEB3CA54D5D801D40005B7819BEA94D6A3734A54A024D295630DCE5F98584248D26B4046E23B9826994A79ED9023F0130273603388036481F618FB65FA46AF184DF6DE"}}}"#;
        let (base_url, server) =
            super::super::mock_server(vec![(200, search.to_string()), (200, lyric.to_string())])
                .await;

        let provider = QqMusicProvider::with_base_url(base_url).unwrap();
        let answer = provider.resolve(&query()).await.unwrap().unwrap();

        assert_eq!(answer.provider, Provider::QqMusic);
        assert_eq!(answer.track.lines[0].text, "Look & shine");
        assert_eq!(answer.track.lines[0].words.len(), 2);
        server.await.unwrap();
    }
}

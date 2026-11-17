use std::collections::HashSet;
use std::time::Duration;

use reqwest::StatusCode;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::http;
use super::sanitize::{clean_artist, clean_title, normalize_for_match, split_artist_title};
use super::score;
use super::{
    CandidateSummary, Provider, ProviderAnswer, ProviderError, Tier, TokenBucketRateLimiter,
    TrackQuery,
};
use crate::core::model::LineRole;
use crate::core::normalize::{normalize_plain, normalize_track_checked};
use crate::core::parse::{ParsedLine, ParsedMetadata, ParsedTrack};

const BASE_URL: &str = "https://music.youtube.com/youtubei/v1";
const WEB_CLIENT_NAME: &str = "WEB_REMIX";
const WEB_CLIENT_VERSION: &str = "1.20240101.01.00";
const WEB_CLIENT_ID: &str = "67";
const SEARCH_CLIENT_NAME: &str = "TVHTML5_SIMPLY_EMBEDDED_PLAYER";
const SEARCH_CLIENT_VERSION: &str = "2.0";
const SEARCH_CLIENT_ID: &str = "85";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(8);
const REFERER: &str = "https://music.youtube.com/";
const MAX_SEARCH_CANDIDATES: usize = 5;

#[derive(Debug, Clone)]
struct YtSearchCandidate {
    video_id: String,
    title: String,
    artist: Option<String>,
    album: Option<String>,
    duration_ms: Option<u64>,
    score: f32,
}

#[derive(Clone)]
pub struct YtMusicProvider {
    client: reqwest::Client,
    base_url: String,
}

impl YtMusicProvider {
    pub fn new() -> Result<Self, ProviderError> {
        Self::with_base_url(BASE_URL)
    }

    pub fn with_base_url(base_url: impl Into<String>) -> Result<Self, ProviderError> {
        let client = http::browser_client_builder()
            .timeout(REQUEST_TIMEOUT)
            .build()?;
        let base_url = base_url.into().trim_end_matches('/').to_string();
        Ok(Self { client, base_url })
    }

    fn api_post(
        &self,
        endpoint: &str,
        client_id: &'static str,
        client_version: &'static str,
    ) -> reqwest::RequestBuilder {
        self.client
            .post(format!("{}/{endpoint}", self.base_url))
            .header("X-YouTube-Client-Name", client_id)
            .header("X-YouTube-Client-Version", client_version)
            .header("Referer", REFERER)
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

        let (video_id, confidence) =
            if let Some(id) = query.track_url.as_deref().and_then(extract_video_id) {
                (id, 1.0)
            } else {
                match self.search_track(query, cancel).await? {
                    Some((id, score)) => (id, score),
                    None => return Ok(None),
                }
            };

        let browse_id = match self.get_lyrics_browse_id(&video_id, cancel).await? {
            Some(id) => id,
            None => return Ok(None),
        };

        self.fetch_lyrics(&browse_id, &video_id, confidence, query, cancel)
            .await
    }

    pub async fn search_candidates(
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

        let candidates = self.search_raw(query, cancel).await?;
        Ok(candidates
            .into_iter()
            .take(MAX_SEARCH_CANDIDATES)
            .map(|item| CandidateSummary {
                provider: Provider::YtMusic,
                source_track_id: Some(item.video_id.clone()),
                title: if item.title.is_empty() {
                    query.title.clone()
                } else {
                    item.title
                },
                artist: item
                    .artist
                    .or_else(|| (!query.artist.is_empty()).then(|| query.artist.clone())),
                album: item.album.or_else(|| query.album.clone()),
                duration_ms: item.duration_ms.or(query.duration_ms),
                tier: Tier::Plain,
                confidence: item.score,
                local_path: None,
                browse_hint: Some(item.video_id),
                query_duration_ms: query.duration_ms,
                query_user_offset_ms: query.user_offset_ms,
                hydrated: None,
            })
            .collect())
    }

    pub async fn hydrate_candidate(
        &self,
        summary: &CandidateSummary,
        limiter: Option<&TokenBucketRateLimiter>,
        cancel: &CancellationToken,
    ) -> Result<Option<ProviderAnswer>, ProviderError> {
        if cancel.is_cancelled() {
            return Err(ProviderError::Cancelled);
        }
        let Some(video_id) = summary
            .browse_hint
            .as_deref()
            .or(summary.source_track_id.as_deref())
        else {
            return Ok(None);
        };

        if let Some(limiter) = limiter {
            limiter.acquire(cancel).await?;
        }
        let Some(browse_id) = self.get_lyrics_browse_id(video_id, cancel).await? else {
            return Ok(None);
        };

        if let Some(limiter) = limiter {
            limiter.acquire(cancel).await?;
        }
        let hydrate_query = TrackQuery::from_meta(
            &crate::media::TrackMeta {
                title: summary.title.clone(),
                artist: summary.artist.clone().unwrap_or_default(),
                album: summary.album.clone(),
                duration_ms: summary.query_duration_ms.or(summary.duration_ms),
                track_id: summary.source_track_id.clone(),
                url: None,
            },
            summary.query_user_offset_ms,
        );
        self.fetch_lyrics(
            &browse_id,
            video_id,
            summary.confidence,
            &hydrate_query,
            cancel,
        )
        .await
    }

    async fn search_track(
        &self,
        query: &TrackQuery,
        cancel: &CancellationToken,
    ) -> Result<Option<(String, f32)>, ProviderError> {
        let candidates = self.search_raw(query, cancel).await?;
        Ok(candidates
            .into_iter()
            .find(|item| score::is_confident(item.score))
            .map(|item| (item.video_id, item.score)))
    }

    async fn search_raw(
        &self,
        query: &TrackQuery,
        cancel: &CancellationToken,
    ) -> Result<Vec<YtSearchCandidate>, ProviderError> {
        let query_text = format!("{} {}", query.artist, query.title);
        let payload = serde_json::json!({
            "context": {
                "client": {
                    "clientName": SEARCH_CLIENT_NAME,
                    "clientVersion": SEARCH_CLIENT_VERSION,
                    "hl": "en"
                }
            },
            "query": query_text
        });

        let send_fut = self
            .api_post("search", SEARCH_CLIENT_ID, SEARCH_CLIENT_VERSION)
            .json(&payload)
            .send();

        let response = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(ProviderError::Cancelled),
            res = send_fut => res?,
        };

        match response.status() {
            StatusCode::NOT_FOUND => Ok(Vec::new()),
            status if status.is_success() => {
                let body = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => return Err(ProviderError::Cancelled),
                    res = http::read_limited_text(response) => res?,
                };
                self.parse_search_candidates(&body, query)
            }
            StatusCode::TOO_MANY_REQUESTS => Err(ProviderError::RateLimited {
                retry_after_ms: http::retry_after_ms(&response),
            }),
            status if status.is_client_error() => Err(ProviderError::Invalid(format!(
                "YouTube Music search returned HTTP {status}"
            ))),
            status => Err(ProviderError::Unavailable(format!(
                "YouTube Music search returned HTTP {status}"
            ))),
        }
    }

    fn parse_search_candidates(
        &self,
        body: &str,
        query: &TrackQuery,
    ) -> Result<Vec<YtSearchCandidate>, ProviderError> {
        let body = body.trim();
        if body.is_empty() {
            return Ok(Vec::new());
        }
        let val: Value = serde_json::from_str(body).map_err(invalid_json)?;

        let mut items = Vec::new();
        find_music_responsive_items(&val, &mut items);
        find_compact_video_items(&val, &mut items);

        if items.is_empty() {
            return Ok(Vec::new());
        }

        let mut seen = HashSet::new();
        let mut results = Vec::new();

        for item in items {
            let Some(video_id) = extract_item_video_id(item) else {
                continue;
            };
            if !seen.insert(video_id.clone()) {
                continue;
            }
            let (title, artist, album, duration_sec) = extract_item_metadata(item);
            let title_str = title.as_deref().unwrap_or_default();
            let artist_str = artist.as_deref().unwrap_or_default();

            let score = score::score_metadata(query, title_str, artist_str, duration_sec);
            let duration_ms = duration_sec
                .filter(|d| d.is_finite() && *d > 0.0)
                .map(|d| (d * 1_000.0).round() as u64);

            results.push(YtSearchCandidate {
                video_id,
                title: title.unwrap_or_default(),
                artist: artist.filter(|a| !a.trim().is_empty()),
                album: album.filter(|a| !a.trim().is_empty()),
                duration_ms,
                score,
            });
        }

        results.sort_by(|left, right| right.score.total_cmp(&left.score));
        Ok(results)
    }

    #[cfg(test)]
    fn parse_search_response(
        &self,
        body: &str,
        query: &TrackQuery,
    ) -> Result<Option<(String, f32)>, ProviderError> {
        let candidates = self.parse_search_candidates(body, query)?;
        Ok(candidates
            .into_iter()
            .find(|item| score::is_confident(item.score))
            .map(|item| (item.video_id, item.score)))
    }

    async fn get_lyrics_browse_id(
        &self,
        video_id: &str,
        cancel: &CancellationToken,
    ) -> Result<Option<String>, ProviderError> {
        let payload = serde_json::json!({
            "context": {
                "client": {
                    "clientName": WEB_CLIENT_NAME,
                    "clientVersion": WEB_CLIENT_VERSION,
                    "hl": "en"
                }
            },
            "videoId": video_id
        });

        let send_fut = self
            .api_post("next", WEB_CLIENT_ID, WEB_CLIENT_VERSION)
            .json(&payload)
            .send();

        let response = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(ProviderError::Cancelled),
            res = send_fut => res?,
        };

        match response.status() {
            StatusCode::NOT_FOUND => Ok(None),
            status if status.is_success() => {
                let body = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => return Err(ProviderError::Cancelled),
                    res = http::read_limited_text(response) => res?,
                };
                self.parse_next_response(&body)
            }
            StatusCode::TOO_MANY_REQUESTS => Err(ProviderError::RateLimited {
                retry_after_ms: http::retry_after_ms(&response),
            }),
            status if status.is_client_error() => Err(ProviderError::Invalid(format!(
                "YouTube Music /next returned HTTP {status}"
            ))),
            status => Err(ProviderError::Unavailable(format!(
                "YouTube Music /next returned HTTP {status}"
            ))),
        }
    }

    fn parse_next_response(&self, body: &str) -> Result<Option<String>, ProviderError> {
        let body = body.trim();
        if body.is_empty() {
            return Ok(None);
        }
        let val: Value = serde_json::from_str(body).map_err(invalid_json)?;

        let mut tabs = Vec::new();
        find_tabs(&val, &mut tabs);

        if tabs.is_empty() {
            if val.get("error").is_some() {
                return Err(ProviderError::Invalid(format!(
                    "YouTube Music /next returned error: {val}"
                )));
            }
            return Err(ProviderError::Invalid(
                "unrecognized YouTube Music /next response format".to_string(),
            ));
        }

        for tab in tabs {
            let title = tab
                .pointer("/title/runs/0/text")
                .and_then(Value::as_str)
                .unwrap_or("");
            let page_type = tab
                .pointer("/endpoint/browseEndpoint/browseEndpointContextSupportedConfigs/browseEndpointContextMusicConfig/pageType")
                .and_then(Value::as_str)
                .unwrap_or("");
            let browse_id = tab
                .pointer("/endpoint/browseEndpoint/browseId")
                .and_then(Value::as_str);

            let is_lyrics_tab = title.eq_ignore_ascii_case("lyrics")
                || page_type == "MUSIC_PAGE_TYPE_TRACK_LYRICS"
                || browse_id.is_some_and(|id| id.starts_with("MPLYt"));

            if is_lyrics_tab {
                if tab.get("unselectable").and_then(Value::as_bool) == Some(true) {
                    return Ok(None);
                }
                if let Some(id) = browse_id {
                    return Ok(Some(id.to_string()));
                }
            }
        }

        Ok(None)
    }

    async fn fetch_lyrics(
        &self,
        browse_id: &str,
        video_id: &str,
        confidence: f32,
        query: &TrackQuery,
        cancel: &CancellationToken,
    ) -> Result<Option<ProviderAnswer>, ProviderError> {
        let payload = serde_json::json!({
            "context": {
                "client": {
                    "clientName": WEB_CLIENT_NAME,
                    "clientVersion": WEB_CLIENT_VERSION,
                    "hl": "en"
                }
            },
            "browseId": browse_id
        });

        let send_fut = self
            .api_post("browse", WEB_CLIENT_ID, WEB_CLIENT_VERSION)
            .json(&payload)
            .send();

        let response = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(ProviderError::Cancelled),
            res = send_fut => res?,
        };

        match response.status() {
            StatusCode::NOT_FOUND => Ok(None),
            status if status.is_success() => {
                let body = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => return Err(ProviderError::Cancelled),
                    res = http::read_limited_text(response) => res?,
                };
                Self::parse_browse_response(&body, Some(video_id), confidence, query)
            }
            StatusCode::TOO_MANY_REQUESTS => Err(ProviderError::RateLimited {
                retry_after_ms: http::retry_after_ms(&response),
            }),
            status if status.is_client_error() => Err(ProviderError::Invalid(format!(
                "YouTube Music /browse returned HTTP {status}"
            ))),
            status => Err(ProviderError::Unavailable(format!(
                "YouTube Music /browse returned HTTP {status}"
            ))),
        }
    }

    pub fn parse_browse_response(
        body: &str,
        video_id: Option<&str>,
        confidence: f32,
        query: &TrackQuery,
    ) -> Result<Option<ProviderAnswer>, ProviderError> {
        let body = body.trim();
        if body.is_empty() {
            return Ok(None);
        }

        let val: Value = serde_json::from_str(body).map_err(invalid_json)?;

        if let Some(timed_renderer) = find_object_with_key(&val, "musicTimedLyricsRenderer")
            && let Some(answer) = parse_timed_lyrics(timed_renderer, video_id, confidence, query)?
        {
            return Ok(Some(answer));
        }

        if let Some(shelf_renderer) = find_object_with_key(&val, "musicDescriptionShelfRenderer")
            && let Some(answer) = parse_plain_lyrics(shelf_renderer, video_id, confidence, query)
        {
            return Ok(Some(answer));
        }

        if is_known_empty_lyrics(&val) {
            return Ok(None);
        }

        Err(ProviderError::Invalid(
            "unrecognized YouTube Music browse response format".to_string(),
        ))
    }
}

fn invalid_json(err: serde_json::Error) -> ProviderError {
    ProviderError::Invalid(format!("malformed YouTube Music JSON payload: {err}"))
}

pub fn extract_video_id(url: &str) -> Option<String> {
    let url = url.trim();
    if url.is_empty() {
        return None;
    }

    if let Ok(parsed) = reqwest::Url::parse(url) {
        let host = parsed.host_str().unwrap_or("");
        if host.ends_with("youtu.be") {
            let path = parsed.path().trim_start_matches('/');
            let id = path.split(['/', '?', '&']).next().unwrap_or("");
            if is_valid_video_id(id) {
                return Some(id.to_string());
            }
        } else if host.ends_with("youtube.com") {
            for (key, val) in parsed.query_pairs() {
                if (key == "v" || key == "vi") && is_valid_video_id(&val) {
                    return Some(val.to_string());
                }
            }
        }
    }

    if let Some(pos) = url.find("v=") {
        let after = &url[pos + 2..];
        let id: String = after
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
            .collect();
        if is_valid_video_id(&id) {
            return Some(id);
        }
    }

    None
}

fn is_valid_video_id(id: &str) -> bool {
    id.len() == 11
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn parse_duration_str(s: &str) -> Option<f64> {
    let parts: Vec<&str> = s.split(':').collect();
    match parts.as_slice() {
        [min, sec] => {
            let m: f64 = min.parse().ok()?;
            let s: f64 = sec.parse().ok()?;
            Some(m * 60.0 + s)
        }
        [hr, min, sec] => {
            let h: f64 = hr.parse().ok()?;
            let m: f64 = min.parse().ok()?;
            let s: f64 = sec.parse().ok()?;
            Some(h * 3600.0 + m * 60.0 + s)
        }
        _ => None,
    }
}

fn parse_ms_value(val: Option<&Value>) -> Option<u64> {
    match val? {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => s.trim().parse::<u64>().ok(),
        _ => None,
    }
}

fn find_object_with_key<'a>(val: &'a Value, key: &str) -> Option<&'a Value> {
    if let Some(obj) = val.as_object() {
        if let Some(found) = obj.get(key) {
            return Some(found);
        }
        for (_, v) in obj {
            if let Some(found) = find_object_with_key(v, key) {
                return Some(found);
            }
        }
    } else if let Some(arr) = val.as_array() {
        for v in arr {
            if let Some(found) = find_object_with_key(v, key) {
                return Some(found);
            }
        }
    }
    None
}

fn find_music_responsive_items<'a>(val: &'a Value, out: &mut Vec<&'a Value>) {
    if let Some(obj) = val.as_object() {
        if let Some(item) = obj.get("musicResponsiveListItemRenderer") {
            out.push(item);
        } else {
            for (_, v) in obj {
                find_music_responsive_items(v, out);
            }
        }
    } else if let Some(arr) = val.as_array() {
        for v in arr {
            find_music_responsive_items(v, out);
        }
    }
}

fn find_compact_video_items<'a>(val: &'a Value, out: &mut Vec<&'a Value>) {
    if let Some(obj) = val.as_object() {
        if let Some(item) = obj.get("compactVideoRenderer") {
            out.push(item);
        } else {
            for (_, v) in obj {
                find_compact_video_items(v, out);
            }
        }
    } else if let Some(arr) = val.as_array() {
        for v in arr {
            find_compact_video_items(v, out);
        }
    }
}

fn find_tabs<'a>(val: &'a Value, out: &mut Vec<&'a Value>) {
    if let Some(obj) = val.as_object() {
        if let Some(tab) = obj.get("tabRenderer") {
            out.push(tab);
        } else {
            for (_, v) in obj {
                find_tabs(v, out);
            }
        }
    } else if let Some(arr) = val.as_array() {
        for v in arr {
            find_tabs(v, out);
        }
    }
}

fn extract_item_video_id(item: &Value) -> Option<String> {
    if let Some(id) = item.get("videoId").and_then(Value::as_str)
        && is_valid_video_id(id)
    {
        return Some(id.to_string());
    }
    if let Some(id) = item
        .pointer("/playlistItemData/videoId")
        .and_then(Value::as_str)
        && is_valid_video_id(id)
    {
        return Some(id.to_string());
    }
    if let Some(id) = item
        .pointer("/navigationEndpoint/watchEndpoint/videoId")
        .and_then(Value::as_str)
        && is_valid_video_id(id)
    {
        return Some(id.to_string());
    }
    if let Some(id) = item
        .pointer("/overlay/musicItemThumbnailOverlayRenderer/content/musicPlayButtonRenderer/playNavigationEndpoint/watchEndpoint/videoId")
        .and_then(Value::as_str)
        && is_valid_video_id(id)
    {
        return Some(id.to_string());
    }
    None
}

fn extract_item_metadata(
    item: &Value,
) -> (Option<String>, Option<String>, Option<String>, Option<f64>) {
    let Some(cols) = item.get("flexColumns").and_then(Value::as_array) else {
        return extract_compact_video_metadata(item);
    };

    let mut title = None;
    let mut artist = None;
    let mut album = None;
    let mut duration_sec = None;

    {
        if let Some(col0) = cols.first() {
            title = flex_column_runs_text(col0);
        }
        if cols.len() > 1 {
            let runs = flex_column_runs(&cols[1]);
            for run_text in runs {
                let trimmed = run_text.trim();
                if trimmed.is_empty()
                    || trimmed == "•"
                    || trimmed.eq_ignore_ascii_case("song")
                    || trimmed.eq_ignore_ascii_case("video")
                {
                    continue;
                }
                if let Some(dur) = parse_duration_str(trimmed) {
                    duration_sec = Some(dur);
                } else if artist.is_none() {
                    artist = Some(trimmed.to_string());
                } else if album.is_none() {
                    album = Some(trimmed.to_string());
                }
            }
        }
    }

    (title, artist, album, duration_sec)
}

fn extract_compact_video_metadata(
    item: &Value,
) -> (Option<String>, Option<String>, Option<String>, Option<f64>) {
    let raw_title = text_runs(item.pointer("/title/runs"));
    let byline = text_runs(item.pointer("/longBylineText/runs"))
        .or_else(|| text_runs(item.pointer("/shortBylineText/runs")))
        .map(|artist| clean_artist(&artist))
        .filter(|artist| !artist.is_empty());
    let duration_sec = item
        .pointer("/lengthText/runs/0/text")
        .and_then(Value::as_str)
        .and_then(parse_duration_str);

    let Some(raw_title) = raw_title else {
        return (None, byline, None, duration_sec);
    };

    let (title, artist) = match split_artist_title(&raw_title) {
        Some((left, right)) => {
            let left_artist = clean_artist(&left);
            let right_title = clean_title(&right);
            let artist_matches = byline.as_deref().is_some_and(|artist| {
                normalize_for_match(artist) == normalize_for_match(&left_artist)
            });
            if !left_artist.is_empty()
                && !right_title.is_empty()
                && (byline.is_none() || artist_matches)
            {
                (Some(right_title), byline.or(Some(left_artist)))
            } else {
                (Some(clean_title(&raw_title)), byline)
            }
        }
        None => (Some(clean_title(&raw_title)), byline),
    };

    (title, artist, None, duration_sec)
}

fn text_runs(runs: Option<&Value>) -> Option<String> {
    let mut text = String::new();
    for run in runs?.as_array()? {
        if let Some(value) = run.get("text").and_then(Value::as_str) {
            text.push_str(value);
        }
    }
    let text = text.trim();
    if text.is_empty() {
        None
    } else {
        Some(text.to_string())
    }
}

fn flex_column_runs(col: &Value) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(runs) = col
        .pointer("/musicResponsiveListItemFlexColumnRenderer/text/runs")
        .and_then(Value::as_array)
    {
        for r in runs {
            if let Some(t) = r.get("text").and_then(Value::as_str) {
                out.push(t.to_string());
            }
        }
    }
    out
}

fn flex_column_runs_text(col: &Value) -> Option<String> {
    let runs = flex_column_runs(col);
    if runs.is_empty() {
        None
    } else {
        Some(runs.join("").trim().to_string())
    }
}

fn parse_timed_lyrics(
    renderer: &Value,
    video_id: Option<&str>,
    confidence: f32,
    query: &TrackQuery,
) -> Result<Option<ProviderAnswer>, ProviderError> {
    let Some(timed_data) = renderer.get("timedLyricsData").and_then(Value::as_array) else {
        return Ok(None);
    };

    if timed_data.is_empty() {
        return Ok(None);
    }

    let mut lines = Vec::new();
    for item in timed_data {
        let lyric_line = item
            .get("lyricLine")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        let cue_range = item.get("cueRange");

        let Some(start_ms) = cue_range.and_then(|c| parse_ms_value(c.get("startTimeMs"))) else {
            continue;
        };
        let end_ms = cue_range.and_then(|c| parse_ms_value(c.get("endTimeMs")));

        if lyric_line.is_empty() {
            continue;
        }

        lines.push(ParsedLine {
            start_ms,
            end_ms,
            role: LineRole::Primary,
            text: lyric_line.to_string(),
            words: Vec::new(),
            annotations: Vec::new(),
            explicit_last_word_end_ms: None,
        });
    }

    if lines.is_empty() {
        return Ok(None);
    }

    let parsed = ParsedTrack {
        lines,
        metadata: ParsedMetadata {
            title: Some(query.title.clone()),
            artist: Some(query.artist.clone()),
            album: query.album.clone(),
            language: None,
        },
        ..ParsedTrack::default()
    };

    let track = normalize_track_checked(
        parsed,
        Provider::YtMusic.as_str(),
        video_id.map(str::to_string),
        confidence,
        query.duration_ms,
        query.user_offset_ms,
    )
    .map_err(|error| ProviderError::Invalid(error.to_string()))?;

    if track.lines.is_empty() {
        return Ok(None);
    }

    Ok(Some(ProviderAnswer::new(Provider::YtMusic, track)))
}

fn parse_plain_lyrics(
    renderer: &Value,
    video_id: Option<&str>,
    confidence: f32,
    query: &TrackQuery,
) -> Option<ProviderAnswer> {
    let runs = renderer
        .pointer("/description/runs")
        .and_then(Value::as_array)?;

    let mut text = String::new();
    for run in runs {
        if let Some(t) = run.get("text").and_then(Value::as_str) {
            text.push_str(t);
        }
    }

    let text = text.trim();
    if text.is_empty() {
        return None;
    }

    let track = normalize_plain(
        text,
        Provider::YtMusic.as_str(),
        video_id.map(str::to_string),
        confidence,
        query.user_offset_ms,
    );

    if track.lines.is_empty() {
        return None;
    }

    Some(ProviderAnswer::new(Provider::YtMusic, track))
}

fn is_known_empty_lyrics(val: &Value) -> bool {
    if let Some(contents) = val
        .pointer("/contents/sectionListRenderer/contents")
        .and_then(Value::as_array)
        && contents.is_empty()
    {
        return true;
    }
    if find_object_with_key(val, "messageRenderer").is_some() {
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::TrackMeta;
    use crate::providers::Tier;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn query() -> TrackQuery {
        let meta = TrackMeta {
            title: "Yellow".to_string(),
            artist: "Coldplay".to_string(),
            album: Some("Parachutes".to_string()),
            duration_ms: Some(269_000),
            track_id: None,
            url: None,
        };
        TrackQuery::from_meta(&meta, 0)
    }

    #[test]
    fn fixture_parses_as_line_sync() {
        let fixture = include_str!("../../tests/fixtures/ytmusic_response.json");
        let query = query();
        let answer =
            YtMusicProvider::parse_browse_response(fixture, Some("yKNxeF4KMsY"), 1.0, &query)
                .unwrap()
                .expect("should produce answer");

        assert_eq!(answer.provider, Provider::YtMusic);
        assert_eq!(answer.tier, Tier::Line);
        assert_eq!(answer.track.lines.len(), 4);
        assert_eq!(answer.track.lines[0].text, "Look at the stars");
        assert_eq!(answer.track.lines[0].start_ms, 33370);
        assert_eq!(answer.track.lines[0].end_ms, 36740);
        assert_eq!(answer.track.lines[3].text, "Yeah, they were all yellow");
        assert_eq!(answer.track.source_provider, "ytmusic");
        assert_eq!(answer.track.source_track_id.as_deref(), Some("yKNxeF4KMsY"));
    }

    #[test]
    fn search_parses_compact_video_renderer() {
        let json = r#"{
            "contents": {
                "sectionListRenderer": {
                    "contents": [
                        {
                            "itemSectionRenderer": {
                                "contents": [
                                    {
                                        "compactVideoRenderer": {
                                            "videoId": "yKNxeF4KMsY",
                                            "title": {
                                                "runs": [{ "text": "Coldplay - Yellow (Official Video)" }]
                                            },
                                            "longBylineText": {
                                                "runs": [{ "text": "Coldplay" }]
                                            },
                                            "lengthText": {
                                                "runs": [{ "text": "4:33" }]
                                            }
                                        }
                                    }
                                ]
                            }
                        }
                    ]
                }
            }
        }"#;

        let provider = YtMusicProvider::with_base_url("http://127.0.0.1:1").unwrap();
        let (video_id, confidence) = provider
            .parse_search_response(json, &query())
            .unwrap()
            .expect("should select compact video result");

        assert_eq!(video_id, "yKNxeF4KMsY");
        assert!(confidence >= 0.65);
    }

    #[test]
    fn plain_lyrics_parses_as_unsynced() {
        let json = r#"{
            "contents": {
                "sectionListRenderer": {
                    "contents": [
                        {
                            "musicDescriptionShelfRenderer": {
                                "description": {
                                    "runs": [
                                        { "text": "Look at the stars\n" },
                                        { "text": "Look how they shine for you\n" },
                                        { "text": "And everything you do\n" },
                                        { "text": "Yeah, they were all yellow" }
                                    ]
                                },
                                "footer": {
                                    "runs": [
                                        { "text": "Source: LyricFind" }
                                    ]
                                }
                            }
                        }
                    ]
                }
            }
        }"#;

        let query = query();
        let answer =
            YtMusicProvider::parse_browse_response(json, Some("yKNxeF4KMsY"), 0.95, &query)
                .unwrap()
                .expect("should parse plain lyrics");

        assert_eq!(answer.provider, Provider::YtMusic);
        assert_eq!(answer.tier, Tier::Plain);
        assert_eq!(answer.track.lines.len(), 4);
        assert_eq!(answer.track.lines[0].text, "Look at the stars");
        assert_eq!(answer.track.lines[3].text, "Yeah, they were all yellow");
    }

    #[test]
    fn unrecognized_json_returns_invalid() {
        let json = r#"{"contents": {"unexpectedRenderer": {"foo": "bar"}}}"#;
        let query = query();
        let err = YtMusicProvider::parse_browse_response(json, None, 1.0, &query).unwrap_err();
        assert!(matches!(err, ProviderError::Invalid(_)));
    }

    #[test]
    fn malformed_json_returns_invalid() {
        let json = "{ not valid json }";
        let query = query();
        let err = YtMusicProvider::parse_browse_response(json, None, 1.0, &query).unwrap_err();
        assert!(matches!(err, ProviderError::Invalid(_)));
    }

    #[test]
    fn empty_lyrics_returns_none() {
        let json = r#"{"contents": {"sectionListRenderer": {"contents": []}}}"#;
        let query = query();
        let res = YtMusicProvider::parse_browse_response(json, None, 1.0, &query).unwrap();
        assert!(res.is_none());

        let msg_json = r#"{"contents": {"messageRenderer": {"text": {"runs": [{"text": "Lyrics not available"}]}}}}"#;
        let res2 = YtMusicProvider::parse_browse_response(msg_json, None, 1.0, &query).unwrap();
        assert!(res2.is_none());
    }

    #[test]
    fn extract_video_id_handles_various_urls() {
        assert_eq!(
            extract_video_id("https://music.youtube.com/watch?v=yKNxeF4KMsY"),
            Some("yKNxeF4KMsY".to_string())
        );
        assert_eq!(
            extract_video_id("https://www.youtube.com/watch?v=yKNxeF4KMsY&feature=share"),
            Some("yKNxeF4KMsY".to_string())
        );
        assert_eq!(
            extract_video_id("https://youtu.be/yKNxeF4KMsY?si=12345"),
            Some("yKNxeF4KMsY".to_string())
        );
        assert_eq!(
            extract_video_id("https://youtu.be/yKNxeF4KMsY"),
            Some("yKNxeF4KMsY".to_string())
        );
        assert_eq!(
            extract_video_id("https://example.com/watch?v=too_short"),
            None
        );
        assert_eq!(
            extract_video_id("https://open.spotify.com/track/4cOdK2wGLETKBW3PvgPWqT"),
            None
        );
        assert_eq!(extract_video_id(""), None);
    }

    #[tokio::test]
    async fn cancelled_request_returns_cancelled() {
        let provider = YtMusicProvider::with_base_url("http://127.0.0.1:1").unwrap();
        let cancel = CancellationToken::new();
        cancel.cancel();

        let error = provider
            .resolve_with_cancel(&query(), &cancel)
            .await
            .unwrap_err();

        assert!(matches!(error, ProviderError::Cancelled));
    }

    #[tokio::test]
    async fn mock_server_resolves_via_http() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let fixture = include_str!("../../tests/fixtures/ytmusic_response.json").to_string();

        tokio::spawn(async move {
            for _ in 0..2 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut buffer = vec![0u8; 4096];
                let n = socket.read(&mut buffer).await.unwrap();
                let req_text = String::from_utf8_lossy(&buffer[..n]);

                if req_text.contains("POST /next") {
                    let next_body = r#"{
                        "contents": {
                            "singleColumnMusicWatchNextResultsRenderer": {
                                "tabbedRenderer": {
                                    "watchNextTabbedResultsRenderer": {
                                        "tabs": [
                                            {
                                                "tabRenderer": {
                                                    "title": { "runs": [{ "text": "Lyrics" }] },
                                                    "endpoint": {
                                                        "browseEndpoint": {
                                                            "browseId": "MPLYt_12345"
                                                        }
                                                    },
                                                    "unselectable": false
                                                }
                                            }
                                        ]
                                    }
                                }
                            }
                        }
                    }"#;
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                        next_body.len(),
                        next_body
                    );
                    socket.write_all(resp.as_bytes()).await.unwrap();
                } else if req_text.contains("POST /browse") {
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                        fixture.len(),
                        fixture
                    );
                    socket.write_all(resp.as_bytes()).await.unwrap();
                }
            }
        });

        let provider = YtMusicProvider::with_base_url(format!("http://{addr}")).unwrap();
        let mut q = query();
        q.track_url = Some("https://music.youtube.com/watch?v=yKNxeF4KMsY".to_string());

        let answer = provider
            .resolve_with_cancel(&q, &CancellationToken::new())
            .await
            .unwrap()
            .expect("should resolve through mock server");

        assert_eq!(answer.provider, Provider::YtMusic);
        assert_eq!(answer.tier, Tier::Line);
        assert_eq!(answer.track.lines.len(), 4);
    }

    #[tokio::test]
    async fn search_candidates_caps_at_5_and_delays_next_browse_until_hydrate() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let fixture = include_str!("../../tests/fixtures/ytmusic_response.json");
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let search_hits = Arc::new(AtomicUsize::new(0));
        let next_hits = Arc::new(AtomicUsize::new(0));
        let browse_hits = Arc::new(AtomicUsize::new(0));

        let search_hits_srv = Arc::clone(&search_hits);
        let next_hits_srv = Arc::clone(&next_hits);
        let browse_hits_srv = Arc::clone(&browse_hits);

        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = vec![0u8; 8192];
                let n = socket.read(&mut buf).await.unwrap_or(0);
                if n == 0 {
                    continue;
                }
                let req_text = String::from_utf8_lossy(&buf[..n]).into_owned();

                if req_text.contains("POST /search") {
                    search_hits_srv.fetch_add(1, Ordering::SeqCst);
                    let items = (1..=7)
                        .map(|idx| {
                            serde_json::json!({
                                "musicResponsiveListItemRenderer": {
                                    "playlistItemData": { "videoId": format!("vid0000000{idx}") },
                                    "flexColumns": [
                                        {
                                            "musicResponsiveListItemFlexColumnRenderer": {
                                                "text": {
                                                    "runs": [{ "text": "Yellow" }]
                                                }
                                            }
                                        },
                                        {
                                            "musicResponsiveListItemFlexColumnRenderer": {
                                                "text": {
                                                    "runs": [
                                                        { "text": "Coldplay" },
                                                        { "text": " • " },
                                                        { "text": format!("Album {idx}") },
                                                        { "text": " • " },
                                                        { "text": "4:29" }
                                                    ]
                                                }
                                            }
                                        }
                                    ]
                                }
                            })
                        })
                        .collect::<Vec<_>>();
                    let search_body = serde_json::json!({ "contents": items }).to_string();
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                        search_body.len(),
                        search_body
                    );
                    let _ = socket.write_all(resp.as_bytes()).await;
                } else if req_text.contains("POST /next") {
                    next_hits_srv.fetch_add(1, Ordering::SeqCst);
                    let next_body = r#"{
                        "contents": {
                            "singleColumnMusicWatchNextResultsRenderer": {
                                "tabbedRenderer": {
                                    "watchNextTabbedResultsRenderer": {
                                        "tabs": [{
                                            "tabRenderer": {
                                                "title": "Lyrics",
                                                "endpoint": { "browseEndpoint": { "browseId": "MPLYt_123" } },
                                                "unselectable": false
                                            }
                                        }]
                                    }
                                }
                            }
                        }
                    }"#;
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                        next_body.len(),
                        next_body
                    );
                    let _ = socket.write_all(resp.as_bytes()).await;
                } else if req_text.contains("POST /browse") {
                    browse_hits_srv.fetch_add(1, Ordering::SeqCst);
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                        fixture.len(),
                        fixture
                    );
                    let _ = socket.write_all(resp.as_bytes()).await;
                }
            }
        });

        let provider = YtMusicProvider::with_base_url(format!("http://{addr}")).unwrap();
        let cancel = CancellationToken::new();
        let summaries = provider.search_candidates(&query(), &cancel).await.unwrap();

        assert_eq!(summaries.len(), MAX_SEARCH_CANDIDATES);
        assert!(summaries.iter().all(|s| s.provider == Provider::YtMusic));
        assert!(summaries.iter().all(|s| !s.is_hydrated()));
        assert_eq!(search_hits.load(Ordering::SeqCst), 1);
        assert_eq!(
            next_hits.load(Ordering::SeqCst),
            0,
            "/next must NOT be called during search_candidates"
        );
        assert_eq!(
            browse_hits.load(Ordering::SeqCst),
            0,
            "/browse must NOT be called during search_candidates"
        );

        let answer = provider
            .hydrate_candidate(&summaries[0], None, &cancel)
            .await
            .unwrap()
            .expect("should hydrate selected summary");
        assert_eq!(answer.provider, Provider::YtMusic);
        assert_eq!(answer.tier, Tier::Line);
        assert_eq!(answer.track.lines.len(), 4);
        assert_eq!(next_hits.load(Ordering::SeqCst), 1);
        assert_eq!(browse_hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    #[ignore = "requires live network access to YouTube Music"]
    async fn live_ytmusic_returns_yellow() {
        let provider = YtMusicProvider::new().unwrap();
        let query = query();
        let answer = provider
            .resolve_with_cancel(&query, &CancellationToken::new())
            .await
            .unwrap();

        assert!(answer.is_some());
        let answer = answer.unwrap();
        assert_eq!(answer.provider, Provider::YtMusic);
        assert!(!answer.track.lines.is_empty());
    }
}

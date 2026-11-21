use std::collections::BTreeMap;
use std::time::Duration;

use reqwest::StatusCode;
use serde::{Deserialize, Deserializer};
use tokio_util::sync::CancellationToken;
use tracing::warn;

use super::http;
use super::score;
use super::{
    CandidateSummary, LyricProvider, Provider, ProviderAnswer, ProviderCapabilities, ProviderError,
    TrackQuery,
};
use crate::core::model::{Annotation, AnnotationKind, LineRole};
use crate::core::normalize::normalize_track_checked;
use crate::core::parse::ttml::parse_ttml;
use crate::core::parse::{
    ParsedLine, ParsedMetadata, ParsedTrack, ParsedWord, trim_line_with_words,
};

const BASE_URL: &str = "https://lyricsplus.prjktla.my.id";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(8);

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct LyricsPlusResponse {
    #[serde(deserialize_with = "null_as_default")]
    metadata: LyricsPlusMetadata,
    #[serde(deserialize_with = "null_as_default")]
    lyrics: Vec<LyricsPlusLine>,
    ttml: Option<String>,
    #[serde(deserialize_with = "null_as_default")]
    processing_time: LyricsPlusProcessingTime,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct LyricsPlusMetadata {
    title: Option<String>,
    artist: Option<String>,
    album: Option<String>,
    language: Option<String>,
    #[serde(deserialize_with = "null_as_default")]
    agents: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct LyricsPlusProcessingTime {
    selected_song_metadata: Option<LyricsPlusSelectedSongMetadata>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct LyricsPlusSelectedSongMetadata {
    title: Option<String>,
    artist: Option<String>,
    duration: Option<f64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct LyricsPlusLine {
    time: Option<f64>,
    duration: Option<f64>,
    text: Option<String>,
    #[serde(deserialize_with = "null_as_default")]
    syllabus: Vec<LyricsPlusWord>,
    #[serde(deserialize_with = "null_as_default")]
    element: LyricsPlusElement,
    transliteration: Option<LyricsPlusAnnotation>,
    translation: Option<LyricsPlusAnnotation>,
    #[serde(deserialize_with = "null_as_default")]
    translations: Vec<LyricsPlusAnnotation>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct LyricsPlusWord {
    time: Option<f64>,
    duration: Option<f64>,
    text: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct LyricsPlusElement {
    singer: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct LyricsPlusAnnotation {
    text: Option<String>,
}

fn null_as_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    T: Default + Deserialize<'de>,
    D: Deserializer<'de>,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

struct Piece<'a> {
    text: &'a str,
    start_ms: u64,
    end_ms: Option<u64>,
}

pub struct LyricsPlusProvider {
    client: reqwest::Client,
    base_url: String,
}

impl LyricsPlusProvider {
    pub fn new() -> Result<Self, ProviderError> {
        Self::with_base_url(BASE_URL)
    }

    pub fn with_base_url(base_url: impl Into<String>) -> Result<Self, ProviderError> {
        Self::with_base_url_and_timeout(base_url, REQUEST_TIMEOUT)
    }

    fn with_base_url_and_timeout(
        base_url: impl Into<String>,
        timeout: Duration,
    ) -> Result<Self, ProviderError> {
        let client = http::client_builder().timeout(timeout).build()?;
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
        let Some(body) = self.fetch_raw(query, cancel).await? else {
            return Ok(None);
        };
        Self::parse_response(&body, query)
    }

    pub async fn search_candidates(
        &self,
        query: &TrackQuery,
        cancel: &CancellationToken,
    ) -> Result<Vec<CandidateSummary>, ProviderError> {
        let Some(body) = self.fetch_raw(query, cancel).await? else {
            return Ok(Vec::new());
        };
        let Some((answer, meta)) = Self::parse_response_with_meta(&body, query)? else {
            return Ok(Vec::new());
        };

        Ok(vec![CandidateSummary {
            provider: Provider::LyricsPlus,
            source_track_id: answer.track.source_track_id.clone(),
            title: meta.title,
            artist: meta.artist,
            album: meta.album,
            duration_ms: meta.duration_ms,
            tier: answer.tier,
            confidence: answer.confidence,
            local_path: None,
            browse_hint: None,
            query_duration_ms: query.duration_ms,
            query_user_offset_ms: query.user_offset_ms,
            hydrated: Some(answer),
        }])
    }

    async fn fetch_raw(
        &self,
        query: &TrackQuery,
        cancel: &CancellationToken,
    ) -> Result<Option<String>, ProviderError> {
        if cancel.is_cancelled() {
            return Err(ProviderError::Cancelled);
        }
        if !query.is_usable() || query.canonical.norm_artist.is_empty() {
            return Ok(None);
        }

        let send_fut = self
            .client
            .get(format!("{}/v2/lyrics/get", self.base_url))
            .query(&get_params(query))
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
                Ok(Some(body))
            }
            StatusCode::TOO_MANY_REQUESTS => Err(ProviderError::RateLimited {
                retry_after_ms: http::retry_after_ms(&response),
            }),
            status if status.is_client_error() => Err(ProviderError::Invalid(format!(
                "LyricsPlus returned HTTP {status}"
            ))),
            status => Err(ProviderError::Unavailable(format!(
                "LyricsPlus returned HTTP {status}"
            ))),
        }
    }

    fn parse_response(
        body: &str,
        query: &TrackQuery,
    ) -> Result<Option<ProviderAnswer>, ProviderError> {
        Ok(Self::parse_response_with_meta(body, query)?.map(|(answer, _)| answer))
    }

    fn parse_response_with_meta(
        body: &str,
        query: &TrackQuery,
    ) -> Result<Option<(ProviderAnswer, SummaryMeta)>, ProviderError> {
        let body = body.trim();
        if body.is_empty() {
            return Ok(None);
        }

        if body.starts_with('<') {
            let Some(answer) = ttml_answer(body, 1.0, query)? else {
                return Ok(None);
            };
            return Ok(Some((answer, fallback_summary_meta(query))));
        }

        let response: LyricsPlusResponse = serde_json::from_str(body).map_err(invalid_json)?;
        let confidence = response_confidence(&response, query);
        let summary_meta = summary_meta_from_response(&response, query);

        if let Some(ttml) = response
            .ttml
            .as_deref()
            .filter(|ttml| !ttml.trim().is_empty())
        {
            if !score::is_confident(confidence) {
                warn!(
                    score = confidence,
                    "LyricsPlus TTML response did not match the track"
                );
                return Ok(None);
            }
            let Some(answer) = ttml_answer(ttml, confidence, query)? else {
                return Ok(None);
            };
            return Ok(Some((answer, summary_meta)));
        }

        if response.lyrics.is_empty() {
            return Ok(None);
        }
        if !score::is_confident(confidence) {
            warn!(
                score = confidence,
                "LyricsPlus response did not match the track"
            );
            return Ok(None);
        }

        let parsed = response_to_parsed(&response)?;
        if parsed.lines.is_empty() {
            return Ok(None);
        }

        let track = normalize_track_checked(
            parsed,
            Provider::LyricsPlus.as_str(),
            None,
            confidence,
            query.duration_ms,
            query.user_offset_ms,
        )
        .map_err(|error| ProviderError::Invalid(error.to_string()))?;

        if track.lines.is_empty() {
            return Ok(None);
        }

        Ok(Some((
            ProviderAnswer::new(Provider::LyricsPlus, track),
            summary_meta,
        )))
    }
}

fn ttml_answer(
    ttml: &str,
    confidence: f32,
    query: &TrackQuery,
) -> Result<Option<ProviderAnswer>, ProviderError> {
    let mut track = parse_ttml(ttml, Provider::LyricsPlus.as_str())?;
    if track.lines.is_empty() {
        return Ok(None);
    }
    track.source_track_id = None;
    track.match_confidence = confidence;
    track.user_offset_ms = query.user_offset_ms;

    Ok(Some(ProviderAnswer::new(Provider::LyricsPlus, track)))
}

struct SummaryMeta {
    title: String,
    artist: Option<String>,
    album: Option<String>,
    duration_ms: Option<u64>,
}

fn fallback_summary_meta(query: &TrackQuery) -> SummaryMeta {
    SummaryMeta {
        title: query.title.clone(),
        artist: (!query.artist.is_empty()).then(|| query.artist.clone()),
        album: query.album.clone(),
        duration_ms: query.duration_ms,
    }
}

fn summary_meta_from_response(response: &LyricsPlusResponse, query: &TrackQuery) -> SummaryMeta {
    let selected = response.processing_time.selected_song_metadata.as_ref();

    let title = response
        .metadata
        .title
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            selected
                .and_then(|m| m.title.as_deref())
                .filter(|s| !s.trim().is_empty())
        })
        .map(str::to_string)
        .unwrap_or_else(|| query.title.clone());

    let artist = response
        .metadata
        .artist
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            selected
                .and_then(|m| m.artist.as_deref())
                .filter(|s| !s.trim().is_empty())
        })
        .map(str::to_string)
        .or_else(|| (!query.artist.is_empty()).then(|| query.artist.clone()));

    let album = response
        .metadata
        .album
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string)
        .or_else(|| query.album.clone());

    let duration_ms = selected
        .and_then(|m| m.duration)
        .filter(|d| d.is_finite() && *d > 0.0)
        .map(|d| (d * 1_000.0).round() as u64)
        .or(query.duration_ms);

    SummaryMeta {
        title,
        artist,
        album,
        duration_ms,
    }
}

fn response_to_parsed(response: &LyricsPlusResponse) -> Result<ParsedTrack, ProviderError> {
    let primary_singer = primary_singer(response);
    let mut track = ParsedTrack {
        metadata: ParsedMetadata {
            title: response
                .metadata
                .title
                .as_deref()
                .and_then(non_empty)
                .map(str::to_string),
            artist: response
                .metadata
                .artist
                .as_deref()
                .and_then(non_empty)
                .map(str::to_string),
            album: response
                .metadata
                .album
                .as_deref()
                .and_then(non_empty)
                .map(str::to_string),
            language: response
                .metadata
                .language
                .as_deref()
                .and_then(non_empty)
                .map(str::to_string),
        },
        ..ParsedTrack::default()
    };

    for line in &response.lyrics {
        let Some(time) = line.time else {
            continue;
        };
        let start_ms = millis(time, "line time")?;
        let end_ms = optional_end_ms(line.duration, start_ms, "line duration")?;
        let (text, words) = build_line_content(line, start_ms)?;
        if text.trim().is_empty() && words.is_empty() {
            continue;
        }

        let singer = line.element.singer.as_deref().and_then(non_empty);
        let role = if singer
            .zip(primary_singer.as_deref())
            .is_some_and(|(singer, primary)| singer != primary)
        {
            LineRole::DuetSecondary
        } else {
            LineRole::Primary
        };

        track.lines.push(ParsedLine {
            start_ms,
            end_ms,
            role,
            text,
            words,
            annotations: line_annotations(line),
            explicit_last_word_end_ms: None,
        });
    }

    track.lines.sort_by_key(|line| line.start_ms);
    Ok(track)
}

fn primary_singer(response: &LyricsPlusResponse) -> Option<String> {
    let mut singers = Vec::new();
    for line in &response.lyrics {
        let Some(singer) = line.element.singer.as_deref().and_then(non_empty) else {
            continue;
        };
        if !singers.iter().any(|known| known == singer) {
            singers.push(singer.to_string());
        }
    }

    match singers.as_slice() {
        [] => None,
        [only] => Some(only.clone()),
        _ if singers.iter().any(|singer| singer == "v1") => Some("v1".to_string()),
        _ => Some(singers[0].clone()),
    }
}

fn build_line_content(
    line: &LyricsPlusLine,
    line_start_ms: u64,
) -> Result<(String, Vec<ParsedWord>), ProviderError> {
    let mut pieces = Vec::with_capacity(line.syllabus.len());
    for word in &line.syllabus {
        let Some(text) = word.text.as_deref() else {
            continue;
        };
        if text.is_empty() {
            continue;
        }
        let start_ms = match word.time {
            Some(time) => millis(time, "word time")?,
            None => line_start_ms,
        };
        let end_ms = optional_end_ms(word.duration, start_ms, "word duration")?;
        pieces.push(Piece {
            text,
            start_ms,
            end_ms,
        });
    }

    if pieces.is_empty() {
        return Ok((
            line.text.as_deref().unwrap_or_default().trim().to_string(),
            Vec::new(),
        ));
    }

    if let Some(text) = line.text.as_deref().filter(|text| !text.trim().is_empty()) {
        if let Some(words) = map_pieces(text, &pieces) {
            return Ok(trim_line_with_words(text.to_string(), words));
        }
        warn!("LyricsPlus syllable text did not match line text; using joined words");
    }

    let joined = pieces.iter().map(|piece| piece.text).collect::<String>();
    Ok(trim_line_with_words(
        joined.clone(),
        sequential_pieces(&joined, &pieces),
    ))
}

fn map_pieces(text: &str, pieces: &[Piece<'_>]) -> Option<Vec<ParsedWord>> {
    let mut cursor = 0usize;
    let mut mapped = Vec::with_capacity(pieces.len());

    for piece in pieces {
        let needle = piece.text.trim();
        if needle.is_empty() {
            continue;
        }
        let relative = text.get(cursor..)?.find(needle)?;
        let start = cursor + relative;
        let end = start + needle.len();
        mapped.push(ParsedWord {
            byte_range: (start, end),
            start_ms: piece.start_ms,
            end_ms: piece.end_ms,
        });
        cursor = end;
    }

    (!mapped.is_empty()).then_some(mapped)
}

fn sequential_pieces(text: &str, pieces: &[Piece<'_>]) -> Vec<ParsedWord> {
    let mut cursor = 0usize;
    let mut mapped = Vec::with_capacity(pieces.len());

    for piece in pieces {
        let start = cursor;
        let end = start + piece.text.len();
        if end > text.len() || !text.is_char_boundary(start) || !text.is_char_boundary(end) {
            break;
        }
        mapped.push(ParsedWord {
            byte_range: (start, end),
            start_ms: piece.start_ms,
            end_ms: piece.end_ms,
        });
        cursor = end;
    }

    mapped
}

fn line_annotations(line: &LyricsPlusLine) -> Vec<Annotation> {
    let mut annotations = Vec::new();

    if let Some(text) = line.transliteration.as_ref().and_then(annotation_text) {
        push_annotation(&mut annotations, AnnotationKind::Romanization, text);
    }
    if let Some(text) = line.translation.as_ref().and_then(annotation_text) {
        push_annotation(&mut annotations, AnnotationKind::Translation, text);
    }
    for translation in &line.translations {
        if let Some(text) = annotation_text(translation) {
            push_annotation(&mut annotations, AnnotationKind::Translation, text);
        }
    }

    annotations
}

fn annotation_text(annotation: &LyricsPlusAnnotation) -> Option<&str> {
    annotation.text.as_deref().and_then(non_empty)
}

fn push_annotation(annotations: &mut Vec<Annotation>, kind: AnnotationKind, text: &str) {
    if annotations
        .iter()
        .any(|annotation| annotation.kind == kind && annotation.text == text)
    {
        return;
    }
    annotations.push(Annotation {
        kind,
        text: text.to_string(),
    });
}

fn response_confidence(response: &LyricsPlusResponse, query: &TrackQuery) -> f32 {
    let selected = response.processing_time.selected_song_metadata.as_ref();
    let title = response
        .metadata
        .title
        .as_deref()
        .and_then(non_empty)
        .or_else(|| {
            selected
                .and_then(|metadata| metadata.title.as_deref())
                .and_then(non_empty)
        })
        .unwrap_or(&query.title);
    let artist = response
        .metadata
        .artist
        .as_deref()
        .and_then(non_empty)
        .or_else(|| {
            selected
                .and_then(|metadata| metadata.artist.as_deref())
                .and_then(non_empty)
        })
        .unwrap_or(&query.artist);
    let duration = selected
        .and_then(|metadata| metadata.duration)
        .filter(|duration| duration.is_finite() && *duration > 0.0)
        .map(|duration| {
            if duration >= 10_000.0 {
                duration / 1_000.0
            } else {
                duration
            }
        });

    score::score_metadata(query, title, artist, duration)
}

fn millis(value: f64, field: &str) -> Result<u64, ProviderError> {
    if !value.is_finite() || value < 0.0 {
        return Err(ProviderError::Invalid(format!(
            "invalid LyricsPlus {field}: {value}"
        )));
    }
    Ok(value.round() as u64)
}

fn optional_end_ms(
    duration: Option<f64>,
    start_ms: u64,
    field: &str,
) -> Result<Option<u64>, ProviderError> {
    let Some(duration) = duration else {
        return Ok(None);
    };
    if !duration.is_finite() || duration < 0.0 {
        return Err(ProviderError::Invalid(format!(
            "invalid LyricsPlus {field}: {duration}"
        )));
    }
    if duration == 0.0 {
        return Ok(None);
    }

    Ok(Some(start_ms.saturating_add(duration.round() as u64)))
}

fn get_params(query: &TrackQuery) -> Vec<(&'static str, String)> {
    let mut params = vec![
        ("title", query.title.clone()),
        ("artist", query.artist.clone()),
    ];

    if let Some(album) = &query.album {
        params.push(("album", album.clone()));
    }
    if let Some(duration) = query.duration_ms.and_then(format_duration_seconds) {
        params.push(("duration", duration));
    }

    params
}

fn format_duration_seconds(duration_ms: u64) -> Option<String> {
    if duration_ms == 0 {
        return None;
    }

    let mut value = format!("{:.3}", duration_ms as f64 / 1_000.0);
    while value.ends_with('0') {
        value.pop();
    }
    if value.ends_with('.') {
        value.pop();
    }

    Some(value)
}

fn invalid_json(error: serde_json::Error) -> ProviderError {
    ProviderError::Invalid(format!("invalid LyricsPlus response: {error}"))
}

fn non_empty(value: &str) -> Option<&str> {
    let value = value.trim();
    (!value.is_empty()).then_some(value)
}

impl LyricProvider for LyricsPlusProvider {
    fn id(&self) -> Provider {
        Provider::LyricsPlus
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
        query: &'a TrackQuery,
        cancel: &'a CancellationToken,
    ) -> crate::media::BoxFuture<'a, Result<Vec<CandidateSummary>, ProviderError>> {
        Box::pin(self.search_candidates(query, cancel))
    }

    fn hydrate_candidate<'a>(
        &'a self,
        _summary: &'a CandidateSummary,
        _limiter: Option<&'a crate::providers::engine::TokenBucketRateLimiter>,
        _cancel: &'a CancellationToken,
    ) -> crate::media::BoxFuture<'a, Result<Option<ProviderAnswer>, ProviderError>> {
        Box::pin(async { Ok(None) })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::*;
    use crate::core::model::{AnnotationKind, LineRole, SyncGranularity};
    use crate::media::TrackMeta;
    use crate::providers::Tier;

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

    fn fixture_body() -> &'static str {
        include_str!("../../tests/fixtures/lyricsplus_response.json")
    }

    fn fixture_answer() -> ProviderAnswer {
        LyricsPlusProvider::parse_response(fixture_body(), &query())
            .unwrap()
            .unwrap()
    }

    struct MockResponse {
        status: &'static str,
        headers: Vec<(&'static str, &'static str)>,
        body: String,
    }

    impl MockResponse {
        fn new(status: &'static str, body: impl Into<String>) -> Self {
            Self {
                status,
                headers: Vec::new(),
                body: body.into(),
            }
        }

        fn header(mut self, name: &'static str, value: &'static str) -> Self {
            self.headers.push((name, value));
            self
        }
    }

    async fn spawn_mock_server(
        responses: Vec<MockResponse>,
    ) -> (String, Arc<Mutex<Vec<String>>>, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&requests);
        let mut queue = VecDeque::from(responses);

        let handle = tokio::spawn(async move {
            while let Some(response) = queue.pop_front() {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut buffer = [0_u8; 4096];
                let count = stream.read(&mut buffer).await.unwrap();
                let request = String::from_utf8_lossy(&buffer[..count]);
                let target = request
                    .lines()
                    .next()
                    .and_then(|line| line.split_whitespace().nth(1))
                    .unwrap_or_default()
                    .to_string();
                recorded.lock().unwrap().push(target);

                let headers = response
                    .headers
                    .iter()
                    .map(|(name, value)| format!("{name}: {value}\r\n"))
                    .collect::<String>();
                let raw = format!(
                    "HTTP/1.1 {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n{}",
                    response.status,
                    response.body.len(),
                    response.body
                );
                let _ = stream.write_all(raw.as_bytes()).await;
            }
        });

        (format!("http://{address}"), requests, handle)
    }

    #[test]
    fn fixture_parses_as_syllable_sync() {
        let answer = fixture_answer();

        assert_eq!(answer.provider, Provider::LyricsPlus);
        assert_eq!(answer.tier, Tier::Syllable);
        assert!(answer.confidence >= 0.65);
        assert_eq!(answer.track.source_provider, "lyricsplus");
        assert_eq!(answer.track.granularity, SyncGranularity::Syllable);
        assert_eq!(answer.track.lines.len(), 2);

        let first = &answer.track.lines[0];
        assert_eq!(first.text, "Look at the stars");
        assert_eq!(first.words.len(), 4);
        assert_eq!(first.role, LineRole::Primary);
        assert!(first.annotations.iter().any(|annotation| {
            annotation.kind == AnnotationKind::Romanization && annotation.text == "Luk at de stars"
        }));
        assert!(first.annotations.iter().any(|annotation| {
            annotation.kind == AnnotationKind::Translation
                && annotation.text == "Lihat bintang-bintang"
        }));

        let second = &answer.track.lines[1];
        assert_eq!(second.text, "Yellow");
        assert_eq!(second.words.len(), 2);
        assert_eq!(second.role, LineRole::DuetSecondary);
        assert!(
            second
                .annotations
                .iter()
                .any(|annotation| annotation.kind == AnnotationKind::Translation)
        );
    }

    #[test]
    fn single_used_singer_maps_every_line_to_primary() {
        let body = serde_json::json!({
            "metadata": {
                "title": "Yellow",
                "artist": "Coldplay",
                "agents": {
                    "v1": { "type": "person", "name": "Chris Martin" },
                    "v2": { "type": "person", "name": "Jonny Buckland" }
                }
            },
            "lyrics": [
                { "time": 1000, "duration": 2000, "text": "One", "element": { "singer": "v2" } },
                { "time": 4000, "duration": 2000, "text": "Two", "element": { "singer": "v2" } }
            ]
        })
        .to_string();

        let answer = LyricsPlusProvider::parse_response(&body, &query())
            .unwrap()
            .unwrap();

        assert!(
            answer
                .track
                .lines
                .iter()
                .all(|line| line.role == LineRole::Primary)
        );
    }

    #[test]
    fn empty_payload_returns_none() {
        assert!(
            LyricsPlusProvider::parse_response("", &query())
                .unwrap()
                .is_none()
        );
        assert!(
            LyricsPlusProvider::parse_response("{}", &query())
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn ttml_wrapper_and_raw_ttml_are_supported() {
        let ttml = include_str!("../../tests/fixtures/sample_syllable.ttml");
        let wrapped = serde_json::json!({ "ttml": ttml }).to_string();

        for body in [wrapped.as_str(), ttml] {
            let answer = LyricsPlusProvider::parse_response(body, &query())
                .unwrap()
                .unwrap();
            assert_eq!(answer.tier, Tier::Syllable);
            assert_eq!(answer.track.source_provider, "lyricsplus");
            assert_eq!(answer.track.granularity, SyncGranularity::Syllable);
        }
    }

    #[test]
    fn line_and_word_payloads_map_to_expected_tiers() {
        let line = serde_json::json!({
            "metadata": { "title": "Yellow", "artist": "Coldplay" },
            "lyrics": [{ "time": 1000, "duration": 2000, "text": "Hello world" }]
        })
        .to_string();
        let word = serde_json::json!({
            "metadata": { "title": "Yellow", "artist": "Coldplay" },
            "lyrics": [{
                "time": 1000,
                "duration": 2000,
                "text": "Hello world",
                "syllabus": [
                    { "time": 1000, "duration": 500, "text": "Hello " },
                    { "time": 1500, "duration": 500, "text": "world" }
                ]
            }]
        })
        .to_string();

        let line = LyricsPlusProvider::parse_response(&line, &query())
            .unwrap()
            .unwrap();
        let word = LyricsPlusProvider::parse_response(&word, &query())
            .unwrap()
            .unwrap();

        assert_eq!(line.tier, Tier::Line);
        assert_eq!(line.track.granularity, SyncGranularity::Line);
        assert_eq!(word.tier, Tier::Word);
        assert_eq!(word.track.granularity, SyncGranularity::Word);
    }

    #[test]
    fn mismatched_metadata_is_rejected() {
        let mut body: serde_json::Value = serde_json::from_str(fixture_body()).unwrap();
        body["metadata"]["title"] = serde_json::Value::String("Something Else".to_string());
        body["metadata"]["artist"] = serde_json::Value::String("Another Artist".to_string());
        let body = body.to_string();

        assert!(
            LyricsPlusProvider::parse_response(&body, &query())
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn resolves_fixture_over_http() {
        let (base_url, requests, server) =
            spawn_mock_server(vec![MockResponse::new("200 OK", fixture_body())]).await;
        let client = LyricsPlusProvider::with_base_url(base_url).unwrap();

        let answer = client.resolve(&query()).await.unwrap().unwrap();

        assert_eq!(answer.tier, Tier::Syllable);
        let target = requests.lock().unwrap()[0].clone();
        assert!(target.starts_with("/v2/lyrics/get?"));
        assert!(target.contains("title=Yellow"));
        assert!(target.contains("artist=Coldplay"));
        assert!(target.contains("album=Parachutes"));
        assert!(target.contains("duration=267"));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn empty_http_body_returns_none() {
        let (base_url, _, server) = spawn_mock_server(vec![MockResponse::new("200 OK", "")]).await;
        let client = LyricsPlusProvider::with_base_url(base_url).unwrap();

        assert!(client.resolve(&query()).await.unwrap().is_none());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn not_found_returns_none() {
        let (base_url, _, server) = spawn_mock_server(vec![MockResponse::new(
            "404 Not Found",
            r#"{"error":"Lyrics not found"}"#,
        )])
        .await;
        let client = LyricsPlusProvider::with_base_url(base_url).unwrap();

        assert!(client.resolve(&query()).await.unwrap().is_none());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn rate_limit_preserves_retry_after() {
        let (base_url, _, server) = spawn_mock_server(vec![
            MockResponse::new("429 Too Many Requests", r#"{"error":"rate limited"}"#)
                .header("Retry-After", "2"),
        ])
        .await;
        let client = LyricsPlusProvider::with_base_url(base_url).unwrap();

        let error = client.resolve(&query()).await.unwrap_err();

        assert!(matches!(
            error,
            ProviderError::RateLimited {
                retry_after_ms: Some(2_000)
            }
        ));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn invalid_json_is_invalid_payload() {
        let (base_url, _, server) =
            spawn_mock_server(vec![MockResponse::new("200 OK", r#"{"lyrics": [}"#)]).await;
        let client = LyricsPlusProvider::with_base_url(base_url).unwrap();

        let error = client.resolve(&query()).await.unwrap_err();

        assert!(matches!(error, ProviderError::Invalid(_)));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn skips_requests_without_an_artist() {
        let client = LyricsPlusProvider::with_base_url("http://127.0.0.1:1").unwrap();
        let query = TrackQuery::from_meta(
            &TrackMeta {
                title: "Yellow".to_string(),
                artist: String::new(),
                album: None,
                duration_ms: Some(267_000),
                track_id: None,
                url: None,
            },
            0,
        );

        assert!(client.resolve(&query).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn cancelled_request_returns_cancelled() {
        let client = LyricsPlusProvider::with_base_url("http://127.0.0.1:1").unwrap();
        let cancel = CancellationToken::new();
        cancel.cancel();

        let error = client
            .resolve_with_cancel(&query(), &cancel)
            .await
            .unwrap_err();

        assert!(matches!(error, ProviderError::Cancelled));
    }

    #[tokio::test]
    async fn search_candidates_returns_at_most_one_prehydrated_summary() {
        let (base_url, requests, server) =
            spawn_mock_server(vec![MockResponse::new("200 OK", fixture_body())]).await;
        let client = LyricsPlusProvider::with_base_url(base_url).unwrap();
        let cancel = CancellationToken::new();

        let candidates = client.search_candidates(&query(), &cancel).await.unwrap();
        server.await.unwrap();

        assert_eq!(requests.lock().unwrap().len(), 1);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].provider, Provider::LyricsPlus);
        assert_eq!(candidates[0].tier, Tier::Syllable);
        assert!(candidates[0].is_hydrated());
        assert_eq!(candidates[0].title, "Yellow");
        assert_eq!(candidates[0].artist.as_deref(), Some("Coldplay"));
    }

    #[tokio::test]
    #[ignore = "requires live network access to LyricsPlus"]
    async fn live_lyricsplus_returns_yellow() {
        let client = LyricsPlusProvider::new().unwrap();
        let answer = client.resolve(&query()).await.unwrap();

        assert!(answer.is_some());
    }
}

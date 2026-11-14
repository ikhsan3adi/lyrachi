use std::time::Duration;

use reqwest::StatusCode;
use serde::{Deserialize, Deserializer};
use tokio_util::sync::CancellationToken;
use tracing::warn;

use super::score;
use super::{Provider, ProviderError, TrackQuery};
use crate::core::model::{SyncGranularity, SyncedTrack};
use crate::core::normalize::{normalize_instrumental, normalize_plain, normalize_track};
use crate::core::parse::lrc::parse_lrc;
use crate::core::parse::lyricsfile::parse_lyricsfile;

const BASE_URL: &str = "https://lrclib.net";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(4);
const USER_AGENT: &str = concat!(
    "lyrachi/",
    env!("CARGO_PKG_VERSION"),
    " (https://github.com/ikhsan3adi/lyrachi)"
);
const MAX_CANDIDATES: usize = 20;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct LrclibRecord {
    #[serde(deserialize_with = "null_as_default")]
    pub id: u64,
    pub name: Option<String>,
    pub track_name: Option<String>,
    pub artist_name: Option<String>,
    pub album_name: Option<String>,
    pub duration: Option<f64>,
    #[serde(deserialize_with = "null_as_default")]
    pub instrumental: bool,
    #[serde(deserialize_with = "null_as_default")]
    pub has_word_sync: bool,
    pub plain_lyrics: Option<String>,
    pub synced_lyrics: Option<String>,
    pub lyricsfile: Option<String>,
}

impl LrclibRecord {
    pub fn title(&self) -> &str {
        self.track_name
            .as_deref()
            .filter(|title| !title.trim().is_empty())
            .or_else(|| self.name.as_deref().filter(|name| !name.trim().is_empty()))
            .unwrap_or_default()
    }
}

fn null_as_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    T: Default + Deserialize<'de>,
    D: Deserializer<'de>,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

pub struct LrclibClient {
    client: reqwest::Client,
    base_url: String,
}

impl LrclibClient {
    pub fn new() -> Result<Self, ProviderError> {
        Self::with_base_url(BASE_URL)
    }

    pub fn with_base_url(base_url: impl Into<String>) -> Result<Self, ProviderError> {
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .user_agent(USER_AGENT)
            .build()?;
        let base_url = base_url.into().trim_end_matches('/').to_string();

        Ok(Self { client, base_url })
    }

    pub async fn resolve(&self, query: &TrackQuery) -> Result<Option<SyncedTrack>, ProviderError> {
        self.resolve_with_cancel(query, &CancellationToken::new())
            .await
    }

    pub async fn resolve_with_cancel(
        &self,
        query: &TrackQuery,
        cancel: &CancellationToken,
    ) -> Result<Option<SyncedTrack>, ProviderError> {
        if cancel.is_cancelled() {
            return Err(ProviderError::Cancelled);
        }
        if let Some(record) = self.get(query, cancel).await?
            && let Some(track) = Self::record_to_track(&record, 1.0, query)?
        {
            return Ok(Some(track));
        }

        let candidates = self.search(query, cancel).await?;
        let ranked =
            score::rank_candidates(query, &candidates[..candidates.len().min(MAX_CANDIDATES)]);

        for (candidate, confidence) in ranked {
            if let Some(track) = Self::record_to_track(candidate, confidence, query)? {
                return Ok(Some(track));
            }
        }

        Ok(None)
    }

    async fn get(
        &self,
        query: &TrackQuery,
        cancel: &CancellationToken,
    ) -> Result<Option<LrclibRecord>, ProviderError> {
        if query.canonical.norm_artist.is_empty() || query.canonical.norm_title.is_empty() {
            return Ok(None);
        }

        let params = get_params(query);
        let send_fut = self
            .client
            .get(format!("{}/api/get", self.base_url))
            .query(&params)
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
                    res = response.text() => res?,
                };
                Ok(Some(serde_json::from_str(&body)?))
            }
            status => Err(ProviderError::Status(status)),
        }
    }

    async fn search(
        &self,
        query: &TrackQuery,
        cancel: &CancellationToken,
    ) -> Result<Vec<LrclibRecord>, ProviderError> {
        if query.canonical.norm_title.is_empty() {
            return Ok(Vec::new());
        }

        let mut records = Vec::new();

        if !query.canonical.norm_artist.is_empty() {
            let params = [
                ("track_name", query.title.clone()),
                ("artist_name", query.artist.clone()),
            ];
            records = self.search_request(&params, cancel).await?;
        }

        if records.is_empty() {
            let q = if query.canonical.norm_artist.is_empty() {
                query.title.trim().to_string()
            } else {
                format!("{} {}", query.artist, query.title)
                    .trim()
                    .to_string()
            };
            records = self.search_request(&[("q", q)], cancel).await?;
        }

        Ok(records)
    }

    async fn search_request(
        &self,
        params: &[(&str, String)],
        cancel: &CancellationToken,
    ) -> Result<Vec<LrclibRecord>, ProviderError> {
        let send_fut = self
            .client
            .get(format!("{}/api/search", self.base_url))
            .query(params)
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
                    res = response.text() => res?,
                };
                Ok(serde_json::from_str(&body)?)
            }
            status => Err(ProviderError::Status(status)),
        }
    }

    fn record_to_track(
        record: &LrclibRecord,
        confidence: f32,
        query: &TrackQuery,
    ) -> Result<Option<SyncedTrack>, ProviderError> {
        let duration_ms = record
            .duration
            .filter(|duration| duration.is_finite() && *duration > 0.0)
            .map(|duration| (duration * 1_000.0).round() as u64)
            .or(query.duration_ms);
        let source_track_id = (record.id != 0).then(|| record.id.to_string());

        if record.instrumental {
            return Ok(Some(normalize_instrumental(
                Provider::Lrclib.as_str(),
                source_track_id,
                confidence,
                duration_ms,
                query.user_offset_ms,
            )));
        }

        if record.has_word_sync
            && let Some(lyricsfile) = record
                .lyricsfile
                .as_deref()
                .filter(|lyricsfile| !lyricsfile.trim().is_empty())
        {
            match parse_lyricsfile(lyricsfile) {
                Ok(parsed) => {
                    let track = normalize_track(
                        parsed,
                        Provider::Lrclib.as_str(),
                        source_track_id.clone(),
                        confidence,
                        duration_ms,
                        query.user_offset_ms,
                    );
                    if (track.is_instrumental || track.granularity == SyncGranularity::Word)
                        && !track.lines.is_empty()
                    {
                        return Ok(Some(track));
                    }
                    warn!("LRCLIB lyricsfile did not contain word sync; falling back");
                }
                Err(error) => {
                    warn!(%error, "failed to parse LRCLIB lyricsfile; falling back");
                }
            }
        }

        if let Some(lyrics) = record
            .synced_lyrics
            .as_deref()
            .filter(|lyrics| !lyrics.trim().is_empty())
        {
            match parse_lrc(lyrics, duration_ms) {
                Ok(parsed) => {
                    let track = normalize_track(
                        parsed,
                        Provider::Lrclib.as_str(),
                        source_track_id.clone(),
                        confidence,
                        duration_ms,
                        query.user_offset_ms,
                    );
                    if track.is_instrumental || !track.lines.is_empty() {
                        return Ok(Some(track));
                    }
                }
                Err(error) => {
                    warn!(%error, "failed to parse LRCLIB synced lyrics; falling back");
                }
            }
        }

        if let Some(lyrics) = record
            .plain_lyrics
            .as_deref()
            .filter(|lyrics| !lyrics.trim().is_empty())
        {
            let track = normalize_plain(
                lyrics,
                Provider::Lrclib.as_str(),
                source_track_id,
                confidence,
                query.user_offset_ms,
            );
            if !track.lines.is_empty() {
                return Ok(Some(track));
            }
        }

        Ok(None)
    }
}

fn get_params(query: &TrackQuery) -> Vec<(&'static str, String)> {
    let mut params = vec![
        ("track_name", query.title.clone()),
        ("artist_name", query.artist.clone()),
    ];

    if let Some(album) = &query.album {
        params.push(("album_name", album.clone()));
    }
    if let Some(duration) = query
        .duration_sec()
        .filter(|duration| duration.is_finite() && duration.round() >= 1.0)
    {
        params.push(("duration", format!("{:.0}", duration.round())));
    }

    params
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::io::ErrorKind;
    use std::sync::{Arc, Mutex};

    use tokio::net::{TcpListener, TcpStream};

    use super::*;
    use crate::media::TrackMeta;

    fn fixture() -> Vec<LrclibRecord> {
        serde_json::from_str(include_str!("../../tests/fixtures/lrclib_search.json")).unwrap()
    }

    fn query() -> TrackQuery {
        TrackQuery::from_meta(
            &TrackMeta {
                title: "Yellow".to_string(),
                artist: "Coldplay".to_string(),
                album: None,
                duration_ms: Some(267_000),
                track_id: None,
            },
            0,
        )
    }

    async fn spawn_mock_server(
        responses: Vec<(&'static str, String)>,
    ) -> (String, Arc<Mutex<Vec<String>>>, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&requests);
        let mut queue = VecDeque::from(responses);

        let handle = tokio::spawn(async move {
            while let Some((status, body)) = queue.pop_front() {
                let (stream, _) = listener.accept().await.unwrap();
                let target = read_request_target(&stream).await;
                recorded.lock().unwrap().push(target);
                write_http_response(&stream, status, &body).await;
            }
        });

        (format!("http://{addr}"), requests, handle)
    }

    async fn read_request_target(stream: &TcpStream) -> String {
        let mut raw = Vec::new();
        let mut buf = [0_u8; 2048];
        loop {
            stream.readable().await.unwrap();
            match stream.try_read(&mut buf) {
                Ok(0) => break,
                Ok(count) => {
                    raw.extend_from_slice(&buf[..count]);
                    if raw.windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                }
                Err(error) if error.kind() == ErrorKind::WouldBlock => continue,
                Err(error) => panic!("failed to read mock request: {error}"),
            }
        }

        let header_text = String::from_utf8_lossy(&raw);
        let first_line = header_text.lines().next().unwrap_or_default();
        first_line
            .split_whitespace()
            .nth(1)
            .unwrap_or_default()
            .to_string()
    }

    async fn write_http_response(stream: &TcpStream, status: &str, body: &str) {
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let bytes = response.as_bytes();
        let mut written = 0usize;
        while written < bytes.len() {
            stream.writable().await.unwrap();
            match stream.try_write(&bytes[written..]) {
                Ok(count) => written += count,
                Err(error) if error.kind() == ErrorKind::WouldBlock => continue,
                Err(error) => panic!("failed to write mock response: {error}"),
            }
        }
    }

    #[test]
    fn parses_search_fixture() {
        let records = fixture();

        assert_eq!(records.len(), 4);
        assert_eq!(records[0].track_name.as_deref(), Some("Yellow"));
        assert_eq!(records[0].name.as_deref(), Some("Yellow"));
    }

    #[test]
    fn deserializes_null_scalar_fields_to_default() {
        let json = r#"{
            "id": null,
            "instrumental": null,
            "hasWordSync": null,
            "trackName": "Test"
        }"#;
        let record: LrclibRecord = serde_json::from_str(json).unwrap();

        assert_eq!(record.id, 0);
        assert!(!record.instrumental);
        assert!(!record.has_word_sync);
        assert_eq!(record.title(), "Test");
    }

    #[test]
    fn omits_non_finite_or_subsecond_duration_from_get_params() {
        let subsecond = TrackQuery::from_meta(
            &TrackMeta {
                title: "Song".to_string(),
                artist: "Artist".to_string(),
                album: Some("Album".to_string()),
                duration_ms: Some(400),
                track_id: None,
            },
            0,
        );
        let params = get_params(&subsecond);

        assert!(!params.iter().any(|(key, _)| *key == "duration"));
        assert!(params.iter().any(|(key, _)| *key == "album_name"));
    }

    #[test]
    fn selects_best_candidate_from_fixture() {
        let records = fixture();
        let (candidate, confidence) = score::select_best(&query(), &records).unwrap();

        assert_eq!(candidate.id, 1);
        assert!(candidate.has_word_sync);
        assert!(confidence >= 0.65);
    }

    #[test]
    fn builds_word_synced_track_from_lyricsfile() {
        let records = fixture();
        let track = LrclibClient::record_to_track(&records[0], 1.0, &query())
            .unwrap()
            .unwrap();

        assert_eq!(track.source_provider, "lrclib");
        assert_eq!(track.granularity, SyncGranularity::Word);
        assert_eq!(track.lines[0].text, "Look at the stars");
    }

    #[test]
    fn builds_instrumental_track() {
        let records = fixture();
        let record = records.iter().find(|record| record.instrumental).unwrap();
        let track = LrclibClient::record_to_track(record, 0.9, &query())
            .unwrap()
            .unwrap();

        assert!(track.is_instrumental);
        assert_eq!(track.lines[0].text, "[Instrumental]");
    }

    #[test]
    fn falls_back_to_synced_then_plain_lyrics() {
        let synced = LrclibRecord {
            id: 10,
            synced_lyrics: Some("[00:01.00]First\n[00:03.00]Second\n".to_string()),
            ..LrclibRecord::default()
        };
        let track = LrclibClient::record_to_track(&synced, 0.9, &query())
            .unwrap()
            .unwrap();
        assert_eq!(track.lines.len(), 2);

        let plain = LrclibRecord {
            id: 11,
            plain_lyrics: Some("First\nSecond\n".to_string()),
            ..LrclibRecord::default()
        };
        let track = LrclibClient::record_to_track(&plain, 0.9, &query())
            .unwrap()
            .unwrap();
        assert_eq!(track.granularity, SyncGranularity::Unsynced);
    }

    #[test]
    fn falls_back_when_lyricsfile_is_malformed_or_lacks_word_sync() {
        let malformed = LrclibRecord {
            id: 13,
            has_word_sync: true,
            lyricsfile: Some("not: [valid".to_string()),
            synced_lyrics: Some("[00:01.00]First\n".to_string()),
            ..LrclibRecord::default()
        };
        let track = LrclibClient::record_to_track(&malformed, 0.9, &query())
            .unwrap()
            .unwrap();
        assert_eq!(track.lines.len(), 1);
        assert_eq!(track.lines[0].text, "First");

        let line_only_yaml = LrclibRecord {
            id: 14,
            has_word_sync: true,
            lyricsfile: Some(
                "version: '1.0'\nlines:\n- text: Line only\n  start_ms: 1000\n".to_string(),
            ),
            synced_lyrics: Some("[00:01.00]From ELRC\n".to_string()),
            ..LrclibRecord::default()
        };
        let track = LrclibClient::record_to_track(&line_only_yaml, 0.9, &query())
            .unwrap()
            .unwrap();
        assert_eq!(track.lines[0].text, "From ELRC");
    }

    #[test]
    fn rejects_records_without_lyrics() {
        let record = LrclibRecord {
            id: 12,
            ..LrclibRecord::default()
        };

        assert!(
            LrclibClient::record_to_track(&record, 0.9, &query())
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn mock_get_returns_word_sync_track() {
        let body = include_str!("../../tests/fixtures/lrclib_yellow_wordsync.json").to_string();
        let (base_url, requests, handle) = spawn_mock_server(vec![("200 OK", body)]).await;
        let client = LrclibClient::with_base_url(base_url).unwrap();

        let track = client.resolve(&query()).await.unwrap().unwrap();
        handle.await.unwrap();

        assert_eq!(track.source_provider, "lrclib");
        assert_eq!(track.granularity, SyncGranularity::Word);
        assert!((track.match_confidence - 1.0).abs() < f32::EPSILON);

        let recorded = requests.lock().unwrap();
        assert_eq!(recorded.len(), 1);
        assert!(recorded[0].starts_with("/api/get?"));
        assert!(recorded[0].contains("track_name=Yellow"));
        assert!(recorded[0].contains("artist_name=Coldplay"));
        assert!(recorded[0].contains("duration=267"));
    }

    #[tokio::test]
    async fn mock_fallback_get_404_then_structured_empty_then_q_search() {
        let search_body = include_str!("../../tests/fixtures/lrclib_search.json").to_string();
        let (base_url, requests, handle) = spawn_mock_server(vec![
            ("404 Not Found", "{}".to_string()),
            ("200 OK", "[]".to_string()),
            ("200 OK", search_body),
        ])
        .await;
        let client = LrclibClient::with_base_url(base_url).unwrap();

        let track = client.resolve(&query()).await.unwrap().unwrap();
        handle.await.unwrap();

        assert_eq!(track.source_track_id.as_deref(), Some("1"));
        assert_eq!(track.granularity, SyncGranularity::Word);

        let recorded = requests.lock().unwrap();
        assert_eq!(recorded.len(), 3);
        assert!(recorded[0].starts_with("/api/get?"));
        assert!(recorded[1].starts_with("/api/search?track_name="));
        assert!(recorded[2].starts_with("/api/search?q="));
    }

    #[tokio::test]
    async fn mock_handles_malformed_json_and_http_500() {
        let (bad_json_url, _, bad_handle) =
            spawn_mock_server(vec![("200 OK", "{not valid json".to_string())]).await;
        let bad_client = LrclibClient::with_base_url(bad_json_url).unwrap();
        let err = bad_client.resolve(&query()).await.unwrap_err();
        bad_handle.await.unwrap();
        assert!(matches!(err, ProviderError::Json(_)));

        let (err_500_url, _, err_handle) = spawn_mock_server(vec![
            ("404 Not Found", "{}".to_string()),
            ("500 Internal Server Error", "oops".to_string()),
        ])
        .await;
        let err_client = LrclibClient::with_base_url(err_500_url).unwrap();
        let err = err_client.resolve(&query()).await.unwrap_err();
        err_handle.await.unwrap();
        assert!(matches!(
            err,
            ProviderError::Status(StatusCode::INTERNAL_SERVER_ERROR)
        ));
    }

    #[tokio::test]
    async fn mock_cancels_in_flight_http_request() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (_socket, _) = listener.accept().await.unwrap();
            tokio::time::sleep(Duration::from_secs(2)).await;
        });

        let client = LrclibClient::with_base_url(format!("http://{addr}")).unwrap();
        let cancel = CancellationToken::new();
        let cancel_trigger = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            cancel_trigger.cancel();
        });

        let err = client
            .resolve_with_cancel(&query(), &cancel)
            .await
            .unwrap_err();
        server.abort();
        assert!(matches!(err, ProviderError::Cancelled));
    }

    #[tokio::test]
    #[ignore = "requires network access to lrclib.net"]
    async fn live_get_returns_yellow() {
        let client = LrclibClient::new().unwrap();
        let track = client.resolve(&query()).await.unwrap().unwrap();

        assert_eq!(track.source_provider, "lrclib");
        assert!(!track.lines.is_empty());
    }
}

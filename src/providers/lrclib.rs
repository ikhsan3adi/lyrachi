use std::time::Duration;

use reqwest::StatusCode;
use serde::Deserialize;
use tracing::warn;

use super::score;
use super::{Provider, ProviderError, TrackQuery};
use crate::core::model::SyncedTrack;
use crate::core::normalize::{normalize_instrumental, normalize_plain, normalize_track};
use crate::core::parse::lrc::parse_lrc;
use crate::core::parse::lyricsfile::parse_lyricsfile;

const BASE_URL: &str = "https://lrclib.net";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(4);
const USER_AGENT: &str = concat!(
    "lyrachi/",
    env!("CARGO_PKG_VERSION"),
    " (https://codeberg.org/ikhsan3adi/lyrachi)"
);
const MAX_CANDIDATES: usize = 20;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct LrclibRecord {
    pub id: u64,
    pub track_name: Option<String>,
    pub artist_name: Option<String>,
    pub album_name: Option<String>,
    pub duration: Option<f64>,
    pub instrumental: bool,
    pub has_word_sync: bool,
    pub plain_lyrics: Option<String>,
    pub synced_lyrics: Option<String>,
    pub lyricsfile: Option<String>,
}

pub struct LrclibClient {
    client: reqwest::Client,
}

impl LrclibClient {
    pub fn new() -> Result<Self, ProviderError> {
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .user_agent(USER_AGENT)
            .build()?;

        Ok(Self { client })
    }

    pub async fn resolve(&self, query: &TrackQuery) -> Result<Option<SyncedTrack>, ProviderError> {
        if let Some(record) = self.get(query).await?
            && let Some(track) = Self::record_to_track(&record, 1.0, query)?
        {
            return Ok(Some(track));
        }

        let candidates = self.search(query).await?;
        let ranked =
            score::rank_candidates(query, &candidates[..candidates.len().min(MAX_CANDIDATES)]);

        for (candidate, confidence) in ranked {
            if let Some(track) = Self::record_to_track(candidate, confidence, query)? {
                return Ok(Some(track));
            }
        }

        Ok(None)
    }

    async fn get(&self, query: &TrackQuery) -> Result<Option<LrclibRecord>, ProviderError> {
        if query.artist.is_empty() || query.title.is_empty() {
            return Ok(None);
        }

        let params = get_params(query);
        let response = self
            .client
            .get(format!("{BASE_URL}/api/get"))
            .query(&params)
            .send()
            .await?;

        match response.status() {
            StatusCode::NOT_FOUND => Ok(None),
            status if status.is_success() => {
                let body = response.text().await?;
                Ok(Some(serde_json::from_str(&body)?))
            }
            status => Err(ProviderError::Status(status)),
        }
    }

    async fn search(&self, query: &TrackQuery) -> Result<Vec<LrclibRecord>, ProviderError> {
        if query.title.is_empty() {
            return Ok(Vec::new());
        }

        let mut records = Vec::new();

        if !query.artist.is_empty() {
            let params = [
                ("track_name", query.title.clone()),
                ("artist_name", query.artist.clone()),
            ];
            records = self.search_request(&params).await?;
        }

        if records.is_empty() {
            let q = format!("{} {}", query.artist, query.title)
                .trim()
                .to_string();
            records = self.search_request(&[("q", q)]).await?;
        }

        Ok(records)
    }

    async fn search_request(
        &self,
        params: &[(&str, String)],
    ) -> Result<Vec<LrclibRecord>, ProviderError> {
        let response = self
            .client
            .get(format!("{BASE_URL}/api/search"))
            .query(params)
            .send()
            .await?;

        if response.status() == StatusCode::NOT_FOUND {
            return Ok(Vec::new());
        }

        let response = response.error_for_status()?;
        let body = response.text().await?;

        Ok(serde_json::from_str(&body)?)
    }

    fn record_to_track(
        record: &LrclibRecord,
        confidence: f32,
        query: &TrackQuery,
    ) -> Result<Option<SyncedTrack>, ProviderError> {
        let duration_ms = record
            .duration
            .filter(|duration| *duration > 0.0)
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
                    return Ok(Some(normalize_track(
                        parsed,
                        Provider::Lrclib.as_str(),
                        source_track_id,
                        confidence,
                        duration_ms,
                        query.user_offset_ms,
                    )));
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
                    return Ok(Some(normalize_track(
                        parsed,
                        Provider::Lrclib.as_str(),
                        source_track_id,
                        confidence,
                        duration_ms,
                        query.user_offset_ms,
                    )));
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
            return Ok(Some(normalize_plain(
                lyrics,
                Provider::Lrclib.as_str(),
                source_track_id,
                confidence,
                query.user_offset_ms,
            )));
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
    if let Some(duration) = query.duration_sec() {
        params.push(("duration", format!("{duration:.0}")));
    }

    params
}

#[cfg(test)]
mod tests {
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

    #[test]
    fn parses_search_fixture() {
        let records = fixture();

        assert_eq!(records.len(), 4);
        assert_eq!(records[0].track_name.as_deref(), Some("Yellow"));
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
        assert_eq!(track.granularity, crate::core::model::SyncGranularity::Word);
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
        assert_eq!(
            track.granularity,
            crate::core::model::SyncGranularity::Unsynced
        );
    }

    #[test]
    fn falls_back_when_lyricsfile_is_malformed() {
        let record = LrclibRecord {
            id: 13,
            has_word_sync: true,
            lyricsfile: Some("not: [valid".to_string()),
            synced_lyrics: Some("[00:01.00]First\n".to_string()),
            ..LrclibRecord::default()
        };

        let track = LrclibClient::record_to_track(&record, 0.9, &query())
            .unwrap()
            .unwrap();

        assert_eq!(track.lines.len(), 1);
        assert_eq!(track.lines[0].text, "First");
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
    #[ignore = "requires network access to lrclib.net"]
    async fn live_get_returns_yellow() {
        let client = LrclibClient::new().unwrap();
        let track = client.resolve(&query()).await.unwrap().unwrap();

        assert_eq!(track.source_provider, "lrclib");
        assert!(!track.lines.is_empty());
    }
}

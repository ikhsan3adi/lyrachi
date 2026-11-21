use std::collections::HashSet;
use std::path::PathBuf;

use serde_json::Error as JsonError;
use tokio::sync::{mpsc, oneshot};

use crate::core::model::{CanonicalTrackKey, PARSER_VERSION, SyncedTrack};
use crate::providers::sanitize::blake3_track_key;
use crate::providers::{Provider, ProviderAnswer, ProviderId};

mod schema;
mod worker;

pub const CACHE_CHANNEL_CAPACITY: usize = 64;
pub const CACHE_TOUCH_DEBOUNCE_SECS: i64 = 60 * 60;
pub const CACHE_MAX_ENTRY_AGE_SECS: i64 = 90 * 24 * 60 * 60;
pub const CACHE_NEGATIVE_TTL_SECS: i64 = 14 * 24 * 60 * 60;
pub const CACHE_UPGRADE_INTERVAL_SECS: i64 = 14 * 24 * 60 * 60;
pub const CACHE_PRUNE_INTERVAL_SECS: i64 = 24 * 60 * 60;
pub const CACHE_PRUNE_CHECK_SECS: i64 = 60 * 60;
pub const CACHE_PREFERENCES_MAX_ENTRIES: usize = 5_000;
pub const CACHE_PREFERENCES_MAX_AGE_SECS: i64 = 180 * 24 * 60 * 60;

#[derive(Debug, thiserror::Error)]
pub enum CacheError {
    #[error("cache database error: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("cache file error: {0}")]
    Io(#[from] std::io::Error),
    #[error("cache payload error: {0}")]
    Payload(#[from] JsonError),
    #[error("unsupported cache schema version: {0}")]
    UnsupportedSchemaVersion(String),
    #[error("cache worker stopped")]
    WorkerStopped,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackPreference {
    pub preferred_provider: Option<ProviderId>,
    pub preferred_track_id: Option<String>,
    pub pinned_payload_json: Option<Vec<u8>>,
    pub preferred_style: Option<String>,
    pub updated_at: i64,
}

impl Default for TrackPreference {
    fn default() -> Self {
        Self {
            preferred_provider: None,
            preferred_track_id: None,
            pinned_payload_json: None,
            preferred_style: None,
            updated_at: now_secs(),
        }
    }
}

impl TrackPreference {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_preferred_provider(provider: Provider) -> Self {
        Self {
            preferred_provider: Some(provider),
            ..Self::default()
        }
    }

    pub fn with_pinned_track(
        provider: Provider,
        track_id: Option<String>,
        track: &SyncedTrack,
        style: Option<String>,
    ) -> Result<Self, serde_json::Error> {
        let mut clean_track = track.clone();
        clean_track.user_offset_ms = 0;
        let payload = serde_json::to_vec(&clean_track)?;
        Ok(Self {
            preferred_provider: Some(provider),
            preferred_track_id: track_id,
            pinned_payload_json: Some(payload),
            preferred_style: style,
            updated_at: now_secs(),
        })
    }

    pub fn provider(&self) -> Option<Provider> {
        self.preferred_provider.clone()
    }

    pub fn pinned_track(&self) -> Option<SyncedTrack> {
        let bytes = self.pinned_payload_json.as_deref()?;
        let track: SyncedTrack = serde_json::from_slice(bytes).ok()?;
        if track.parser_version != PARSER_VERSION {
            return None;
        }
        Some(track)
    }
}

#[derive(Debug, Clone, Default)]
pub struct CacheLookup {
    pub answer: Option<ProviderAnswer>,
    pub needs_upgrade: bool,
    pub negative_providers: HashSet<Provider>,
    pub is_pinned: bool,
    pub preference: Option<TrackPreference>,
}

#[derive(Debug, Clone)]
pub struct CacheHandle {
    tx: mpsc::Sender<worker::CacheCommand>,
}

impl CacheHandle {
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, CacheError> {
        worker::spawn(path.into())
    }

    pub async fn lookup(&self, key: &CanonicalTrackKey) -> Result<CacheLookup, CacheError> {
        self.lookup_at(key, now_secs()).await
    }

    pub(crate) async fn lookup_at(
        &self,
        key: &CanonicalTrackKey,
        now: i64,
    ) -> Result<CacheLookup, CacheError> {
        let (reply, response) = oneshot::channel();
        self.tx
            .send(worker::CacheCommand::Lookup {
                track_key: blake3_track_key(key),
                now,
                reply,
            })
            .await
            .map_err(|_| CacheError::WorkerStopped)?;
        response.await.map_err(|_| CacheError::WorkerStopped)?
    }

    pub async fn store(
        &self,
        key: &CanonicalTrackKey,
        label: impl Into<String>,
        answer: &ProviderAnswer,
    ) -> Result<(), CacheError> {
        self.store_at(key, label.into(), answer, now_secs()).await
    }

    pub(crate) async fn store_at(
        &self,
        key: &CanonicalTrackKey,
        label: String,
        answer: &ProviderAnswer,
        now: i64,
    ) -> Result<(), CacheError> {
        let (reply, response) = oneshot::channel();
        self.tx
            .send(worker::CacheCommand::Store {
                track_key: blake3_track_key(key),
                label,
                answer: answer.clone(),
                now,
                reply,
            })
            .await
            .map_err(|_| CacheError::WorkerStopped)?;
        response.await.map_err(|_| CacheError::WorkerStopped)?
    }

    pub async fn record_negative(
        &self,
        key: &CanonicalTrackKey,
        provider: Provider,
    ) -> Result<(), CacheError> {
        self.record_negative_at(key, provider, now_secs()).await
    }

    pub(crate) async fn record_negative_at(
        &self,
        key: &CanonicalTrackKey,
        provider: Provider,
        now: i64,
    ) -> Result<(), CacheError> {
        let (reply, response) = oneshot::channel();
        self.tx
            .send(worker::CacheCommand::RecordNegative {
                track_key: blake3_track_key(key),
                provider,
                now,
                reply,
            })
            .await
            .map_err(|_| CacheError::WorkerStopped)?;
        response.await.map_err(|_| CacheError::WorkerStopped)?
    }

    pub async fn mark_upgrade_attempted(
        &self,
        key: &CanonicalTrackKey,
        provider: Provider,
    ) -> Result<(), CacheError> {
        let (reply, response) = oneshot::channel();
        self.tx
            .send(worker::CacheCommand::MarkUpgradeAttempted {
                track_key: blake3_track_key(key),
                provider,
                reply,
            })
            .await
            .map_err(|_| CacheError::WorkerStopped)?;
        response.await.map_err(|_| CacheError::WorkerStopped)?
    }

    pub async fn invalidate_track(&self, key: &CanonicalTrackKey) -> Result<(), CacheError> {
        let (reply, response) = oneshot::channel();
        self.tx
            .send(worker::CacheCommand::InvalidateTrack {
                track_key: blake3_track_key(key),
                reply,
            })
            .await
            .map_err(|_| CacheError::WorkerStopped)?;
        response.await.map_err(|_| CacheError::WorkerStopped)?
    }

    pub async fn get_track_preference(
        &self,
        key: &CanonicalTrackKey,
    ) -> Result<Option<TrackPreference>, CacheError> {
        let (reply, response) = oneshot::channel();
        self.tx
            .send(worker::CacheCommand::GetTrackPreference {
                track_key: blake3_track_key(key),
                reply,
            })
            .await
            .map_err(|_| CacheError::WorkerStopped)?;
        response.await.map_err(|_| CacheError::WorkerStopped)?
    }

    pub async fn set_track_preference(
        &self,
        key: &CanonicalTrackKey,
        preference: TrackPreference,
    ) -> Result<(), CacheError> {
        let (reply, response) = oneshot::channel();
        self.tx
            .send(worker::CacheCommand::SetTrackPreference {
                track_key: blake3_track_key(key),
                preference,
                reply,
            })
            .await
            .map_err(|_| CacheError::WorkerStopped)?;
        response.await.map_err(|_| CacheError::WorkerStopped)?
    }

    pub async fn set_pinned_lyrics(
        &self,
        key: &CanonicalTrackKey,
        provider: Provider,
        track_id: Option<String>,
        track: &SyncedTrack,
    ) -> Result<(), CacheError> {
        let mut clean_track = track.clone();
        clean_track.user_offset_ms = 0;
        let payload_json = serde_json::to_vec(&clean_track)?;
        let (reply, response) = oneshot::channel();
        self.tx
            .send(worker::CacheCommand::SetPinnedLyrics {
                track_key: blake3_track_key(key),
                provider,
                track_id,
                payload_json,
                now: now_secs(),
                reply,
            })
            .await
            .map_err(|_| CacheError::WorkerStopped)?;
        response.await.map_err(|_| CacheError::WorkerStopped)?
    }

    pub async fn set_pinned_style(
        &self,
        key: &CanonicalTrackKey,
        style: Option<String>,
    ) -> Result<(), CacheError> {
        let (reply, response) = oneshot::channel();
        self.tx
            .send(worker::CacheCommand::SetPinnedStyle {
                track_key: blake3_track_key(key),
                style,
                now: now_secs(),
                reply,
            })
            .await
            .map_err(|_| CacheError::WorkerStopped)?;
        response.await.map_err(|_| CacheError::WorkerStopped)?
    }

    pub async fn clear_pinned_lyrics(&self, key: &CanonicalTrackKey) -> Result<(), CacheError> {
        self.clear_pinned_lyrics_at(key, now_secs()).await
    }

    pub(crate) async fn clear_pinned_lyrics_at(
        &self,
        key: &CanonicalTrackKey,
        now: i64,
    ) -> Result<(), CacheError> {
        let (reply, response) = oneshot::channel();
        self.tx
            .send(worker::CacheCommand::ClearPinnedLyrics {
                track_key: blake3_track_key(key),
                now,
                reply,
            })
            .await
            .map_err(|_| CacheError::WorkerStopped)?;
        response.await.map_err(|_| CacheError::WorkerStopped)?
    }

    pub async fn shutdown(&self) -> Result<(), CacheError> {
        let (reply, response) = oneshot::channel();
        self.tx
            .send(worker::CacheCommand::Shutdown {
                now: now_secs(),
                reply,
            })
            .await
            .map_err(|_| CacheError::WorkerStopped)?;
        response.await.map_err(|_| CacheError::WorkerStopped)?
    }
}

pub(crate) fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| {
            i64::try_from(duration.as_secs()).unwrap_or(i64::MAX)
        })
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;
    use crate::core::model::{
        CanonicalTrackKey, LineRole, LyricLine, PARSER_VERSION, SyncGranularity, SyncedTrack,
    };
    use crate::providers::{Provider, Tier};

    fn temp_path(name: &str) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "lyrachi-cache-api-{name}-{}-{stamp}.db",
            std::process::id()
        ))
    }

    fn sample_key() -> CanonicalTrackKey {
        CanonicalTrackKey {
            norm_artist: "coldplay".to_string(),
            norm_title: "yellow".to_string(),
            norm_album: Some("parachutes".to_string()),
            duration_bucket_sec: Some(268),
        }
    }

    fn sample_answer() -> ProviderAnswer {
        ProviderAnswer::new(
            Provider::Lrclib,
            SyncedTrack {
                source_provider: "lrclib".to_string(),
                source_track_id: Some("yellow".to_string()),
                parser_version: PARSER_VERSION,
                is_instrumental: false,
                granularity: SyncGranularity::Word,
                intrinsic_offset_ms: 0,
                user_offset_ms: 125,
                match_confidence: 0.97,
                lines: vec![LyricLine {
                    start_ms: 1_000,
                    end_ms: 2_000,
                    role: LineRole::Primary,
                    text: "Look at the stars".to_string(),
                    words: Vec::new(),
                    annotations: Vec::new(),
                }],
            },
        )
    }

    fn cleanup(path: &Path) {
        let _ = fs::remove_file(path);
        for suffix in ["-wal", "-shm"] {
            let mut name = path.file_name().unwrap().to_os_string();
            name.push(suffix);
            let _ = fs::remove_file(path.with_file_name(name));
        }
        let parent = match path.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent,
            _ => Path::new("."),
        };
        if let Some(file_name) = path.file_name() {
            let prefix = format!("{}.corrupt.", file_name.to_string_lossy());
            if let Ok(entries) = fs::read_dir(parent) {
                for entry in entries.flatten() {
                    if entry.file_name().to_string_lossy().starts_with(&prefix) {
                        let _ = fs::remove_file(entry.path());
                    }
                }
            }
        }
    }

    #[tokio::test]
    async fn positive_entry_survives_across_cache_handles() {
        let path = temp_path("roundtrip");
        let key = sample_key();
        let answer = sample_answer();

        let first = CacheHandle::open(&path).unwrap();
        first
            .store(&key, "Coldplay - Yellow", &answer)
            .await
            .unwrap();
        first.shutdown().await.unwrap();
        drop(first);

        let second = CacheHandle::open(&path).unwrap();
        let cached = second.lookup(&key).await.unwrap().answer.unwrap();
        assert_eq!(cached.provider, Provider::Lrclib);
        assert_eq!(cached.tier, Tier::Word);
        assert_eq!(cached.track.source_track_id.as_deref(), Some("yellow"));
        assert_eq!(cached.track.lines[0].text, "Look at the stars");
        assert_eq!(cached.track.user_offset_ms, 0);
        second.shutdown().await.unwrap();

        cleanup(&path);
    }

    #[tokio::test]
    async fn invalidate_track_clears_positive_and_negative_entries() {
        let path = temp_path("invalidate");
        let key = sample_key();
        let answer = sample_answer();
        let handle = CacheHandle::open(&path).unwrap();

        handle
            .store(&key, "Coldplay - Yellow", &answer)
            .await
            .unwrap();
        handle
            .record_negative(&key, Provider::LyricsPlus)
            .await
            .unwrap();
        let cached = handle.lookup(&key).await.unwrap();
        assert!(cached.answer.is_some());
        assert!(cached.negative_providers.contains(&Provider::LyricsPlus));

        handle.invalidate_track(&key).await.unwrap();

        let cleared = handle.lookup(&key).await.unwrap();
        assert!(cleared.answer.is_none());
        assert!(cleared.negative_providers.is_empty());
        handle.shutdown().await.unwrap();

        cleanup(&path);
    }

    #[tokio::test]
    async fn corrupt_database_is_rebuilt_without_panicking() {
        let path = temp_path("corrupt");
        fs::write(&path, [0x7f, 0xff, 0x00, 0x12, 0x99, 0x42, 0xef, 0xbe]).unwrap();

        let handle = CacheHandle::open(&path).unwrap();
        assert!(handle.lookup(&sample_key()).await.unwrap().answer.is_none());

        // Storing into the rebuilt database succeeds and can be read back.
        let key = sample_key();
        let answer = sample_answer();
        handle
            .store(&key, "Coldplay - Yellow", &answer)
            .await
            .unwrap();
        let cached = handle.lookup(&key).await.unwrap().answer.unwrap();
        assert_eq!(cached.track.lines[0].text, "Look at the stars");
        handle.shutdown().await.unwrap();

        let backup_prefix = format!("{}.corrupt.", path.file_name().unwrap().to_string_lossy());
        let backups = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .flatten()
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(&backup_prefix)
            })
            .count();
        assert_eq!(backups, 1);

        cleanup(&path);
    }

    #[tokio::test]
    async fn bare_relative_corrupt_database_recovers_cleanly() {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let bare_name = format!("lyrachi-bare-corrupt-{}-{stamp}.db", std::process::id());
        let bare_path = PathBuf::from(&bare_name);
        fs::write(&bare_path, b"garbage data").unwrap();

        let handle = CacheHandle::open(&bare_path).expect("bare relative path should recover");
        assert!(handle.lookup(&sample_key()).await.unwrap().answer.is_none());
        handle.shutdown().await.unwrap();

        cleanup(&bare_path);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn two_handles_can_read_and_write_the_same_database() {
        let path = temp_path("concurrent");
        let first = CacheHandle::open(&path).unwrap();
        let second = CacheHandle::open(&path).unwrap();
        let key = sample_key();
        let answer = sample_answer();

        let first_task = {
            let handle = first.clone();
            let key = key.clone();
            let answer = answer.clone();
            tokio::spawn(async move {
                for index in 0..20 {
                    handle
                        .store(&key, format!("first {index}"), &answer)
                        .await
                        .unwrap();
                    handle.lookup(&key).await.unwrap();
                }
            })
        };
        let second_task = {
            let handle = second.clone();
            let key = key.clone();
            let answer = answer.clone();
            tokio::spawn(async move {
                for index in 0..20 {
                    handle
                        .store(&key, format!("second {index}"), &answer)
                        .await
                        .unwrap();
                    handle.lookup(&key).await.unwrap();
                }
            })
        };

        first_task.await.unwrap();
        second_task.await.unwrap();
        first.shutdown().await.unwrap();
        second.shutdown().await.unwrap();

        cleanup(&path);
    }

    #[tokio::test]
    async fn shutdown_flushes_debounced_touches() {
        let path = temp_path("shutdown");
        let key = sample_key();
        let answer = sample_answer();

        let handle = CacheHandle::open(&path).unwrap();
        handle
            .store(&key, "Coldplay - Yellow", &answer)
            .await
            .unwrap();
        assert!(handle.lookup(&key).await.unwrap().answer.is_some());
        handle.shutdown().await.unwrap();

        let connection = rusqlite::Connection::open(&path).unwrap();
        let parser_version: i64 = connection
            .query_row(
                "SELECT parser_version FROM lyric_entries LIMIT 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(parser_version, i64::from(PARSER_VERSION));

        cleanup(&path);
    }

    #[tokio::test]
    async fn pinned_lyrics_survive_invalidate_track_and_prune() {
        let path = temp_path("pinned-survives");
        let handle = CacheHandle::open(&path).unwrap();
        let key = sample_key();
        let answer = sample_answer();

        let pref = TrackPreference::with_pinned_track(
            Provider::Lrclib,
            Some("12345".to_string()),
            &answer.track,
            Some("tokyo-night".to_string()),
        )
        .unwrap();
        handle
            .set_track_preference(&key, pref.clone())
            .await
            .unwrap();

        handle
            .store(&key, "Coldplay - Yellow", &answer)
            .await
            .unwrap();

        let lookup = handle.lookup(&key).await.unwrap();
        assert!(lookup.is_pinned);
        assert!(!lookup.needs_upgrade);
        assert_eq!(
            lookup.answer.unwrap().track.lines[0].text,
            answer.track.lines[0].text
        );

        handle.invalidate_track(&key).await.unwrap();

        let lookup_after_invalid = handle.lookup(&key).await.unwrap();
        assert!(lookup_after_invalid.is_pinned);
        assert!(lookup_after_invalid.answer.is_some());

        handle.clear_pinned_lyrics(&key).await.unwrap();

        let lookup_cleared = handle.lookup(&key).await.unwrap();
        assert!(!lookup_cleared.is_pinned);
        assert!(lookup_cleared.answer.is_none());
        let cleared_pref = lookup_cleared
            .preference
            .expect("style preference should be preserved after clearing pin");
        assert!(cleared_pref.preferred_provider.is_none());
        assert!(cleared_pref.preferred_track_id.is_none());
        assert!(cleared_pref.pinned_payload_json.is_none());
        assert_eq!(cleared_pref.preferred_style.as_deref(), Some("tokyo-night"));

        handle.shutdown().await.unwrap();
        cleanup(&path);
    }

    #[tokio::test]
    async fn atomic_pin_lyrics_and_pin_style_do_not_clobber_each_other() {
        let path = temp_path("atomic-pref-columns");
        let handle = CacheHandle::open(&path).unwrap();
        let key = sample_key();
        let answer = sample_answer();

        handle
            .set_pinned_style(&key, Some("builtin:dark".to_string()))
            .await
            .unwrap();
        handle
            .set_pinned_lyrics(
                &key,
                Provider::Lrclib,
                Some("999".to_string()),
                &answer.track,
            )
            .await
            .unwrap();

        let lookup = handle.lookup(&key).await.unwrap();
        assert!(lookup.is_pinned);
        let pref = lookup.preference.expect("preference must exist");
        assert_eq!(
            pref.preferred_provider.as_ref().map(ProviderId::as_str),
            Some("lrclib")
        );
        assert_eq!(pref.preferred_track_id.as_deref(), Some("999"));
        assert_eq!(pref.preferred_style.as_deref(), Some("builtin:dark"));

        handle.set_pinned_style(&key, None).await.unwrap();
        let lookup_after_unpin_style = handle.lookup(&key).await.unwrap();
        assert!(lookup_after_unpin_style.is_pinned);
        let pref2 = lookup_after_unpin_style.preference.unwrap();
        assert_eq!(
            pref2.preferred_provider.as_ref().map(ProviderId::as_str),
            Some("lrclib")
        );
        assert_eq!(pref2.preferred_style, None);

        handle.shutdown().await.unwrap();
        cleanup(&path);
    }
}

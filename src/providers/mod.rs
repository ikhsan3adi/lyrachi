pub mod local;
pub mod lrclib;
pub mod sanitize;
pub mod score;

use std::path::PathBuf;

use tokio_util::sync::CancellationToken;

use crate::core::model::{CanonicalTrackKey, SyncedTrack};
use crate::media::TrackMeta;
use local::LocalProvider;
use lrclib::LrclibClient;
use sanitize::{
    canonical_key, clean_artist, clean_text, clean_title, normalize_for_match, split_artist_title,
    strip_dash_qualifier,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    Local,
    Lrclib,
}

impl Provider {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Lrclib => "lrclib",
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("local lyric file error: {0}")]
    Io(#[from] std::io::Error),
    #[error("LRCLIB request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("LRCLIB returned HTTP {0}")]
    Status(reqwest::StatusCode),
    #[error("invalid LRCLIB response: {0}")]
    Json(#[from] serde_json::Error),
    #[error("lyric resolution cancelled")]
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackQuery {
    pub artist: String,
    pub title: String,
    pub album: Option<String>,
    pub duration_ms: Option<u64>,
    pub user_offset_ms: i64,
    pub canonical: CanonicalTrackKey,
}

impl TrackQuery {
    pub fn from_meta(meta: &TrackMeta, user_offset_ms: i64) -> Self {
        let mut artist = clean_artist(&meta.artist);
        let split_source = strip_dash_qualifier(&meta.title);
        let title = if let Some((left, right)) = split_artist_title(&split_source) {
            let left_artist = clean_artist(&left);
            let right_title = clean_title(&right);
            if !left_artist.is_empty()
                && !right_title.is_empty()
                && (artist.is_empty()
                    || normalize_for_match(&left_artist) == normalize_for_match(&artist))
            {
                artist = left_artist;
                right_title
            } else {
                clean_title(&meta.title)
            }
        } else {
            clean_title(&meta.title)
        };

        let album = meta
            .album
            .as_deref()
            .map(clean_text)
            .filter(|album| !album.is_empty());
        let canonical = canonical_key(&artist, &title, album.as_deref(), meta.duration_ms);

        Self {
            artist,
            title,
            album,
            duration_ms: meta.duration_ms,
            user_offset_ms,
            canonical,
        }
    }

    pub fn is_usable(&self) -> bool {
        !self.title.is_empty() && !self.canonical.norm_title.is_empty()
    }

    pub fn duration_sec(&self) -> Option<f64> {
        self.duration_ms
            .map(|duration_ms| duration_ms as f64 / 1_000.0)
    }
}

pub struct ProviderResolver {
    local: LocalProvider,
    lrclib: LrclibClient,
}

impl ProviderResolver {
    pub fn new(local_dirs: Vec<PathBuf>) -> Result<Self, ProviderError> {
        Ok(Self {
            local: LocalProvider::new(local_dirs),
            lrclib: LrclibClient::new()?,
        })
    }

    pub async fn resolve(
        &self,
        meta: &TrackMeta,
        user_offset_ms: i64,
    ) -> Result<Option<SyncedTrack>, ProviderError> {
        let query = TrackQuery::from_meta(meta, user_offset_ms);
        self.resolve_query(&query, &CancellationToken::new()).await
    }

    pub async fn resolve_query(
        &self,
        query: &TrackQuery,
        cancel: &CancellationToken,
    ) -> Result<Option<SyncedTrack>, ProviderError> {
        if cancel.is_cancelled() {
            return Err(ProviderError::Cancelled);
        }
        if !query.is_usable() {
            return Ok(None);
        }

        if let Some(track) = self.local.find_with_cancel(query, cancel).await? {
            return Ok(Some(track));
        }

        self.lrclib.resolve_with_cancel(query, cancel).await
    }
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("lyrachi-{name}-{}-{unique}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn meta(title: &str, artist: &str, album: Option<&str>, duration_ms: Option<u64>) -> TrackMeta {
        TrackMeta {
            title: title.to_string(),
            artist: artist.to_string(),
            album: album.map(str::to_string),
            duration_ms,
            track_id: None,
        }
    }

    #[test]
    fn builds_clean_query_from_browser_metadata() {
        let query = TrackQuery::from_meta(
            &meta(
                "Coldplay - Yellow (Official Video)",
                "",
                Some("Parachutes"),
                Some(267_000),
            ),
            100,
        );

        assert_eq!(query.artist, "Coldplay");
        assert_eq!(query.title, "Yellow");
        assert_eq!(query.album.as_deref(), Some("Parachutes"));
        assert_eq!(query.duration_ms, Some(267_000));
        assert_eq!(query.user_offset_ms, 100);
        assert_eq!(query.canonical.norm_artist, "coldplay");
        assert_eq!(query.canonical.norm_title, "yellow");
    }

    #[test]
    fn splits_browser_title_with_featured_artist_before_noise_strip() {
        let query = TrackQuery::from_meta(
            &meta("Artist feat. Guest - Song (Official Video)", "", None, None),
            0,
        );

        assert_eq!(query.artist, "Artist");
        assert_eq!(query.title, "Song");
    }

    #[test]
    fn avoids_duplicate_artist_prefix_when_artist_matches_topic_channel() {
        let query = TrackQuery::from_meta(
            &meta(
                "Coldplay - Yellow (Official Video)",
                "Coldplay - Topic",
                None,
                None,
            ),
            0,
        );

        assert_eq!(query.artist, "Coldplay");
        assert_eq!(query.title, "Yellow");
    }

    #[test]
    fn rejects_empty_or_punctuation_only_titles() {
        let blank = TrackQuery::from_meta(&meta("   ", "Artist", None, None), 0);
        let punctuation = TrackQuery::from_meta(&meta("...", "Artist", None, None), 0);

        assert!(!blank.is_usable());
        assert!(!punctuation.is_usable());
    }

    #[test]
    fn computes_duration_seconds() {
        let query = TrackQuery::from_meta(&meta("Song", "Artist", None, Some(12_345)), 0);

        assert_eq!(query.duration_sec(), Some(12.345));
    }

    #[tokio::test]
    async fn local_provider_wins_before_lrclib() {
        let dir = temp_dir("resolver-local");
        std::fs::write(
            dir.join("Coldplay - Yellow.lrc"),
            "[00:01.00]Look at the stars\n",
        )
        .unwrap();
        let resolver = ProviderResolver::new(vec![dir.clone()]).unwrap();

        let track = resolver
            .resolve(&meta("Yellow", "Coldplay", None, None), 0)
            .await
            .unwrap()
            .unwrap();

        assert_eq!(track.source_provider, "local");

        std::fs::remove_dir_all(dir).unwrap();
    }
}

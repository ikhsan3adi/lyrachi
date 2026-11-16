pub mod engine;
mod http;
pub mod local;
pub mod lrclib;
pub mod sanitize;
pub mod score;

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::core::model::{CanonicalTrackKey, SyncGranularity, SyncedTrack};
use crate::media::TrackMeta;
use local::LocalProvider;
use lrclib::LrclibClient;
use sanitize::{
    canonical_key, clean_artist, clean_text, clean_title, normalize_for_match, split_artist_title,
    strip_dash_qualifier,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Provider {
    Local,
    Lrclib,
}

impl Provider {
    pub const fn all() -> [Self; 2] {
        [Self::Local, Self::Lrclib]
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Lrclib => "lrclib",
        }
    }

    pub fn from_config_name(name: &str) -> Option<Self> {
        match name {
            "local" => Some(Self::Local),
            "lrclib" => Some(Self::Lrclib),
            _ => None,
        }
    }

    pub const fn valid_config_names() -> &'static str {
        "local, lrclib"
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Tier {
    Instrumental = 1,
    Plain = 2,
    Line = 3,
    Word = 4,
    Syllable = 5,
}

impl Tier {
    pub fn from_track(track: &SyncedTrack) -> Self {
        if track.is_instrumental {
            return Self::Instrumental;
        }
        match track.granularity {
            SyncGranularity::Syllable => Self::Syllable,
            SyncGranularity::Word => Self::Word,
            SyncGranularity::Line => Self::Line,
            SyncGranularity::Unsynced => Self::Plain,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ProviderAnswer {
    pub provider: Provider,
    pub track: SyncedTrack,
    pub tier: Tier,
    pub confidence: f32,
}

impl ProviderAnswer {
    pub fn new(provider: Provider, track: SyncedTrack) -> Self {
        let tier = Tier::from_track(&track);
        let confidence = track.match_confidence;
        Self {
            provider,
            track,
            tier,
            confidence,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("provider unavailable: {0}")]
    Unavailable(String),
    #[error("provider rate limited (retry after {retry_after_ms:?} ms)")]
    RateLimited { retry_after_ms: Option<u64> },
    #[error("provider request timed out")]
    Timeout,
    #[error("invalid provider payload: {0}")]
    Invalid(String),
    #[error("provider lookup cancelled")]
    Cancelled,
}

impl From<reqwest::Error> for ProviderError {
    fn from(error: reqwest::Error) -> Self {
        if error.is_timeout() {
            Self::Timeout
        } else {
            Self::Unavailable(error.to_string())
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackQuery {
    pub artist: String,
    pub title: String,
    pub album: Option<String>,
    pub duration_ms: Option<u64>,
    pub track_url: Option<String>,
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
            track_url: meta.url.clone(),
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
    lrclib: Arc<LrclibClient>,
    order: Vec<Provider>,
    network: bool,
}

impl ProviderResolver {
    pub fn new(local_dirs: Vec<PathBuf>) -> Result<Self, ProviderError> {
        Self::with_config(local_dirs, Provider::all().to_vec(), true)
    }

    pub fn with_config(
        local_dirs: Vec<PathBuf>,
        order: Vec<Provider>,
        network: bool,
    ) -> Result<Self, ProviderError> {
        let lrclib = LrclibClient::new()?;
        Ok(Self {
            local: LocalProvider::new(local_dirs),
            lrclib: Arc::new(lrclib),
            order,
            network,
        })
    }

    #[cfg(test)]
    fn with_config_and_base_url(
        local_dirs: Vec<PathBuf>,
        order: Vec<Provider>,
        network: bool,
        base_url: impl Into<String>,
    ) -> Result<Self, ProviderError> {
        Ok(Self {
            local: LocalProvider::new(local_dirs),
            lrclib: Arc::new(LrclibClient::with_base_url(base_url)?),
            order,
            network,
        })
    }

    pub fn network_enabled(&self) -> bool {
        self.network
    }

    pub(crate) fn network_specs(&self) -> Vec<engine::NetworkSpec> {
        if !self.network {
            return Vec::new();
        }
        let mut seen = HashSet::new();
        self.order
            .iter()
            .enumerate()
            .filter_map(|(priority, provider)| {
                if !seen.insert(*provider) {
                    return None;
                }
                let source: Arc<dyn engine::NetworkProvider> = match provider {
                    Provider::Local => return None,
                    Provider::Lrclib => self.lrclib.clone(),
                };
                Some(engine::NetworkSpec {
                    provider: *provider,
                    priority,
                    source,
                })
            })
            .collect()
    }

    pub async fn resolve(
        &self,
        meta: &TrackMeta,
        user_offset_ms: i64,
    ) -> Result<Option<ProviderAnswer>, ProviderError> {
        let query = TrackQuery::from_meta(meta, user_offset_ms);
        self.resolve_query(&query, &CancellationToken::new()).await
    }

    pub async fn resolve_local(
        &self,
        query: &TrackQuery,
        cancel: &CancellationToken,
    ) -> Result<Option<ProviderAnswer>, ProviderError> {
        if cancel.is_cancelled() {
            return Err(ProviderError::Cancelled);
        }
        if !query.is_usable() || !self.order.contains(&Provider::Local) {
            return Ok(None);
        }
        self.local.find_with_cancel(query, cancel).await
    }

    pub async fn resolve_network(
        &self,
        query: &TrackQuery,
        cancel: &CancellationToken,
    ) -> Result<Option<ProviderAnswer>, ProviderError> {
        if cancel.is_cancelled() {
            return Err(ProviderError::Cancelled);
        }
        if !query.is_usable() || !self.network {
            return Ok(None);
        }

        for provider in &self.order {
            if cancel.is_cancelled() {
                return Err(ProviderError::Cancelled);
            }
            let answer = match provider {
                Provider::Local => None,
                Provider::Lrclib => self.lrclib.resolve_with_cancel(query, cancel).await?,
            };
            if answer.is_some() {
                return Ok(answer);
            }
        }

        Ok(None)
    }

    pub async fn resolve_query(
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

        for provider in &self.order {
            if cancel.is_cancelled() {
                return Err(ProviderError::Cancelled);
            }
            let answer = match provider {
                Provider::Local => self.local.find_with_cancel(query, cancel).await?,
                Provider::Lrclib if self.network => {
                    self.lrclib.resolve_with_cancel(query, cancel).await?
                }
                Provider::Lrclib => None,
            };
            if answer.is_some() {
                return Ok(answer);
            }
        }

        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use tokio::net::TcpListener;

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
            url: None,
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

        assert_eq!(track.provider, Provider::Local);
        assert_eq!(track.track.source_provider, "local");

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn offline_skips_network_provider_without_opening_a_socket() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let resolver = ProviderResolver::with_config_and_base_url(
            Vec::new(),
            vec![Provider::Lrclib],
            false,
            base_url,
        )
        .unwrap();

        let track = resolver
            .resolve(&meta("Yellow", "Coldplay", None, None), 0)
            .await
            .unwrap();

        assert!(track.is_none());
        assert!(
            tokio::time::timeout(Duration::from_millis(100), listener.accept())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn honors_configured_provider_order() {
        let dir = temp_dir("resolver-order");
        std::fs::write(dir.join("Yellow.lrc"), "[00:01.00]Local\n").unwrap();
        let resolver = ProviderResolver::with_config(
            vec![dir.clone()],
            vec![Provider::Lrclib, Provider::Local],
            false,
        )
        .unwrap();

        let track = resolver
            .resolve(&meta("Yellow", "Coldplay", None, None), 0)
            .await
            .unwrap()
            .unwrap();

        assert_eq!(track.provider, Provider::Local);
        assert_eq!(track.track.source_provider, "local");
        std::fs::remove_dir_all(dir).unwrap();
    }
}

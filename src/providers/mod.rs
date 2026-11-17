pub mod engine;
mod http;
pub mod local;
pub mod lrclib;
pub mod lyricsplus;
pub mod sanitize;
pub mod score;
#[cfg(feature = "ytmusic")]
pub mod ytmusic;

pub use engine::{
    CircuitBreaker, CircuitBreakers, CircuitState, RateLimiters, TokenBucketRateLimiter,
};

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::task::JoinSet;
use tokio::time::sleep_until;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

const CANDIDATE_SEARCH_TIMEOUT: Duration = Duration::from_secs(12);

use crate::core::model::{CanonicalTrackKey, SyncGranularity, SyncedTrack};
use crate::media::TrackMeta;
use local::LocalProvider;
use lrclib::LrclibClient;
use lyricsplus::LyricsPlusProvider;
use sanitize::{
    canonical_key, clean_artist, clean_text, clean_title, normalize_for_match, split_artist_title,
    strip_dash_qualifier,
};
#[cfg(feature = "ytmusic")]
use ytmusic::YtMusicProvider;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderCapabilities {
    pub supports_search: bool,
    pub needs_hydration: bool,
    pub max_candidates: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Provider {
    Local,
    Lrclib,
    LyricsPlus,
    #[cfg(feature = "ytmusic")]
    YtMusic,
}

impl Provider {
    pub const fn all() -> [Self; 3] {
        [Self::Local, Self::Lrclib, Self::LyricsPlus]
    }

    pub const fn capabilities(self) -> ProviderCapabilities {
        match self {
            Self::Local => ProviderCapabilities {
                supports_search: true,
                needs_hydration: true,
                max_candidates: usize::MAX,
            },
            Self::Lrclib => ProviderCapabilities {
                supports_search: true,
                needs_hydration: false,
                max_candidates: 20,
            },
            Self::LyricsPlus => ProviderCapabilities {
                supports_search: false,
                needs_hydration: false,
                max_candidates: 1,
            },
            #[cfg(feature = "ytmusic")]
            Self::YtMusic => ProviderCapabilities {
                supports_search: true,
                needs_hydration: true,
                max_candidates: 5,
            },
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Lrclib => "lrclib",
            Self::LyricsPlus => "lyricsplus",
            #[cfg(feature = "ytmusic")]
            Self::YtMusic => "ytmusic",
        }
    }

    pub fn from_config_name(name: &str) -> Option<Self> {
        match name {
            "local" => Some(Self::Local),
            "lrclib" => Some(Self::Lrclib),
            "lyricsplus" => Some(Self::LyricsPlus),
            #[cfg(feature = "ytmusic")]
            "ytmusic" => Some(Self::YtMusic),
            _ => None,
        }
    }

    pub const fn valid_config_names() -> &'static str {
        #[cfg(feature = "ytmusic")]
        {
            "local, lrclib, lyricsplus, ytmusic"
        }
        #[cfg(not(feature = "ytmusic"))]
        {
            "local, lrclib, lyricsplus"
        }
    }

    pub const fn is_tier2(self) -> bool {
        match self {
            Self::LyricsPlus => true,
            #[cfg(feature = "ytmusic")]
            Self::YtMusic => true,
            _ => false,
        }
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

#[derive(Debug, Clone)]
pub struct CandidateSummary {
    pub provider: Provider,
    pub source_track_id: Option<String>,
    pub title: String,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub duration_ms: Option<u64>,
    pub tier: Tier,
    pub confidence: f32,
    pub(crate) local_path: Option<PathBuf>,
    pub(crate) browse_hint: Option<String>,
    pub(crate) query_duration_ms: Option<u64>,
    pub(crate) query_user_offset_ms: i64,
    pub(crate) hydrated: Option<ProviderAnswer>,
}

impl CandidateSummary {
    pub fn local_path(&self) -> Option<&Path> {
        self.local_path.as_deref()
    }

    pub fn browse_hint(&self) -> Option<&str> {
        self.browse_hint.as_deref()
    }

    pub fn is_hydrated(&self) -> bool {
        self.hydrated.is_some()
    }
}

#[derive(Debug, Clone, thiserror::Error)]
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
    lyricsplus: Arc<LyricsPlusProvider>,
    #[cfg(feature = "ytmusic")]
    ytmusic: Arc<YtMusicProvider>,
    order: Vec<Provider>,
    network: bool,
    breakers: Arc<engine::CircuitBreakers>,
    limiters: Arc<engine::RateLimiters>,
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
        let lyricsplus = LyricsPlusProvider::new()?;
        #[cfg(feature = "ytmusic")]
        let ytmusic = YtMusicProvider::new()?;
        Ok(Self {
            local: LocalProvider::new(local_dirs),
            lrclib: Arc::new(lrclib),
            lyricsplus: Arc::new(lyricsplus),
            #[cfg(feature = "ytmusic")]
            ytmusic: Arc::new(ytmusic),
            order,
            network,
            breakers: Arc::new(engine::CircuitBreakers::default()),
            limiters: Arc::new(engine::RateLimiters::default()),
        })
    }

    #[cfg(test)]
    fn with_config_and_base_url(
        local_dirs: Vec<PathBuf>,
        order: Vec<Provider>,
        network: bool,
        base_url: impl Into<String>,
    ) -> Result<Self, ProviderError> {
        let base_url = base_url.into();
        Ok(Self {
            local: LocalProvider::new(local_dirs),
            lrclib: Arc::new(LrclibClient::with_base_url(base_url.clone())?),
            lyricsplus: Arc::new(LyricsPlusProvider::with_base_url(base_url.clone())?),
            #[cfg(feature = "ytmusic")]
            ytmusic: Arc::new(YtMusicProvider::with_base_url(base_url)?),
            order,
            network,
            breakers: Arc::new(engine::CircuitBreakers::default()),
            limiters: Arc::new(engine::RateLimiters::default()),
        })
    }

    #[cfg(test)]
    pub fn with_config_and_breakers(
        local_dirs: Vec<PathBuf>,
        order: Vec<Provider>,
        network: bool,
        breakers: Arc<engine::CircuitBreakers>,
    ) -> Result<Self, ProviderError> {
        let lrclib = LrclibClient::new()?;
        let lyricsplus = LyricsPlusProvider::new()?;
        #[cfg(feature = "ytmusic")]
        let ytmusic = YtMusicProvider::new()?;
        Ok(Self {
            local: LocalProvider::new(local_dirs),
            lrclib: Arc::new(lrclib),
            lyricsplus: Arc::new(lyricsplus),
            #[cfg(feature = "ytmusic")]
            ytmusic: Arc::new(ytmusic),
            order,
            network,
            breakers,
            limiters: Arc::new(engine::RateLimiters::default()),
        })
    }

    pub fn network_enabled(&self) -> bool {
        self.network
    }

    pub fn breakers(&self) -> &Arc<engine::CircuitBreakers> {
        &self.breakers
    }

    pub fn limiters(&self) -> &Arc<engine::RateLimiters> {
        &self.limiters
    }

    pub fn circuit_breaker_statuses(&self) -> Vec<(Provider, &'static str)> {
        let mut seen = HashSet::new();
        let now = tokio::time::Instant::now();
        self.order
            .iter()
            .copied()
            .filter(|p| *p != Provider::Local && seen.insert(*p))
            .map(|p| (p, self.breakers.status(p, now)))
            .collect()
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
                    Provider::LyricsPlus => self.lyricsplus.clone(),
                    #[cfg(feature = "ytmusic")]
                    Provider::YtMusic => self.ytmusic.clone(),
                };
                let limiter = if provider.is_tier2() {
                    Some(self.limiters.get_or_create(*provider))
                } else {
                    None
                };
                Some(engine::NetworkSpec {
                    provider: *provider,
                    priority,
                    source,
                    limiter,
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
                Provider::LyricsPlus => self.lyricsplus.resolve_with_cancel(query, cancel).await?,
                #[cfg(feature = "ytmusic")]
                Provider::YtMusic => self.ytmusic.resolve_with_cancel(query, cancel).await?,
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
                Provider::LyricsPlus if self.network => {
                    self.lyricsplus.resolve_with_cancel(query, cancel).await?
                }
                Provider::LyricsPlus => None,
                #[cfg(feature = "ytmusic")]
                Provider::YtMusic if self.network => {
                    self.ytmusic.resolve_with_cancel(query, cancel).await?
                }
                #[cfg(feature = "ytmusic")]
                Provider::YtMusic => None,
            };
            if answer.is_some() {
                return Ok(answer);
            }
        }

        Ok(None)
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

        let now = tokio::time::Instant::now();
        let mut seen = HashSet::new();
        let mut tasks = JoinSet::new();

        for (priority, &provider) in self.order.iter().enumerate() {
            if !seen.insert(provider) {
                continue;
            }
            if provider != Provider::Local
                && (!self.network || !self.breakers.can_attempt(provider, now))
            {
                continue;
            }

            let query = query.clone();
            let cancel = cancel.clone();
            let limiter = provider
                .is_tier2()
                .then(|| self.limiters.get_or_create(provider));

            match provider {
                Provider::Local => {
                    let local = self.local.clone();
                    tasks.spawn(async move {
                        let result = local.search_candidates(&query, &cancel).await;
                        (provider, priority, result)
                    });
                }
                Provider::Lrclib => {
                    let lrclib = self.lrclib.clone();
                    tasks.spawn(async move {
                        if let Some(limiter) = limiter
                            && let Err(error) = limiter.acquire(&cancel).await
                        {
                            return (provider, priority, Err(error));
                        }
                        let result = lrclib.search_candidates(&query, &cancel).await;
                        (provider, priority, result)
                    });
                }
                Provider::LyricsPlus => {
                    let lyricsplus = self.lyricsplus.clone();
                    tasks.spawn(async move {
                        if let Some(limiter) = limiter
                            && let Err(error) = limiter.acquire(&cancel).await
                        {
                            return (provider, priority, Err(error));
                        }
                        let result = lyricsplus.search_candidates(&query, &cancel).await;
                        (provider, priority, result)
                    });
                }
                #[cfg(feature = "ytmusic")]
                Provider::YtMusic => {
                    let ytmusic = self.ytmusic.clone();
                    tasks.spawn(async move {
                        if let Some(limiter) = limiter
                            && let Err(error) = limiter.acquire(&cancel).await
                        {
                            return (provider, priority, Err(error));
                        }
                        let result = ytmusic.search_candidates(&query, &cancel).await;
                        (provider, priority, result)
                    });
                }
            }
        }

        let deadline = now + CANDIDATE_SEARCH_TIMEOUT;
        let mut all_candidates: Vec<(usize, CandidateSummary)> = Vec::new();
        let mut succeeded = 0usize;
        let mut timed_out = false;
        let mut first_error: Option<ProviderError> = None;

        while !tasks.is_empty() {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    tasks.abort_all();
                    return Err(ProviderError::Cancelled);
                }
                joined = tasks.join_next() => {
                    let Some(joined) = joined else {
                        break;
                    };
                    let (provider, priority, result) = match joined {
                        Ok(res) => res,
                        Err(error) => {
                            warn!(%error, "candidate search task failed");
                            continue;
                        }
                    };

                    if provider != Provider::Local {
                        let now = tokio::time::Instant::now();
                        match &result {
                            Ok(_) => self.breakers.record_success(provider),
                            Err(error) => self.breakers.record_error(provider, error, now),
                        }
                    }

                    match result {
                        Ok(list) => {
                            succeeded += 1;
                            for item in list {
                                all_candidates.push((priority, item));
                            }
                        }
                        Err(ProviderError::Cancelled) => {}
                        Err(error) => {
                            debug!(provider = provider.as_str(), %error, "candidate search failed for provider");
                            if first_error.is_none() {
                                first_error = Some(error);
                            }
                        }
                    }
                }
                _ = sleep_until(deadline) => {
                    debug!("candidate search timed out; returning collected results");
                    timed_out = true;
                    tasks.abort_all();
                    break;
                }
            }
        }

        if all_candidates.is_empty() && succeeded == 0 {
            if let Some(error) = first_error {
                return Err(error);
            }
            if timed_out {
                return Err(ProviderError::Timeout);
            }
        }

        sort_candidates(&mut all_candidates);

        Ok(all_candidates
            .into_iter()
            .map(|(_, summary)| summary)
            .collect())
    }

    pub async fn hydrate_candidate(
        &self,
        summary: &CandidateSummary,
        cancel: &CancellationToken,
    ) -> Result<Option<ProviderAnswer>, ProviderError> {
        if cancel.is_cancelled() {
            return Err(ProviderError::Cancelled);
        }
        if let Some(answer) = &summary.hydrated {
            return Ok(Some(answer.clone()));
        }

        match summary.provider {
            Provider::Local => self.local.hydrate_candidate(summary, cancel).await,
            Provider::Lrclib | Provider::LyricsPlus => Ok(None),
            #[cfg(feature = "ytmusic")]
            Provider::YtMusic => {
                if !self.network {
                    return Ok(None);
                }
                let limiter = self.limiters.get_or_create(Provider::YtMusic);
                let result = self
                    .ytmusic
                    .hydrate_candidate(summary, Some(limiter.as_ref()), cancel)
                    .await;
                self.breakers.record_result(
                    Provider::YtMusic,
                    &result,
                    tokio::time::Instant::now(),
                );
                result
            }
        }
    }
}

fn confidence_band(confidence: f32) -> i32 {
    if !confidence.is_finite() {
        return i32::MIN;
    }
    (confidence.clamp(0.0, 1.0) * 20.0).floor() as i32
}

fn sort_candidates(candidates: &mut [(usize, CandidateSummary)]) {
    candidates.sort_by(|(left_pri, left), (right_pri, right)| {
        confidence_band(right.confidence)
            .cmp(&confidence_band(left.confidence))
            .then_with(|| right.tier.cmp(&left.tier))
            .then_with(|| left_pri.cmp(right_pri))
            .then_with(|| right.confidence.total_cmp(&left.confidence))
            .then_with(|| left.source_track_id.cmp(&right.source_track_id))
    });
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
    async fn lyricsplus_network_spec_uses_the_tier2_limiter() {
        let resolver =
            ProviderResolver::with_config(Vec::new(), vec![Provider::LyricsPlus], true).unwrap();
        let specs = resolver.network_specs();

        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].provider, Provider::LyricsPlus);
        assert!(specs[0].limiter.is_some());
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

    #[tokio::test]
    #[cfg(feature = "ytmusic")]
    async fn ytmusic_not_called_when_not_in_order() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());

        let resolver = ProviderResolver::with_config_and_base_url(
            Vec::new(),
            Provider::all().to_vec(),
            true,
            base_url,
        )
        .unwrap();

        let specs = resolver.network_specs();
        assert!(specs.iter().all(|s| s.provider != Provider::YtMusic));
        assert!(!resolver.order.contains(&Provider::YtMusic));

        let q = TrackQuery::from_meta(&meta("Yellow", "Coldplay", None, None), 0);
        let cancel = CancellationToken::new();

        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = vec![0u8; 2048];
                let n = socket.read(&mut buf).await.unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]);
                assert!(!req.contains("POST /search"));
                assert!(!req.contains("/next"));
                assert!(!req.contains("/browse"));
                assert!(!req.contains("X-YouTube-Client-Name"));

                let _ = socket
                    .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n")
                    .await;
            }
        });

        let result = resolver.resolve_query(&q, &cancel).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    #[cfg(feature = "ytmusic")]
    async fn ytmusic_network_spec_uses_the_tier2_limiter() {
        let resolver =
            ProviderResolver::with_config(Vec::new(), vec![Provider::YtMusic], true).unwrap();
        let specs = resolver.network_specs();

        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].provider, Provider::YtMusic);
        assert!(specs[0].limiter.is_some());
    }

    #[test]
    fn provider_capabilities_match_endpoint_contracts() {
        assert_eq!(
            Provider::Local.capabilities(),
            ProviderCapabilities {
                supports_search: true,
                needs_hydration: true,
                max_candidates: usize::MAX,
            }
        );
        assert_eq!(
            Provider::Lrclib.capabilities(),
            ProviderCapabilities {
                supports_search: true,
                needs_hydration: false,
                max_candidates: 20,
            }
        );
        assert_eq!(
            Provider::LyricsPlus.capabilities(),
            ProviderCapabilities {
                supports_search: false,
                needs_hydration: false,
                max_candidates: 1,
            }
        );
        #[cfg(feature = "ytmusic")]
        assert_eq!(
            Provider::YtMusic.capabilities(),
            ProviderCapabilities {
                supports_search: true,
                needs_hydration: true,
                max_candidates: 5,
            }
        );
    }

    #[tokio::test]
    async fn search_candidates_offline_returns_only_local_without_touching_network() {
        let dir = temp_dir("search-offline");
        std::fs::write(
            dir.join("Coldplay - Yellow.elrc"),
            "[00:01.00]<00:01.00>Look at the stars\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("Yellow.lrc"),
            "[00:01.00]Look how they shine for you\n",
        )
        .unwrap();

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let resolver = ProviderResolver::with_config_and_base_url(
            vec![dir.clone()],
            vec![Provider::Local, Provider::Lrclib, Provider::LyricsPlus],
            false,
            base_url,
        )
        .unwrap();

        let q = TrackQuery::from_meta(&meta("Yellow", "Coldplay", None, Some(269_000)), 0);
        let cancel = CancellationToken::new();
        let candidates = resolver.search_candidates(&q, &cancel).await.unwrap();

        assert_eq!(candidates.len(), 2);
        assert!(candidates.iter().all(|c| c.provider == Provider::Local));
        assert!(candidates.iter().all(|c| !c.is_hydrated()));
        assert!(candidates.iter().all(|c| c.local_path().is_some()));

        // Hydrate the second candidate (.lrc) cleanly without network
        let hydrated = resolver
            .hydrate_candidate(&candidates[1], &cancel)
            .await
            .unwrap()
            .expect("should hydrate local candidate");
        assert_eq!(hydrated.provider, Provider::Local);

        assert!(
            tokio::time::timeout(Duration::from_millis(80), listener.accept())
                .await
                .is_err(),
            "offline search must never open a network socket"
        );

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn search_candidates_tolerates_partial_failure_and_hydrates_without_n_plus_1() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());

        let lrclib_hits = Arc::new(AtomicUsize::new(0));
        let lp_hits = Arc::new(AtomicUsize::new(0));
        let lrclib_hits_srv = Arc::clone(&lrclib_hits);
        let lp_hits_srv = Arc::clone(&lp_hits);

        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = vec![0u8; 4096];
                let n = socket.read(&mut buf).await.unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).into_owned();
                let first_line = req.lines().next().unwrap_or("");

                if first_line.starts_with("GET /api/search") {
                    lrclib_hits_srv.fetch_add(1, Ordering::SeqCst);
                    let body = r#"[
                        {"id": 10, "trackName": "Yellow", "artistName": "Coldplay", "duration": 269.0, "syncedLyrics": "[00:01.00]Stars"}
                    ]"#;
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = socket.write_all(resp.as_bytes()).await;
                } else if first_line.starts_with("GET /v2/lyrics/get") {
                    lp_hits_srv.fetch_add(1, Ordering::SeqCst);
                    let _ = socket
                        .write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n")
                        .await;
                } else {
                    let _ = socket
                        .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n")
                        .await;
                }
            }
        });

        let resolver = ProviderResolver::with_config_and_base_url(
            Vec::new(),
            vec![Provider::LyricsPlus, Provider::Lrclib],
            true,
            base_url,
        )
        .unwrap();

        let q = TrackQuery::from_meta(&meta("Yellow", "Coldplay", None, Some(269_000)), 0);
        let cancel = CancellationToken::new();

        // LyricsPlus returns 503, Lrclib succeeds -> partial failure succeeds with Lrclib result
        let candidates = resolver.search_candidates(&q, &cancel).await.unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].provider, Provider::Lrclib);
        assert!(candidates[0].is_hydrated());
        assert_eq!(lrclib_hits.load(Ordering::SeqCst), 1);
        assert_eq!(lp_hits.load(Ordering::SeqCst), 1);

        // Hydrating Lrclib candidate must make ZERO extra HTTP requests!
        let answer = resolver
            .hydrate_candidate(&candidates[0], &cancel)
            .await
            .unwrap()
            .expect("should return pre-hydrated answer");
        assert_eq!(answer.provider, Provider::Lrclib);
        assert_eq!(lrclib_hits.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn sort_candidates_is_transitive_across_band_boundaries() {
        let make = |id: &str, confidence: f32, tier: Tier| CandidateSummary {
            provider: Provider::Lrclib,
            source_track_id: Some(id.to_string()),
            title: "Song".to_string(),
            artist: Some("Artist".to_string()),
            album: None,
            duration_ms: Some(200_000),
            tier,
            confidence,
            local_path: None,
            browse_hint: None,
            query_duration_ms: Some(200_000),
            query_user_offset_ms: 0,
            hydrated: None,
        };

        // Case that creates a cycle with non-transitive `|a - b| <= 0.05` comparison
        let mut items = vec![
            (0, make("c", 0.84, Tier::Syllable)),
            (0, make("b", 0.87, Tier::Word)),
            (0, make("a", 0.90, Tier::Plain)),
            (0, make("nan", f32::NAN, Tier::Syllable)),
        ];
        sort_candidates(&mut items);

        assert_eq!(items[0].1.source_track_id.as_deref(), Some("a"));
        assert_eq!(items[1].1.source_track_id.as_deref(), Some("b"));
        assert_eq!(items[2].1.source_track_id.as_deref(), Some("c"));
        assert_eq!(items[3].1.source_track_id.as_deref(), Some("nan"));
    }

    #[tokio::test]
    #[cfg(feature = "ytmusic")]
    async fn resolver_search_and_hydrate_ytmusic_respects_offline_and_avoids_n_plus_1() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let fixture = include_str!("../../tests/fixtures/ytmusic_response.json");
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());

        let search_hits = Arc::new(AtomicUsize::new(0));
        let next_hits = Arc::new(AtomicUsize::new(0));
        let browse_hits = Arc::new(AtomicUsize::new(0));

        let search_hits_srv = Arc::clone(&search_hits);
        let next_hits_srv = Arc::clone(&next_hits);
        let browse_hits_srv = Arc::clone(&browse_hits);

        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = vec![0u8; 4096];
                let n = socket.read(&mut buf).await.unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).into_owned();
                let first_line = req.lines().next().unwrap_or("");

                if first_line.starts_with("POST /search") {
                    search_hits_srv.fetch_add(1, Ordering::SeqCst);
                    let body = serde_json::json!({
                        "contents": [{
                            "musicResponsiveListItemRenderer": {
                                "playlistItemData": { "videoId": "yKNxeF4KMsY" },
                                "flexColumns": [
                                    { "musicResponsiveListItemFlexColumnRenderer": { "text": { "runs": [{ "text": "Yellow" }] } } },
                                    { "musicResponsiveListItemFlexColumnRenderer": { "text": { "runs": [{ "text": "Coldplay" }] } } }
                                ]
                            }
                        }]
                    })
                    .to_string();
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = socket.write_all(resp.as_bytes()).await;
                } else if first_line.starts_with("POST /next") {
                    next_hits_srv.fetch_add(1, Ordering::SeqCst);
                    let next_body = r#"{
                        "contents": {
                            "singleColumnMusicWatchNextResultsRenderer": {
                                "tabbedRenderer": {
                                    "watchNextTabbedResultsRenderer": {
                                        "tabs": [{
                                            "tabRenderer": {
                                                "title": "Lyrics",
                                                "endpoint": { "browseEndpoint": { "browseId": "MPLYt_res" } },
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
                } else if first_line.starts_with("POST /browse") {
                    browse_hits_srv.fetch_add(1, Ordering::SeqCst);
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                        fixture.len(),
                        fixture
                    );
                    let _ = socket.write_all(resp.as_bytes()).await;
                } else {
                    let _ = socket
                        .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n")
                        .await;
                }
            }
        });

        let resolver = ProviderResolver::with_config_and_base_url(
            Vec::new(),
            vec![Provider::YtMusic],
            true,
            base_url.clone(),
        )
        .unwrap();

        let q = TrackQuery::from_meta(&meta("Yellow", "Coldplay", None, Some(269_000)), 0);
        let cancel = CancellationToken::new();

        let candidates = resolver.search_candidates(&q, &cancel).await.unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(search_hits.load(Ordering::SeqCst), 1);
        assert_eq!(next_hits.load(Ordering::SeqCst), 0);
        assert_eq!(browse_hits.load(Ordering::SeqCst), 0);

        // Offline resolver must refuse to hydrate a network candidate
        let offline_resolver = ProviderResolver::with_config_and_base_url(
            Vec::new(),
            vec![Provider::YtMusic],
            false,
            base_url,
        )
        .unwrap();
        assert!(
            offline_resolver
                .hydrate_candidate(&candidates[0], &cancel)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(next_hits.load(Ordering::SeqCst), 0);

        // Online resolver hydrates via /next + /browse
        let answer = resolver
            .hydrate_candidate(&candidates[0], &cancel)
            .await
            .unwrap()
            .expect("should hydrate YtMusic candidate");
        assert_eq!(answer.provider, Provider::YtMusic);
        assert_eq!(next_hits.load(Ordering::SeqCst), 1);
        assert_eq!(browse_hits.load(Ordering::SeqCst), 1);
    }
}

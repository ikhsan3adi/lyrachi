pub mod betterlyrics;
pub mod engine;
#[cfg(feature = "experimental-providers")]
pub mod experimental;
mod http;
pub mod local;
pub mod lrclib;
pub mod lyricsplus;
pub mod sanitize;
pub mod score;
pub mod unison;
#[cfg(feature = "ytmusic")]
pub mod ytmusic;

pub use engine::{
    CircuitBreaker, CircuitBreakers, CircuitState, RateLimiters, TokenBucketRateLimiter,
};

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::fmt;
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
use betterlyrics::BetterLyricsProvider;
use local::LocalProvider;
use lrclib::LrclibClient;
use lyricsplus::LyricsPlusProvider;
use sanitize::{
    canonical_key, clean_artist, clean_text, clean_title, normalize_for_match, split_artist_title,
    strip_dash_qualifier,
};
use unison::UnisonProvider;
#[cfg(feature = "ytmusic")]
use ytmusic::YtMusicProvider;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderCapabilities {
    pub supports_search: bool,
    pub needs_hydration: bool,
    pub max_candidates: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ProviderId(Cow<'static, str>);

impl ProviderId {
    #[allow(non_upper_case_globals)]
    pub const Local: Self = Self(Cow::Borrowed("local"));
    #[allow(non_upper_case_globals)]
    pub const Lrclib: Self = Self(Cow::Borrowed("lrclib"));
    #[allow(non_upper_case_globals)]
    pub const LyricsPlus: Self = Self(Cow::Borrowed("lyricsplus"));
    #[allow(non_upper_case_globals)]
    pub const Unison: Self = Self(Cow::Borrowed("unison"));
    #[allow(non_upper_case_globals)]
    pub const BetterLyrics: Self = Self(Cow::Borrowed("betterlyrics"));
    #[cfg(feature = "experimental-providers")]
    #[allow(non_upper_case_globals)]
    pub const Netease: Self = Self(Cow::Borrowed("netease"));
    #[cfg(feature = "experimental-providers")]
    #[allow(non_upper_case_globals)]
    pub const QqMusic: Self = Self(Cow::Borrowed("qqmusic"));
    #[cfg(feature = "experimental-providers")]
    #[allow(non_upper_case_globals)]
    pub const Kugou: Self = Self(Cow::Borrowed("kugou"));
    #[cfg(feature = "ytmusic")]
    #[allow(non_upper_case_globals)]
    pub const YtMusic: Self = Self(Cow::Borrowed("ytmusic"));

    pub fn new(name: impl Into<Cow<'static, str>>) -> Self {
        Self(name.into())
    }

    pub fn all() -> Vec<Self> {
        vec![
            Self::Local,
            Self::BetterLyrics,
            Self::Unison,
            Self::Lrclib,
            Self::LyricsPlus,
        ]
    }

    pub fn as_str(&self) -> &str {
        self.0.as_ref()
    }

    pub fn from_config_name(name: &str) -> Option<Self> {
        match name {
            "local" => Some(Self::Local),
            "lrclib" => Some(Self::Lrclib),
            "lyricsplus" => Some(Self::LyricsPlus),
            "unison" => Some(Self::Unison),
            "betterlyrics" => Some(Self::BetterLyrics),
            #[cfg(feature = "experimental-providers")]
            "netease" => Some(Self::Netease),
            #[cfg(feature = "experimental-providers")]
            "qqmusic" => Some(Self::QqMusic),
            #[cfg(feature = "experimental-providers")]
            "kugou" => Some(Self::Kugou),
            #[cfg(feature = "ytmusic")]
            "ytmusic" => Some(Self::YtMusic),
            _ => None,
        }
    }

    pub fn from_cache_name(name: &str) -> Option<Self> {
        let trimmed = name.trim();
        (!trimmed.is_empty()).then(|| Self::new(trimmed.to_string()))
    }

    pub fn valid_config_names() -> &'static str {
        #[cfg(all(feature = "ytmusic", feature = "experimental-providers"))]
        {
            "local, betterlyrics, unison, lrclib, lyricsplus, netease, qqmusic, kugou, ytmusic"
        }
        #[cfg(all(feature = "ytmusic", not(feature = "experimental-providers")))]
        {
            "local, betterlyrics, unison, lrclib, lyricsplus, ytmusic"
        }
        #[cfg(all(not(feature = "ytmusic"), feature = "experimental-providers"))]
        {
            "local, betterlyrics, unison, lrclib, lyricsplus, netease, qqmusic, kugou"
        }
        #[cfg(all(not(feature = "ytmusic"), not(feature = "experimental-providers")))]
        {
            "local, betterlyrics, unison, lrclib, lyricsplus"
        }
    }

    pub fn is_local(&self) -> bool {
        *self == Self::Local
    }

    pub fn capabilities(&self) -> ProviderCapabilities {
        match self.as_str() {
            "local" => ProviderCapabilities {
                supports_search: true,
                needs_hydration: true,
                max_candidates: usize::MAX,
            },
            "lrclib" => ProviderCapabilities {
                supports_search: true,
                needs_hydration: false,
                max_candidates: 20,
            },
            "lyricsplus" => ProviderCapabilities {
                supports_search: false,
                needs_hydration: false,
                max_candidates: 1,
            },
            "unison" => ProviderCapabilities {
                supports_search: true,
                needs_hydration: true,
                max_candidates: 20,
            },
            "betterlyrics" => ProviderCapabilities {
                supports_search: false,
                needs_hydration: false,
                max_candidates: 1,
            },
            "netease" | "qqmusic" | "kugou" => ProviderCapabilities {
                supports_search: true,
                needs_hydration: true,
                max_candidates: 10,
            },
            "ytmusic" => ProviderCapabilities {
                supports_search: true,
                needs_hydration: true,
                max_candidates: 5,
            },
            _ => ProviderCapabilities {
                supports_search: true,
                needs_hydration: true,
                max_candidates: 20,
            },
        }
    }
}

impl fmt::Display for ProviderId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl From<String> for ProviderId {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

impl From<&'static str> for ProviderId {
    fn from(value: &'static str) -> Self {
        Self::new(value)
    }
}

pub type Provider = ProviderId;

pub trait LyricProvider: Send + Sync {
    fn id(&self) -> ProviderId;
    fn capabilities(&self) -> ProviderCapabilities;
    fn rate_limited(&self) -> bool {
        false
    }
    fn resolve<'a>(
        &'a self,
        query: &'a TrackQuery,
        cancel: &'a CancellationToken,
    ) -> crate::media::BoxFuture<'a, Result<Option<ProviderAnswer>, ProviderError>>;
    fn search_candidates<'a>(
        &'a self,
        query: &'a TrackQuery,
        cancel: &'a CancellationToken,
    ) -> crate::media::BoxFuture<'a, Result<Vec<CandidateSummary>, ProviderError>>;
    fn hydrate_candidate<'a>(
        &'a self,
        summary: &'a CandidateSummary,
        limiter: Option<&'a engine::TokenBucketRateLimiter>,
        cancel: &'a CancellationToken,
    ) -> crate::media::BoxFuture<'a, Result<Option<ProviderAnswer>, ProviderError>>;
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

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Instrumental => "instrumental",
            Self::Plain => "plain",
            Self::Line => "line",
            Self::Word => "word",
            Self::Syllable => "syllable",
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
    #[error("provider payload exceeds {0} bytes")]
    PayloadTooLarge(usize),
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

pub struct ProviderRegistry {
    providers: HashMap<ProviderId, Arc<dyn LyricProvider>>,
    local: Arc<LocalProvider>,
    order: Vec<ProviderId>,
    network: bool,
    breakers: Arc<engine::CircuitBreakers>,
    limiters: Arc<engine::RateLimiters>,
}

pub type ProviderResolver = ProviderRegistry;

impl ProviderRegistry {
    pub fn new(local_dirs: Vec<PathBuf>) -> Result<Self, ProviderError> {
        Self::with_config(local_dirs, ProviderId::all(), true)
    }

    pub fn with_config(
        local_dirs: Vec<PathBuf>,
        order: Vec<ProviderId>,
        network: bool,
    ) -> Result<Self, ProviderError> {
        #[allow(unused_mut)]
        let mut registry = Self::assemble(
            LocalProvider::new(local_dirs),
            LrclibClient::new()?,
            LyricsPlusProvider::new()?,
            UnisonProvider::new()?,
            BetterLyricsProvider::new()?,
            #[cfg(feature = "ytmusic")]
            Some(YtMusicProvider::new()?),
            #[cfg(not(feature = "ytmusic"))]
            None,
            order,
            network,
            Arc::new(engine::CircuitBreakers::default()),
            Arc::new(engine::RateLimiters::default()),
        );
        #[cfg(feature = "experimental-providers")]
        registry.install_experimental(None)?;
        Ok(registry)
    }

    #[cfg(test)]
    fn with_config_and_base_url(
        local_dirs: Vec<PathBuf>,
        order: Vec<ProviderId>,
        network: bool,
        base_url: impl Into<String>,
    ) -> Result<Self, ProviderError> {
        let base_url = base_url.into();
        #[allow(unused_mut)]
        let mut registry = Self::assemble(
            LocalProvider::new(local_dirs),
            LrclibClient::with_base_url(base_url.clone())?,
            LyricsPlusProvider::with_base_url(base_url.clone())?,
            UnisonProvider::with_base_url(base_url.clone())?,
            BetterLyricsProvider::with_base_url(base_url.clone())?,
            #[cfg(feature = "ytmusic")]
            Some(YtMusicProvider::with_base_url(base_url.clone())?),
            #[cfg(not(feature = "ytmusic"))]
            None,
            order,
            network,
            Arc::new(engine::CircuitBreakers::default()),
            Arc::new(engine::RateLimiters::default()),
        );
        #[cfg(feature = "experimental-providers")]
        registry.install_experimental(Some(&base_url))?;
        Ok(registry)
    }

    #[cfg(test)]
    pub fn with_config_and_breakers(
        local_dirs: Vec<PathBuf>,
        order: Vec<ProviderId>,
        network: bool,
        breakers: Arc<engine::CircuitBreakers>,
    ) -> Result<Self, ProviderError> {
        #[allow(unused_mut)]
        let mut registry = Self::assemble(
            LocalProvider::new(local_dirs),
            LrclibClient::new()?,
            LyricsPlusProvider::new()?,
            UnisonProvider::new()?,
            BetterLyricsProvider::new()?,
            #[cfg(feature = "ytmusic")]
            Some(YtMusicProvider::new()?),
            #[cfg(not(feature = "ytmusic"))]
            None,
            order,
            network,
            breakers,
            Arc::new(engine::RateLimiters::default()),
        );
        #[cfg(feature = "experimental-providers")]
        registry.install_experimental(None)?;
        Ok(registry)
    }

    #[allow(clippy::too_many_arguments)]
    fn assemble(
        local: LocalProvider,
        lrclib: LrclibClient,
        lyricsplus: LyricsPlusProvider,
        unison: UnisonProvider,
        betterlyrics: BetterLyricsProvider,
        #[cfg(feature = "ytmusic")] ytmusic: Option<YtMusicProvider>,
        #[cfg(not(feature = "ytmusic"))] _ytmusic: Option<()>,
        order: Vec<ProviderId>,
        network: bool,
        breakers: Arc<engine::CircuitBreakers>,
        limiters: Arc<engine::RateLimiters>,
    ) -> Self {
        let local = Arc::new(local);
        let mut providers: HashMap<ProviderId, Arc<dyn LyricProvider>> = HashMap::new();
        providers.insert(ProviderId::Local, local.clone());
        providers.insert(ProviderId::Lrclib, Arc::new(lrclib));
        providers.insert(ProviderId::LyricsPlus, Arc::new(lyricsplus));
        providers.insert(ProviderId::Unison, Arc::new(unison));
        providers.insert(ProviderId::BetterLyrics, Arc::new(betterlyrics));
        #[cfg(feature = "ytmusic")]
        if let Some(ytmusic) = ytmusic {
            providers.insert(ProviderId::YtMusic, Arc::new(ytmusic));
        }

        Self {
            providers,
            local,
            order,
            network,
            breakers,
            limiters,
        }
    }

    #[cfg(feature = "experimental-providers")]
    fn install_experimental(&mut self, base_url: Option<&str>) -> Result<(), ProviderError> {
        use experimental::{KugouProvider, NeteaseProvider, QqMusicProvider};

        let netease = match base_url {
            Some(base_url) => NeteaseProvider::with_base_url(base_url)?,
            None => NeteaseProvider::new()?,
        };
        let qqmusic = match base_url {
            Some(base_url) => QqMusicProvider::with_base_url(base_url)?,
            None => QqMusicProvider::new()?,
        };
        let kugou = match base_url {
            Some(base_url) => KugouProvider::with_base_url(base_url)?,
            None => KugouProvider::new()?,
        };

        self.providers
            .insert(ProviderId::Netease, Arc::new(netease));
        self.providers
            .insert(ProviderId::QqMusic, Arc::new(qqmusic));
        self.providers.insert(ProviderId::Kugou, Arc::new(kugou));
        Ok(())
    }

    pub fn register(&mut self, provider: Arc<dyn LyricProvider>) {
        let id = provider.id();
        if id.is_local() {
            debug!("ignoring attempt to replace the local provider");
            return;
        }
        if !self.order.contains(&id) {
            self.order.push(id.clone());
        }
        self.providers.insert(id, provider);
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

    pub fn circuit_breaker_statuses(&self) -> Vec<(ProviderId, &'static str)> {
        let mut seen = HashSet::new();
        let now = tokio::time::Instant::now();
        self.order
            .iter()
            .filter(|id| !id.is_local() && seen.insert((*id).clone()))
            .map(|id| (id.clone(), self.breakers.status(id.clone(), now)))
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
            .filter_map(|(priority, id)| {
                if id.is_local() || !seen.insert(id.clone()) {
                    return None;
                }
                let source = self.providers.get(id)?.clone();
                let limiter = source
                    .rate_limited()
                    .then(|| self.limiters.get_or_create(id.clone()));
                Some(engine::NetworkSpec {
                    provider: id.clone(),
                    priority,
                    source,
                    limiter,
                })
            })
            .collect()
    }

    pub fn invalidate_local_index(&self) {
        self.local.invalidate_index();
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
        if !query.is_usable() || !self.order.iter().any(ProviderId::is_local) {
            return Ok(None);
        }
        LyricProvider::resolve(self.local.as_ref(), query, cancel).await
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

        for id in &self.order {
            if cancel.is_cancelled() {
                return Err(ProviderError::Cancelled);
            }
            if id.is_local() {
                continue;
            }
            let Some(provider) = self.providers.get(id) else {
                continue;
            };
            if let Some(answer) = provider.resolve(query, cancel).await? {
                return Ok(Some(answer));
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

        for id in &self.order {
            if cancel.is_cancelled() {
                return Err(ProviderError::Cancelled);
            }
            if !id.is_local() && !self.network {
                continue;
            }
            let Some(provider) = self.providers.get(id) else {
                continue;
            };
            if let Some(answer) = provider.resolve(query, cancel).await? {
                return Ok(Some(answer));
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
        let mut in_flight_network = HashSet::new();
        let mut tasks = JoinSet::new();

        for (priority, id) in self.order.iter().enumerate() {
            let id = id.clone();
            if !seen.insert(id.clone()) {
                continue;
            }
            let Some(provider) = self.providers.get(&id).cloned() else {
                continue;
            };
            if !id.is_local() && (!self.network || !self.breakers.can_attempt(id.clone(), now)) {
                continue;
            }
            if !id.is_local() {
                in_flight_network.insert(id.clone());
            }

            let query = query.clone();
            let cancel = cancel.clone();
            let limiter = provider
                .rate_limited()
                .then(|| self.limiters.get_or_create(id.clone()));

            tasks.spawn(async move {
                if let Some(limiter) = limiter
                    && let Err(error) = limiter.acquire(&cancel).await
                {
                    return (id, priority, Err(error));
                }
                let result = provider.search_candidates(&query, &cancel).await;
                (id, priority, result)
            });
        }

        let deadline = now + CANDIDATE_SEARCH_TIMEOUT;
        let mut all_candidates: Vec<(usize, CandidateSummary)> = Vec::new();
        let mut network_succeeded = 0usize;
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

                    if !provider.is_local() {
                        in_flight_network.remove(&provider);
                        let now = tokio::time::Instant::now();
                        match &result {
                            Ok(_) => self.breakers.record_success(provider.clone()),
                            Err(error) => self.breakers.record_error(provider.clone(), error, now),
                        }
                    }

                    match result {
                        Ok(list) => {
                            if !provider.is_local() {
                                network_succeeded += 1;
                            }
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
                    let timeout_now = tokio::time::Instant::now();
                    for provider in in_flight_network.drain() {
                        self.breakers
                            .record_error(provider, &ProviderError::Timeout, timeout_now);
                    }
                    tasks.abort_all();
                    break;
                }
            }
        }

        if all_candidates.is_empty() && network_succeeded == 0 {
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
        let Some(provider) = self.providers.get(&summary.provider).cloned() else {
            return Ok(None);
        };
        if provider.id().is_local() {
            return provider.hydrate_candidate(summary, None, cancel).await;
        }
        if !self.network {
            return Ok(None);
        }
        if !self
            .breakers
            .can_attempt(summary.provider.clone(), tokio::time::Instant::now())
        {
            return Err(ProviderError::Unavailable(format!(
                "{} circuit breaker open",
                summary.provider
            )));
        }
        let limiter = provider
            .rate_limited()
            .then(|| self.limiters.get_or_create(summary.provider.clone()));
        let result = provider
            .hydrate_candidate(summary, limiter.as_deref(), cancel)
            .await;
        self.breakers.record_result(
            summary.provider.clone(),
            &result,
            tokio::time::Instant::now(),
        );
        result
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
            .then_with(|| left.title.cmp(&right.title))
            .then_with(|| left.artist.cmp(&right.artist))
            .then_with(|| left.local_path.cmp(&right.local_path))
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

    struct DynamicProvider {
        id: ProviderId,
    }

    impl LyricProvider for DynamicProvider {
        fn id(&self) -> ProviderId {
            self.id.clone()
        }

        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                supports_search: true,
                needs_hydration: false,
                max_candidates: 1,
            }
        }

        fn rate_limited(&self) -> bool {
            true
        }

        fn resolve<'a>(
            &'a self,
            _query: &'a TrackQuery,
            _cancel: &'a CancellationToken,
        ) -> crate::media::BoxFuture<'a, Result<Option<ProviderAnswer>, ProviderError>> {
            Box::pin(async { Ok(None) })
        }

        fn search_candidates<'a>(
            &'a self,
            query: &'a TrackQuery,
            _cancel: &'a CancellationToken,
        ) -> crate::media::BoxFuture<'a, Result<Vec<CandidateSummary>, ProviderError>> {
            let candidate = CandidateSummary {
                provider: self.id.clone(),
                source_track_id: Some("dynamic-1".to_string()),
                title: query.title.clone(),
                artist: Some(query.artist.clone()),
                album: query.album.clone(),
                duration_ms: query.duration_ms,
                tier: Tier::Line,
                confidence: 0.9,
                local_path: None,
                browse_hint: None,
                query_duration_ms: query.duration_ms,
                query_user_offset_ms: query.user_offset_ms,
                hydrated: None,
            };
            Box::pin(async move { Ok(vec![candidate]) })
        }

        fn hydrate_candidate<'a>(
            &'a self,
            _summary: &'a CandidateSummary,
            _limiter: Option<&'a engine::TokenBucketRateLimiter>,
            _cancel: &'a CancellationToken,
        ) -> crate::media::BoxFuture<'a, Result<Option<ProviderAnswer>, ProviderError>> {
            Box::pin(async { Ok(None) })
        }
    }

    #[tokio::test]
    async fn open_providers_are_registered_with_shared_limiters() {
        let resolver = ProviderResolver::with_config(
            Vec::new(),
            vec![
                Provider::Local,
                Provider::BetterLyrics,
                Provider::Unison,
                Provider::Lrclib,
            ],
            true,
        )
        .unwrap();

        let specs = resolver.network_specs();
        assert_eq!(specs.len(), 3);
        for id in [Provider::BetterLyrics, Provider::Unison] {
            let spec = specs
                .iter()
                .find(|spec| spec.provider == id)
                .expect("open provider should have a network spec");
            assert!(spec.limiter.is_some());
        }
    }

    #[tokio::test]
    async fn registered_dynamic_provider_uses_the_shared_search_and_limiter_path() {
        let id = ProviderId::new("dynamic-test");
        let mut resolver =
            ProviderResolver::with_config(Vec::new(), vec![ProviderId::Local], true).unwrap();
        resolver.register(Arc::new(DynamicProvider { id: id.clone() }));

        assert!(resolver.order.contains(&id));

        let specs = resolver.network_specs();
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].provider, id);
        assert!(specs[0].limiter.is_some());

        let query = TrackQuery::from_meta(&meta("Yellow", "Coldplay", None, None), 0);
        let candidates = resolver
            .search_candidates(&query, &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].provider, id);
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
    fn provider_ids_are_distinct_and_round_trip_from_config_and_cache_names() {
        assert_eq!(
            ProviderId::from_config_name("lrclib"),
            Some(ProviderId::Lrclib)
        );
        assert_eq!(
            ProviderId::from_config_name("lyricsplus"),
            Some(ProviderId::LyricsPlus)
        );
        #[cfg(feature = "ytmusic")]
        assert_eq!(
            ProviderId::from_config_name("ytmusic"),
            Some(ProviderId::YtMusic)
        );
        assert_eq!(
            ProviderId::from_config_name("unison"),
            Some(ProviderId::Unison)
        );
        assert_eq!(
            ProviderId::from_config_name("betterlyrics"),
            Some(ProviderId::BetterLyrics)
        );

        let unison = ProviderId::from_cache_name("unison").unwrap();
        let legacy = ProviderId::from_cache_name("legacy-provider").unwrap();
        assert_eq!(unison.as_str(), "unison");
        assert_ne!(unison, legacy);
        assert_ne!(unison, ProviderId::Lrclib);

        assert_eq!(
            ProviderId::from_cache_name("  lrclib "),
            Some(ProviderId::Lrclib)
        );
        assert_eq!(ProviderId::from_cache_name("   "), None);
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
        assert_eq!(
            Provider::Unison.capabilities(),
            ProviderCapabilities {
                supports_search: true,
                needs_hydration: true,
                max_candidates: 20,
            }
        );
        assert_eq!(
            Provider::BetterLyrics.capabilities(),
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

    #[tokio::test]
    async fn search_candidates_does_not_swallow_network_failure_when_local_is_empty() {
        let dir = temp_dir("empty-local-net-fail");
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base_url = format!("http://{addr}");

        let server = tokio::spawn(async move {
            for _ in 0..2 {
                if let Ok((mut stream, _)) = listener.accept().await {
                    use tokio::io::AsyncWriteExt;
                    let _ = stream
                        .write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n")
                        .await;
                }
            }
        });

        let resolver = ProviderResolver::with_config_and_base_url(
            vec![dir.clone()],
            vec![Provider::Local, Provider::Lrclib],
            true,
            base_url,
        )
        .unwrap();

        let q = TrackQuery::from_meta(&meta("Yellow", "Coldplay", None, Some(269_000)), 0);
        let err = resolver
            .search_candidates(&q, &CancellationToken::new())
            .await
            .unwrap_err();
        assert!(!matches!(err, ProviderError::Cancelled));

        server.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(feature = "experimental-providers")]
    #[test]
    fn experimental_providers_are_available_but_not_default() {
        assert_eq!(
            ProviderId::from_config_name("netease"),
            Some(ProviderId::Netease)
        );
        assert_eq!(
            ProviderId::from_config_name("qqmusic"),
            Some(ProviderId::QqMusic)
        );
        assert_eq!(
            ProviderId::from_config_name("kugou"),
            Some(ProviderId::Kugou)
        );
        assert!(!ProviderId::all().contains(&ProviderId::Netease));
        assert!(!ProviderId::all().contains(&ProviderId::QqMusic));
        assert!(!ProviderId::all().contains(&ProviderId::Kugou));

        let resolver = ProviderResolver::with_config(
            Vec::new(),
            vec![ProviderId::Netease, ProviderId::QqMusic, ProviderId::Kugou],
            false,
        )
        .unwrap();
        assert!(resolver.providers.contains_key(&ProviderId::Netease));
        assert!(resolver.providers.contains_key(&ProviderId::QqMusic));
        assert!(resolver.providers.contains_key(&ProviderId::Kugou));
    }

    #[cfg(feature = "ytmusic")]
    #[tokio::test]
    async fn hydrate_candidate_rejects_when_ytmusic_breaker_is_open() {
        let breakers = Arc::new(engine::CircuitBreakers::default());
        let now = tokio::time::Instant::now();
        let server_err = ProviderError::Unavailable("http 503".to_string());
        for _ in 0..3 {
            breakers.record_error(Provider::YtMusic, &server_err, now);
        }
        assert!(!breakers.can_attempt(Provider::YtMusic, now));

        let resolver = ProviderResolver::with_config_and_breakers(
            Vec::new(),
            vec![Provider::YtMusic],
            true,
            Arc::clone(&breakers),
        )
        .unwrap();

        let summary = CandidateSummary {
            provider: Provider::YtMusic,
            source_track_id: Some("dQw4w9WgXcQ".to_string()),
            title: "Yellow".to_string(),
            artist: Some("Coldplay".to_string()),
            album: None,
            duration_ms: Some(269_000),
            tier: Tier::Line,
            confidence: 0.9,
            local_path: None,
            browse_hint: Some("MPREb_test".to_string()),
            query_duration_ms: Some(269_000),
            query_user_offset_ms: 0,
            hydrated: None,
        };

        let err = resolver
            .hydrate_candidate(&summary, &CancellationToken::new())
            .await
            .unwrap_err();
        assert!(matches!(err, ProviderError::Unavailable(_)));
    }
}

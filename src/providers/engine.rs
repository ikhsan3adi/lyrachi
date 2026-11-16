use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio::time::{Instant, sleep_until};
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use super::lrclib::LrclibClient;
use super::lyricsplus::LyricsPlusProvider;
#[cfg(feature = "ytmusic")]
use super::ytmusic::YtMusicProvider;
use super::{Provider, ProviderAnswer, ProviderError, ProviderResolver, Tier, TrackQuery};
use crate::cache::CacheHandle;

pub const SYNC_GRACE: Duration = Duration::from_secs(2);
pub const LOOKUP_TIMEOUT: Duration = Duration::from_secs(12);
pub const BREAKER_FAILURE_THRESHOLD: u32 = 3;
pub const BREAKER_COOLDOWN: Duration = Duration::from_secs(5 * 60);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CircuitState {
    Closed,
    Open,
    HalfOpen,
}

impl CircuitState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Closed => "closed",
            Self::Open => "open",
            Self::HalfOpen => "half_open",
        }
    }
}

pub fn is_breaker_failure(error: &ProviderError) -> bool {
    matches!(
        error,
        ProviderError::Unavailable(_) | ProviderError::RateLimited { .. } | ProviderError::Timeout
    )
}

#[derive(Debug, Clone)]
pub struct CircuitBreaker {
    consecutive_failures: u32,
    state: CircuitState,
    opened_at: Option<Instant>,
    cooldown: Duration,
    probe_in_flight: bool,
}

impl Default for CircuitBreaker {
    fn default() -> Self {
        Self::new()
    }
}

impl CircuitBreaker {
    pub fn new() -> Self {
        Self {
            consecutive_failures: 0,
            state: CircuitState::Closed,
            opened_at: None,
            cooldown: BREAKER_COOLDOWN,
            probe_in_flight: false,
        }
    }

    pub fn state_at(&self, now: Instant) -> CircuitState {
        match self.state {
            CircuitState::Closed => CircuitState::Closed,
            CircuitState::Open => {
                if let Some(opened) = self.opened_at {
                    if now.saturating_duration_since(opened) >= self.cooldown {
                        CircuitState::HalfOpen
                    } else {
                        CircuitState::Open
                    }
                } else {
                    CircuitState::Open
                }
            }
            CircuitState::HalfOpen => CircuitState::HalfOpen,
        }
    }

    pub fn can_attempt(&mut self, now: Instant) -> bool {
        match self.state {
            CircuitState::Closed => true,
            CircuitState::Open => {
                if let Some(opened) = self.opened_at {
                    if now.saturating_duration_since(opened) >= self.cooldown {
                        self.state = CircuitState::HalfOpen;
                        self.probe_in_flight = true;
                        true
                    } else {
                        false
                    }
                } else {
                    false
                }
            }
            CircuitState::HalfOpen => {
                if self.probe_in_flight {
                    false
                } else {
                    self.probe_in_flight = true;
                    true
                }
            }
        }
    }

    pub fn record_success(&mut self) {
        self.consecutive_failures = 0;
        self.state = CircuitState::Closed;
        self.opened_at = None;
        self.cooldown = BREAKER_COOLDOWN;
        self.probe_in_flight = false;
    }

    pub fn record_error(&mut self, error: &ProviderError, now: Instant) {
        if !is_breaker_failure(error) {
            if self.state == CircuitState::HalfOpen {
                self.probe_in_flight = false;
            }
            return;
        }

        let retry_cooldown = match error {
            ProviderError::RateLimited {
                retry_after_ms: Some(ms),
            } => Duration::from_millis(*ms),
            _ => Duration::ZERO,
        };
        let cooldown = BREAKER_COOLDOWN.max(retry_cooldown);

        match self.state {
            CircuitState::HalfOpen => {
                self.state = CircuitState::Open;
                self.opened_at = Some(now);
                self.cooldown = cooldown;
                self.probe_in_flight = false;
            }
            CircuitState::Closed => {
                self.consecutive_failures += 1;
                if self.consecutive_failures >= BREAKER_FAILURE_THRESHOLD {
                    self.state = CircuitState::Open;
                    self.opened_at = Some(now);
                    self.cooldown = cooldown;
                }
            }
            CircuitState::Open => {
                self.probe_in_flight = false;
            }
        }
    }

    pub fn consecutive_failures(&self) -> u32 {
        self.consecutive_failures
    }
}

#[derive(Debug, Default)]
pub struct CircuitBreakers {
    breakers: Mutex<HashMap<Provider, CircuitBreaker>>,
}

impl CircuitBreakers {
    pub fn can_attempt(&self, provider: Provider, now: Instant) -> bool {
        let mut map = self.breakers.lock().unwrap();
        map.entry(provider).or_default().can_attempt(now)
    }

    pub fn record_success(&self, provider: Provider) {
        let mut map = self.breakers.lock().unwrap();
        map.entry(provider).or_default().record_success();
    }

    pub fn record_error(&self, provider: Provider, error: &ProviderError, now: Instant) {
        let mut map = self.breakers.lock().unwrap();
        map.entry(provider).or_default().record_error(error, now);
    }

    pub fn record_result(
        &self,
        provider: Provider,
        result: &Result<Option<ProviderAnswer>, ProviderError>,
        now: Instant,
    ) {
        match result {
            Ok(_) => self.record_success(provider),
            Err(error) => self.record_error(provider, error, now),
        }
    }

    pub fn status(&self, provider: Provider, now: Instant) -> &'static str {
        let map = self.breakers.lock().unwrap();
        map.get(&provider)
            .map(|breaker| breaker.state_at(now).as_str())
            .unwrap_or("closed")
    }
}

#[derive(Debug)]
pub struct TokenBucketRateLimiter {
    capacity: f64,
    refill_rate_per_sec: f64,
    state: Mutex<RateLimiterState>,
}

#[derive(Debug)]
struct RateLimiterState {
    tat: Instant,
}

impl TokenBucketRateLimiter {
    pub fn new(capacity: f64, refill_rate_per_sec: f64) -> Self {
        assert!(capacity >= 1.0 && refill_rate_per_sec > 0.0);
        let now = Instant::now();
        Self {
            capacity,
            refill_rate_per_sec,
            state: Mutex::new(RateLimiterState { tat: now }),
        }
    }

    pub fn tier2_default() -> Self {
        Self::new(1.0, 1.0)
    }

    pub async fn acquire(&self, cancel: &CancellationToken) -> Result<(), ProviderError> {
        if cancel.is_cancelled() {
            return Err(ProviderError::Cancelled);
        }

        let wait_until = {
            let mut state = self.state.lock().unwrap();
            let now = Instant::now();
            let interval = Duration::from_secs_f64(1.0 / self.refill_rate_per_sec);
            let burst_tolerance =
                Duration::from_secs_f64((self.capacity - 1.0) / self.refill_rate_per_sec);

            let earliest = now.checked_sub(burst_tolerance).unwrap_or(now);
            let scheduled = state.tat.max(earliest);
            state.tat = scheduled + interval;

            scheduled.checked_sub(burst_tolerance).unwrap_or(scheduled)
        };

        let now = Instant::now();
        if wait_until > now {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    return Err(ProviderError::Cancelled);
                }
                _ = sleep_until(wait_until) => {}
            }
        }

        if cancel.is_cancelled() {
            return Err(ProviderError::Cancelled);
        }

        Ok(())
    }
}

#[derive(Debug, Default)]
pub struct RateLimiters {
    limiters: Mutex<HashMap<Provider, Arc<TokenBucketRateLimiter>>>,
}

impl RateLimiters {
    pub fn get_or_create(&self, provider: Provider) -> Arc<TokenBucketRateLimiter> {
        let mut map = self.limiters.lock().unwrap();
        map.entry(provider)
            .or_insert_with(|| Arc::new(TokenBucketRateLimiter::tier2_default()))
            .clone()
    }
}

type ProviderFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Option<ProviderAnswer>, ProviderError>> + Send + 'a>>;

pub(crate) trait NetworkProvider: Send + Sync {
    fn lookup<'a>(
        &'a self,
        query: &'a TrackQuery,
        cancel: &'a CancellationToken,
    ) -> ProviderFuture<'a>;
}

impl NetworkProvider for LrclibClient {
    fn lookup<'a>(
        &'a self,
        query: &'a TrackQuery,
        cancel: &'a CancellationToken,
    ) -> ProviderFuture<'a> {
        Box::pin(self.resolve_with_cancel(query, cancel))
    }
}

impl NetworkProvider for LyricsPlusProvider {
    fn lookup<'a>(
        &'a self,
        query: &'a TrackQuery,
        cancel: &'a CancellationToken,
    ) -> ProviderFuture<'a> {
        Box::pin(self.resolve_with_cancel(query, cancel))
    }
}

#[cfg(feature = "ytmusic")]
impl NetworkProvider for YtMusicProvider {
    fn lookup<'a>(
        &'a self,
        query: &'a TrackQuery,
        cancel: &'a CancellationToken,
    ) -> ProviderFuture<'a> {
        Box::pin(self.resolve_with_cancel(query, cancel))
    }
}

#[derive(Debug)]
pub struct EngineUpdate {
    pub result: Result<Option<ProviderAnswer>, ProviderError>,
    pub is_final: bool,
}

pub(crate) struct NetworkSpec {
    pub provider: Provider,
    pub priority: usize,
    pub source: Arc<dyn NetworkProvider>,
    pub limiter: Option<Arc<TokenBucketRateLimiter>>,
}

struct Candidate {
    priority: usize,
    answer: ProviderAnswer,
}

impl Candidate {
    fn is_better_than(&self, other: &Self) -> bool {
        self.answer.tier > other.answer.tier
            || (self.answer.tier == other.answer.tier && self.priority < other.priority)
    }
}

#[derive(Debug)]
struct RaceOutcome {
    answer: Option<ProviderAnswer>,
    negative_providers: Vec<Provider>,
    error: Option<ProviderError>,
    had_error: bool,
}

impl RaceOutcome {
    fn result(&self) -> Result<Option<ProviderAnswer>, ProviderError> {
        match &self.answer {
            Some(answer) => Ok(Some(answer.clone())),
            None => match &self.error {
                Some(error) => Err(error.clone()),
                None => Ok(None),
            },
        }
    }
}

pub async fn resolve(
    resolver: &ProviderResolver,
    cache: Option<&CacheHandle>,
    query: &TrackQuery,
    cancel: &CancellationToken,
    updates: &mpsc::UnboundedSender<EngineUpdate>,
    bypass_cache: bool,
) -> Result<(), ProviderError> {
    if cancel.is_cancelled() {
        return Err(ProviderError::Cancelled);
    }
    if !query.is_usable() {
        emit(updates, Ok(None), true);
        return Ok(());
    }

    match resolver.resolve_local(query, cancel).await {
        Ok(Some(answer)) => {
            emit(updates, Ok(Some(answer)), true);
            return Ok(());
        }
        Ok(None) => {}
        Err(ProviderError::Cancelled) => return Err(ProviderError::Cancelled),
        Err(error) => {
            warn!(%error, "local lyrics provider failed; continuing resolution");
        }
    }

    let specs = resolver.network_specs();
    let breakers = resolver.breakers();
    resolve_with_specs(
        specs,
        cache,
        Some(breakers.as_ref()),
        query,
        cancel,
        updates,
        bypass_cache,
    )
    .await
}

async fn resolve_with_specs(
    specs: Vec<NetworkSpec>,
    cache: Option<&CacheHandle>,
    breakers: Option<&CircuitBreakers>,
    query: &TrackQuery,
    cancel: &CancellationToken,
    updates: &mpsc::UnboundedSender<EngineUpdate>,
    bypass_cache: bool,
) -> Result<(), ProviderError> {
    let lookup = match cache {
        Some(cache) if !bypass_cache => {
            let lookup = cache.lookup(&query.canonical);
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(ProviderError::Cancelled),
                result = lookup => match result {
                    Ok(lookup) => lookup,
                    Err(error) => {
                        warn!(%error, "failed to read lyric cache");
                        Default::default()
                    }
                }
            }
        }
        _ => Default::default(),
    };
    let now = Instant::now();
    let available: Vec<NetworkSpec> = specs
        .into_iter()
        .filter(|spec| !lookup.negative_providers.contains(&spec.provider))
        .filter(|spec| breakers.is_none_or(|b| b.can_attempt(spec.provider, now)))
        .collect();

    if let Some(cached) = lookup.answer {
        if !lookup.needs_upgrade || available.is_empty() {
            emit(updates, Ok(Some(cached)), true);
            return Ok(());
        }

        emit(updates, Ok(Some(cached.clone())), false);
        let outcome = race_network(
            available,
            query,
            cancel,
            updates,
            Some(cached.tier),
            breakers,
        )
        .await?;
        record_negatives(cache, &query.canonical, &outcome.negative_providers).await;

        if let Some(answer) = outcome.answer.filter(|answer| answer.tier > cached.tier) {
            store_answer(cache, query, &answer).await;
            emit(updates, Ok(Some(answer)), true);
            return Ok(());
        }

        if let Some(error) = &outcome.error {
            debug!(%error, "background lyric upgrade failed");
        }
        if !outcome.had_error
            && let Some(cache) = cache
            && let Err(error) = cache
                .mark_upgrade_attempted(&query.canonical, cached.provider)
                .await
        {
            warn!(%error, "failed to mark lyric upgrade as attempted");
        }
        emit(updates, Ok(Some(cached)), true);
        return Ok(());
    }

    if available.is_empty() {
        emit(updates, Ok(None), true);
        return Ok(());
    }

    let outcome = race_network(available, query, cancel, updates, None, breakers).await?;
    record_negatives(cache, &query.canonical, &outcome.negative_providers).await;
    if let Some(answer) = &outcome.answer {
        store_answer(cache, query, answer).await;
    }
    let result = outcome.result();
    emit(updates, result, true);
    Ok(())
}

async fn store_answer(cache: Option<&CacheHandle>, query: &TrackQuery, answer: &ProviderAnswer) {
    let Some(cache) = cache else {
        return;
    };
    let label = query_label(query);
    if let Err(error) = cache.store(&query.canonical, label, answer).await {
        warn!(%error, "failed to store lyric cache entry");
    }
}

async fn record_negatives(
    cache: Option<&CacheHandle>,
    key: &crate::core::model::CanonicalTrackKey,
    providers: &[Provider],
) {
    let Some(cache) = cache else {
        return;
    };
    for provider in providers {
        if let Err(error) = cache.record_negative(key, *provider).await {
            warn!(%error, provider = provider.as_str(), "failed to record negative cache entry");
        }
    }
}

pub async fn resolve_final(
    resolver: &ProviderResolver,
    cache: Option<&CacheHandle>,
    query: &TrackQuery,
    cancel: &CancellationToken,
) -> Result<Option<ProviderAnswer>, ProviderError> {
    let (updates, mut receiver) = mpsc::unbounded_channel();
    resolve(resolver, cache, query, cancel, &updates, false).await?;
    drop(updates);

    let mut final_result = None;
    while let Some(update) = receiver.recv().await {
        if update.is_final {
            final_result = Some(update.result);
        }
    }
    match final_result {
        Some(result) => result,
        None => Ok(None),
    }
}

async fn race_network(
    specs: Vec<NetworkSpec>,
    query: &TrackQuery,
    cancel: &CancellationToken,
    updates: &mpsc::UnboundedSender<EngineUpdate>,
    min_stand_in_tier: Option<Tier>,
    breakers: Option<&CircuitBreakers>,
) -> Result<RaceOutcome, ProviderError> {
    let start = Instant::now();
    let grace = start + SYNC_GRACE;
    let deadline = start + LOOKUP_TIMEOUT;
    let mut tasks = JoinSet::new();
    let mut pending = Vec::with_capacity(specs.len());

    for spec in specs {
        pending.push(spec.priority);
        let query = query.clone();
        let cancel = cancel.clone();
        let source = spec.source;
        let provider = spec.provider;
        let priority = spec.priority;
        let limiter = spec.limiter;
        tasks.spawn(async move {
            if let Some(limiter) = limiter
                && let Err(error) = limiter.acquire(&cancel).await
            {
                return (provider, priority, Err(error));
            }
            let result = source.lookup(&query, &cancel).await;
            (provider, priority, result)
        });
    }

    let mut best: Option<Candidate> = None;
    let mut first_error: Option<(usize, ProviderError)> = None;
    let mut negative_providers = Vec::new();
    let mut had_error = false;
    let mut grace_elapsed = false;
    let mut stand_in_sent = false;

    loop {
        if tasks.is_empty() {
            break;
        }

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
                    Ok(result) => result,
                    Err(error) => {
                        warn!(%error, "network provider task failed");
                        continue;
                    }
                };
                if let Some(index) = pending.iter().position(|value| *value == priority) {
                    pending.remove(index);
                }

                if let Some(breakers) = breakers {
                    let now = Instant::now();
                    match &result {
                        Ok(_) => breakers.record_success(provider),
                        Err(error) => breakers.record_error(provider, error, now),
                    }
                }

                let mut improved = false;
                match result {
                    Ok(Some(answer)) => {
                        let candidate = Candidate { priority, answer };
                        if best.as_ref().is_none_or(|current| candidate.is_better_than(current)) {
                            best = Some(candidate);
                            improved = true;
                        }
                    }
                    Ok(None) => {
                        if !negative_providers.contains(&provider) {
                            negative_providers.push(provider);
                        }
                    }
                    Err(ProviderError::Cancelled) => {}
                    Err(error) => {
                        debug!(provider = provider.as_str(), %error, "network provider failed");
                        had_error = true;
                        if first_error
                            .as_ref()
                            .is_none_or(|(existing, _)| priority < *existing)
                        {
                            first_error = Some((priority, error));
                        }
                    }
                }

                if let Some(candidate) = &best
                    && !pending_can_beat(candidate, &pending)
                {
                    tasks.abort_all();
                    break;
                }

                if grace_elapsed
                    && !tasks.is_empty()
                    && let Some(candidate) = &best
                    && (!stand_in_sent || improved)
                    && min_stand_in_tier.is_none_or(|min| candidate.answer.tier > min)
                {
                    emit(updates, Ok(Some(candidate.answer.clone())), false);
                    stand_in_sent = true;
                }
            }
            _ = sleep_until(grace), if !grace_elapsed => {
                grace_elapsed = true;
                if let Some(candidate) = &best
                    && !tasks.is_empty()
                    && min_stand_in_tier.is_none_or(|min| candidate.answer.tier > min)
                {
                    emit(updates, Ok(Some(candidate.answer.clone())), false);
                    stand_in_sent = true;
                }
            }
            _ = sleep_until(deadline) => {
                tasks.abort_all();
                break;
            }
        }
    }

    let answer = best.as_ref().map(|candidate| candidate.answer.clone());
    let error = if answer.is_none() {
        first_error.map(|(_, error)| error)
    } else {
        None
    };
    Ok(RaceOutcome {
        answer,
        negative_providers,
        error,
        had_error,
    })
}

fn pending_can_beat(best: &Candidate, pending: &[usize]) -> bool {
    if best.answer.tier < Tier::Syllable {
        return !pending.is_empty();
    }
    pending.iter().any(|priority| *priority < best.priority)
}

fn emit(
    updates: &mpsc::UnboundedSender<EngineUpdate>,
    result: Result<Option<ProviderAnswer>, ProviderError>,
    is_final: bool,
) {
    let _ = updates.send(EngineUpdate { result, is_final });
}

fn query_label(query: &TrackQuery) -> String {
    if query.artist.is_empty() {
        query.title.clone()
    } else {
        format!("{} - {}", query.artist, query.title)
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use super::*;
    use crate::cache::{CACHE_UPGRADE_INTERVAL_SECS, CacheHandle, now_secs};
    use crate::core::model::{LineRole, LyricLine, SyncGranularity, SyncedTrack};
    use crate::media::TrackMeta;

    struct FakeProvider {
        provider: Provider,
        latency: Duration,
        tier: Option<Tier>,
        id: &'static str,
        started: Option<Arc<tokio::sync::Notify>>,
        calls: Option<Arc<AtomicUsize>>,
        fails: bool,
    }

    impl NetworkProvider for FakeProvider {
        fn lookup<'a>(
            &'a self,
            _query: &'a TrackQuery,
            cancel: &'a CancellationToken,
        ) -> ProviderFuture<'a> {
            Box::pin(async move {
                if let Some(calls) = &self.calls {
                    calls.fetch_add(1, Ordering::SeqCst);
                }
                if let Some(started) = &self.started {
                    started.notify_one();
                }
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => Err(ProviderError::Cancelled),
                    _ = tokio::time::sleep(self.latency) => {
                        if self.fails {
                            return Err(ProviderError::Unavailable("mock failure".to_string()));
                        }
                        Ok(self.tier.map(|tier| {
                            ProviderAnswer::new(self.provider, track(tier, self.id))
                        }))
                    }
                }
            })
        }
    }

    fn track(tier: Tier, id: &str) -> SyncedTrack {
        let granularity = match tier {
            Tier::Syllable => SyncGranularity::Syllable,
            Tier::Word => SyncGranularity::Word,
            Tier::Line => SyncGranularity::Line,
            Tier::Plain => SyncGranularity::Unsynced,
            Tier::Instrumental => SyncGranularity::Line,
        };
        SyncedTrack {
            source_provider: "fake".to_string(),
            source_track_id: Some(id.to_string()),
            parser_version: crate::core::model::PARSER_VERSION,
            is_instrumental: tier == Tier::Instrumental,
            granularity,
            intrinsic_offset_ms: 0,
            user_offset_ms: 0,
            match_confidence: 0.9,
            lines: vec![LyricLine {
                start_ms: 1_000,
                end_ms: 2_000,
                role: LineRole::Primary,
                text: format!("line {id}"),
                words: Vec::new(),
                annotations: Vec::new(),
            }],
        }
    }

    fn query() -> TrackQuery {
        TrackQuery::from_meta(
            &TrackMeta {
                title: "Song".to_string(),
                artist: "Artist".to_string(),
                album: None,
                duration_ms: Some(200_000),
                track_id: None,
                url: None,
            },
            0,
        )
    }

    fn spec(priority: usize, latency_ms: u64, tier: Option<Tier>, id: &'static str) -> NetworkSpec {
        spec_for(
            Provider::Lrclib,
            priority,
            latency_ms,
            tier,
            id,
            None,
            false,
        )
    }

    fn spec_for(
        provider: Provider,
        priority: usize,
        latency_ms: u64,
        tier: Option<Tier>,
        id: &'static str,
        calls: Option<Arc<AtomicUsize>>,
        fails: bool,
    ) -> NetworkSpec {
        NetworkSpec {
            provider,
            priority,
            source: Arc::new(FakeProvider {
                provider,
                latency: Duration::from_millis(latency_ms),
                tier,
                id,
                started: None,
                calls,
                fails,
            }),
            limiter: None,
        }
    }

    fn temp_cache_path(name: &str) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "lyrachi-engine-cache-{name}-{}-{stamp}.db",
            std::process::id()
        ))
    }

    fn cleanup_cache(path: &Path) {
        let _ = fs::remove_file(path);
        for suffix in ["-wal", "-shm"] {
            let mut name = path.file_name().unwrap().to_os_string();
            name.push(suffix);
            let _ = fs::remove_file(path.with_file_name(name));
        }
    }

    async fn run_resolution(
        specs: Vec<NetworkSpec>,
        cache: Option<&CacheHandle>,
        query: &TrackQuery,
    ) -> (Result<(), ProviderError>, Vec<EngineUpdate>) {
        run_resolution_with_breakers(specs, cache, None, query).await
    }

    async fn run_resolution_with_breakers(
        specs: Vec<NetworkSpec>,
        cache: Option<&CacheHandle>,
        breakers: Option<&CircuitBreakers>,
        query: &TrackQuery,
    ) -> (Result<(), ProviderError>, Vec<EngineUpdate>) {
        let cancel = CancellationToken::new();
        let (updates, mut receiver) = mpsc::unbounded_channel();
        let result =
            resolve_with_specs(specs, cache, breakers, query, &cancel, &updates, false).await;
        drop(updates);
        let mut messages = Vec::new();
        while let Some(update) = receiver.recv().await {
            messages.push(update);
        }
        (result, messages)
    }

    fn start_race(
        specs: Vec<NetworkSpec>,
    ) -> (
        tokio::task::JoinHandle<Result<RaceOutcome, ProviderError>>,
        mpsc::UnboundedReceiver<EngineUpdate>,
        CancellationToken,
    ) {
        let cancel = CancellationToken::new();
        let race_cancel = cancel.clone();
        let (updates, receiver) = mpsc::unbounded_channel();
        let query = query();
        let handle = tokio::spawn(async move {
            let result = race_network(specs, &query, &race_cancel, &updates, None, None).await;
            if let Ok(outcome) = &result {
                emit(&updates, outcome.result(), true);
            }
            result
        });
        (handle, receiver, cancel)
    }

    fn final_answer(update: EngineUpdate) -> ProviderAnswer {
        assert!(update.is_final);
        update.result.unwrap().unwrap()
    }

    fn assert_elapsed(started: Instant, expected_ms: u64) {
        let expected = Duration::from_millis(expected_ms);
        let elapsed = started.elapsed();
        assert!(
            elapsed >= expected && elapsed <= expected + Duration::from_millis(50),
            "expected about {expected:?}, got {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn waits_for_a_higher_tier_within_sync_grace() {
        tokio::time::pause();
        let (handle, mut receiver, _cancel) = start_race(vec![
            spec(0, 300, Some(Tier::Line), "line"),
            spec(1, 1_200, Some(Tier::Syllable), "syllable"),
        ]);

        let started = Instant::now();
        let answer = final_answer(receiver.recv().await.unwrap());
        assert_elapsed(started, 1_200);
        assert_eq!(answer.tier, Tier::Syllable);
        assert_eq!(answer.track.source_track_id.as_deref(), Some("syllable"));
        assert!(receiver.try_recv().is_err());
        handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn emits_a_stand_in_at_sync_grace_then_upgrades_without_waiting() {
        tokio::time::pause();
        let (handle, mut receiver, _cancel) = start_race(vec![
            spec(0, 500, Some(Tier::Line), "line"),
            spec(1, 3_500, Some(Tier::Syllable), "syllable"),
        ]);

        let started = Instant::now();
        let stand_in = receiver.recv().await.unwrap();
        assert_elapsed(started, 2_000);
        assert!(!stand_in.is_final);
        let stand_in = stand_in.result.unwrap().unwrap();
        assert_eq!(stand_in.tier, Tier::Line);
        assert_eq!(stand_in.track.source_track_id.as_deref(), Some("line"));
        assert!(receiver.try_recv().is_err());

        let final_answer = final_answer(receiver.recv().await.unwrap());
        assert_elapsed(started, 3_500);
        assert_eq!(final_answer.tier, Tier::Syllable);
        assert_eq!(
            final_answer.track.source_track_id.as_deref(),
            Some("syllable")
        );
        assert!(receiver.try_recv().is_err());
        handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn earlier_provider_order_wins_a_same_tier_race() {
        tokio::time::pause();
        let (handle, mut receiver, _cancel) = start_race(vec![
            spec(1, 300, Some(Tier::Line), "later"),
            spec(0, 600, Some(Tier::Line), "earlier"),
        ]);

        let started = Instant::now();
        let answer = final_answer(receiver.recv().await.unwrap());
        assert_elapsed(started, 600);
        assert_eq!(answer.track.source_track_id.as_deref(), Some("earlier"));
        assert!(receiver.try_recv().is_err());
        handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn cancellation_aborts_the_race_without_a_final_update() {
        tokio::time::pause();
        let started = Arc::new(tokio::sync::Notify::new());
        let specs = vec![
            NetworkSpec {
                provider: Provider::Lrclib,
                priority: 0,
                source: Arc::new(FakeProvider {
                    provider: Provider::Lrclib,
                    latency: Duration::from_millis(1_000),
                    tier: Some(Tier::Line),
                    id: "line",
                    started: Some(started.clone()),
                    calls: None,
                    fails: false,
                }),
                limiter: None,
            },
            spec(1, 2_000, Some(Tier::Syllable), "syllable"),
        ];
        let (handle, mut receiver, cancel) = start_race(specs);

        started.notified().await;
        cancel.cancel();
        assert!(matches!(
            handle.await.unwrap(),
            Err(ProviderError::Cancelled)
        ));
        assert!(receiver.try_recv().is_err());
    }

    #[tokio::test]
    async fn lookup_timeout_finishes_without_an_answer() {
        tokio::time::pause();
        let (handle, mut receiver, _cancel) =
            start_race(vec![spec(0, 20_000, Some(Tier::Syllable), "slow")]);

        let started = Instant::now();
        let update = receiver.recv().await.unwrap();
        assert_elapsed(started, 12_000);
        assert!(update.is_final);
        assert!(matches!(update.result, Ok(None)));
        handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn negative_cache_skips_only_the_provider_that_returned_none() {
        tokio::time::pause();
        let path = temp_cache_path("partial-negative");
        let cache = CacheHandle::open(&path).unwrap();
        let now = now_secs();
        let query = query();
        let specs = vec![
            spec_for(Provider::Lrclib, 0, 100, None, "none", None, false),
            spec_for(
                Provider::Local,
                1,
                100,
                Some(Tier::Word),
                "remaining",
                None,
                false,
            ),
        ];

        let (result, updates) = run_resolution(specs, Some(&cache), &query).await;
        assert!(result.is_ok());
        assert_eq!(updates.len(), 1);
        let answer = final_answer(updates.into_iter().next().unwrap());
        assert_eq!(answer.provider, Provider::Local);
        assert_eq!(answer.tier, Tier::Word);

        let lookup = cache.lookup_at(&query.canonical, now).await.unwrap();
        assert!(lookup.negative_providers.contains(&Provider::Lrclib));
        assert!(!lookup.negative_providers.contains(&Provider::Local));
        assert_eq!(lookup.answer.unwrap().provider, Provider::Local);

        cache.shutdown().await.unwrap();
        cleanup_cache(&path);
    }

    #[tokio::test]
    async fn all_negative_providers_skip_the_network() {
        tokio::time::pause();
        let path = temp_cache_path("all-negative");
        let cache = CacheHandle::open(&path).unwrap();
        let now = now_secs();
        let query = query();
        cache
            .record_negative_at(&query.canonical, Provider::Lrclib, now)
            .await
            .unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let specs = vec![spec_for(
            Provider::Lrclib,
            0,
            100,
            Some(Tier::Syllable),
            "network",
            Some(calls.clone()),
            false,
        )];

        let (result, updates) = run_resolution(specs, Some(&cache), &query).await;
        assert!(result.is_ok());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(updates.len(), 1);
        let update = updates.into_iter().next().unwrap();
        assert!(update.is_final);
        assert!(matches!(update.result, Ok(None)));

        cache.shutdown().await.unwrap();
        cleanup_cache(&path);
    }

    #[tokio::test]
    async fn provider_errors_do_not_create_negative_cache_entries() {
        tokio::time::pause();
        let path = temp_cache_path("error-negative");
        let cache = CacheHandle::open(&path).unwrap();
        let now = now_secs();
        let query = query();
        let specs = vec![spec_for(
            Provider::Lrclib,
            0,
            100,
            Some(Tier::Line),
            "error",
            None,
            true,
        )];

        let (result, updates) = run_resolution(specs, Some(&cache), &query).await;
        assert!(result.is_ok());
        assert_eq!(updates.len(), 1);
        let update = updates.into_iter().next().unwrap();
        assert!(update.is_final);
        assert!(matches!(update.result, Err(ProviderError::Unavailable(_))));
        assert!(
            cache
                .lookup_at(&query.canonical, now)
                .await
                .unwrap()
                .negative_providers
                .is_empty()
        );

        cache.shutdown().await.unwrap();
        cleanup_cache(&path);
    }

    #[tokio::test]
    async fn background_upgrade_replaces_a_stale_line_entry() {
        tokio::time::pause();
        let path = temp_cache_path("upgrade");
        let cache = CacheHandle::open(&path).unwrap();
        let now = now_secs();
        let query = query();
        let old = now.saturating_sub(CACHE_UPGRADE_INTERVAL_SECS + 1);
        let line = ProviderAnswer::new(Provider::Lrclib, track(Tier::Line, "cached-line"));
        cache
            .store_at(&query.canonical, "Artist - Song".to_string(), &line, old)
            .await
            .unwrap();
        let specs = vec![spec_for(
            Provider::Lrclib,
            0,
            100,
            Some(Tier::Syllable),
            "upgraded",
            None,
            false,
        )];

        let (result, updates) = run_resolution(specs, Some(&cache), &query).await;
        assert!(result.is_ok());
        assert_eq!(updates.len(), 2);
        let mut updates = updates.into_iter();
        let stand_in = updates.next().unwrap();
        assert!(!stand_in.is_final);
        assert_eq!(
            stand_in
                .result
                .unwrap()
                .unwrap()
                .track
                .source_track_id
                .as_deref(),
            Some("cached-line")
        );
        let final_answer = final_answer(updates.next().unwrap());
        assert_eq!(final_answer.tier, Tier::Syllable);
        assert_eq!(
            final_answer.track.source_track_id.as_deref(),
            Some("upgraded")
        );
        assert!(updates.next().is_none());

        let lookup = cache.lookup_at(&query.canonical, now).await.unwrap();
        assert_eq!(lookup.answer.unwrap().tier, Tier::Syllable);
        assert!(!lookup.needs_upgrade);

        cache.shutdown().await.unwrap();
        cleanup_cache(&path);
    }

    #[tokio::test]
    async fn failed_background_upgrade_keeps_line_and_clears_upgrade_marker() {
        tokio::time::pause();
        let path = temp_cache_path("upgrade-failed");
        let cache = CacheHandle::open(&path).unwrap();
        let now = now_secs();
        let query = query();
        let old = now.saturating_sub(CACHE_UPGRADE_INTERVAL_SECS + 1);
        let line = ProviderAnswer::new(Provider::Lrclib, track(Tier::Line, "cached-line"));
        cache
            .store_at(&query.canonical, "Artist - Song".to_string(), &line, old)
            .await
            .unwrap();
        let specs = vec![spec_for(
            Provider::Lrclib,
            0,
            100,
            None,
            "none",
            None,
            false,
        )];

        let (result, updates) = run_resolution(specs, Some(&cache), &query).await;
        assert!(result.is_ok());
        assert_eq!(updates.len(), 2);
        let mut updates = updates.into_iter();
        let stand_in = updates.next().unwrap();
        assert!(!stand_in.is_final);
        assert_eq!(
            stand_in
                .result
                .unwrap()
                .unwrap()
                .track
                .source_track_id
                .as_deref(),
            Some("cached-line")
        );
        let final_answer = final_answer(updates.next().unwrap());
        assert_eq!(final_answer.tier, Tier::Line);
        assert_eq!(
            final_answer.track.source_track_id.as_deref(),
            Some("cached-line")
        );

        let lookup = cache.lookup_at(&query.canonical, now).await.unwrap();
        assert_eq!(lookup.answer.unwrap().tier, Tier::Line);
        assert!(!lookup.needs_upgrade);

        cache.shutdown().await.unwrap();
        cleanup_cache(&path);
    }

    #[tokio::test]
    async fn background_upgrade_does_not_emit_lower_or_same_tier_stand_in() {
        tokio::time::pause();
        let path = temp_cache_path("upgrade-stand-in");
        let cache = CacheHandle::open(&path).unwrap();
        let now = now_secs();
        let query = query();
        let old = now.saturating_sub(CACHE_UPGRADE_INTERVAL_SECS + 1);
        let line = ProviderAnswer::new(Provider::Lrclib, track(Tier::Line, "cached-line"));
        cache
            .store_at(&query.canonical, "Artist - Song".to_string(), &line, old)
            .await
            .unwrap();
        let specs = vec![
            spec_for(
                Provider::Lrclib,
                0,
                500,
                Some(Tier::Plain),
                "worse-plain",
                None,
                false,
            ),
            spec_for(
                Provider::Local,
                1,
                3_500,
                Some(Tier::Syllable),
                "better-syllable",
                None,
                false,
            ),
        ];

        let (result, updates) = run_resolution(specs, Some(&cache), &query).await;
        assert!(result.is_ok());
        assert_eq!(updates.len(), 2);
        let mut updates = updates.into_iter();
        let stand_in = updates.next().unwrap();
        assert!(!stand_in.is_final);
        assert_eq!(
            stand_in
                .result
                .unwrap()
                .unwrap()
                .track
                .source_track_id
                .as_deref(),
            Some("cached-line")
        );
        let final_answer = final_answer(updates.next().unwrap());
        assert_eq!(final_answer.tier, Tier::Syllable);
        assert_eq!(
            final_answer.track.source_track_id.as_deref(),
            Some("better-syllable")
        );

        cache.shutdown().await.unwrap();
        cleanup_cache(&path);
    }

    #[test]
    fn circuit_breaker_opens_on_third_failure_cooldowns_and_recovers_on_probe_success() {
        let base = Instant::now();
        let mut cb = CircuitBreaker::new();
        assert_eq!(cb.state_at(base), CircuitState::Closed);
        assert!(cb.can_attempt(base));

        // 1st network failure
        cb.record_error(&ProviderError::Unavailable("down".to_string()), base);
        assert_eq!(cb.consecutive_failures(), 1);
        assert_eq!(cb.state_at(base), CircuitState::Closed);
        assert!(cb.can_attempt(base));

        // Non-network failure (Invalid) does not increment failures
        cb.record_error(&ProviderError::Invalid("bad json".to_string()), base);
        assert_eq!(cb.consecutive_failures(), 1);
        assert_eq!(cb.state_at(base), CircuitState::Closed);

        // Non-network failure (Cancelled) does not increment failures
        cb.record_error(&ProviderError::Cancelled, base);
        assert_eq!(cb.consecutive_failures(), 1);
        assert_eq!(cb.state_at(base), CircuitState::Closed);

        // 2nd network failure
        cb.record_error(&ProviderError::Timeout, base);
        assert_eq!(cb.consecutive_failures(), 2);
        assert_eq!(cb.state_at(base), CircuitState::Closed);
        assert!(cb.can_attempt(base));

        // 3rd network failure -> opens breaker
        cb.record_error(
            &ProviderError::RateLimited {
                retry_after_ms: None,
            },
            base,
        );
        assert_eq!(cb.consecutive_failures(), 3);
        assert_eq!(cb.state_at(base), CircuitState::Open);
        assert!(!cb.can_attempt(base));

        // Within 5 minutes cooldown, provider is skipped
        let four_mins = base + Duration::from_secs(4 * 60);
        assert_eq!(cb.state_at(four_mins), CircuitState::Open);
        assert!(!cb.can_attempt(four_mins));

        let four_fifty_nine = base + Duration::from_secs(5 * 60 - 1);
        assert_eq!(cb.state_at(four_fifty_nine), CircuitState::Open);
        assert!(!cb.can_attempt(four_fifty_nine));

        // At exactly 5 minutes, breaker becomes half-open and allows 1 probe
        let five_mins = base + Duration::from_secs(5 * 60);
        assert_eq!(cb.state_at(five_mins), CircuitState::HalfOpen);
        assert!(cb.can_attempt(five_mins));

        // Concurrent request while probe is in flight is rejected
        assert!(!cb.can_attempt(five_mins));

        // Probe succeeds -> closes breaker and resets failures
        cb.record_success();
        assert_eq!(cb.state_at(five_mins), CircuitState::Closed);
        assert_eq!(cb.consecutive_failures(), 0);
        assert!(cb.can_attempt(five_mins));
    }

    #[test]
    fn circuit_breaker_probe_failure_reopens_for_five_minutes() {
        let base = Instant::now();
        let mut cb = CircuitBreaker::new();

        // 3 consecutive failures
        cb.record_error(&ProviderError::Unavailable("1".into()), base);
        cb.record_error(&ProviderError::Timeout, base);
        cb.record_error(&ProviderError::Unavailable("3".into()), base);
        assert_eq!(cb.state_at(base), CircuitState::Open);

        // 5 minutes later: probe allowed
        let five_mins = base + Duration::from_secs(5 * 60);
        assert!(cb.can_attempt(five_mins));

        // Probe fails -> opens breaker again for 5 minutes from probe time
        cb.record_error(&ProviderError::Unavailable("still down".into()), five_mins);
        assert_eq!(cb.state_at(five_mins), CircuitState::Open);

        // 4 minutes after probe failure: still open
        let nine_mins = base + Duration::from_secs(9 * 60);
        assert!(!cb.can_attempt(nine_mins));

        // 5 minutes after probe failure (t = 10m): half-open again
        let ten_mins = base + Duration::from_secs(10 * 60);
        assert_eq!(cb.state_at(ten_mins), CircuitState::HalfOpen);
        assert!(cb.can_attempt(ten_mins));
    }

    #[test]
    fn circuit_breaker_rate_limited_retry_after_longer_than_five_minutes() {
        let base = Instant::now();
        let mut cb = CircuitBreaker::new();

        cb.record_error(&ProviderError::Unavailable("1".into()), base);
        cb.record_error(&ProviderError::Timeout, base);
        // 3rd failure has 10 minutes retry_after (600_000 ms)
        cb.record_error(
            &ProviderError::RateLimited {
                retry_after_ms: Some(600_000),
            },
            base,
        );
        assert_eq!(cb.state_at(base), CircuitState::Open);

        // At 5 minutes: still open because retry_after was 10 minutes
        let five_mins = base + Duration::from_secs(5 * 60);
        assert!(!cb.can_attempt(five_mins));

        let nine_mins = base + Duration::from_secs(9 * 60);
        assert!(!cb.can_attempt(nine_mins));

        // At 10 minutes: half-open
        let ten_mins = base + Duration::from_secs(10 * 60);
        assert_eq!(cb.state_at(ten_mins), CircuitState::HalfOpen);
        assert!(cb.can_attempt(ten_mins));
    }

    #[tokio::test]
    async fn token_bucket_rate_limiter_spaces_three_requests_by_at_least_one_second() {
        tokio::time::pause();
        let limiter = Arc::new(TokenBucketRateLimiter::tier2_default());
        let cancel = CancellationToken::new();

        let started = Instant::now();
        limiter.acquire(&cancel).await.unwrap();
        assert_elapsed(started, 0);

        limiter.acquire(&cancel).await.unwrap();
        assert_elapsed(started, 1_000);

        limiter.acquire(&cancel).await.unwrap();
        assert_elapsed(started, 2_000);
    }

    #[tokio::test]
    async fn token_bucket_rate_limiter_cancels_immediately_when_waiting() {
        tokio::time::pause();
        let limiter = Arc::new(TokenBucketRateLimiter::tier2_default());

        // First acquire consumes the 1 token immediately
        limiter.acquire(&CancellationToken::new()).await.unwrap();

        // Second acquire is queued
        let cancel = CancellationToken::new();
        let cancel_task = cancel.clone();
        let limiter_task = limiter.clone();
        let handle = tokio::spawn(async move { limiter_task.acquire(&cancel_task).await });

        // Cancel while waiting
        cancel.cancel();
        let result = handle.await.unwrap();
        assert!(matches!(result, Err(ProviderError::Cancelled)));
    }

    #[tokio::test]
    async fn resolution_skips_provider_when_circuit_breaker_is_open() {
        tokio::time::pause();
        let breakers = CircuitBreakers::default();
        let now = Instant::now();

        // Trip Provider::Lrclib with 3 failures
        breakers.record_error(
            Provider::Lrclib,
            &ProviderError::Unavailable("down".into()),
            now,
        );
        breakers.record_error(Provider::Lrclib, &ProviderError::Timeout, now);
        breakers.record_error(
            Provider::Lrclib,
            &ProviderError::Unavailable("down".into()),
            now,
        );
        assert_eq!(breakers.status(Provider::Lrclib, now), "open");

        let calls = Arc::new(AtomicUsize::new(0));
        let query = query();
        let specs = vec![
            spec_for(
                Provider::Lrclib,
                0,
                100,
                Some(Tier::Syllable),
                "lrclib",
                Some(calls.clone()),
                false,
            ),
            spec_for(
                Provider::Local,
                1,
                100,
                Some(Tier::Line),
                "local-fallback",
                None,
                false,
            ),
        ];

        let (result, updates) =
            run_resolution_with_breakers(specs, None, Some(&breakers), &query).await;
        assert!(result.is_ok());
        // Lrclib was skipped (0 calls made to it)
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(updates.len(), 1);
        let answer = final_answer(updates.into_iter().next().unwrap());
        assert_eq!(answer.provider, Provider::Local);
        assert_eq!(answer.tier, Tier::Line);
    }

    fn temp_path(name: &str) -> std::path::PathBuf {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "lyrachi-engine-test-{name}-{}-{stamp}",
            std::process::id()
        ))
    }

    fn cleanup(path: &std::path::Path) {
        let _ = std::fs::remove_dir_all(path);
    }

    #[tokio::test]
    async fn bypass_cache_ignores_cached_answer_and_runs_race() {
        let root = temp_path("bypass");
        std::fs::create_dir_all(&root).unwrap();
        let db = root.join("cache.db");
        let cache = CacheHandle::open(&db).unwrap();
        let query = query();

        cache
            .store(
                &query.canonical,
                "Coldplay - Yellow",
                &ProviderAnswer::new(Provider::Lrclib, track(Tier::Line, "cached-line")),
            )
            .await
            .unwrap();

        let specs = vec![spec_for(
            Provider::LyricsPlus,
            0,
            10,
            Some(Tier::Syllable),
            "fresh-syllable",
            None,
            false,
        )];

        let cancel = CancellationToken::new();
        let (updates, mut receiver) = mpsc::unbounded_channel();
        let result =
            resolve_with_specs(specs, Some(&cache), None, &query, &cancel, &updates, true).await;

        assert!(result.is_ok());
        drop(updates);
        let mut messages = Vec::new();
        while let Some(update) = receiver.recv().await {
            messages.push(update);
        }

        assert_eq!(messages.len(), 1);
        let final_ans = messages[0].result.as_ref().unwrap().as_ref().unwrap();
        assert_eq!(
            final_ans.track.source_track_id.as_deref(),
            Some("fresh-syllable")
        );

        cache.shutdown().await.unwrap();
        cleanup(&root);
    }
}

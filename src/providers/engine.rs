use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio::time::{Instant, sleep_until};
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use super::lrclib::LrclibClient;
use super::{Provider, ProviderAnswer, ProviderError, ProviderResolver, Tier, TrackQuery};
use crate::cache::CacheHandle;

pub const SYNC_GRACE: Duration = Duration::from_secs(2);
pub const LOOKUP_TIMEOUT: Duration = Duration::from_secs(12);

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

#[derive(Debug)]
pub struct EngineUpdate {
    pub result: Result<Option<ProviderAnswer>, ProviderError>,
    pub is_final: bool,
}

pub(crate) struct NetworkSpec {
    pub provider: Provider,
    pub priority: usize,
    pub source: Arc<dyn NetworkProvider>,
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
    resolve_with_specs(specs, cache, query, cancel, updates).await
}

async fn resolve_with_specs(
    specs: Vec<NetworkSpec>,
    cache: Option<&CacheHandle>,
    query: &TrackQuery,
    cancel: &CancellationToken,
    updates: &mpsc::UnboundedSender<EngineUpdate>,
) -> Result<(), ProviderError> {
    let lookup = match cache {
        Some(cache) => {
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
        None => Default::default(),
    };
    let available: Vec<NetworkSpec> = specs
        .into_iter()
        .filter(|spec| !lookup.negative_providers.contains(&spec.provider))
        .collect();

    if let Some(cached) = lookup.answer {
        if !lookup.needs_upgrade || available.is_empty() {
            emit(updates, Ok(Some(cached)), true);
            return Ok(());
        }

        emit(updates, Ok(Some(cached.clone())), false);
        let outcome = race_network(available, query, cancel, updates, Some(cached.tier)).await?;
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

    let outcome = race_network(available, query, cancel, updates, None).await?;
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
    resolve(resolver, cache, query, cancel, &updates).await?;
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
        tasks.spawn(async move {
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
        let cancel = CancellationToken::new();
        let (updates, mut receiver) = mpsc::unbounded_channel();
        let result = resolve_with_specs(specs, cache, query, &cancel, &updates).await;
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
            let result = race_network(specs, &query, &race_cancel, &updates, None).await;
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
}

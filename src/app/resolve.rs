use std::collections::{HashMap, VecDeque};

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::cache::CacheHandle;
use crate::core::model::CanonicalTrackKey;
use crate::media::TrackMeta;
use crate::providers::engine::{self, EngineUpdate};
use crate::providers::{ProviderAnswer, ProviderError, ProviderResolver, TrackQuery};

pub const L1_CACHE_CAPACITY: usize = 64;

#[derive(Debug, Clone)]
pub struct L1Cache {
    capacity: usize,
    entries: VecDeque<(CanonicalTrackKey, ProviderAnswer)>,
}

impl Default for L1Cache {
    fn default() -> Self {
        Self::new()
    }
}

impl L1Cache {
    pub fn new() -> Self {
        Self::with_capacity(L1_CACHE_CAPACITY)
    }

    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            capacity,
            entries: VecDeque::with_capacity(capacity),
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn get(&mut self, key: &CanonicalTrackKey) -> Option<ProviderAnswer> {
        let index = self
            .entries
            .iter()
            .position(|(entry_key, _)| entry_key == key)?;
        let (entry_key, answer) = self.entries.remove(index)?;
        self.entries.push_back((entry_key, answer.clone()));
        Some(answer)
    }

    pub fn peek(&self, key: &CanonicalTrackKey) -> Option<&ProviderAnswer> {
        self.entries
            .iter()
            .find(|(entry_key, _)| entry_key == key)
            .map(|(_, answer)| answer)
    }

    pub fn insert(&mut self, key: CanonicalTrackKey, answer: ProviderAnswer) {
        if self.capacity == 0 {
            return;
        }
        if let Some(index) = self
            .entries
            .iter()
            .position(|(entry_key, _)| entry_key == &key)
        {
            self.entries.remove(index);
        } else if self.entries.len() >= self.capacity {
            self.entries.pop_front();
        }
        self.entries.push_back((key, answer));
    }

    pub fn remove(&mut self, key: &CanonicalTrackKey) -> Option<ProviderAnswer> {
        let index = self
            .entries
            .iter()
            .position(|(entry_key, _)| entry_key == key)?;
        self.entries.remove(index).map(|(_, answer)| answer)
    }
}

pub async fn resolve_with_cache(
    resolver: &ProviderResolver,
    cache: Option<&CacheHandle>,
    query: &TrackQuery,
    cancel: &CancellationToken,
) -> Result<Option<ProviderAnswer>, ProviderError> {
    engine::resolve_final(resolver, cache, query, cancel).await
}

pub async fn resolve_with_updates(
    resolver: &ProviderResolver,
    cache: Option<&CacheHandle>,
    query: &TrackQuery,
    cancel: &CancellationToken,
    updates: &mpsc::UnboundedSender<EngineUpdate>,
    bypass_cache: bool,
) -> Result<(), ProviderError> {
    engine::resolve(resolver, cache, query, cancel, updates, bypass_cache).await
}

#[derive(Debug)]
pub enum TrackPlan {
    Cached {
        epoch: u64,
        user_offset_ms: i64,
        answer: ProviderAnswer,
    },
    Fetch {
        epoch: u64,
        user_offset_ms: i64,
        query: TrackQuery,
        cancel: CancellationToken,
        bypass_cache: bool,
    },
    Unusable {
        epoch: u64,
        user_offset_ms: i64,
    },
}

#[derive(Debug, Default)]
pub struct ResolveCoordinator {
    epoch: u64,
    active_token: Option<CancellationToken>,
    active_key: Option<CanonicalTrackKey>,
    active_query: Option<TrackQuery>,
    cache: L1Cache,
    user_offsets: HashMap<CanonicalTrackKey, i64>,
}

impl ResolveCoordinator {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_cache_capacity(capacity: usize) -> Self {
        Self {
            cache: L1Cache::with_capacity(capacity),
            ..Self::default()
        }
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    pub fn active_key(&self) -> Option<&CanonicalTrackKey> {
        self.active_key.as_ref()
    }

    pub fn active_query(&self) -> Option<&TrackQuery> {
        self.active_query.as_ref()
    }

    pub fn cache(&self) -> &L1Cache {
        &self.cache
    }

    pub fn cache_mut(&mut self) -> &mut L1Cache {
        &mut self.cache
    }

    pub fn cancel_active(&mut self) -> u64 {
        if let Some(token) = self.active_token.take() {
            token.cancel();
        }
        self.active_key = None;
        self.active_query = None;
        self.epoch = self.epoch.wrapping_add(1);
        self.epoch
    }

    pub fn prepare_track(&mut self, meta: &TrackMeta) -> TrackPlan {
        if let Some(token) = self.active_token.take() {
            token.cancel();
        }
        self.epoch = self.epoch.wrapping_add(1);
        let epoch = self.epoch;

        let mut query = TrackQuery::from_meta(meta, 0);
        if !query.is_usable() {
            self.active_key = None;
            self.active_query = None;
            return TrackPlan::Unusable {
                epoch,
                user_offset_ms: 0,
            };
        }

        let user_offset_ms = self
            .user_offsets
            .get(&query.canonical)
            .copied()
            .unwrap_or(0);
        query.user_offset_ms = user_offset_ms;
        self.active_key = Some(query.canonical.clone());
        self.active_query = Some(query.clone());

        if let Some(mut answer) = self.cache.get(&query.canonical) {
            answer.track.user_offset_ms = user_offset_ms;
            return TrackPlan::Cached {
                epoch,
                user_offset_ms,
                answer,
            };
        }

        let token = CancellationToken::new();
        self.active_token = Some(token.clone());

        TrackPlan::Fetch {
            epoch,
            user_offset_ms,
            query,
            cancel: token,
            bypass_cache: false,
        }
    }

    pub fn force_refetch_current_track(&mut self) -> Option<TrackPlan> {
        let mut query = self.active_query.clone()?;
        let key = query.canonical.clone();

        if let Some(token) = self.active_token.take() {
            token.cancel();
        }
        self.epoch = self.epoch.wrapping_add(1);
        let epoch = self.epoch;

        self.cache.remove(&key);
        let user_offset_ms = self.user_offsets.get(&key).copied().unwrap_or(0);
        query.user_offset_ms = user_offset_ms;
        self.active_key = Some(key);

        let token = CancellationToken::new();
        self.active_token = Some(token.clone());
        Some(TrackPlan::Fetch {
            epoch,
            user_offset_ms,
            query,
            cancel: token,
            bypass_cache: true,
        })
    }

    pub fn apply_manual_candidate(
        &mut self,
        expected_key: &CanonicalTrackKey,
        mut answer: ProviderAnswer,
    ) -> Option<(u64, ProviderAnswer)> {
        if self.active_key.as_ref() != Some(expected_key) {
            return None;
        }
        if let Some(token) = self.active_token.take() {
            token.cancel();
        }
        self.epoch = self.epoch.wrapping_add(1);
        let epoch = self.epoch;

        let user_offset_ms = self.user_offsets.get(expected_key).copied().unwrap_or(0);
        answer.track.user_offset_ms = user_offset_ms;
        self.cache.insert(expected_key.clone(), answer.clone());

        Some((epoch, answer))
    }

    pub fn complete_epoch(
        &mut self,
        resolved_epoch: u64,
        answer: Option<ProviderAnswer>,
        is_final: bool,
    ) -> Option<Option<ProviderAnswer>> {
        if resolved_epoch != self.epoch {
            return None;
        }
        if is_final {
            self.active_token = None;
        }

        let Some(mut answer) = answer else {
            return is_final.then_some(None);
        };

        if let Some(key) = self.active_key.clone() {
            let user_offset_ms = self.user_offsets.get(&key).copied().unwrap_or(0);
            answer.track.user_offset_ms = user_offset_ms;
            if is_final {
                self.cache.insert(key, answer.clone());
            }
        }

        Some(Some(answer))
    }

    pub fn seed_user_offset(&mut self, key: CanonicalTrackKey, user_offset_ms: i64) {
        if user_offset_ms == 0 {
            self.user_offsets.remove(&key);
        } else {
            self.user_offsets.insert(key, user_offset_ms);
        }
    }

    pub fn record_user_offset(&mut self, user_offset_ms: i64) {
        let Some(key) = self.active_key.clone() else {
            return;
        };
        if user_offset_ms == 0 {
            self.user_offsets.remove(&key);
        } else {
            self.user_offsets.insert(key.clone(), user_offset_ms);
        }
        if let Some(mut answer) = self.cache.get(&key) {
            answer.track.user_offset_ms = user_offset_ms;
            self.cache.insert(key, answer);
        }
    }

    pub fn user_offset_for(&self, key: &CanonicalTrackKey) -> i64 {
        self.user_offsets.get(key).copied().unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::model::{LineRole, LyricLine, PARSER_VERSION, SyncGranularity, SyncedTrack};

    fn sample_key(index: usize) -> CanonicalTrackKey {
        CanonicalTrackKey {
            norm_artist: format!("artist-{index}"),
            norm_title: format!("title-{index}"),
            norm_album: None,
            duration_bucket_sec: Some(200),
        }
    }

    fn sample_track(id: &str) -> SyncedTrack {
        SyncedTrack {
            source_provider: "lrclib".to_string(),
            source_track_id: Some(id.to_string()),
            parser_version: PARSER_VERSION,
            is_instrumental: false,
            granularity: SyncGranularity::Line,
            intrinsic_offset_ms: 0,
            user_offset_ms: 0,
            match_confidence: 0.95,
            lines: vec![LyricLine {
                start_ms: 1_000,
                end_ms: 2_000,
                role: LineRole::Primary,
                text: format!("lyric {id}"),
                words: Vec::new(),
                annotations: Vec::new(),
            }],
        }
    }

    fn sample_answer(id: &str) -> ProviderAnswer {
        ProviderAnswer::new(crate::providers::Provider::Lrclib, sample_track(id))
    }

    fn sample_meta(title: &str, artist: &str) -> TrackMeta {
        TrackMeta {
            title: title.to_string(),
            artist: artist.to_string(),
            album: Some("Album".to_string()),
            duration_ms: Some(200_000),
            track_id: None,
            url: None,
        }
    }

    #[test]
    fn l1_cache_evicts_least_recently_used_at_64_entries() {
        let mut cache = L1Cache::new();
        for index in 0..L1_CACHE_CAPACITY {
            cache.insert(sample_key(index), sample_answer(&index.to_string()));
        }
        assert_eq!(cache.len(), 64);

        // Touch entry 0 so entry 1 becomes least recently used.
        assert!(cache.get(&sample_key(0)).is_some());

        cache.insert(sample_key(64), sample_answer("64"));
        assert_eq!(cache.len(), 64);
        assert!(cache.get(&sample_key(0)).is_some());
        assert!(cache.get(&sample_key(1)).is_none());
        assert!(cache.get(&sample_key(64)).is_some());
    }

    #[test]
    fn coordinator_cancels_previous_token_and_ignores_stale_epochs() {
        let mut coordinator = ResolveCoordinator::new();
        let meta_a = sample_meta("Song A", "Artist");
        let meta_b = sample_meta("Song B", "Artist");

        let (epoch_a, token_a) = match coordinator.prepare_track(&meta_a) {
            TrackPlan::Fetch { epoch, cancel, .. } => (epoch, cancel),
            other => panic!("expected fetch plan, got {other:?}"),
        };
        assert!(!token_a.is_cancelled());

        let (epoch_b, token_b) = match coordinator.prepare_track(&meta_b) {
            TrackPlan::Fetch { epoch, cancel, .. } => (epoch, cancel),
            other => panic!("expected fetch plan, got {other:?}"),
        };
        assert!(token_a.is_cancelled());
        assert!(!token_b.is_cancelled());
        assert_eq!(epoch_b, epoch_a + 1);

        assert!(
            coordinator
                .complete_epoch(epoch_a, Some(sample_answer("A")), true)
                .is_none()
        );
        let resolved_b = coordinator
            .complete_epoch(epoch_b, Some(sample_answer("B")), true)
            .expect("current epoch should resolve")
            .expect("track should be present");
        assert_eq!(resolved_b.track.source_track_id.as_deref(), Some("B"));
    }

    #[test]
    fn force_refetch_cancels_epoch_clears_l1_and_preserves_user_offset() {
        let mut coordinator = ResolveCoordinator::new();
        let meta = sample_meta("Song A", "Artist");

        let (epoch_a, token_a) = match coordinator.prepare_track(&meta) {
            TrackPlan::Fetch { epoch, cancel, .. } => (epoch, cancel),
            other => panic!("expected fetch plan, got {other:?}"),
        };
        coordinator.record_user_offset(-250);

        let plan = coordinator
            .force_refetch_current_track()
            .expect("active track should refetch");
        assert!(token_a.is_cancelled());

        let (epoch_b, query, user_offset_ms, token_b, bypass_cache) = match plan {
            TrackPlan::Fetch {
                epoch,
                query,
                user_offset_ms,
                cancel,
                bypass_cache,
            } => (epoch, query, user_offset_ms, cancel, bypass_cache),
            other => panic!("expected refetch plan, got {other:?}"),
        };
        assert_eq!(epoch_b, epoch_a + 1);
        assert_eq!(user_offset_ms, -250);
        assert!(!token_b.is_cancelled());
        assert!(bypass_cache);
        assert!(coordinator.cache().is_empty());

        assert!(
            coordinator
                .complete_epoch(epoch_a, Some(sample_answer("old")), true)
                .is_none()
        );
        let resolved = coordinator
            .complete_epoch(epoch_b, Some(sample_answer("new")), true)
            .expect("refetch epoch should resolve")
            .expect("track should be present");
        assert_eq!(resolved.track.source_track_id.as_deref(), Some("new"));
        assert_eq!(resolved.track.user_offset_ms, -250);
        assert_eq!(coordinator.cache().len(), 1);

        let plan = coordinator
            .force_refetch_current_track()
            .expect("settled track should refetch again");
        assert!(coordinator.cache().is_empty());
        match plan {
            TrackPlan::Fetch {
                query: next_query,
                user_offset_ms,
                ..
            } => {
                assert_eq!(next_query.canonical, query.canonical);
                assert_eq!(user_offset_ms, -250);
            }
            other => panic!("expected refetch plan, got {other:?}"),
        }
        assert!(
            ResolveCoordinator::new()
                .force_refetch_current_track()
                .is_none()
        );
    }

    #[test]
    fn coordinator_serves_l1_cache_and_preserves_per_track_user_offset() {
        let mut coordinator = ResolveCoordinator::new();
        let meta_a = sample_meta("Song A", "Artist");
        let meta_b = sample_meta("Song B", "Artist");

        let epoch_a = match coordinator.prepare_track(&meta_a) {
            TrackPlan::Fetch { epoch, .. } => epoch,
            other => panic!("expected fetch plan, got {other:?}"),
        };
        coordinator.record_user_offset(-200);
        let resolved_a = coordinator
            .complete_epoch(epoch_a, Some(sample_answer("A")), true)
            .unwrap()
            .unwrap();
        assert_eq!(resolved_a.track.user_offset_ms, -200);

        let _ = coordinator.prepare_track(&meta_b);
        coordinator.record_user_offset(100);

        match coordinator.prepare_track(&meta_a) {
            TrackPlan::Cached {
                user_offset_ms,
                answer,
                ..
            } => {
                assert_eq!(user_offset_ms, -200);
                assert_eq!(answer.track.user_offset_ms, -200);
                assert_eq!(answer.track.source_track_id.as_deref(), Some("A"));
            }
            other => panic!("expected cached plan, got {other:?}"),
        }
    }
}

#[cfg(test)]
mod cache_tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::core::model::{LineRole, LyricLine, PARSER_VERSION, SyncGranularity, SyncedTrack};
    use crate::providers::Provider;

    fn temp_path(name: &str) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "lyrachi-cache-resolve-{name}-{}-{stamp}",
            std::process::id()
        ))
    }

    fn cleanup(path: &Path) {
        let _ = fs::remove_dir_all(path);
    }

    fn meta() -> TrackMeta {
        TrackMeta {
            title: "Song A".to_string(),
            artist: "Artist".to_string(),
            album: Some("Album".to_string()),
            duration_ms: Some(200_000),
            track_id: None,
            url: None,
        }
    }

    fn answer(id: &str) -> ProviderAnswer {
        ProviderAnswer::new(
            Provider::Lrclib,
            SyncedTrack {
                source_provider: "lrclib".to_string(),
                source_track_id: Some(id.to_string()),
                parser_version: PARSER_VERSION,
                is_instrumental: false,
                granularity: SyncGranularity::Line,
                intrinsic_offset_ms: 0,
                user_offset_ms: 0,
                match_confidence: 0.9,
                lines: vec![LyricLine {
                    start_ms: 1_000,
                    end_ms: 2_000,
                    role: LineRole::Primary,
                    text: format!("lyric {id}"),
                    words: Vec::new(),
                    annotations: Vec::new(),
                }],
            },
        )
    }

    #[tokio::test]
    async fn cache_hit_returns_answer_without_a_network_provider() {
        let root = temp_path("hit");
        fs::create_dir_all(&root).unwrap();
        let db = root.join("cache.db");
        let query = TrackQuery::from_meta(&meta(), 0);
        let cache = CacheHandle::open(&db).unwrap();
        cache
            .store(&query.canonical, "Artist - Song A", &answer("cached"))
            .await
            .unwrap();

        let resolver = ProviderResolver::with_config(
            Vec::new(),
            vec![Provider::Local, Provider::Lrclib],
            false,
        )
        .unwrap();
        let resolved =
            resolve_with_cache(&resolver, Some(&cache), &query, &CancellationToken::new())
                .await
                .unwrap()
                .unwrap();

        assert_eq!(resolved.provider, Provider::Lrclib);
        assert_eq!(resolved.track.source_track_id.as_deref(), Some("cached"));
        cache.shutdown().await.unwrap();
        cleanup(&root);
    }

    #[tokio::test]
    async fn local_sidecar_wins_over_a_cached_network_answer() {
        let root = temp_path("local");
        let music = root.join("music");
        fs::create_dir_all(&music).unwrap();
        fs::write(music.join("Artist - Song A.lrc"), "[00:01.00]local lyric\n").unwrap();
        let db = root.join("cache.db");
        let query = TrackQuery::from_meta(&meta(), 0);
        let cache = CacheHandle::open(&db).unwrap();
        cache
            .store(&query.canonical, "Artist - Song A", &answer("cached"))
            .await
            .unwrap();

        let resolver = ProviderResolver::with_config(
            vec![music],
            vec![Provider::Local, Provider::Lrclib],
            false,
        )
        .unwrap();
        let resolved =
            resolve_with_cache(&resolver, Some(&cache), &query, &CancellationToken::new())
                .await
                .unwrap()
                .unwrap();

        assert_eq!(resolved.provider, Provider::Local);
        assert_eq!(resolved.track.lines[0].text, "local lyric");
        cache.shutdown().await.unwrap();
        cleanup(&root);
    }

    #[tokio::test]
    async fn refetch_invalidates_l2_and_bypasses_cache_preserving_offset() {
        let root = temp_path("refetch");
        fs::create_dir_all(&root).unwrap();
        let db = root.join("cache.db");
        let cache = CacheHandle::open(&db).unwrap();

        let mut coordinator = ResolveCoordinator::new();
        let meta = meta();
        let query = TrackQuery::from_meta(&meta, 0);

        cache
            .store(&query.canonical, "Artist - Song A", &answer("cached-v1"))
            .await
            .unwrap();

        cache
            .record_negative(&query.canonical, Provider::LyricsPlus)
            .await
            .unwrap();

        let plan_1 = coordinator.prepare_track(&meta);
        let epoch_1 = match plan_1 {
            TrackPlan::Fetch {
                epoch,
                bypass_cache,
                ..
            } => {
                assert!(!bypass_cache);
                epoch
            }
            other => panic!("expected fetch plan, got {other:?}"),
        };
        coordinator.record_user_offset(-350);

        let resolver = ProviderResolver::with_config(
            Vec::new(),
            vec![Provider::Local, Provider::Lrclib],
            false,
        )
        .unwrap();

        let cached = resolve_with_cache(&resolver, Some(&cache), &query, &CancellationToken::new())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(cached.track.source_track_id.as_deref(), Some("cached-v1"));

        let completed = coordinator
            .complete_epoch(epoch_1, Some(cached), true)
            .unwrap()
            .unwrap();
        assert_eq!(completed.track.user_offset_ms, -350);
        assert_eq!(coordinator.cache().len(), 1);

        let refetch_plan = coordinator
            .force_refetch_current_track()
            .expect("should produce refetch plan");
        let (refetch_epoch, user_offset_ms, bypass_cache, cancel) = match refetch_plan {
            TrackPlan::Fetch {
                epoch,
                user_offset_ms,
                bypass_cache,
                cancel,
                ..
            } => (epoch, user_offset_ms, bypass_cache, cancel),
            other => panic!("expected refetch plan, got {other:?}"),
        };

        assert_eq!(refetch_epoch, epoch_1 + 1);
        assert_eq!(user_offset_ms, -350);
        assert!(bypass_cache);
        assert!(!cancel.is_cancelled());
        assert!(coordinator.cache().is_empty());

        cache.invalidate_track(&query.canonical).await.unwrap();

        let lookup_after = cache.lookup(&query.canonical).await.unwrap();
        assert!(lookup_after.answer.is_none());
        assert!(lookup_after.negative_providers.is_empty());

        let (updates_tx, mut updates_rx) = mpsc::unbounded_channel();
        let cache_task = cache.clone();
        let resolve_task = tokio::spawn(async move {
            resolve_with_updates(
                &resolver,
                Some(&cache_task),
                &query,
                &cancel,
                &updates_tx,
                bypass_cache,
            )
            .await
        });

        resolve_task.await.unwrap().unwrap();
        let mut final_answer = None;
        while let Some(update) = updates_rx.recv().await {
            if update.is_final {
                final_answer = Some(update.result);
            }
        }
        assert!(final_answer.is_some());

        let result = coordinator.complete_epoch(refetch_epoch, None, true);
        assert!(matches!(result, Some(None)));

        cache.shutdown().await.unwrap();
        cleanup(&root);
    }

    #[test]
    fn apply_manual_candidate_cancels_in_flight_and_ignores_stale_epoch() {
        let mut coordinator = ResolveCoordinator::new();
        let plan = coordinator.prepare_track(&meta());
        let (epoch_1, token) = match plan {
            TrackPlan::Fetch { epoch, cancel, .. } => (epoch, cancel),
            other => panic!("expected fetch plan, got {other:?}"),
        };
        assert!(!token.is_cancelled());
        let active_key = coordinator.active_key().cloned().unwrap();
        coordinator.seed_user_offset(active_key.clone(), 200);

        // Stale track key must be rejected cleanly
        let wrong_key = CanonicalTrackKey {
            norm_artist: "other".to_string(),
            norm_title: "song".to_string(),
            norm_album: None,
            duration_bucket_sec: Some(120),
        };
        assert!(
            coordinator
                .apply_manual_candidate(&wrong_key, answer("wrong"))
                .is_none()
        );

        let mut manual_answer = answer("manual-pick");
        manual_answer.provider = Provider::LyricsPlus;
        let (new_epoch, applied) = coordinator
            .apply_manual_candidate(&active_key, manual_answer)
            .expect("matching key must apply");
        assert_eq!(new_epoch, epoch_1 + 1);
        assert_eq!(applied.track.user_offset_ms, 200);
        assert!(token.is_cancelled(), "previous token must be cancelled");

        let stale_result = coordinator.complete_epoch(epoch_1, Some(answer("old")), true);
        assert!(stale_result.is_none(), "stale epoch must return None");

        let cached = coordinator
            .cache_mut()
            .get(&active_key)
            .expect("manual candidate should be in cache");
        assert_eq!(cached.provider, Provider::LyricsPlus);
        assert_eq!(cached.track.user_offset_ms, 200);
    }
}

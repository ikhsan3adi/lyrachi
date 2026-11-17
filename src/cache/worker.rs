use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use rusqlite::{Connection, OptionalExtension, params};
use tokio::sync::{mpsc, oneshot};
use tracing::warn;

use super::{
    CACHE_CHANNEL_CAPACITY, CACHE_MAX_ENTRY_AGE_SECS, CACHE_NEGATIVE_TTL_SECS,
    CACHE_PREFERENCES_MAX_AGE_SECS, CACHE_PREFERENCES_MAX_ENTRIES, CACHE_PRUNE_CHECK_SECS,
    CACHE_PRUNE_INTERVAL_SECS, CACHE_TOUCH_DEBOUNCE_SECS, CACHE_UPGRADE_INTERVAL_SECS, CacheError,
    CacheHandle, CacheLookup, TrackPreference, now_secs, schema,
};
use crate::core::model::{PARSER_VERSION, SyncedTrack};
use crate::providers::{Provider, ProviderAnswer, Tier};

type TrackKey = [u8; 32];

pub(super) enum CacheCommand {
    Lookup {
        track_key: TrackKey,
        now: i64,
        reply: oneshot::Sender<Result<CacheLookup, CacheError>>,
    },
    Store {
        track_key: TrackKey,
        label: String,
        answer: ProviderAnswer,
        now: i64,
        reply: oneshot::Sender<Result<(), CacheError>>,
    },
    RecordNegative {
        track_key: TrackKey,
        provider: Provider,
        now: i64,
        reply: oneshot::Sender<Result<(), CacheError>>,
    },
    MarkUpgradeAttempted {
        track_key: TrackKey,
        provider: Provider,
        reply: oneshot::Sender<Result<(), CacheError>>,
    },
    InvalidateTrack {
        track_key: TrackKey,
        reply: oneshot::Sender<Result<(), CacheError>>,
    },
    GetTrackPreference {
        track_key: TrackKey,
        reply: oneshot::Sender<Result<Option<TrackPreference>, CacheError>>,
    },
    SetTrackPreference {
        track_key: TrackKey,
        preference: TrackPreference,
        reply: oneshot::Sender<Result<(), CacheError>>,
    },
    ClearPinnedLyrics {
        track_key: TrackKey,
        now: i64,
        reply: oneshot::Sender<Result<(), CacheError>>,
    },
    Shutdown {
        now: i64,
        reply: oneshot::Sender<Result<(), CacheError>>,
    },
}

impl CacheCommand {
    fn now(&self) -> i64 {
        match self {
            Self::Lookup { now, .. }
            | Self::Store { now, .. }
            | Self::RecordNegative { now, .. }
            | Self::ClearPinnedLyrics { now, .. }
            | Self::Shutdown { now, .. } => *now,
            Self::MarkUpgradeAttempted { .. }
            | Self::InvalidateTrack { .. }
            | Self::GetTrackPreference { .. }
            | Self::SetTrackPreference { .. } => now_secs(),
        }
    }
}

pub(super) fn spawn(path: PathBuf) -> Result<CacheHandle, CacheError> {
    let (tx, mut rx) = mpsc::channel(CACHE_CHANNEL_CAPACITY);
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);

    std::thread::Builder::new()
        .name("lyrachi-cache".to_string())
        .spawn(move || match schema::open(&path) {
            Ok(connection) => {
                let _ = ready_tx.send(Ok(()));
                Worker::new(connection).run(&mut rx);
            }
            Err(error) => {
                let _ = ready_tx.send(Err(error));
            }
        })?;

    ready_rx.recv().map_err(|_| CacheError::WorkerStopped)??;
    Ok(CacheHandle { tx })
}

struct Worker {
    connection: Connection,
    last_touch: HashMap<(TrackKey, Provider), i64>,
    pending_touch: HashMap<(TrackKey, Provider), i64>,
    last_prune_check: Option<i64>,
}

impl Worker {
    fn new(connection: Connection) -> Self {
        Self {
            connection,
            last_touch: HashMap::new(),
            pending_touch: HashMap::new(),
            last_prune_check: None,
        }
    }

    fn run(mut self, rx: &mut mpsc::Receiver<CacheCommand>) {
        let started = now_secs();
        self.last_prune_check = Some(started);
        if let Err(error) = self.prune_if_due(started) {
            warn!(%error, "failed to prune lyric cache");
        }

        while let Some(command) = rx.blocking_recv() {
            let command_now = command.now();
            match command {
                CacheCommand::Lookup {
                    track_key,
                    now,
                    reply,
                } => {
                    let _ = reply.send(self.lookup(&track_key, now));
                }
                CacheCommand::Store {
                    track_key,
                    label,
                    answer,
                    now,
                    reply,
                } => {
                    let _ = reply.send(self.store(&track_key, &label, &answer, now));
                }
                CacheCommand::RecordNegative {
                    track_key,
                    provider,
                    now,
                    reply,
                } => {
                    let _ = reply.send(self.record_negative(&track_key, provider, now));
                }
                CacheCommand::MarkUpgradeAttempted {
                    track_key,
                    provider,
                    reply,
                } => {
                    let _ = reply.send(self.mark_upgrade_attempted(&track_key, provider));
                }
                CacheCommand::InvalidateTrack { track_key, reply } => {
                    let _ = reply.send(self.invalidate_track(&track_key));
                }
                CacheCommand::GetTrackPreference { track_key, reply } => {
                    let _ = reply.send(self.get_track_preference(&track_key));
                }
                CacheCommand::SetTrackPreference {
                    track_key,
                    preference,
                    reply,
                } => {
                    let _ = reply.send(self.set_track_preference(&track_key, &preference));
                }
                CacheCommand::ClearPinnedLyrics {
                    track_key,
                    now,
                    reply,
                } => {
                    let _ = reply.send(self.clear_pinned_lyrics(&track_key, now));
                }
                CacheCommand::Shutdown { now, reply } => {
                    let _ = reply.send(self.flush_touches(now));
                    break;
                }
            }
            if let Err(error) = self.maybe_prune(command_now) {
                warn!(%error, "failed to prune lyric cache");
            }
        }
        let _ = self.flush_touches(now_secs());
        if let Err(error) = self.maybe_prune(now_secs()) {
            warn!(%error, "failed to prune lyric cache");
        }
    }

    fn lookup(&mut self, track_key: &TrackKey, now: i64) -> Result<CacheLookup, CacheError> {
        let negative_providers = {
            let mut statement = self.connection.prepare(
                "SELECT provider FROM negative_cache
                 WHERE track_key = ?1 AND expires_at > ?2",
            )?;
            let rows = statement.query_map(params![track_key.as_slice(), now], |row| {
                row.get::<_, String>(0)
            })?;
            let mut providers = HashSet::new();
            for row in rows {
                let provider_name = match row {
                    Ok(name) => name,
                    Err(error) => {
                        warn!(%error, "failed to read negative cache row");
                        continue;
                    }
                };
                if let Some(provider) = Provider::from_config_name(&provider_name) {
                    providers.insert(provider);
                }
            }
            providers
        };

        let preference = self.get_track_preference(track_key)?;
        if let Some(ref pref) = preference
            && let Some(ref payload) = pref.pinned_payload_json
        {
            match serde_json::from_slice::<SyncedTrack>(payload) {
                Ok(mut track) if track.parser_version == PARSER_VERSION => {
                    track.user_offset_ms = 0;
                    let provider = pref
                        .provider()
                        .or_else(|| Provider::from_config_name(&track.source_provider))
                        .unwrap_or(Provider::Lrclib);
                    let tier = Tier::from_track(&track);
                    let confidence = finite_confidence(track.match_confidence);
                    let answer = ProviderAnswer {
                        provider,
                        tier,
                        confidence,
                        track,
                    };
                    return Ok(CacheLookup {
                        answer: Some(answer),
                        needs_upgrade: false,
                        negative_providers,
                        is_pinned: true,
                        preference,
                    });
                }
                Ok(_) => {
                    warn!("ignoring pinned lyric payload with incompatible parser version");
                }
                Err(error) => {
                    warn!(%error, "ignoring invalid pinned lyric payload");
                }
            }
        }

        let preferred_provider = preference.as_ref().and_then(TrackPreference::provider);
        let cutoff = now.saturating_sub(CACHE_MAX_ENTRY_AGE_SECS);
        let best = {
            let mut statement = self.connection.prepare(
                "SELECT provider, parser_version, confidence, payload_json, last_accessed_at, upgrade_after
                 FROM lyric_entries
                 WHERE track_key = ?1",
            )?;
            let rows = statement.query_map(params![track_key.as_slice()], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, f64>(2)?,
                    row.get::<_, Vec<u8>>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, Option<i64>>(5)?,
                ))
            })?;

            let mut best: Option<(ProviderAnswer, i64, Option<i64>)> = None;
            for row in rows {
                let (
                    provider_name,
                    parser_version,
                    confidence,
                    payload,
                    last_accessed_at,
                    upgrade_after,
                ) = match row {
                    Ok(row) => row,
                    Err(error) => {
                        warn!(%error, "failed to read cached lyric row");
                        continue;
                    }
                };

                if parser_version != i64::from(PARSER_VERSION) || last_accessed_at < cutoff {
                    continue;
                }

                let Some(provider) = Provider::from_config_name(&provider_name) else {
                    warn!(%provider_name, "ignoring cached lyric with unknown provider");
                    continue;
                };
                let mut track: SyncedTrack = match serde_json::from_slice(&payload) {
                    Ok(track) => track,
                    Err(error) => {
                        warn!(%provider_name, %error, "ignoring invalid cached lyric payload");
                        continue;
                    }
                };
                if track.parser_version != PARSER_VERSION {
                    continue;
                }
                track.user_offset_ms = 0;

                let confidence = finite_confidence(confidence as f32);
                let answer = ProviderAnswer {
                    provider,
                    tier: Tier::from_track(&track),
                    confidence,
                    track,
                };

                let should_replace = match best.as_ref() {
                    None => true,
                    Some((current_best, _, _)) => {
                        if preferred_provider == Some(answer.provider)
                            && preferred_provider != Some(current_best.provider)
                        {
                            true
                        } else if preferred_provider == Some(current_best.provider)
                            && preferred_provider != Some(answer.provider)
                        {
                            false
                        } else {
                            is_better(&answer, Some(current_best))
                        }
                    }
                };

                if should_replace {
                    best = Some((answer, last_accessed_at, upgrade_after));
                }
            }
            best
        };

        let Some((answer, last_accessed_at, upgrade_after)) = best else {
            return Ok(CacheLookup {
                answer: None,
                needs_upgrade: false,
                negative_providers,
                is_pinned: false,
                preference,
            });
        };
        self.touch(track_key, answer.provider, now, last_accessed_at)?;
        let needs_upgrade =
            answer.tier == Tier::Line && upgrade_after.is_some_and(|stamp| now >= stamp);
        Ok(CacheLookup {
            answer: Some(answer),
            needs_upgrade,
            negative_providers,
            is_pinned: false,
            preference,
        })
    }

    fn store(
        &mut self,
        track_key: &TrackKey,
        label: &str,
        answer: &ProviderAnswer,
        now: i64,
    ) -> Result<(), CacheError> {
        let mut track = answer.track.clone();
        track.user_offset_ms = 0;
        let payload = serde_json::to_vec(&track)?;
        let provider = answer.provider;
        let upgrade_after =
            (answer.tier == Tier::Line).then(|| now.saturating_add(CACHE_UPGRADE_INTERVAL_SECS));

        let transaction = self.connection.transaction()?;
        transaction.execute(
            "INSERT INTO lyric_entries (
                track_key, provider, label, granularity, parser_version, confidence,
                payload_json, fetched_at, last_accessed_at, upgrade_after
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(track_key, provider) DO UPDATE SET
                label = excluded.label,
                granularity = excluded.granularity,
                parser_version = excluded.parser_version,
                confidence = excluded.confidence,
                payload_json = excluded.payload_json,
                fetched_at = excluded.fetched_at,
                last_accessed_at = excluded.last_accessed_at,
                upgrade_after = excluded.upgrade_after",
            params![
                track_key.as_slice(),
                provider.as_str(),
                label,
                track.granularity.as_str(),
                i64::from(track.parser_version),
                f64::from(answer.confidence),
                payload,
                now,
                now,
                upgrade_after,
            ],
        )?;
        transaction.execute(
            "DELETE FROM negative_cache WHERE track_key = ?1 AND provider = ?2",
            params![track_key.as_slice(), provider.as_str()],
        )?;
        transaction.commit()?;

        let id = (*track_key, provider);
        self.last_touch.insert(id, now);
        self.pending_touch.remove(&id);
        Ok(())
    }

    fn touch(
        &mut self,
        track_key: &TrackKey,
        provider: Provider,
        now: i64,
        row_last_accessed_at: i64,
    ) -> Result<(), CacheError> {
        let id = (*track_key, provider);
        let last_written = self
            .last_touch
            .get(&id)
            .copied()
            .unwrap_or(row_last_accessed_at);

        if now.saturating_sub(last_written) >= CACHE_TOUCH_DEBOUNCE_SECS {
            self.connection.execute(
                "UPDATE lyric_entries
                 SET last_accessed_at = ?1
                 WHERE track_key = ?2 AND provider = ?3",
                params![now, track_key.as_slice(), provider.as_str()],
            )?;
            self.last_touch.insert(id, now);
            self.pending_touch.remove(&id);
        } else {
            self.pending_touch.insert(id, now);
        }
        Ok(())
    }

    fn record_negative(
        &mut self,
        track_key: &TrackKey,
        provider: Provider,
        now: i64,
    ) -> Result<(), CacheError> {
        let expires_at = now.saturating_add(CACHE_NEGATIVE_TTL_SECS);
        self.connection.execute(
            "INSERT INTO negative_cache (track_key, provider, expires_at)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(track_key, provider) DO UPDATE SET expires_at = excluded.expires_at",
            params![track_key.as_slice(), provider.as_str(), expires_at],
        )?;
        Ok(())
    }

    fn mark_upgrade_attempted(
        &mut self,
        track_key: &TrackKey,
        provider: Provider,
    ) -> Result<(), CacheError> {
        self.connection.execute(
            "UPDATE lyric_entries
             SET upgrade_after = NULL
             WHERE track_key = ?1 AND provider = ?2",
            params![track_key.as_slice(), provider.as_str()],
        )?;
        Ok(())
    }

    fn invalidate_track(&mut self, track_key: &TrackKey) -> Result<(), CacheError> {
        let transaction = self.connection.transaction()?;
        transaction.execute(
            "DELETE FROM lyric_entries WHERE track_key = ?1",
            params![track_key.as_slice()],
        )?;
        transaction.execute(
            "DELETE FROM negative_cache WHERE track_key = ?1",
            params![track_key.as_slice()],
        )?;
        transaction.commit()?;

        self.last_touch.retain(|(key, _), _| key != track_key);
        self.pending_touch.retain(|(key, _), _| key != track_key);
        Ok(())
    }

    fn maybe_prune(&mut self, now: i64) -> Result<(), CacheError> {
        if self
            .last_prune_check
            .is_some_and(|last| now.saturating_sub(last) < CACHE_PRUNE_CHECK_SECS)
        {
            return Ok(());
        }
        self.last_prune_check = Some(now);
        self.prune_if_due(now)
    }

    fn prune_if_due(&mut self, now: i64) -> Result<(), CacheError> {
        let last_prune = self
            .connection
            .query_row(
                "SELECT value FROM meta WHERE key = 'last_prune'",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        let due = last_prune
            .and_then(|value| value.parse::<i64>().ok())
            .is_none_or(|last| now.saturating_sub(last) >= CACHE_PRUNE_INTERVAL_SECS);
        if !due {
            return Ok(());
        }

        let cutoff = now.saturating_sub(CACHE_MAX_ENTRY_AGE_SECS);
        let pref_cutoff = now.saturating_sub(CACHE_PREFERENCES_MAX_AGE_SECS);
        let transaction = self.connection.transaction()?;
        transaction.execute(
            "DELETE FROM lyric_entries WHERE last_accessed_at < ?1",
            params![cutoff],
        )?;
        transaction.execute(
            "DELETE FROM negative_cache WHERE expires_at <= ?1",
            params![now],
        )?;
        prune_preferences_in_tx(&transaction, pref_cutoff, CACHE_PREFERENCES_MAX_ENTRIES)?;
        transaction.execute(
            "INSERT INTO meta (key, value) VALUES ('last_prune', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![now.to_string()],
        )?;
        transaction.commit()?;
        Ok(())
    }

    fn get_track_preference(
        &mut self,
        track_key: &TrackKey,
    ) -> Result<Option<TrackPreference>, CacheError> {
        let mut statement = self.connection.prepare(
            "SELECT preferred_provider, preferred_track_id, pinned_payload_json, preferred_style, updated_at
             FROM track_preferences
             WHERE track_key = ?1",
        )?;
        let row = statement
            .query_row(params![track_key.as_slice()], |row| {
                Ok(TrackPreference {
                    preferred_provider: row.get(0)?,
                    preferred_track_id: row.get(1)?,
                    pinned_payload_json: row.get(2)?,
                    preferred_style: row.get(3)?,
                    updated_at: row.get(4)?,
                })
            })
            .optional()?;
        Ok(row)
    }

    fn set_track_preference(
        &mut self,
        track_key: &TrackKey,
        preference: &TrackPreference,
    ) -> Result<(), CacheError> {
        self.connection.execute(
            "INSERT INTO track_preferences (
                track_key, preferred_provider, preferred_track_id,
                pinned_payload_json, preferred_style, updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(track_key) DO UPDATE SET
                preferred_provider = excluded.preferred_provider,
                preferred_track_id = excluded.preferred_track_id,
                pinned_payload_json = excluded.pinned_payload_json,
                preferred_style = excluded.preferred_style,
                updated_at = excluded.updated_at",
            params![
                track_key.as_slice(),
                preference.preferred_provider.as_deref(),
                preference.preferred_track_id.as_deref(),
                preference.pinned_payload_json.as_deref(),
                preference.preferred_style.as_deref(),
                preference.updated_at,
            ],
        )?;
        Ok(())
    }

    fn clear_pinned_lyrics(&mut self, track_key: &TrackKey, now: i64) -> Result<(), CacheError> {
        let tx = self.connection.transaction()?;
        tx.execute(
            "UPDATE track_preferences
             SET preferred_provider = NULL,
                 preferred_track_id = NULL,
                 pinned_payload_json = NULL,
                 updated_at = ?1
             WHERE track_key = ?2",
            params![now, track_key.as_slice()],
        )?;
        tx.execute(
            "DELETE FROM track_preferences
             WHERE track_key = ?1 AND preferred_style IS NULL",
            params![track_key.as_slice()],
        )?;
        tx.commit()?;
        Ok(())
    }

    fn flush_touches(&mut self, now: i64) -> Result<(), CacheError> {
        let pending = std::mem::take(&mut self.pending_touch);
        let mut first_error = None;

        for ((track_key, provider), touch_time) in pending {
            let stamp = if now > 0 { now } else { touch_time };
            let result = self.connection.execute(
                "UPDATE lyric_entries
                 SET last_accessed_at = ?1
                 WHERE track_key = ?2 AND provider = ?3",
                params![stamp, track_key.as_slice(), provider.as_str()],
            );
            match result {
                Ok(_) => {
                    self.last_touch.insert((track_key, provider), stamp);
                }
                Err(error) if first_error.is_none() => first_error = Some(error.into()),
                Err(_) => {}
            }
        }

        first_error.map_or(Ok(()), Err)
    }

    #[cfg(test)]
    fn lookup_at(&mut self, track_key: &TrackKey, now: i64) -> Result<CacheLookup, CacheError> {
        self.lookup(track_key, now)
    }
}

fn is_better(candidate: &ProviderAnswer, current: Option<&ProviderAnswer>) -> bool {
    let Some(current) = current else {
        return true;
    };
    match candidate.tier.cmp(&current.tier) {
        Ordering::Greater => true,
        Ordering::Less => false,
        Ordering::Equal => match candidate.confidence.total_cmp(&current.confidence) {
            Ordering::Greater => true,
            Ordering::Less => false,
            Ordering::Equal => candidate.provider.as_str() < current.provider.as_str(),
        },
    }
}

fn prune_preferences_in_tx(
    transaction: &rusqlite::Transaction<'_>,
    pref_cutoff: i64,
    max_entries: usize,
) -> Result<(), CacheError> {
    transaction.execute(
        "DELETE FROM track_preferences WHERE updated_at < ?1",
        params![pref_cutoff],
    )?;
    transaction.execute(
        "DELETE FROM track_preferences
         WHERE rowid NOT IN (
             SELECT rowid FROM track_preferences
             ORDER BY updated_at DESC, rowid DESC
             LIMIT ?1
         )",
        params![i64::try_from(max_entries).unwrap_or(i64::MAX)],
    )?;
    Ok(())
}

fn finite_confidence(confidence: f32) -> f32 {
    if confidence.is_finite() {
        confidence
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;
    use crate::core::model::{LineRole, LyricLine, SyncGranularity};

    fn temp_path(name: &str) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "lyrachi-cache-{name}-{}-{stamp}.db",
            std::process::id()
        ))
    }

    fn sample_key(artist: &str, title: &str) -> crate::core::model::CanonicalTrackKey {
        crate::core::model::CanonicalTrackKey {
            norm_artist: artist.to_string(),
            norm_title: title.to_string(),
            norm_album: None,
            duration_bucket_sec: Some(200),
        }
    }

    fn sample_answer(
        provider: Provider,
        text: &str,
        granularity: SyncGranularity,
    ) -> ProviderAnswer {
        ProviderAnswer::new(
            provider,
            SyncedTrack {
                source_provider: provider.as_str().to_string(),
                source_track_id: Some(text.to_string()),
                parser_version: PARSER_VERSION,
                is_instrumental: false,
                granularity,
                intrinsic_offset_ms: 0,
                user_offset_ms: 75,
                match_confidence: 0.9,
                lines: vec![LyricLine {
                    start_ms: 1_000,
                    end_ms: 2_000,
                    role: LineRole::Primary,
                    text: text.to_string(),
                    words: Vec::new(),
                    annotations: Vec::new(),
                }],
            },
        )
    }

    fn read_last_accessed(path: &Path, key: &TrackKey, provider: Provider) -> i64 {
        let connection = Connection::open(path).unwrap();
        connection
            .query_row(
                "SELECT last_accessed_at FROM lyric_entries
                 WHERE track_key = ?1 AND provider = ?2",
                params![key.as_slice(), provider.as_str()],
                |row| row.get(0),
            )
            .unwrap()
    }

    fn insert_row(
        path: &Path,
        key: &TrackKey,
        provider: Provider,
        parser_version: i64,
        last_accessed_at: i64,
        track: &SyncedTrack,
    ) {
        let connection = Connection::open(path).unwrap();
        connection
            .execute(
                "INSERT INTO lyric_entries (
                    track_key, provider, label, granularity, parser_version, confidence,
                    payload_json, fetched_at, last_accessed_at, upgrade_after
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, NULL)",
                params![
                    key.as_slice(),
                    provider.as_str(),
                    "Artist - Title",
                    track.granularity.as_str(),
                    parser_version,
                    f64::from(track.match_confidence),
                    serde_json::to_vec(track).unwrap(),
                    last_accessed_at,
                    last_accessed_at,
                ],
            )
            .unwrap();
    }

    fn insert_negative_row(path: &Path, key: &TrackKey, provider: Provider, expires_at: i64) {
        let connection = Connection::open(path).unwrap();
        connection
            .execute(
                "INSERT INTO negative_cache (track_key, provider, expires_at)
                 VALUES (?1, ?2, ?3)",
                params![key.as_slice(), provider.as_str(), expires_at],
            )
            .unwrap();
    }

    fn read_upgrade_after(path: &Path, key: &TrackKey, provider: Provider) -> Option<i64> {
        let connection = Connection::open(path).unwrap();
        connection
            .query_row(
                "SELECT upgrade_after FROM lyric_entries
                 WHERE track_key = ?1 AND provider = ?2",
                params![key.as_slice(), provider.as_str()],
                |row| row.get(0),
            )
            .unwrap()
    }

    fn count_rows(path: &Path, table: &str, key: &TrackKey, provider: Provider) -> i64 {
        let connection = Connection::open(path).unwrap();
        connection
            .query_row(
                &format!("SELECT COUNT(*) FROM {table} WHERE track_key = ?1 AND provider = ?2"),
                params![key.as_slice(), provider.as_str()],
                |row| row.get(0),
            )
            .unwrap()
    }

    fn read_meta(path: &Path, key: &str) -> Option<String> {
        let connection = Connection::open(path).unwrap();
        connection
            .query_row(
                "SELECT value FROM meta WHERE key = ?1",
                params![key],
                |row| row.get(0),
            )
            .optional()
            .unwrap()
    }

    fn write_last_prune(path: &Path, value: i64) {
        let connection = Connection::open(path).unwrap();
        connection
            .execute(
                "INSERT INTO meta (key, value) VALUES ('last_prune', ?1)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![value.to_string()],
            )
            .unwrap();
    }

    #[test]
    fn debounces_touches_in_memory_and_flushes_on_shutdown() {
        let path = temp_path("touch");
        let key = sample_key("Artist", "Title");
        let track_key = crate::providers::sanitize::blake3_track_key(&key);
        let answer = sample_answer(Provider::Lrclib, "line", SyncGranularity::Line);
        let connection = schema::open(&path).unwrap();
        let mut worker = Worker::new(connection);

        worker
            .store(&track_key, "Artist - Title", &answer, 1_000)
            .unwrap();
        assert!(
            worker
                .lookup_at(&track_key, 2_000)
                .unwrap()
                .answer
                .is_some()
        );
        worker.flush_touches(2_000).unwrap();
        assert_eq!(
            read_last_accessed(&path, &track_key, Provider::Lrclib),
            2_000
        );

        assert!(
            worker
                .lookup_at(&track_key, 3_000)
                .unwrap()
                .answer
                .is_some()
        );
        assert_eq!(
            read_last_accessed(&path, &track_key, Provider::Lrclib),
            2_000
        );
        worker.flush_touches(3_000).unwrap();
        assert_eq!(
            read_last_accessed(&path, &track_key, Provider::Lrclib),
            3_000
        );

        drop(worker);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn ignores_stale_and_old_parser_rows() {
        let path = temp_path("filters");
        let key = sample_key("Artist", "Title");
        let track_key = crate::providers::sanitize::blake3_track_key(&key);
        let answer = sample_answer(Provider::Lrclib, "line", SyncGranularity::Line);
        let connection = schema::open(&path).unwrap();
        let mut worker = Worker::new(connection);
        let now = 10_000_000;

        insert_row(&path, &track_key, Provider::Local, 1, now, &answer.track);
        assert!(worker.lookup_at(&track_key, now).unwrap().answer.is_none());

        insert_row(
            &path,
            &track_key,
            Provider::Lrclib,
            i64::from(PARSER_VERSION),
            now - CACHE_MAX_ENTRY_AGE_SECS - 1,
            &answer.track,
        );
        assert!(worker.lookup_at(&track_key, now).unwrap().answer.is_none());

        drop(worker);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn chooses_the_highest_tier_across_providers() {
        let path = temp_path("tier");
        let key = sample_key("Artist", "Title");
        let track_key = crate::providers::sanitize::blake3_track_key(&key);
        let connection = schema::open(&path).unwrap();
        let mut worker = Worker::new(connection);

        worker
            .store(
                &track_key,
                "Artist - Title",
                &sample_answer(Provider::Lrclib, "line", SyncGranularity::Line),
                1_000,
            )
            .unwrap();
        worker
            .store(
                &track_key,
                "Artist - Title",
                &sample_answer(Provider::Local, "syllable", SyncGranularity::Syllable),
                1_000,
            )
            .unwrap();

        let answer = worker.lookup_at(&track_key, 2_000).unwrap().answer.unwrap();
        assert_eq!(answer.provider, Provider::Local);
        assert_eq!(answer.tier, Tier::Syllable);

        drop(worker);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn invalidates_positive_and_negative_rows_for_one_track() {
        let path = temp_path("invalidate");
        let key = sample_key("Artist", "Title");
        let track_key = crate::providers::sanitize::blake3_track_key(&key);
        let other_key = sample_key("Artist", "Other");
        let other_track_key = crate::providers::sanitize::blake3_track_key(&other_key);
        let connection = schema::open(&path).unwrap();
        let mut worker = Worker::new(connection);

        worker
            .store(
                &track_key,
                "Artist - Title",
                &sample_answer(Provider::Lrclib, "line", SyncGranularity::Line),
                1_000,
            )
            .unwrap();
        worker
            .record_negative(&track_key, Provider::LyricsPlus, 1_000)
            .unwrap();
        worker
            .store(
                &other_track_key,
                "Artist - Other",
                &sample_answer(Provider::Local, "other", SyncGranularity::Line),
                1_000,
            )
            .unwrap();
        worker
            .record_negative(&other_track_key, Provider::LyricsPlus, 1_000)
            .unwrap();

        worker.invalidate_track(&track_key).unwrap();

        let lookup = worker.lookup_at(&track_key, 2_000).unwrap();
        assert!(lookup.answer.is_none());
        assert!(lookup.negative_providers.is_empty());
        assert_eq!(
            count_rows(&path, "lyric_entries", &track_key, Provider::Lrclib),
            0
        );
        assert_eq!(
            count_rows(&path, "negative_cache", &track_key, Provider::LyricsPlus),
            0
        );

        let other = worker.lookup_at(&other_track_key, 2_000).unwrap();
        assert_eq!(other.answer.unwrap().provider, Provider::Local);
        assert!(other.negative_providers.contains(&Provider::LyricsPlus));

        drop(worker);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn stores_tracks_without_user_offsets() {
        let path = temp_path("offset");
        let key = sample_key("Artist", "Title");
        let track_key = crate::providers::sanitize::blake3_track_key(&key);
        let connection = schema::open(&path).unwrap();
        let mut worker = Worker::new(connection);

        worker
            .store(
                &track_key,
                "Artist - Title",
                &sample_answer(Provider::Lrclib, "line", SyncGranularity::Line),
                1_000,
            )
            .unwrap();
        let answer = worker.lookup_at(&track_key, 2_000).unwrap().answer.unwrap();
        assert_eq!(answer.track.user_offset_ms, 0);

        drop(worker);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn negative_entries_are_per_provider_and_expire() {
        let path = temp_path("negative");
        let key = sample_key("Artist", "Title");
        let track_key = crate::providers::sanitize::blake3_track_key(&key);
        let connection = schema::open(&path).unwrap();
        let mut worker = Worker::new(connection);
        let now = 1_000_000;

        worker
            .record_negative(&track_key, Provider::Lrclib, now)
            .unwrap();
        let lookup = worker.lookup_at(&track_key, now).unwrap();
        assert!(lookup.answer.is_none());
        assert_eq!(lookup.negative_providers.len(), 1);
        assert!(lookup.negative_providers.contains(&Provider::Lrclib));

        let lookup = worker
            .lookup_at(&track_key, now + CACHE_NEGATIVE_TTL_SECS)
            .unwrap();
        assert!(lookup.negative_providers.is_empty());

        drop(worker);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn positive_store_clears_the_matching_negative_entry() {
        let path = temp_path("negative-clear");
        let key = sample_key("Artist", "Title");
        let track_key = crate::providers::sanitize::blake3_track_key(&key);
        let connection = schema::open(&path).unwrap();
        let mut worker = Worker::new(connection);
        let now = 1_000_000;
        let answer = sample_answer(Provider::Lrclib, "line", SyncGranularity::Line);

        worker
            .record_negative(&track_key, Provider::Lrclib, now)
            .unwrap();
        worker
            .store(&track_key, "Artist - Title", &answer, now)
            .unwrap();

        let lookup = worker.lookup_at(&track_key, now).unwrap();
        assert!(lookup.negative_providers.is_empty());
        assert!(lookup.answer.is_some());

        drop(worker);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn line_entries_schedule_an_upgrade_after_fourteen_days() {
        let path = temp_path("upgrade");
        let key = sample_key("Artist", "Title");
        let track_key = crate::providers::sanitize::blake3_track_key(&key);
        let connection = schema::open(&path).unwrap();
        let mut worker = Worker::new(connection);
        let now = 1_000_000;
        let answer = sample_answer(Provider::Lrclib, "line", SyncGranularity::Line);

        worker
            .store(&track_key, "Artist - Title", &answer, now)
            .unwrap();
        assert_eq!(
            read_upgrade_after(&path, &track_key, Provider::Lrclib),
            Some(now + CACHE_UPGRADE_INTERVAL_SECS)
        );
        assert!(!worker.lookup_at(&track_key, now).unwrap().needs_upgrade);

        let due = now + CACHE_UPGRADE_INTERVAL_SECS;
        let lookup = worker.lookup_at(&track_key, due).unwrap();
        assert!(lookup.needs_upgrade);
        assert_eq!(lookup.answer.unwrap().tier, Tier::Line);

        worker
            .mark_upgrade_attempted(&track_key, Provider::Lrclib)
            .unwrap();
        assert_eq!(
            read_upgrade_after(&path, &track_key, Provider::Lrclib),
            None
        );
        let lookup = worker.lookup_at(&track_key, due).unwrap();
        assert!(!lookup.needs_upgrade);
        assert_eq!(lookup.answer.unwrap().tier, Tier::Line);

        drop(worker);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn word_entries_do_not_schedule_an_upgrade() {
        let path = temp_path("upgrade-word");
        let key = sample_key("Artist", "Title");
        let track_key = crate::providers::sanitize::blake3_track_key(&key);
        let connection = schema::open(&path).unwrap();
        let mut worker = Worker::new(connection);
        let now = 1_000_000;
        let answer = sample_answer(Provider::Lrclib, "word", SyncGranularity::Word);

        worker
            .store(&track_key, "Artist - Title", &answer, now)
            .unwrap();
        assert_eq!(
            read_upgrade_after(&path, &track_key, Provider::Lrclib),
            None
        );
        assert!(
            !worker
                .lookup_at(&track_key, now + CACHE_UPGRADE_INTERVAL_SECS)
                .unwrap()
                .needs_upgrade
        );

        drop(worker);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn prune_removes_stale_entries_once_per_day() {
        let path = temp_path("prune");
        let connection = schema::open(&path).unwrap();
        let mut worker = Worker::new(connection);
        let now = 10_000_000;
        let old_key = sample_key("Old", "Track");
        let fresh_key = sample_key("Fresh", "Track");
        let expired_key = sample_key("Expired", "Negative");
        let active_key = sample_key("Active", "Negative");
        let old_track_key = crate::providers::sanitize::blake3_track_key(&old_key);
        let fresh_track_key = crate::providers::sanitize::blake3_track_key(&fresh_key);
        let expired_track_key = crate::providers::sanitize::blake3_track_key(&expired_key);
        let active_track_key = crate::providers::sanitize::blake3_track_key(&active_key);
        let answer = sample_answer(Provider::Lrclib, "line", SyncGranularity::Line);

        insert_row(
            &path,
            &old_track_key,
            Provider::Lrclib,
            i64::from(PARSER_VERSION),
            now - CACHE_MAX_ENTRY_AGE_SECS - 1,
            &answer.track,
        );
        insert_row(
            &path,
            &fresh_track_key,
            Provider::Lrclib,
            i64::from(PARSER_VERSION),
            now,
            &answer.track,
        );
        insert_negative_row(&path, &expired_track_key, Provider::Lrclib, now - 1);
        insert_negative_row(&path, &active_track_key, Provider::Lrclib, now + 1);
        write_last_prune(&path, now - CACHE_PRUNE_INTERVAL_SECS - 1);

        worker.prune_if_due(now).unwrap();

        assert_eq!(
            count_rows(&path, "lyric_entries", &old_track_key, Provider::Lrclib),
            0
        );
        assert_eq!(
            count_rows(&path, "lyric_entries", &fresh_track_key, Provider::Lrclib),
            1
        );
        assert_eq!(
            count_rows(
                &path,
                "negative_cache",
                &expired_track_key,
                Provider::Lrclib
            ),
            0
        );
        assert_eq!(
            count_rows(&path, "negative_cache", &active_track_key, Provider::Lrclib),
            1
        );
        assert_eq!(read_meta(&path, "last_prune"), Some(now.to_string()));

        insert_row(
            &path,
            &old_track_key,
            Provider::Lrclib,
            i64::from(PARSER_VERSION),
            now - CACHE_MAX_ENTRY_AGE_SECS - 1,
            &answer.track,
        );
        worker.prune_if_due(now + 100).unwrap();
        assert_eq!(
            count_rows(&path, "lyric_entries", &old_track_key, Provider::Lrclib),
            1
        );

        drop(worker);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn prune_removes_preferences_older_than_180_days() {
        let path = temp_path("prune-preferences");
        let conn = schema::open(&path).unwrap();
        let mut worker = Worker::new(conn);
        let now = now_secs();

        let old_key = sample_key("old", "artist");
        let old_track_key = crate::providers::sanitize::blake3_track_key(&old_key);
        let old_pref = TrackPreference {
            preferred_provider: Some("lrclib".to_string()),
            preferred_track_id: None,
            pinned_payload_json: None,
            preferred_style: None,
            updated_at: now - CACHE_PREFERENCES_MAX_AGE_SECS - 1,
        };
        worker
            .set_track_preference(&old_track_key, &old_pref)
            .unwrap();

        let fresh_key = sample_key("fresh", "artist");
        let fresh_track_key = crate::providers::sanitize::blake3_track_key(&fresh_key);
        let fresh_pref = TrackPreference {
            preferred_provider: Some("lyricsplus".to_string()),
            preferred_track_id: None,
            pinned_payload_json: None,
            preferred_style: None,
            updated_at: now,
        };
        worker
            .set_track_preference(&fresh_track_key, &fresh_pref)
            .unwrap();

        write_last_prune(&path, now - CACHE_PRUNE_INTERVAL_SECS - 1);
        worker.prune_if_due(now).unwrap();

        assert!(
            worker
                .get_track_preference(&old_track_key)
                .unwrap()
                .is_none()
        );
        assert!(
            worker
                .get_track_preference(&fresh_track_key)
                .unwrap()
                .is_some()
        );

        drop(worker);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn pinned_lyrics_survive_90_day_prune_while_lyric_entries_are_pruned() {
        let path = temp_path("prune-90-day-pin");
        let conn = schema::open(&path).unwrap();
        let mut worker = Worker::new(conn);
        let now = now_secs();

        let key = sample_key("pinned", "artist");
        let track_key = crate::providers::sanitize::blake3_track_key(&key);
        let answer = sample_answer(Provider::Lrclib, "Pinned Track", SyncGranularity::Line);

        // Insert lyric_entries at `now - 100 days` (should be pruned)
        insert_row(
            &path,
            &track_key,
            Provider::Lrclib,
            i64::from(PARSER_VERSION),
            now - CACHE_MAX_ENTRY_AGE_SECS - 10,
            &answer.track,
        );

        // Insert pinned track_preferences at `now - 100 days` (< 180 days, MUST survive!)
        let mut pref = TrackPreference::with_pinned_track(
            Provider::Lrclib,
            Some("id-1".to_string()),
            &answer.track,
            None,
        )
        .unwrap();
        pref.updated_at = now - CACHE_MAX_ENTRY_AGE_SECS - 10;
        worker.set_track_preference(&track_key, &pref).unwrap();

        write_last_prune(&path, now - CACHE_PRUNE_INTERVAL_SECS - 1);
        worker.prune_if_due(now).unwrap();

        assert_eq!(
            count_rows(&path, "lyric_entries", &track_key, Provider::Lrclib),
            0,
            "90-day old lyric_entries should be pruned"
        );

        let lookup = worker.lookup(&track_key, now).unwrap();
        assert!(
            lookup.is_pinned,
            "100-day old pin must survive 90-day prune"
        );
        assert!(lookup.answer.is_some());

        drop(worker);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn prune_caps_preferences_at_max_entries_limit() {
        let path = temp_path("prune-cap-preferences");
        let conn = schema::open(&path).unwrap();
        let mut worker = Worker::new(conn);
        let now = now_secs();

        for idx in 0..5 {
            let key = sample_key(&format!("artist-{idx}"), "song");
            let track_key = crate::providers::sanitize::blake3_track_key(&key);
            let pref = TrackPreference {
                preferred_provider: Some("lrclib".to_string()),
                preferred_track_id: None,
                pinned_payload_json: None,
                preferred_style: None,
                updated_at: now + idx,
            };
            worker.set_track_preference(&track_key, &pref).unwrap();
        }

        let tx = worker.connection.transaction().unwrap();
        prune_preferences_in_tx(&tx, now - 10, 3).unwrap();
        tx.commit().unwrap();

        let remaining: i64 = worker
            .connection
            .query_row("SELECT COUNT(*) FROM track_preferences", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(remaining, 3);

        // Oldest two (idx 0 and 1) must be gone; newest three (2, 3, 4) kept
        for idx in 0..5 {
            let key = sample_key(&format!("artist-{idx}"), "song");
            let track_key = crate::providers::sanitize::blake3_track_key(&key);
            let exists = worker.get_track_preference(&track_key).unwrap().is_some();
            assert_eq!(exists, idx >= 2, "unexpected presence for idx {idx}");
        }

        drop(worker);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn preference_preserves_unavailable_provider_string_without_losing_pinned_payload() {
        let path = temp_path("preserve-unavailable-provider");
        let conn = schema::open(&path).unwrap();
        let mut worker = Worker::new(conn);
        let now = now_secs();

        let key = sample_key("artist", "ytmusic-track");
        let track_key = crate::providers::sanitize::blake3_track_key(&key);
        let mut track = sample_answer(
            Provider::LyricsPlus,
            "YTMusic Lyrics",
            SyncGranularity::Line,
        )
        .track;
        track.source_provider = "ytmusic".to_string();
        let payload = serde_json::to_vec(&track).unwrap();

        let pref = TrackPreference {
            preferred_provider: Some("ytmusic_future_or_disabled".to_string()),
            preferred_track_id: Some("yt-1".to_string()),
            pinned_payload_json: Some(payload),
            preferred_style: None,
            updated_at: now,
        };
        worker.set_track_preference(&track_key, &pref).unwrap();

        let stored = worker.get_track_preference(&track_key).unwrap().unwrap();
        assert_eq!(
            stored.preferred_provider.as_deref(),
            Some("ytmusic_future_or_disabled")
        );

        let lookup = worker.lookup(&track_key, now).unwrap();
        assert!(lookup.is_pinned);
        assert!(lookup.answer.is_some());

        drop(worker);
        let _ = std::fs::remove_file(path);
    }
}

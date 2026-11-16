use std::cmp::Ordering;
use std::collections::HashMap;
use std::path::PathBuf;

use rusqlite::{Connection, params};
use tokio::sync::{mpsc, oneshot};
use tracing::warn;

use super::{
    CACHE_CHANNEL_CAPACITY, CACHE_MAX_ENTRY_AGE_SECS, CACHE_TOUCH_DEBOUNCE_SECS, CacheError,
    CacheHandle, CacheLookup, now_secs, schema,
};
use crate::core::model::{PARSER_VERSION, SyncGranularity, SyncedTrack};
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
    Shutdown {
        now: i64,
        reply: oneshot::Sender<Result<(), CacheError>>,
    },
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
}

impl Worker {
    fn new(connection: Connection) -> Self {
        Self {
            connection,
            last_touch: HashMap::new(),
            pending_touch: HashMap::new(),
        }
    }

    fn run(mut self, rx: &mut mpsc::Receiver<CacheCommand>) {
        while let Some(command) = rx.blocking_recv() {
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
                CacheCommand::Shutdown { now, reply } => {
                    let _ = reply.send(self.flush_touches(now));
                    break;
                }
            }
        }
        let _ = self.flush_touches(now_secs());
    }

    fn lookup(&mut self, track_key: &TrackKey, now: i64) -> Result<CacheLookup, CacheError> {
        let cutoff = now.saturating_sub(CACHE_MAX_ENTRY_AGE_SECS);
        let best = {
            let mut statement = self.connection.prepare(
                "SELECT provider, parser_version, confidence, payload_json, last_accessed_at
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
                ))
            })?;

            let mut best: Option<(ProviderAnswer, i64)> = None;
            for row in rows {
                let (provider_name, parser_version, confidence, payload, last_accessed_at) =
                    match row {
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
                if is_better(&answer, best.as_ref().map(|(answer, _)| answer)) {
                    best = Some((answer, last_accessed_at));
                }
            }
            best
        };

        let Some((answer, last_accessed_at)) = best else {
            return Ok(CacheLookup { answer: None });
        };
        self.touch(track_key, answer.provider, now, last_accessed_at)?;
        Ok(CacheLookup {
            answer: Some(answer),
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

        self.connection.execute(
            "INSERT INTO lyric_entries (
                track_key, provider, label, granularity, parser_version, confidence,
                payload_json, fetched_at, last_accessed_at, upgrade_after
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, NULL)
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
                granularity_name(track.granularity),
                i64::from(track.parser_version),
                f64::from(answer.confidence),
                payload,
                now,
                now,
            ],
        )?;

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

fn finite_confidence(confidence: f32) -> f32 {
    if confidence.is_finite() {
        confidence
    } else {
        0.0
    }
}

fn granularity_name(granularity: SyncGranularity) -> &'static str {
    match granularity {
        SyncGranularity::Syllable => "syllable",
        SyncGranularity::Word => "word",
        SyncGranularity::Line => "line",
        SyncGranularity::Unsynced => "unsynced",
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;
    use crate::core::model::{LineRole, LyricLine};

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
                    granularity_name(track.granularity),
                    parser_version,
                    f64::from(track.match_confidence),
                    serde_json::to_vec(track).unwrap(),
                    last_accessed_at,
                    last_accessed_at,
                ],
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
}

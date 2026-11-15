use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::core::model::CanonicalTrackKey;
use crate::providers::sanitize::encode_offset_key;

pub const MIN_OFFSET_MS: i64 = -30_000;
pub const MAX_OFFSET_MS: i64 = 30_000;
pub const OFFSET_DEBOUNCE: Duration = Duration::from_millis(500);
const OFFSETS_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OffsetEntry {
    pub label: String,
    pub offset_ms: i64,
}

#[derive(Debug, Serialize, Deserialize)]
struct OffsetFile {
    version: u32,
    entries: BTreeMap<String, OffsetEntry>,
}

impl OffsetFile {
    fn new(entries: BTreeMap<String, OffsetEntry>) -> Self {
        Self {
            version: OFFSETS_VERSION,
            entries,
        }
    }
}

#[derive(Debug, Clone)]
pub struct OffsetStore {
    path: Option<PathBuf>,
    entries: BTreeMap<String, OffsetEntry>,
    dirty: BTreeMap<String, Option<OffsetEntry>>,
}

impl OffsetStore {
    pub fn load(path: &Path) -> Self {
        Self {
            path: Some(path.to_path_buf()),
            entries: read_entries(path),
            dirty: BTreeMap::new(),
        }
    }

    pub fn in_memory() -> Self {
        Self {
            path: None,
            entries: BTreeMap::new(),
            dirty: BTreeMap::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn offset_for(&self, key: &CanonicalTrackKey) -> i64 {
        self.entry_for(key).map_or(0, |entry| entry.offset_ms)
    }

    pub fn entry_for(&self, key: &CanonicalTrackKey) -> Option<&OffsetEntry> {
        self.entries.get(&encode_offset_key(key))
    }

    pub fn set(&mut self, key: &CanonicalTrackKey, label: &str, offset_ms: i64) {
        let offset_ms = offset_ms.clamp(MIN_OFFSET_MS, MAX_OFFSET_MS);
        let encoded = encode_offset_key(key);

        if offset_ms == 0 {
            self.entries.remove(&encoded);
            self.dirty.insert(encoded, None);
            return;
        }

        let entry = OffsetEntry {
            label: label.to_string(),
            offset_ms,
        };
        self.entries.insert(encoded.clone(), entry.clone());
        self.dirty.insert(encoded, Some(entry));
    }

    pub fn flush(&mut self) -> io::Result<()> {
        if self.dirty.is_empty() {
            return Ok(());
        }

        let Some(path) = self.path.clone() else {
            apply_dirty(&mut self.entries, &self.dirty);
            self.dirty.clear();
            return Ok(());
        };

        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent)?;
        }

        let lock_path = sibling_path(&path, ".lock");
        let _lock = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(lock_path)?;
        _lock.lock()?;

        let mut merged = read_entries(&path);
        apply_dirty(&mut merged, &self.dirty);
        write_atomic(&path, &merged)?;

        self.entries = merged;
        self.dirty.clear();
        Ok(())
    }
}

#[derive(Debug)]
pub struct DebouncedOffsetWriter {
    store: OffsetStore,
    due: Option<Instant>,
}

impl DebouncedOffsetWriter {
    pub fn new(store: OffsetStore) -> Self {
        Self { store, due: None }
    }

    pub fn offset_for(&self, key: &CanonicalTrackKey) -> i64 {
        self.store.offset_for(key)
    }

    pub fn set(&mut self, key: &CanonicalTrackKey, label: &str, offset_ms: i64) {
        self.store.set(key, label, offset_ms);
        self.due = Some(Instant::now() + OFFSET_DEBOUNCE);
    }

    pub fn poll(&mut self, now: Instant) -> io::Result<()> {
        if self.due.is_some_and(|due| now >= due) {
            self.due = None;
            return self.store.flush();
        }
        Ok(())
    }

    pub fn flush(&mut self) -> io::Result<()> {
        self.due = None;
        self.store.flush()
    }

    pub fn time_until_flush(&self, now: Instant) -> Option<Duration> {
        self.due.map(|due| due.saturating_duration_since(now))
    }
}

fn apply_dirty(
    entries: &mut BTreeMap<String, OffsetEntry>,
    dirty: &BTreeMap<String, Option<OffsetEntry>>,
) {
    for (key, entry) in dirty {
        match entry {
            Some(entry) => {
                entries.insert(key.clone(), entry.clone());
            }
            None => {
                entries.remove(key);
            }
        }
    }
}

fn read_entries(path: &Path) -> BTreeMap<String, OffsetEntry> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return BTreeMap::new(),
        Err(error) => {
            warn!(%error, path = %path.display(), "failed to read offsets file");
            return BTreeMap::new();
        }
    };

    let file = match serde_json::from_slice::<OffsetFile>(&bytes) {
        Ok(file) => file,
        Err(error) => {
            warn!(%error, path = %path.display(), "offsets file is corrupt; starting empty");
            return BTreeMap::new();
        }
    };

    if file.version != OFFSETS_VERSION {
        warn!(
            version = file.version,
            path = %path.display(),
            "offsets file has unsupported version; starting empty"
        );
        return BTreeMap::new();
    }

    file.entries
}

fn write_atomic(path: &Path, entries: &BTreeMap<String, OffsetEntry>) -> io::Result<()> {
    let temp_path = sibling_path(path, ".tmp");
    let payload =
        serde_json::to_vec_pretty(&OffsetFile::new(entries.clone())).map_err(io::Error::other)?;

    let result = (|| {
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&temp_path)?;
        file.write_all(&payload)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temp_path, path)?;
        sync_parent(path);
        Ok(())
    })();

    if result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    result
}

fn sync_parent(path: &Path) {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        && let Ok(directory) = File::open(parent)
    {
        let _ = directory.sync_all();
    }
}

fn sibling_path(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(suffix);
    PathBuf::from(value)
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
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn key(artist: &str, title: &str) -> CanonicalTrackKey {
        CanonicalTrackKey {
            norm_artist: artist.to_string(),
            norm_title: title.to_string(),
            norm_album: Some("album".to_string()),
            duration_bucket_sec: Some(200),
        }
    }

    #[test]
    fn persists_offsets_across_restart_and_zero_removes_them() {
        let dir = temp_dir("offset-roundtrip");
        let path = dir.join("offsets.json");
        let yellow = key("coldplay", "yellow");
        let mut store = OffsetStore::load(&path);

        store.set(&yellow, "Coldplay - Yellow", -200);
        store.flush().unwrap();

        let mut restarted = OffsetStore::load(&path);
        assert_eq!(restarted.offset_for(&yellow), -200);
        assert_eq!(
            restarted
                .entry_for(&yellow)
                .map(|entry| entry.label.as_str()),
            Some("Coldplay - Yellow")
        );

        restarted.set(&yellow, "Coldplay - Yellow", 0);
        restarted.flush().unwrap();

        let restarted_again = OffsetStore::load(&path);
        assert_eq!(restarted_again.offset_for(&yellow), 0);
        assert!(restarted_again.is_empty());

        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn corrupt_file_loads_empty_without_panicking() {
        let dir = temp_dir("offset-corrupt");
        let path = dir.join("offsets.json");
        fs::write(&path, "{not json").unwrap();

        let mut store = OffsetStore::load(&path);
        assert!(store.is_empty());
        store.set(&key("artist", "title"), "Artist - Title", 100);
        store.flush().unwrap();

        let restarted = OffsetStore::load(&path);
        assert_eq!(restarted.offset_for(&key("artist", "title")), 100);

        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn two_instances_merge_distinct_tracks() {
        let dir = temp_dir("offset-merge");
        let path = dir.join("offsets.json");
        let first = key("artist", "first");
        let second = key("artist", "second");

        let mut left = OffsetStore::load(&path);
        let mut right = OffsetStore::load(&path);

        left.set(&first, "Artist - First", -100);
        left.flush().unwrap();
        right.set(&second, "Artist - Second", 250);
        right.flush().unwrap();

        let restarted = OffsetStore::load(&path);
        assert_eq!(restarted.offset_for(&first), -100);
        assert_eq!(restarted.offset_for(&second), 250);

        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn explicit_flush_writes_before_debounce_elapses() {
        let dir = temp_dir("offset-explicit-flush");
        let path = dir.join("offsets.json");
        let track = key("artist", "title");
        let mut writer = DebouncedOffsetWriter::new(OffsetStore::load(&path));

        writer.set(&track, "Artist - Title", -150);
        writer.flush().unwrap();

        let restarted = OffsetStore::load(&path);
        assert_eq!(restarted.offset_for(&track), -150);

        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn clamps_offsets_and_schedules_debounce() {
        let mut writer = DebouncedOffsetWriter::new(OffsetStore::in_memory());
        let track = key("artist", "title");

        writer.set(&track, "Artist - Title", 99_000);
        let now = Instant::now();
        assert_eq!(writer.offset_for(&track), MAX_OFFSET_MS);
        assert!(writer.time_until_flush(now).is_some_and(|wait| {
            wait <= OFFSET_DEBOUNCE
                && wait > OFFSET_DEBOUNCE.saturating_sub(Duration::from_millis(50))
        }));

        writer.set(&track, "Artist - Title", 0);
        assert_eq!(writer.offset_for(&track), 0);
    }
}

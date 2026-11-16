use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, ErrorCode, OptionalExtension};
use tracing::warn;

use super::CacheError;

pub const SCHEMA_VERSION: &str = "1";

const BUSY_TIMEOUT: Duration = Duration::from_millis(2_500);
const SETUP_SQL: &str = r#"
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;

CREATE TABLE IF NOT EXISTS meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS lyric_entries (
    track_key BLOB NOT NULL,
    provider TEXT NOT NULL,
    label TEXT NOT NULL,
    granularity TEXT NOT NULL,
    parser_version INTEGER NOT NULL,
    confidence REAL NOT NULL,
    payload_json BLOB NOT NULL,
    fetched_at INTEGER NOT NULL,
    last_accessed_at INTEGER NOT NULL,
    upgrade_after INTEGER,
    PRIMARY KEY (track_key, provider)
);

CREATE INDEX IF NOT EXISTS idx_lyric_entries_last_accessed
    ON lyric_entries (last_accessed_at);

CREATE TABLE IF NOT EXISTS negative_cache (
    track_key BLOB NOT NULL,
    provider TEXT NOT NULL,
    expires_at INTEGER NOT NULL,
    PRIMARY KEY (track_key, provider)
);

CREATE INDEX IF NOT EXISTS idx_negative_cache_expires
    ON negative_cache (expires_at);
"#;

pub fn open(path: &Path) -> Result<Connection, CacheError> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)?;
    }

    match initialize(path) {
        Ok(connection) => Ok(connection),
        Err(error) if should_recover(&error) => {
            warn!(%error, path = %path.display(), "rebuilding corrupt lyric cache");
            let backup = backup_corrupt(path)?;
            warn!(backup = %backup.display(), "moved corrupt lyric cache aside");
            initialize(path)
        }
        Err(error) => Err(error),
    }
}

fn initialize(path: &Path) -> Result<Connection, CacheError> {
    let connection = Connection::open(path)?;
    connection.busy_timeout(BUSY_TIMEOUT)?;
    connection.execute_batch(SETUP_SQL)?;

    let version = connection
        .query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()?;

    match version.as_deref() {
        None => {
            connection.execute(
                "INSERT INTO meta (key, value) VALUES ('schema_version', ?1)",
                [SCHEMA_VERSION],
            )?;
            Ok(connection)
        }
        Some(SCHEMA_VERSION) => Ok(connection),
        Some(other) => Err(CacheError::UnsupportedSchemaVersion(other.to_string())),
    }
}

fn should_recover(error: &CacheError) -> bool {
    match error {
        CacheError::Database(error) => match error {
            rusqlite::Error::SqliteFailure(inner, _) => {
                matches!(
                    inner.code,
                    ErrorCode::DatabaseCorrupt | ErrorCode::NotADatabase | ErrorCode::Unknown
                )
            }
            _ => false,
        },
        CacheError::UnsupportedSchemaVersion(_) => true,
        CacheError::Io(_) | CacheError::Payload(_) | CacheError::WorkerStopped => false,
    }
}

fn backup_corrupt(path: &Path) -> Result<PathBuf, CacheError> {
    remove_old_backups(path)?;
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis());
    let file_name = path
        .file_name()
        .map_or_else(|| "lyrics_v1.db".into(), |name| name.to_os_string());
    let backup_name = format!("{}.corrupt.{stamp}", file_name.to_string_lossy());
    let backup = path.with_file_name(backup_name);

    for sidecar in ["-wal", "-shm"] {
        let sidecar_path = sidecar_path(path, sidecar);
        match fs::remove_file(&sidecar_path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }

    if path.exists() {
        fs::rename(path, &backup)?;
    }
    Ok(backup)
}

fn remove_old_backups(path: &Path) -> Result<(), CacheError> {
    let parent = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    let Some(file_name) = path.file_name() else {
        return Ok(());
    };
    let prefix = format!("{}.corrupt.", file_name.to_string_lossy());

    for entry in fs::read_dir(parent)? {
        let entry = entry?;
        let name = entry.file_name();
        if name.to_string_lossy().starts_with(&prefix) {
            fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}

fn sidecar_path(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path
        .file_name()
        .map_or_else(|| "lyrics_v1.db".into(), |name| name.to_os_string());
    name.push(suffix);
    path.with_file_name(name)
}

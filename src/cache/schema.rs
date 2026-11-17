use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, ErrorCode, OptionalExtension};
use tracing::warn;

use super::CacheError;

pub const SCHEMA_VERSION: &str = "2";

const BUSY_TIMEOUT: Duration = Duration::from_millis(2_500);

const SETUP_SQL_V2: &str = r#"
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

CREATE TABLE IF NOT EXISTS track_preferences (
    track_key BLOB PRIMARY KEY,
    preferred_provider TEXT,
    preferred_track_id TEXT,
    pinned_payload_json BLOB,
    preferred_style TEXT,
    updated_at INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_track_preferences_updated_at
    ON track_preferences (updated_at);
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
    let mut connection = Connection::open(path)?;
    connection.busy_timeout(BUSY_TIMEOUT)?;

    let has_meta: bool = connection
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'meta'",
            [],
            |_| Ok(true),
        )
        .optional()?
        .unwrap_or(false);

    if !has_meta {
        connection.execute_batch(SETUP_SQL_V2)?;
        connection.execute(
            "INSERT INTO meta (key, value) VALUES ('schema_version', ?1)",
            [SCHEMA_VERSION],
        )?;
        return Ok(connection);
    }

    let version = connection
        .query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()?;

    match version.as_deref() {
        None => {
            connection.execute_batch(SETUP_SQL_V2)?;
            connection.execute(
                "INSERT INTO meta (key, value) VALUES ('schema_version', ?1)",
                [SCHEMA_VERSION],
            )?;
            Ok(connection)
        }
        Some("1") => {
            migrate_v1_to_v2(&mut connection)?;
            Ok(connection)
        }
        Some(SCHEMA_VERSION) => {
            connection.execute_batch(SETUP_SQL_V2)?;
            Ok(connection)
        }
        Some(other) => Err(CacheError::UnsupportedSchemaVersion(other.to_string())),
    }
}

fn migrate_v1_to_v2(connection: &mut Connection) -> Result<(), CacheError> {
    let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    tx.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS track_preferences (
            track_key BLOB PRIMARY KEY,
            preferred_provider TEXT,
            preferred_track_id TEXT,
            pinned_payload_json BLOB,
            preferred_style TEXT,
            updated_at INTEGER NOT NULL
        );

        CREATE INDEX IF NOT EXISTS idx_track_preferences_updated_at
            ON track_preferences (updated_at);

        UPDATE meta SET value = '2' WHERE key = 'schema_version';
        "#,
    )?;
    tx.commit()?;
    Ok(())
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
        CacheError::UnsupportedSchemaVersion(_) => false,
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
        .map_or_else(|| "lyrics.db".into(), |name| name.to_os_string());
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
        .map_or_else(|| "lyrics.db".into(), |name| name.to_os_string());
    name.push(suffix);
    path.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_db(name: &str) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "lyrachi-schema-{name}-{}-{stamp}.db",
            std::process::id()
        ))
    }

    #[test]
    fn creates_v2_database_on_empty_path() {
        let path = temp_db("fresh-v2");
        let conn = open(&path).expect("open fresh");
        let version: String = conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'schema_version'",
                [],
                |row| row.get(0),
            )
            .expect("read version");
        assert_eq!(version, "2");

        let has_pref: bool = conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'track_preferences'",
                [],
                |_| Ok(true),
            )
            .expect("check track_preferences table");
        assert!(has_pref);
        drop(conn);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn migrates_v1_database_without_data_loss() {
        let path = temp_db("migrate-v1");
        {
            let conn = Connection::open(&path).expect("open raw sqlite");
            conn.execute_batch(
                r#"
                CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                INSERT INTO meta (key, value) VALUES ('schema_version', '1');

                CREATE TABLE lyric_entries (
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

                CREATE TABLE negative_cache (
                    track_key BLOB NOT NULL,
                    provider TEXT NOT NULL,
                    expires_at INTEGER NOT NULL,
                    PRIMARY KEY (track_key, provider)
                );

                INSERT INTO lyric_entries VALUES (
                    X'0102030405060708090A0B0C0D0E0F101112131415161718191A1B1C1D1E1F20',
                    'lrclib', 'Sample Track', 'line', 1, 0.9, X'7B7D', 1000, 1000, NULL
                );

                INSERT INTO negative_cache VALUES (
                    X'0102030405060708090A0B0C0D0E0F101112131415161718191A1B1C1D1E1F20',
                    'lyricsplus', 2000
                );
                "#,
            )
            .expect("setup v1 database");
        }

        let conn = open(&path).expect("open and migrate");
        let version: String = conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'schema_version'",
                [],
                |row| row.get(0),
            )
            .expect("read version");
        assert_eq!(version, "2");

        let entry_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM lyric_entries", [], |row| row.get(0))
            .expect("count entries");
        assert_eq!(entry_count, 1);

        let neg_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM negative_cache", [], |row| row.get(0))
            .expect("count negative");
        assert_eq!(neg_count, 1);

        let has_pref: bool = conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'track_preferences'",
                [],
                |_| Ok(true),
            )
            .expect("check track_preferences table");
        assert!(has_pref);

        drop(conn);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn future_version_is_rejected_without_deleting_database() {
        let path = temp_db("future-v99");
        {
            let conn = Connection::open(&path).expect("open raw sqlite");
            conn.execute_batch(
                r#"
                CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                INSERT INTO meta (key, value) VALUES ('schema_version', '99');
                "#,
            )
            .expect("setup future database");
        }

        let result = open(&path);
        match result {
            Err(CacheError::UnsupportedSchemaVersion(v)) => assert_eq!(v, "99"),
            other => panic!("expected UnsupportedSchemaVersion, got {other:?}"),
        }

        assert!(
            path.exists(),
            "database file must NOT be deleted or moved when version is unsupported"
        );
        let _ = fs::remove_file(&path);
    }
}

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use tokio::fs;
use tokio::sync::OnceCell;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use super::sanitize::{
    clean_artist, clean_title, normalize_for_match, split_artist_title, strip_dash_qualifier,
};
use super::{Provider, ProviderError, TrackQuery};
use crate::core::model::SyncedTrack;
use crate::core::normalize::normalize_track;
use crate::core::parse::lrc::parse_lrc;

const MAX_SCAN_DEPTH: usize = 16;

#[derive(Debug, Clone)]
struct IndexedLyricFile {
    path: PathBuf,
    stem_key: String,
    artist_title_key: Option<(String, String)>,
    is_elrc: bool,
}

#[derive(Debug, Clone, Default)]
pub struct LocalProvider {
    dirs: Vec<PathBuf>,
    index: Arc<OnceCell<Vec<IndexedLyricFile>>>,
}

impl LocalProvider {
    pub fn new(dirs: Vec<PathBuf>) -> Self {
        Self::with_dirs(resolve_local_dirs(dirs))
    }

    pub fn with_dirs(dirs: Vec<PathBuf>) -> Self {
        let mut resolved = Vec::new();
        let mut seen = HashSet::new();
        let home = home_dir();

        for dir in dirs {
            let expanded = expand_tilde_path(dir, home.as_deref());
            if !expanded.as_os_str().is_empty() && seen.insert(expanded.clone()) {
                resolved.push(expanded);
            }
        }

        Self {
            dirs: resolved,
            index: Arc::new(OnceCell::new()),
        }
    }

    pub fn dirs(&self) -> &[PathBuf] {
        &self.dirs
    }

    pub async fn find(&self, query: &TrackQuery) -> Result<Option<SyncedTrack>, ProviderError> {
        self.find_with_cancel(query, &CancellationToken::new())
            .await
    }

    pub async fn find_with_cancel(
        &self,
        query: &TrackQuery,
        cancel: &CancellationToken,
    ) -> Result<Option<SyncedTrack>, ProviderError> {
        if cancel.is_cancelled() {
            return Err(ProviderError::Cancelled);
        }
        let title_key = &query.canonical.norm_title;
        if query.title.is_empty() || title_key.is_empty() || self.dirs.is_empty() {
            return Ok(None);
        }

        let init_fut = self.index.get_or_init(|| async {
            let dirs = self.dirs.clone();
            match tokio::task::spawn_blocking(move || scan_dirs(&dirs)).await {
                Ok(files) => files,
                Err(error) => {
                    warn!(%error, "local lyric scan task failed");
                    Vec::new()
                }
            }
        });
        let index = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(ProviderError::Cancelled),
            files = init_fut => files,
        };

        let artist_key = &query.canonical.norm_artist;
        let combined_dash_artist_key = (!artist_key.is_empty() && query.artist.contains(" - "))
            .then(|| normalize_for_match(&format!("{} {}", query.artist, query.title)));

        let mut matches = index
            .iter()
            .filter_map(|file| {
                let rank = if !artist_key.is_empty()
                    && (file
                        .artist_title_key
                        .as_ref()
                        .is_some_and(|(artist, title)| artist == artist_key && title == title_key)
                        || combined_dash_artist_key.as_ref() == Some(&file.stem_key))
                {
                    0_u8
                } else if file.stem_key == *title_key && file.artist_title_key.is_none() {
                    1_u8
                } else {
                    return None;
                };

                let ext_rank = u8::from(!file.is_elrc);
                Some((rank, ext_rank, &file.path))
            })
            .collect::<Vec<_>>();

        matches.sort_by(|left, right| {
            left.0
                .cmp(&right.0)
                .then_with(|| left.1.cmp(&right.1))
                .then_with(|| left.2.cmp(right.2))
        });

        for (_, _, path) in matches {
            if cancel.is_cancelled() {
                return Err(ProviderError::Cancelled);
            }
            let read_res = tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(ProviderError::Cancelled),
                res = fs::read_to_string(path) => res,
            };
            let content = match read_res {
                Ok(content) => content,
                Err(error) => {
                    warn!(%error, path = %path.display(), "failed to read local lyric file");
                    continue;
                }
            };
            match parse_lrc(&content, query.duration_ms) {
                Ok(parsed) => {
                    let track = normalize_track(
                        parsed,
                        Provider::Local.as_str(),
                        Some(path.to_string_lossy().into_owned()),
                        1.0,
                        query.duration_ms,
                        query.user_offset_ms,
                    );
                    if !track.is_instrumental && track.lines.is_empty() {
                        warn!(path = %path.display(), "local lyric file produced no usable lines");
                        continue;
                    }
                    return Ok(Some(track));
                }
                Err(error) => {
                    warn!(%error, path = %path.display(), "invalid local lyric file");
                }
            }
        }

        Ok(None)
    }
}

pub fn resolve_local_dirs(dirs: Vec<PathBuf>) -> Vec<PathBuf> {
    if dirs.is_empty() {
        return default_local_dirs();
    }

    let home = home_dir();
    let mut resolved = Vec::new();
    let mut seen = HashSet::new();

    for dir in dirs {
        let expanded = expand_tilde_path(dir, home.as_deref());
        if !expanded.as_os_str().is_empty() && seen.insert(expanded.clone()) {
            resolved.push(expanded);
        }
    }

    resolved
}

pub fn default_local_dirs() -> Vec<PathBuf> {
    let home = home_dir();

    if let Some(xdg_music) = std::env::var_os("XDG_MUSIC_DIR")
        && let Some(path) = expand_user_dir_value(&xdg_music.to_string_lossy(), home.as_deref())
    {
        return vec![path];
    }

    let user_dirs_path = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| home.as_ref().map(|path| path.join(".config")))
        .map(|dir| dir.join("user-dirs.dirs"));

    if let Some(path) = user_dirs_path
        && let Ok(contents) = std::fs::read_to_string(path)
        && let Some(music_dir) = parse_xdg_music_dir(&contents, home.as_deref())
    {
        return vec![music_dir];
    }

    home.map(|path| vec![path.join("Music")])
        .unwrap_or_default()
}

fn scan_dirs(dirs: &[PathBuf]) -> Vec<IndexedLyricFile> {
    let started = Instant::now();
    let mut files = Vec::new();

    for dir in dirs {
        collect_lyric_files(dir, 0, &mut files);
    }

    files.sort();
    files.dedup();

    let indexed = files
        .into_iter()
        .filter_map(build_indexed_file)
        .collect::<Vec<_>>();
    let elapsed = started.elapsed();

    info!(
        files = indexed.len(),
        elapsed_ms = elapsed.as_millis() as u64,
        "indexed local lyric files"
    );

    indexed
}

fn build_indexed_file(path: PathBuf) -> Option<IndexedLyricFile> {
    let stem = path.file_stem()?.to_str()?;
    let stem_clean_dash = strip_dash_qualifier(stem);
    let stem_key = normalize_for_match(&clean_title(stem));
    let artist_title_key = split_artist_title(&stem_clean_dash)
        .map(|(artist, title)| {
            (
                normalize_for_match(&clean_artist(&artist)),
                normalize_for_match(&clean_title(&title)),
            )
        })
        .filter(|(artist, title)| !artist.is_empty() && !title.is_empty());
    let is_elrc = path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("elrc"));

    Some(IndexedLyricFile {
        path,
        stem_key,
        artist_title_key,
        is_elrc,
    })
}

fn collect_lyric_files(dir: &Path, depth: usize, files: &mut Vec<PathBuf>) {
    if depth > MAX_SCAN_DEPTH {
        return;
    }

    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };

        if file_type.is_dir() {
            collect_lyric_files(&path, depth + 1, files);
        } else if file_type.is_file() && is_lyric_file(&path) {
            files.push(path);
        }
    }
}

fn is_lyric_file(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            extension.eq_ignore_ascii_case("lrc") || extension.eq_ignore_ascii_case("elrc")
        })
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn expand_tilde_path(path: PathBuf, home: Option<&Path>) -> PathBuf {
    let raw = path.to_string_lossy();
    if raw == "~"
        && let Some(home) = home
    {
        return home.to_path_buf();
    }
    if let Some(rest) = raw.strip_prefix("~/")
        && let Some(home) = home
    {
        return home.join(rest);
    }
    path
}

fn parse_xdg_music_dir(contents: &str, home: Option<&Path>) -> Option<PathBuf> {
    for raw_line in contents.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some(value) = line.strip_prefix("XDG_MUSIC_DIR=") else {
            continue;
        };
        let value = value.trim();
        let unquoted = value
            .strip_prefix('"')
            .and_then(|inner| inner.strip_suffix('"'))
            .or_else(|| {
                value
                    .strip_prefix('\'')
                    .and_then(|inner| inner.strip_suffix('\''))
            })
            .unwrap_or(value)
            .trim();
        if let Some(path) = expand_user_dir_value(unquoted, home) {
            return Some(path);
        }
    }

    None
}

fn expand_user_dir_value(raw: &str, home: Option<&Path>) -> Option<PathBuf> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed == "$HOME" || trimmed == "~" {
        return home.map(Path::to_path_buf);
    }
    if let Some(rest) = trimmed
        .strip_prefix("$HOME/")
        .or_else(|| trimmed.strip_prefix("~/"))
    {
        if rest.is_empty() {
            return None;
        }
        return home.map(|path| path.join(rest));
    }

    Some(PathBuf::from(trimmed))
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;
    use crate::core::model::SyncGranularity;
    use crate::media::TrackMeta;

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

    fn query(title: &str, artist: &str, album: Option<&str>) -> TrackQuery {
        query_with_url(title, artist, album, None)
    }

    fn query_with_url(
        title: &str,
        artist: &str,
        album: Option<&str>,
        url: Option<String>,
    ) -> TrackQuery {
        TrackQuery::from_meta(
            &TrackMeta {
                title: title.to_string(),
                artist: artist.to_string(),
                album: album.map(str::to_string),
                duration_ms: None,
                track_id: None,
                url,
            },
            0,
        )
    }

    #[tokio::test]
    async fn finds_artist_title_file() {
        let dir = temp_dir("artist-title");
        std::fs::write(
            dir.join("Coldplay - Yellow.lrc"),
            "[ti:Yellow]\n[00:01.00]Look at the stars\n[00:03.00]Look how they shine\n",
        )
        .unwrap();

        let track = LocalProvider::with_dirs(vec![dir.clone()])
            .find(&query("Yellow", "Coldplay", None))
            .await
            .unwrap()
            .unwrap();

        assert_eq!(track.source_provider, "local");
        assert_eq!(track.lines.len(), 2);
        assert_eq!(track.lines[0].text, "Look at the stars");

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn prefers_artist_title_over_bare_title() {
        let dir = temp_dir("artist-title-priority");
        std::fs::write(dir.join("Yellow.lrc"), "[00:01.00]Wrong\n").unwrap();
        std::fs::write(dir.join("Coldplay - Yellow.lrc"), "[00:01.00]Right\n").unwrap();

        let track = LocalProvider::with_dirs(vec![dir.clone()])
            .find(&query("Yellow", "Coldplay", None))
            .await
            .unwrap()
            .unwrap();

        assert_eq!(track.lines[0].text, "Right");

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn prefers_elrc_over_lrc_within_same_match_rank() {
        let dir = temp_dir("elrc-over-lrc");
        std::fs::write(dir.join("Coldplay - Yellow.lrc"), "[00:01.00]Plain line\n").unwrap();
        std::fs::write(
            dir.join("Coldplay - Yellow.elrc"),
            "[00:01.00] <00:01.00>Look <00:01.50>stars\n",
        )
        .unwrap();

        let track = LocalProvider::with_dirs(vec![dir.clone()])
            .find(&query("Yellow", "Coldplay", None))
            .await
            .unwrap()
            .unwrap();

        assert_eq!(track.granularity, SyncGranularity::Word);
        assert_eq!(track.lines[0].text, "Look stars");

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn finds_elrc_word_timing() {
        let dir = temp_dir("elrc");
        std::fs::write(
            dir.join("Yellow.elrc"),
            "[00:01.00] <00:01.00>Look <00:01.40>at <00:01.80>stars\n",
        )
        .unwrap();

        let track = LocalProvider::with_dirs(vec![dir.clone()])
            .find(&query("Yellow", "", None))
            .await
            .unwrap()
            .unwrap();

        assert_eq!(track.granularity, SyncGranularity::Word);
        assert_eq!(track.lines[0].words.len(), 3);

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn ignores_invalid_and_non_utf8_files_then_falls_back() {
        let dir = temp_dir("invalid-and-non-utf8");
        std::fs::write(dir.join("Yellow.lrc"), [0xff, 0xfe, 0x00]).unwrap();
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("sub/Yellow.lrc"), "[00:01.00]Recovered\n").unwrap();

        let track = LocalProvider::with_dirs(vec![dir.clone()])
            .find(&query("Yellow", "", None))
            .await
            .unwrap()
            .unwrap();

        assert_eq!(track.lines[0].text, "Recovered");

        std::fs::write(dir.join("Empty.lrc"), "not a lyric file\n").unwrap();
        let missing = LocalProvider::with_dirs(vec![dir.clone()])
            .find(&query("Empty", "", None))
            .await
            .unwrap();
        assert!(missing.is_none());

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn matches_filename_with_featured_artist_and_noise() {
        let dir = temp_dir("featured-artist-file");
        std::fs::write(
            dir.join("Coldplay feat. Guest - Yellow (Official Video).lrc"),
            "[00:01.00]Matched featured file\n",
        )
        .unwrap();

        let track = LocalProvider::with_dirs(vec![dir.clone()])
            .find(&query("Yellow", "Coldplay", None))
            .await
            .unwrap()
            .unwrap();

        assert_eq!(track.lines[0].text, "Matched featured file");

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn parses_xdg_user_dirs_music_path() {
        let home = Path::new("/home/tester");
        let parsed = parse_xdg_music_dir(
            "# comment\nXDG_DOWNLOAD_DIR=\"$HOME/Downloads\"\nXDG_MUSIC_DIR=\"$HOME/Musik\"\n",
            Some(home),
        );

        assert_eq!(parsed, Some(PathBuf::from("/home/tester/Musik")));
    }
}

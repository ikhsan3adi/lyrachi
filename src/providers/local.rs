use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use tokio::fs;
use tokio::io::AsyncReadExt;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use super::sanitize::{
    clean_artist, clean_title, normalize_for_match, split_artist_title, strip_dash_qualifier,
};
use super::{CandidateSummary, Provider, ProviderAnswer, ProviderError, Tier, TrackQuery};
use crate::core::normalize::normalize_track;
use crate::core::parse::lrc::parse_lrc;

const MAX_SCAN_DEPTH: usize = 16;
pub const MAX_LOCAL_LYRIC_BYTES: u64 = 256 * 1024;

#[derive(Debug, Clone)]
struct IndexedLyricFile {
    path: PathBuf,
    stem_key: String,
    artist_title_key: Option<(String, String)>,
    is_elrc: bool,
}

type IndexWatchSlot = Arc<Mutex<Option<watch::Receiver<Option<Arc<Vec<IndexedLyricFile>>>>>>>;

#[derive(Debug, Clone, Default)]
pub struct LocalProvider {
    dirs: Vec<PathBuf>,
    index: IndexWatchSlot,
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
            index: Arc::new(Mutex::new(None)),
        }
    }

    pub fn dirs(&self) -> &[PathBuf] {
        &self.dirs
    }

    pub async fn find(&self, query: &TrackQuery) -> Result<Option<ProviderAnswer>, ProviderError> {
        self.find_with_cancel(query, &CancellationToken::new())
            .await
    }

    pub async fn find_with_cancel(
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

        if let Some(audio_path) = query.track_url.as_deref().and_then(file_url_path)
            && let Some(answer) = self.find_sidecar(&audio_path, query, cancel).await?
        {
            return Ok(Some(answer));
        }

        if self.dirs.is_empty() {
            return Ok(None);
        }

        let index = self.get_index(cancel).await?;
        let matches = collect_index_matches(&index, query);

        for (_, _, file) in matches {
            if let Some(answer) = read_and_parse_local(
                &file.path,
                query.duration_ms,
                query.user_offset_ms,
                1.0,
                cancel,
            )
            .await?
            {
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

        let mut results = Vec::new();
        let mut seen = HashSet::new();

        if let Some(audio_path) = query.track_url.as_deref().and_then(file_url_path)
            && let Some(stem) = audio_path.file_stem().and_then(|stem| stem.to_str())
        {
            for extension in ["elrc", "lrc"] {
                if cancel.is_cancelled() {
                    return Err(ProviderError::Cancelled);
                }
                let path = audio_path.with_file_name(format!("{stem}.{extension}"));
                if file_exists(&path, cancel).await? && seen.insert(path.clone()) {
                    results.push(build_local_summary(path, query, extension == "elrc", 1.0));
                }
            }
        }

        if !self.dirs.is_empty() {
            let index = self.get_index(cancel).await?;
            for (rank, _, file) in collect_index_matches(&index, query) {
                if cancel.is_cancelled() {
                    return Err(ProviderError::Cancelled);
                }
                if seen.insert(file.path.clone()) {
                    let confidence = if rank == 0 { 0.95 } else { 0.85 };
                    results.push(build_local_summary(
                        file.path.clone(),
                        query,
                        file.is_elrc,
                        confidence,
                    ));
                }
            }
        }

        Ok(results)
    }

    pub async fn hydrate_candidate(
        &self,
        summary: &CandidateSummary,
        cancel: &CancellationToken,
    ) -> Result<Option<ProviderAnswer>, ProviderError> {
        if cancel.is_cancelled() {
            return Err(ProviderError::Cancelled);
        }
        let Some(path) = summary.local_path.as_deref() else {
            return Ok(None);
        };
        read_and_parse_local(
            path,
            summary.query_duration_ms.or(summary.duration_ms),
            summary.query_user_offset_ms,
            summary.confidence,
            cancel,
        )
        .await
    }

    async fn get_index(
        &self,
        cancel: &CancellationToken,
    ) -> Result<Arc<Vec<IndexedLyricFile>>, ProviderError> {
        let mut rx = {
            let mut slot = self.index.lock().expect("local index mutex poisoned");
            if let Some(rx) = slot.as_ref() {
                if let Some(files) = rx.borrow().clone() {
                    return Ok(files);
                }
                rx.clone()
            } else {
                let (tx, rx) = watch::channel(None);
                let dirs = self.dirs.clone();
                tokio::spawn(async move {
                    let files = match tokio::task::spawn_blocking(move || scan_dirs(&dirs)).await {
                        Ok(files) => files,
                        Err(error) => {
                            warn!(%error, "local lyric scan task failed");
                            Vec::new()
                        }
                    };
                    let _ = tx.send(Some(Arc::new(files)));
                });
                *slot = Some(rx.clone());
                rx
            }
        };

        tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(ProviderError::Cancelled),
            res = rx.wait_for(|value| value.is_some()) => match res {
                Ok(guard) => Ok(guard
                    .as_ref()
                    .cloned()
                    .unwrap_or_else(|| Arc::new(Vec::new()))),
                Err(_) => Ok(Arc::new(Vec::new())),
            }
        }
    }

    async fn find_sidecar(
        &self,
        audio_path: &Path,
        query: &TrackQuery,
        cancel: &CancellationToken,
    ) -> Result<Option<ProviderAnswer>, ProviderError> {
        let Some(stem) = audio_path.file_stem().and_then(|stem| stem.to_str()) else {
            return Ok(None);
        };

        for extension in ["elrc", "lrc"] {
            let path = audio_path.with_file_name(format!("{stem}.{extension}"));
            if let Some(answer) =
                read_and_parse_local(&path, query.duration_ms, query.user_offset_ms, 1.0, cancel)
                    .await?
            {
                return Ok(Some(answer));
            }
        }

        Ok(None)
    }
}

fn collect_index_matches<'a>(
    index: &'a [IndexedLyricFile],
    query: &TrackQuery,
) -> Vec<(u8, u8, &'a IndexedLyricFile)> {
    let title_key = &query.canonical.norm_title;
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
            Some((rank, ext_rank, file))
        })
        .collect::<Vec<_>>();

    matches.sort_by(|left, right| {
        left.0
            .cmp(&right.0)
            .then_with(|| left.1.cmp(&right.1))
            .then_with(|| left.2.path.cmp(&right.2.path))
    });
    matches
}

fn build_local_summary(
    path: PathBuf,
    query: &TrackQuery,
    is_elrc: bool,
    confidence: f32,
) -> CandidateSummary {
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .map(str::to_string)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| query.title.clone());
    let path_str = path.to_string_lossy().into_owned();
    CandidateSummary {
        provider: Provider::Local,
        source_track_id: Some(path_str),
        title: stem,
        artist: (!query.artist.is_empty()).then(|| query.artist.clone()),
        album: query.album.clone(),
        duration_ms: query.duration_ms,
        tier: if is_elrc { Tier::Word } else { Tier::Line },
        confidence,
        local_path: Some(path),
        browse_hint: None,
        query_duration_ms: query.duration_ms,
        query_user_offset_ms: query.user_offset_ms,
        hydrated: None,
    }
}

async fn file_exists(path: &Path, cancel: &CancellationToken) -> Result<bool, ProviderError> {
    let meta = tokio::select! {
        biased;
        _ = cancel.cancelled() => return Err(ProviderError::Cancelled),
        result = fs::metadata(path) => result,
    };
    Ok(meta.is_ok_and(|m| m.is_file() && m.len() > 0 && m.len() <= MAX_LOCAL_LYRIC_BYTES))
}

async fn read_and_parse_local(
    path: &Path,
    duration_ms: Option<u64>,
    user_offset_ms: i64,
    confidence: f32,
    cancel: &CancellationToken,
) -> Result<Option<ProviderAnswer>, ProviderError> {
    if cancel.is_cancelled() {
        return Err(ProviderError::Cancelled);
    }
    let Some(content) = read_lyric_text(path, cancel).await? else {
        return Ok(None);
    };
    match parse_lrc(&content, duration_ms) {
        Ok(parsed) => {
            let track = normalize_track(
                parsed,
                Provider::Local.as_str(),
                Some(path.to_string_lossy().into_owned()),
                confidence,
                duration_ms,
                user_offset_ms,
            );
            if track.is_instrumental || !track.lines.is_empty() {
                return Ok(Some(ProviderAnswer::new(Provider::Local, track)));
            }
            warn!(path = %path.display(), "local lyric file produced no usable lines");
        }
        Err(error) => {
            warn!(%error, path = %path.display(), "invalid local lyric file");
        }
    }
    Ok(None)
}

async fn read_lyric_text(
    path: &Path,
    cancel: &CancellationToken,
) -> Result<Option<String>, ProviderError> {
    let metadata = tokio::select! {
        biased;
        _ = cancel.cancelled() => return Err(ProviderError::Cancelled),
        result = fs::metadata(path) => result,
    };

    let metadata = match metadata {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            warn!(%error, path = %path.display(), "failed to inspect local lyric file");
            return Ok(None);
        }
    };

    if metadata.len() > MAX_LOCAL_LYRIC_BYTES {
        return Err(ProviderError::Invalid(format!(
            "local lyric file {} is {} bytes, limit is {MAX_LOCAL_LYRIC_BYTES}",
            path.display(),
            metadata.len()
        )));
    }

    let file = tokio::select! {
        biased;
        _ = cancel.cancelled() => return Err(ProviderError::Cancelled),
        result = fs::File::open(path) => result,
    };

    let file = match file {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            warn!(%error, path = %path.display(), "failed to open local lyric file");
            return Ok(None);
        }
    };

    let mut reader = file.take(MAX_LOCAL_LYRIC_BYTES + 1);
    let mut bytes = Vec::with_capacity(metadata.len().min(MAX_LOCAL_LYRIC_BYTES) as usize);
    let read = tokio::select! {
        biased;
        _ = cancel.cancelled() => return Err(ProviderError::Cancelled),
        result = reader.read_to_end(&mut bytes) => result,
    };

    match read {
        Ok(length) if length as u64 > MAX_LOCAL_LYRIC_BYTES => {
            Err(ProviderError::Invalid(format!(
                "local lyric file {} exceeds {MAX_LOCAL_LYRIC_BYTES} bytes",
                path.display()
            )))
        }
        Ok(_) => match String::from_utf8(bytes) {
            Ok(content) => Ok(Some(content)),
            Err(error) => {
                warn!(%error, path = %path.display(), "local lyric file is not valid UTF-8");
                Ok(None)
            }
        },
        Err(error) => {
            warn!(%error, path = %path.display(), "failed to read local lyric file");
            Ok(None)
        }
    }
}

fn file_url_path(raw: &str) -> Option<PathBuf> {
    let rest = raw.strip_prefix("file://")?;
    let (authority, path) = match rest.find('/') {
        Some(0) => ("", rest),
        Some(index) => (&rest[..index], &rest[index..]),
        None => return None,
    };

    if !authority.is_empty() && !authority.eq_ignore_ascii_case("localhost") {
        return None;
    }

    let decoded = percent_decode(path)?;
    if decoded.contains('\0') {
        return None;
    }
    let path = PathBuf::from(decoded);
    if !path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, Component::ParentDir | Component::CurDir))
    {
        return None;
    }

    Some(path)
}

fn percent_decode(raw: &str) -> Option<String> {
    let bytes = raw.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;

    while index < bytes.len() {
        if bytes[index] == b'%' {
            let high = *bytes.get(index + 1)?;
            let low = *bytes.get(index + 2)?;
            let value = hex_value(high)?
                .checked_mul(16)?
                .checked_add(hex_value(low)?)?;
            decoded.push(value);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }

    String::from_utf8(decoded).ok()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
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

        assert_eq!(track.track.source_provider, "local");
        assert_eq!(track.track.lines.len(), 2);
        assert_eq!(track.track.lines[0].text, "Look at the stars");

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

        assert_eq!(track.track.lines[0].text, "Right");

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

        assert_eq!(track.track.granularity, SyncGranularity::Word);
        assert_eq!(track.track.lines[0].text, "Look stars");

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

        assert_eq!(track.track.granularity, SyncGranularity::Word);
        assert_eq!(track.track.lines[0].words.len(), 3);

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

        assert_eq!(track.track.lines[0].text, "Recovered");

        std::fs::write(dir.join("Empty.lrc"), "not a lyric file\n").unwrap();
        let missing = LocalProvider::with_dirs(vec![dir.clone()])
            .find(&query("Empty", "", None))
            .await
            .unwrap();
        assert!(missing.is_none());

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn rejects_local_lyric_file_over_size_limit() {
        let dir = temp_dir("oversized");
        std::fs::write(
            dir.join("Yellow.lrc"),
            vec![b'x'; MAX_LOCAL_LYRIC_BYTES as usize + 1],
        )
        .unwrap();

        let result = LocalProvider::with_dirs(vec![dir.clone()])
            .find(&query("Yellow", "", None))
            .await;

        assert!(matches!(result, Err(ProviderError::Invalid(_))));

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

        assert_eq!(track.track.lines[0].text, "Matched featured file");

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn sidecar_file_url_wins_over_music_directory_index() {
        let audio_dir = temp_dir("sidecar-audio");
        let music_dir = temp_dir("sidecar-music");
        let audio = audio_dir.join("Coldplay - Yellow.flac");
        std::fs::write(&audio, []).unwrap();
        std::fs::write(
            audio_dir.join("Coldplay - Yellow.elrc"),
            "[00:01.00] <00:01.00>Sidecar <00:01.40>wins\n",
        )
        .unwrap();
        std::fs::write(
            audio_dir.join("Coldplay - Yellow.lrc"),
            "[00:01.00]Plain sidecar\n",
        )
        .unwrap();
        std::fs::write(
            music_dir.join("Coldplay - Yellow.lrc"),
            "[00:01.00]Indexed copy\n",
        )
        .unwrap();

        let url = format!("file://{}", audio.display().to_string().replace(' ', "%20"));
        let track = LocalProvider::with_dirs(vec![music_dir.clone()])
            .find(&query_with_url("Yellow", "Coldplay", None, Some(url)))
            .await
            .unwrap()
            .unwrap();

        assert_eq!(track.track.lines[0].text, "Sidecar wins");
        assert!(
            track
                .track
                .source_track_id
                .as_deref()
                .is_some_and(|path| path.ends_with("Coldplay - Yellow.elrc"))
        );

        std::fs::remove_dir_all(audio_dir).unwrap();
        std::fs::remove_dir_all(music_dir).unwrap();
    }

    #[tokio::test]
    async fn rejects_non_file_urls_and_path_traversal() {
        let empty_dir = temp_dir("sidecar-empty");
        let audio_dir = temp_dir("sidecar-traversal");
        let outside_dir = temp_dir("sidecar-outside");
        std::fs::write(
            outside_dir.join("Song.elrc"),
            "[00:01.00] <00:01.00>Outside\n",
        )
        .unwrap();

        let provider = LocalProvider::with_dirs(vec![empty_dir.clone()]);
        for url in [
            "https://example.com/Song.flac".to_string(),
            "rtsp://example.com/Song.flac".to_string(),
            "Song.flac".to_string(),
            format!(
                "file://{}/../{}/Song.flac",
                audio_dir.display(),
                outside_dir.file_name().unwrap().to_string_lossy()
            ),
        ] {
            let track = provider
                .find(&query_with_url("Song", "Artist", None, Some(url)))
                .await
                .unwrap();
            assert!(track.is_none());
        }

        std::fs::remove_dir_all(empty_dir).unwrap();
        std::fs::remove_dir_all(audio_dir).unwrap();
        std::fs::remove_dir_all(outside_dir).unwrap();
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

    #[tokio::test]
    async fn search_candidates_exposes_sidecar_and_all_index_matches_with_dedup_and_hydration() {
        let dir = temp_dir("local-multi-candidates");
        let audio_path = dir.join("Song.flac");
        let sidecar_elrc = dir.join("Song.elrc");
        let sidecar_lrc = dir.join("Song.lrc");
        let artist_lrc = dir.join("Artist - Song.lrc");

        std::fs::write(&audio_path, b"audio").unwrap();
        std::fs::write(&sidecar_elrc, "[00:01.00]<00:01.00>Sidecar eLRC\n").unwrap();
        std::fs::write(&sidecar_lrc, "[00:01.00]Sidecar LRC\n").unwrap();
        std::fs::write(&artist_lrc, "[00:01.00]Artist LRC\n").unwrap();

        let provider = LocalProvider::with_dirs(vec![dir.clone()]);
        let q = query_with_url(
            "Song",
            "Artist",
            None,
            Some(format!("file://{}", audio_path.display())),
        );
        let cancel = CancellationToken::new();

        let candidates = provider.search_candidates(&q, &cancel).await.unwrap();
        assert_eq!(candidates.len(), 3);
        assert!(candidates.iter().all(|c| !c.is_hydrated()));

        let hydrated = provider
            .hydrate_candidate(&candidates[2], &cancel)
            .await
            .unwrap()
            .expect("should hydrate selected local file");
        assert_eq!(hydrated.provider, Provider::Local);
        assert_eq!(hydrated.track.lines[0].text, "Artist LRC");

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn cancelling_first_index_waiter_keeps_shared_scan_reusable() {
        let dir = temp_dir("local-cancel-reuse");
        let lrc = dir.join("Artist - Song.lrc");
        std::fs::write(&lrc, "[00:01.00]Reusable index line\n").unwrap();

        let provider = LocalProvider::with_dirs(vec![dir.clone()]);
        let q = query("Song", "Artist", None);
        let pre_cancelled = CancellationToken::new();
        pre_cancelled.cancel();

        // Trigger/observe first request cancellation
        let err = provider
            .search_candidates(&q, &pre_cancelled)
            .await
            .unwrap_err();
        assert!(matches!(err, ProviderError::Cancelled));

        // Cancel mid-flight after initiating background scan
        let mid_cancel = CancellationToken::new();
        mid_cancel.cancel();
        let _ = provider.get_index(&mid_cancel).await;

        // Subsequent search reuses completed shared index
        let found = provider
            .search_candidates(&q, &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].title, "Artist - Song");

        std::fs::remove_dir_all(dir).unwrap();
    }
}

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use directories::BaseDirs;
use tokio::fs;
use tokio::io::AsyncReadExt;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use super::sanitize::{
    clean_artist, clean_title, normalize_for_match, split_artist_title, strip_dash_qualifier,
};
use super::{
    CandidateSummary, LyricProvider, Provider, ProviderAnswer, ProviderCapabilities, ProviderError,
    Tier, TrackQuery,
};
use crate::core::normalize::normalize_track;
use crate::core::parse::krc::parse_krc;
use crate::core::parse::lrc::parse_lrc;
use crate::core::parse::qrc::parse_qrc;
use crate::core::parse::ttml::parse_ttml;
use crate::core::parse::yrc::parse_yrc;

const LYRIC_EXTENSIONS: [&str; 6] = ["elrc", "lrc", "ttml", "krc", "qrc", "yrc"];
const MAX_SCAN_DEPTH: usize = 16;
pub const MAX_LOCAL_LYRIC_BYTES: u64 = 256 * 1024;

#[derive(Debug, Clone)]
struct IndexedLyricFile {
    path: PathBuf,
    stem_key: String,
    artist_title_key: Option<(String, String)>,
    tier: Tier,
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

    pub fn invalidate_index(&self) {
        if let Ok(mut slot) = self.index.lock() {
            *slot = None;
        }
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
            for extension in LYRIC_EXTENSIONS {
                if cancel.is_cancelled() {
                    return Err(ProviderError::Cancelled);
                }
                let path = audio_path.with_file_name(format!("{stem}.{extension}"));
                if file_exists(&path, cancel).await? && seen.insert(path.clone()) {
                    results.push(build_local_summary(
                        path,
                        query,
                        extension_tier(extension),
                        1.0,
                    ));
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
                        file.tier,
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

        for extension in LYRIC_EXTENSIONS {
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

impl LyricProvider for LocalProvider {
    fn id(&self) -> Provider {
        Provider::Local
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            supports_search: true,
            needs_hydration: true,
            max_candidates: usize::MAX,
        }
    }

    fn resolve<'a>(
        &'a self,
        query: &'a TrackQuery,
        cancel: &'a CancellationToken,
    ) -> crate::media::BoxFuture<'a, Result<Option<ProviderAnswer>, ProviderError>> {
        Box::pin(self.find_with_cancel(query, cancel))
    }

    fn search_candidates<'a>(
        &'a self,
        query: &'a TrackQuery,
        cancel: &'a CancellationToken,
    ) -> crate::media::BoxFuture<'a, Result<Vec<CandidateSummary>, ProviderError>> {
        Box::pin(self.search_candidates(query, cancel))
    }

    fn hydrate_candidate<'a>(
        &'a self,
        summary: &'a CandidateSummary,
        _limiter: Option<&'a crate::providers::engine::TokenBucketRateLimiter>,
        cancel: &'a CancellationToken,
    ) -> crate::media::BoxFuture<'a, Result<Option<ProviderAnswer>, ProviderError>> {
        Box::pin(self.hydrate_candidate(summary, cancel))
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

            let ext_rank = u8::from(file.tier != Tier::Word);
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

fn extension_tier(extension: &str) -> Tier {
    match extension.to_ascii_lowercase().as_str() {
        "lrc" => Tier::Line,
        _ => Tier::Word,
    }
}

fn build_local_summary(
    path: PathBuf,
    query: &TrackQuery,
    tier: Tier,
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
        tier,
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
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| extension.to_ascii_lowercase())
        .unwrap_or_default();
    let parsed = match extension.as_str() {
        "ttml" => match parse_ttml(&content, Provider::Local.as_str()) {
            Ok(mut track) => {
                track.match_confidence = confidence;
                track.user_offset_ms = user_offset_ms;
                track.source_track_id = Some(path.to_string_lossy().into_owned());
                if track.is_instrumental || !track.lines.is_empty() {
                    return Ok(Some(ProviderAnswer::new(Provider::Local, track)));
                }
                warn!(path = %path.display(), "local lyric file produced no usable lines");
                return Ok(None);
            }
            Err(error) => {
                warn!(%error, path = %path.display(), "invalid local lyric file");
                return Ok(None);
            }
        },
        "krc" => parse_krc(&content, duration_ms),
        "qrc" => parse_qrc(&content, duration_ms),
        "yrc" => parse_yrc(&content, duration_ms),
        _ => parse_lrc(&content, duration_ms),
    };
    match parsed {
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
    let decoded = strip_windows_drive_slash(&decoded);
    // Check the raw segments: `Path::components` drops `.` segments before a
    // components walk could see them, and the result differed per platform.
    if decoded
        .split(['/', '\\'])
        .any(|segment| matches!(segment, "." | ".."))
    {
        return None;
    }
    let path = PathBuf::from(decoded);
    if !path.is_absolute() {
        return None;
    }

    Some(path)
}

/// Windows file URLs keep the drive behind the root slash (`file:///D:/...`),
/// while the absolute form on this platform is `D:/...`. Applied everywhere so
/// the parser behaves identically on every platform; the only casualty is a
/// Unix directory literally named `D:`, which no real library has.
fn strip_windows_drive_slash(decoded: &str) -> &str {
    if let Some(rest) = decoded.strip_prefix('/') {
        let bytes = rest.as_bytes();
        if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
            return rest;
        }
    }
    decoded
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
    crate::paths::default_music_dirs()
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
    let tier = path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(extension_tier)
        .unwrap_or(Tier::Line);

    Some(IndexedLyricFile {
        path,
        stem_key,
        artist_title_key,
        tier,
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
            let extension = extension.to_ascii_lowercase();
            LYRIC_EXTENSIONS.contains(&extension.as_str())
        })
}

fn home_dir() -> Option<PathBuf> {
    BaseDirs::new().map(|base| base.home_dir().to_path_buf())
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

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;
    use crate::core::model::{AnnotationKind, LineRole, SyncGranularity};
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

    /// Build a spec-correct `file://` URL for `path` on every platform:
    /// forward slashes, percent-encoded, and the drive behind the root slash
    /// (`file:///D:/...`). Hand-rolled so the fixtures need no URL crate.
    fn file_url(path: &std::path::Path) -> String {
        let text = path.to_string_lossy().replace('\\', "/");
        let encoded: String = text
            .bytes()
            .map(|byte| match byte {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'-' | b'_' | b'.' | b'~' => {
                    (byte as char).to_string()
                }
                other => format!("%{other:02X}"),
            })
            .collect();
        let slashed = if encoded.starts_with('/') {
            encoded
        } else {
            format!("/{encoded}")
        };
        format!("file://{slashed}")
    }

    #[test]
    fn rejects_non_local_authorities() {
        assert_eq!(file_url_path("file://D:/Music/Song.flac"), None);
        assert_eq!(file_url_path("file://server/share/Song.flac"), None);
    }

    #[test]
    fn rejects_traversal_in_file_urls() {
        assert_eq!(file_url_path("file:///music/./Song.flac"), None);
        assert_eq!(file_url_path("file:///music/../etc/Song.flac"), None);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn parses_windows_drive_file_urls() {
        assert_eq!(
            file_url_path("file:///D:/Music/Coldplay/Song.flac"),
            Some(PathBuf::from("D:/Music/Coldplay/Song.flac"))
        );
        // Encoded spaces survive the drive rewrite.
        assert_eq!(
            file_url_path("file:///D:/My%20Music/Song.flac"),
            Some(PathBuf::from("D:/My Music/Song.flac"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn parses_unix_file_urls() {
        assert_eq!(
            file_url_path("file:///music/Coldplay/Song.flac"),
            Some(PathBuf::from("/music/Coldplay/Song.flac"))
        );
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
    async fn parses_ttml_sidecar_with_background_vocals() {
        let dir = temp_dir("ttml-sidecar");
        std::fs::write(
            dir.join("Yellow.ttml"),
            include_str!("../../tests/fixtures/sample_syllable.ttml"),
        )
        .unwrap();

        let track = LocalProvider::with_dirs(vec![dir.clone()])
            .find(&query("Yellow", "", None))
            .await
            .unwrap()
            .unwrap();

        assert_eq!(track.track.source_provider, "local");
        assert_eq!(track.track.granularity, SyncGranularity::Syllable);
        assert!(
            track
                .track
                .lines
                .iter()
                .any(|line| line.role == LineRole::BackgroundVocal)
        );
        assert!(track.track.lines.iter().any(|line| {
            line.annotations
                .iter()
                .any(|annotation| annotation.kind == AnnotationKind::Background)
        }));

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn parses_krc_sidecar() {
        let dir = temp_dir("krc-sidecar");
        std::fs::write(
            dir.join("Yellow.krc"),
            "[1000,2000]<0,500,0>Hel<500,500,0>lo <1000,1000,0>you\n",
        )
        .unwrap();

        let track = LocalProvider::with_dirs(vec![dir.clone()])
            .find(&query("Yellow", "", None))
            .await
            .unwrap()
            .unwrap();

        assert_eq!(track.track.granularity, SyncGranularity::Syllable);
        assert_eq!(track.track.lines[0].text, "Hello you");

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

        let url = file_url(&audio);
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
                "{}/../{}/Song.flac",
                file_url(&audio_dir),
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
        let q = query_with_url("Song", "Artist", None, Some(file_url(&audio_path)));
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

    #[tokio::test]
    async fn invalidate_index_rescans_newly_added_files() {
        let dir = temp_dir("local-invalidate-test");
        std::fs::write(dir.join("one.lrc"), "[00:01.00] One\n").unwrap();

        let provider = LocalProvider::with_dirs(vec![dir.clone()]);
        let cancel = CancellationToken::new();

        let initial = provider.get_index(&cancel).await.unwrap();
        assert_eq!(initial.len(), 1);

        // Add a second file while index is cached
        std::fs::write(dir.join("two.lrc"), "[00:01.00] Two\n").unwrap();

        let cached = provider.get_index(&cancel).await.unwrap();
        assert_eq!(cached.len(), 1);

        // Invalidate and rescan
        provider.invalidate_index();
        let refreshed = provider.get_index(&cancel).await.unwrap();
        assert_eq!(refreshed.len(), 2);

        std::fs::remove_dir_all(dir).unwrap();
    }
}

use std::path::{Path, PathBuf};

use tokio::fs;
use tracing::warn;

use super::sanitize::{clean_text, normalize_for_match};
use super::{Provider, ProviderError, TrackQuery};
use crate::core::model::SyncedTrack;
use crate::core::normalize::normalize_track;
use crate::core::parse::lrc::parse_lrc;

#[derive(Debug, Clone, Default)]
pub struct LocalProvider {
    dirs: Vec<PathBuf>,
}

impl LocalProvider {
    pub fn new(dirs: Vec<PathBuf>) -> Self {
        Self { dirs }
    }

    pub async fn find(&self, query: &TrackQuery) -> Result<Option<SyncedTrack>, ProviderError> {
        if query.title.is_empty() {
            return Ok(None);
        }

        let title_key = normalize_for_match(&query.title);
        let artist_title_key = (!query.artist.is_empty())
            .then(|| normalize_for_match(&format!("{} - {}", query.artist, query.title)));
        let mut files = Vec::new();

        for dir in &self.dirs {
            collect_lyric_files(dir, &mut files);
        }
        files.sort();

        let mut matches = files
            .into_iter()
            .filter_map(|path| {
                let stem = path.file_stem()?.to_str()?;
                let key = normalize_for_match(&clean_text(stem));
                let rank = if artist_title_key.as_ref() == Some(&key) {
                    0
                } else if key == title_key {
                    1
                } else {
                    return None;
                };

                Some((rank, path))
            })
            .collect::<Vec<_>>();

        matches.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));

        for (_, path) in matches {
            let content = fs::read_to_string(&path).await?;
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

fn collect_lyric_files(dir: &Path, files: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };

        if file_type.is_dir() {
            collect_lyric_files(&path, files);
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
        TrackQuery::from_meta(
            &TrackMeta {
                title: title.to_string(),
                artist: artist.to_string(),
                album: album.map(str::to_string),
                duration_ms: None,
                track_id: None,
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

        let track = LocalProvider::new(vec![dir.clone()])
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

        let track = LocalProvider::new(vec![dir.clone()])
            .find(&query("Yellow", "Coldplay", None))
            .await
            .unwrap()
            .unwrap();

        assert_eq!(track.lines[0].text, "Right");

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

        let track = LocalProvider::new(vec![dir.clone()])
            .find(&query("Yellow", "", None))
            .await
            .unwrap()
            .unwrap();

        assert_eq!(track.granularity, SyncGranularity::Word);
        assert_eq!(track.lines[0].words.len(), 3);

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn ignores_invalid_files_and_returns_none() {
        let dir = temp_dir("invalid");
        std::fs::write(dir.join("Yellow.lrc"), "not a lyric file\n").unwrap();

        let track = LocalProvider::new(vec![dir.clone()])
            .find(&query("Yellow", "", None))
            .await
            .unwrap();

        assert!(track.is_none());

        std::fs::remove_dir_all(dir).unwrap();
    }
}

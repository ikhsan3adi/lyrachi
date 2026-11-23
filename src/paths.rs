use std::path::{Path, PathBuf};

use directories::{BaseDirs, ProjectDirs, UserDirs};

const APP_NAME: &str = "lyrachi";
const CONFIG_FILE: &str = "config.toml";
const OFFSETS_FILE: &str = "offsets.json";
const CACHE_FILE: &str = "lyrics_v1.db";
const LOG_FILE: &str = "lyrachi.log";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PathOverrides {
    pub config_file: Option<PathBuf>,
    pub log_file: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppPaths {
    pub config_file: PathBuf,
    pub styles_dir: PathBuf,
    pub offsets_file: PathBuf,
    pub cache_db_file: PathBuf,
    pub log_file: PathBuf,
    pub default_music_dirs: Vec<PathBuf>,
}

impl AppPaths {
    pub fn resolve(overrides: PathOverrides) -> Self {
        Self::resolve_with_base(overrides, base_dir_override().as_deref())
    }

    fn resolve_with_base(overrides: PathOverrides, base: Option<&Path>) -> Self {
        if let Some(base) = base {
            let config_file = overrides
                .config_file
                .unwrap_or_else(|| base.join("config").join(CONFIG_FILE));
            return Self {
                styles_dir: config_file
                    .parent()
                    .map(|dir| dir.join("styles"))
                    .unwrap_or_else(|| base.join("config").join("styles")),
                config_file,
                offsets_file: base.join("data").join(OFFSETS_FILE),
                cache_db_file: base.join("cache").join(CACHE_FILE),
                log_file: overrides
                    .log_file
                    .unwrap_or_else(|| base.join("state").join(LOG_FILE)),
                default_music_dirs: vec![base.join("Music")],
            };
        }

        let project = ProjectDirs::from("", "", APP_NAME);
        let base = BaseDirs::new();
        let home = base
            .as_ref()
            .map(|base| base.home_dir().to_path_buf())
            .or_else(home_dir);

        let config_file = project
            .as_ref()
            .map(|project| project.config_dir().join(CONFIG_FILE))
            .unwrap_or_else(|| fallback_path(home.as_deref(), ".config", CONFIG_FILE));
        let resolved_config = overrides.config_file.unwrap_or(config_file);
        let styles_dir = resolved_config
            .parent()
            .map(|dir| dir.join("styles"))
            .unwrap_or_else(|| fallback_path(home.as_deref(), ".config", "styles"));
        let offsets_file = project
            .as_ref()
            .map(|project| project.data_dir().join(OFFSETS_FILE))
            .unwrap_or_else(|| fallback_path(home.as_deref(), ".local/share", OFFSETS_FILE));
        let cache_db_file = project
            .as_ref()
            .map(|project| project.cache_dir().join(CACHE_FILE))
            .unwrap_or_else(|| fallback_path(home.as_deref(), ".cache", CACHE_FILE));
        let log_file = project
            .as_ref()
            .and_then(ProjectDirs::state_dir)
            .map(|state_dir| state_dir.join(LOG_FILE))
            .unwrap_or_else(|| fallback_path(home.as_deref(), ".local/state", LOG_FILE));

        Self {
            config_file: resolved_config,
            styles_dir,
            offsets_file,
            cache_db_file,
            log_file: overrides.log_file.unwrap_or(log_file),
            default_music_dirs: default_music_dirs(),
        }
    }

    pub fn lines(&self) -> Vec<(&'static str, String)> {
        let music = if self.default_music_dirs.is_empty() {
            "-".to_string()
        } else {
            self.default_music_dirs
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        };

        vec![
            ("config", self.config_file.display().to_string()),
            ("styles", self.styles_dir.display().to_string()),
            ("offsets", self.offsets_file.display().to_string()),
            ("cache", self.cache_db_file.display().to_string()),
            ("log", self.log_file.display().to_string()),
            ("music", music),
        ]
    }
}

fn fallback_path(home: Option<&Path>, relative: &str, file: &str) -> PathBuf {
    home.map_or_else(
        || PathBuf::from(relative).join(APP_NAME).join(file),
        |home| home.join(relative).join(APP_NAME).join(file),
    )
}

/// `LYRACHI_HOME` replaces the platform base directory with one tree:
/// `config/`, `data/`, `cache/`, `state/`, and `Music`. This is the seam that
/// keeps spawned-binary tests hermetic on every platform, and doubles as the
/// portable-install story. `--config` still wins for the config file.
fn base_dir_override() -> Option<PathBuf> {
    std::env::var_os("LYRACHI_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

pub fn default_music_dirs() -> Vec<PathBuf> {
    let user_dirs = UserDirs::new();
    let home = BaseDirs::new().map(|base| base.home_dir().to_path_buf());
    music_dirs(user_dirs.as_ref(), home.as_deref())
}

fn music_dirs(user_dirs: Option<&UserDirs>, home: Option<&Path>) -> Vec<PathBuf> {
    user_dirs
        .and_then(UserDirs::audio_dir)
        .map(Path::to_path_buf)
        .or_else(|| home.map(|home| home.join("Music")))
        .into_iter()
        .collect()
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_override_redirects_every_directory() {
        let base = Path::new("/base");
        let paths = AppPaths::resolve_with_base(PathOverrides::default(), Some(base));

        assert_eq!(paths.config_file, Path::new("/base/config/config.toml"));
        assert_eq!(paths.styles_dir, Path::new("/base/config/styles"));
        assert_eq!(paths.offsets_file, Path::new("/base/data/offsets.json"));
        assert_eq!(paths.cache_db_file, Path::new("/base/cache/lyrics_v1.db"));
        assert_eq!(paths.log_file, Path::new("/base/state/lyrachi.log"));
        assert_eq!(paths.default_music_dirs, vec![Path::new("/base/Music")]);
    }

    #[test]
    fn base_override_keeps_explicit_config_and_log() {
        let base = Path::new("/base");
        let paths = AppPaths::resolve_with_base(
            PathOverrides {
                config_file: Some(PathBuf::from("/base/elsewhere/my.toml")),
                log_file: Some(PathBuf::from("/base/elsewhere/my.log")),
            },
            Some(base),
        );

        assert_eq!(paths.config_file, Path::new("/base/elsewhere/my.toml"));
        assert_eq!(paths.styles_dir, Path::new("/base/elsewhere/styles"));
        assert_eq!(paths.log_file, Path::new("/base/elsewhere/my.log"));
        assert_eq!(paths.offsets_file, Path::new("/base/data/offsets.json"));
    }

    #[test]
    fn builds_fallback_paths_without_project_dirs() {
        let home = Path::new("/home/tester");

        assert_eq!(
            fallback_path(Some(home), ".config", CONFIG_FILE),
            PathBuf::from("/home/tester/.config/lyrachi/config.toml")
        );
        assert_eq!(
            fallback_path(Some(home), ".local/share", OFFSETS_FILE),
            PathBuf::from("/home/tester/.local/share/lyrachi/offsets.json")
        );
        assert_eq!(
            fallback_path(Some(home), ".local/state", LOG_FILE),
            PathBuf::from("/home/tester/.local/state/lyrachi/lyrachi.log")
        );
    }

    #[test]
    fn uses_home_music_as_final_fallback() {
        assert_eq!(
            music_dirs(None, Some(Path::new("/home/tester"))),
            vec![PathBuf::from("/home/tester/Music")]
        );
    }

    #[test]
    fn paths_output_contains_six_entries() {
        let paths = AppPaths {
            config_file: PathBuf::from("/tmp/config.toml"),
            styles_dir: PathBuf::from("/tmp/styles"),
            offsets_file: PathBuf::from("/tmp/offsets.json"),
            cache_db_file: PathBuf::from("/tmp/cache.db"),
            log_file: PathBuf::from("/tmp/lyrachi.log"),
            default_music_dirs: vec![PathBuf::from("/tmp/music")],
        };
        let lines = paths.lines();

        assert_eq!(lines.len(), 6);
        assert!(lines.iter().any(|(name, _)| *name == "styles"));
        assert!(lines.iter().any(|(name, _)| *name == "music"));
        assert!(lines.iter().any(|(_, value)| value == "/tmp/config.toml"));
    }
}

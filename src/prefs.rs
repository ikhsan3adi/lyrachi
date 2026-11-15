use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use toml_edit::{DocumentMut, Item, Table, value};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UiPrefs {
    pub fullscreen: bool,
    pub show_header: bool,
    pub show_status: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum PrefsError {
    #[error("config {path} is not valid TOML: {message}")]
    InvalidToml { path: String, message: String },
    #[error("could not {action} config {path}: {source}")]
    Io {
        action: &'static str,
        path: String,
        #[source]
        source: std::io::Error,
    },
}

impl PrefsError {
    fn io(action: &'static str, path: &Path, source: std::io::Error) -> Self {
        Self::Io {
            action,
            path: path.display().to_string(),
            source,
        }
    }
}

pub fn save_ui_prefs(path: &Path, prefs: UiPrefs) -> Result<(), PrefsError> {
    let target = resolve_target(path)?;
    let source = match fs::read_to_string(&target) {
        Ok(source) => source,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(PrefsError::io("read", &target, error)),
    };

    let mut document = source
        .parse::<DocumentMut>()
        .map_err(|error| PrefsError::InvalidToml {
            path: path.display().to_string(),
            message: error.to_string(),
        })?;
    let ui = document
        .as_table_mut()
        .entry("ui")
        .or_insert(Item::Table(Table::new()));
    let Some(ui) = ui.as_table_like_mut() else {
        return Err(PrefsError::InvalidToml {
            path: path.display().to_string(),
            message: "`ui` must be a table".to_string(),
        });
    };

    set_bool(ui, "fullscreen", prefs.fullscreen);
    set_bool(ui, "show_header", prefs.show_header);
    set_bool(ui, "show_status", prefs.show_status);

    write_atomic(&target, document.to_string().as_bytes())
}

fn set_bool(ui: &mut dyn toml_edit::TableLike, key: &str, new_value: bool) {
    let mut item = value(new_value);
    if let Some(existing) = ui.get_mut(key) {
        if let Some(decor) = existing.as_value().map(|value| value.decor().clone())
            && let Some(value) = item.as_value_mut()
        {
            *value.decor_mut() = decor;
        }
        *existing = item;
    } else {
        ui.insert(key, item);
    }
}

fn resolve_target(path: &Path) -> Result<PathBuf, PrefsError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            fs::canonicalize(path).map_err(|error| PrefsError::io("resolve", path, error))
        }
        Ok(_) => Ok(path.to_path_buf()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(path.to_path_buf()),
        Err(error) => Err(PrefsError::io("inspect", path, error)),
    }
}

fn write_atomic(path: &Path, contents: &[u8]) -> Result<(), PrefsError> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)
        .map_err(|error| PrefsError::io("create directory for", path, error))?;

    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("config.toml");
    let temp = parent.join(format!(".{file_name}.tmp-{}", std::process::id()));

    let result = (|| -> std::io::Result<()> {
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&temp)?;
        file.write_all(contents)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temp, path)?;
        if let Ok(directory) = File::open(parent) {
            let _ = directory.sync_all();
        }
        Ok(())
    })();

    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }

    result.map_err(|error| PrefsError::io("write", path, error))
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

    fn prefs() -> UiPrefs {
        UiPrefs {
            fullscreen: true,
            show_header: false,
            show_status: false,
        }
    }

    #[test]
    fn preserves_comments_and_table_order() {
        let dir = temp_dir("prefs-order");
        let path = dir.join("config.toml");
        fs::write(
            &path,
            "# keep this comment\n\
             [cache]\n\
             l1_entries = 64\n\
             \n\
             # keep provider note\n\
             [providers]\n\
             network = true\n\
             \n\
             [ui]\n\
             # keep ui note\n\
             show_header = true\n",
        )
        .unwrap();

        save_ui_prefs(&path, prefs()).unwrap();
        let saved = fs::read_to_string(&path).unwrap();

        assert!(saved.contains("# keep this comment"));
        assert!(saved.contains("# keep provider note"));
        assert!(saved.contains("# keep ui note"), "{saved}");
        assert!(saved.find("[cache]").unwrap() < saved.find("[providers]").unwrap());
        assert!(saved.find("[providers]").unwrap() < saved.find("[ui]").unwrap());
        assert!(saved.contains("fullscreen = true"));
        assert!(saved.contains("show_header = false"));
        assert!(saved.contains("show_status = false"));

        fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn keeps_symlink_and_updates_target() {
        use std::os::unix::fs::symlink;

        let dir = temp_dir("prefs-symlink");
        let target_dir = dir.join("dotfiles");
        fs::create_dir_all(&target_dir).unwrap();
        let target = target_dir.join("config.toml");
        fs::write(&target, "# original\n[sync]\nglobal_offset_ms = 0\n").unwrap();
        let link = dir.join("config.toml");
        symlink(&target, &link).unwrap();

        save_ui_prefs(&link, prefs()).unwrap();

        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read_link(&link).unwrap(), target);
        let saved = fs::read_to_string(&target).unwrap();
        assert!(saved.contains("# original"));
        assert!(saved.contains("fullscreen = true"));

        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn refuses_to_overwrite_invalid_toml() {
        let dir = temp_dir("prefs-invalid");
        let path = dir.join("config.toml");
        let original = "[ui\nshow_header = true\n";
        fs::write(&path, original).unwrap();

        let error = save_ui_prefs(&path, prefs()).unwrap_err();

        assert!(matches!(error, PrefsError::InvalidToml { .. }));
        assert_eq!(fs::read_to_string(&path).unwrap(), original);

        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn creates_missing_config_file() {
        let dir = temp_dir("prefs-missing");
        let path = dir.join("nested/config.toml");

        save_ui_prefs(&path, prefs()).unwrap();
        let saved = fs::read_to_string(&path).unwrap();

        assert!(saved.contains("[ui]"));
        assert!(saved.contains("fullscreen = true"));

        fs::remove_dir_all(dir).unwrap();
    }
}

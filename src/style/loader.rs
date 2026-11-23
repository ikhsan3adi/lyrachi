use std::fs::{self, File};
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use thiserror::Error;

use super::{DEFAULT_STYLE_NAME, StyleProfile, builtin_preset_source};

pub const MAX_STYLE_FILE_BYTES: u64 = 64 * 1024;
pub const MAX_EXTENDS_DEPTH: usize = 8;
pub const MAX_VAR_DEPTH: usize = 8;

const ROOT_SECTIONS: &[&str] = &[
    "meta",
    "palette",
    "layout",
    "lines",
    "highlight",
    "animation",
    "instrumental",
    "annotations",
    "header",
    "big_text",
];

/// Result of loading a style profile, including non-fatal warnings (unknown keys, clamps).
#[derive(Debug, Clone, PartialEq)]
pub struct StyleLoadOutcome {
    pub profile: StyleProfile,
    pub warnings: Vec<String>,
}

/// Errors emitted while validating, resolving, or deserializing a style file.
#[derive(Debug, Error)]
pub enum StyleLoadError {
    #[error("style `{name}` violates styles sandbox: {reason}")]
    SandboxViolation { name: String, reason: String },

    #[error("style `{name}` exceeds max size ({bytes} > {max_bytes} bytes)")]
    FileTooLarge {
        name: String,
        bytes: u64,
        max_bytes: u64,
    },

    #[error("unknown builtin style `{0}`")]
    UnknownBuiltin(String),

    #[error("styles directory does not exist: {0}")]
    MissingStylesDir(PathBuf),

    #[error("failed to read style `{path}`: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("invalid TOML in style `{name}`: {source}")]
    Parse {
        name: String,
        #[source]
        source: toml::de::Error,
    },

    #[error("style `{name}` exceeds max extends depth ({max_depth})")]
    ExtendsDepthExceeded { name: String, max_depth: usize },

    #[error("cyclic style inheritance detected: {}", chain.join(" -> "))]
    ExtendsCycle { chain: Vec<String> },

    #[error("invalid breakpoint `{name}`: {reason}")]
    BreakpointInvalid { name: String, reason: String },

    #[error("cyclic `$` variable reference for `{name}`: {}", chain.join(" -> "))]
    VarCycle { name: String, chain: Vec<String> },

    #[error("variable `{name}` exceeds max resolution depth ({max_depth})")]
    VarDepthExceeded { name: String, max_depth: usize },

    #[error("unresolved style variable `{0}`")]
    UnresolvedVar(String),

    #[error("invalid style profile `{name}`: {reason}")]
    InvalidProfile { name: String, reason: String },
}

/// Load a builtin or sandboxed `styles/*.toml` profile for a given viewport width.
pub fn load_style(
    name: &str,
    styles_dir: Option<&Path>,
    viewport_width: u16,
) -> Result<StyleLoadOutcome, StyleLoadError> {
    let trimmed = name.trim();
    let mut stack = Vec::new();
    let mut merged = load_merged_table(trimmed, styles_dir, &mut stack)?;

    apply_breakpoints(&mut merged, viewport_width)?;

    let snapshot = merged.clone();
    resolve_vars_in_table(&mut merged, &snapshot)?;

    deserialize_profile(trimmed, merged)
}

/// Convenience helper for CLI / app initialization returning `builtin:default` fallback if missing.
pub fn load_style_name_or_default(
    name: &str,
    styles_dir: Option<&Path>,
    viewport_width: u16,
) -> StyleLoadOutcome {
    match load_style(name, styles_dir, viewport_width) {
        Ok(outcome) => outcome,
        Err(error) => {
            let mut fallback =
                load_style(DEFAULT_STYLE_NAME, None, viewport_width).unwrap_or_else(|_| {
                    StyleLoadOutcome {
                        profile: StyleProfile::default(),
                        warnings: Vec::new(),
                    }
                });
            fallback.warnings.insert(
                0,
                format!("style fallback to {DEFAULT_STYLE_NAME}: {error}"),
            );
            fallback
        }
    }
}

/// Resolve and sandbox-validate a style file inside `styles_dir`.
///
/// Rejects absolute paths, traversal segments (`..`), path separators, null bytes, and
/// symlinks resolving outside `styles_dir`.
pub fn resolve_sandboxed_path(name: &str, styles_dir: &Path) -> Result<PathBuf, StyleLoadError> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err(StyleLoadError::SandboxViolation {
            name: name.to_string(),
            reason: "style name cannot be empty".to_string(),
        });
    }
    if trimmed.contains('\0') {
        return Err(StyleLoadError::SandboxViolation {
            name: name.to_string(),
            reason: "null bytes are not allowed".to_string(),
        });
    }
    if trimmed.contains('/') || trimmed.contains('\\') {
        return Err(StyleLoadError::SandboxViolation {
            name: name.to_string(),
            reason: "path separators are not allowed".to_string(),
        });
    }

    let raw_path = Path::new(trimmed);
    let mut components = raw_path.components();
    match (components.next(), components.next()) {
        (Some(Component::Normal(_)), None) => {}
        _ => {
            return Err(StyleLoadError::SandboxViolation {
                name: name.to_string(),
                reason: "only single relative file names are permitted".to_string(),
            });
        }
    }

    let file_name = if trimmed.ends_with(".toml") {
        trimmed.to_string()
    } else {
        format!("{trimmed}.toml")
    };

    let canonical_root = match fs::canonicalize(styles_dir) {
        Ok(path) => path,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(StyleLoadError::MissingStylesDir(styles_dir.to_path_buf()));
        }
        Err(source) => {
            return Err(StyleLoadError::Io {
                path: styles_dir.to_path_buf(),
                source,
            });
        }
    };

    let candidate = canonical_root.join(&file_name);
    let canonical_target = fs::canonicalize(&candidate).map_err(|source| StyleLoadError::Io {
        path: candidate.clone(),
        source,
    })?;

    if !canonical_target.starts_with(&canonical_root) || canonical_target == canonical_root {
        return Err(StyleLoadError::SandboxViolation {
            name: name.to_string(),
            reason: format!("resolved path escapes `{}`", canonical_root.display()),
        });
    }

    Ok(canonical_target)
}

fn load_merged_table(
    name: &str,
    styles_dir: Option<&Path>,
    stack: &mut Vec<String>,
) -> Result<toml::Table, StyleLoadError> {
    let (canonical_key, source) = load_raw_source(name, styles_dir)?;

    if stack.contains(&canonical_key) {
        let mut chain = stack.clone();
        chain.push(canonical_key);
        return Err(StyleLoadError::ExtendsCycle { chain });
    }
    if stack.len() > MAX_EXTENDS_DEPTH {
        return Err(StyleLoadError::ExtendsDepthExceeded {
            name: name.to_string(),
            max_depth: MAX_EXTENDS_DEPTH,
        });
    }

    let table: toml::Table = toml::from_str(&source).map_err(|source| StyleLoadError::Parse {
        name: name.to_string(),
        source,
    })?;

    let parent_name = match table.get("extends") {
        None => None,
        Some(toml::Value::String(parent)) => Some(parent.trim().to_string()),
        Some(_) => {
            return Err(StyleLoadError::InvalidProfile {
                name: name.to_string(),
                reason: "`extends` must be a string".to_string(),
            });
        }
    };

    stack.push(canonical_key);
    let result = if let Some(parent_name) = parent_name {
        let mut parent = load_merged_table(&parent_name, styles_dir, stack)?;
        deep_merge_table(&mut parent, table);
        Ok(parent)
    } else {
        Ok(table)
    };
    stack.pop();

    result
}

fn load_raw_source(
    name: &str,
    styles_dir: Option<&Path>,
) -> Result<(String, String), StyleLoadError> {
    if name.starts_with("builtin:") {
        let source = builtin_preset_source(name)
            .ok_or_else(|| StyleLoadError::UnknownBuiltin(name.to_string()))?;
        return Ok((name.to_string(), source.to_string()));
    }

    let Some(dir) = styles_dir else {
        return Err(StyleLoadError::SandboxViolation {
            name: name.to_string(),
            reason: "no styles directory configured for non-builtin style".to_string(),
        });
    };

    let path = resolve_sandboxed_path(name, dir)?;
    let mut file = File::open(&path).map_err(|source| StyleLoadError::Io {
        path: path.clone(),
        source,
    })?;
    let meta = file.metadata().map_err(|source| StyleLoadError::Io {
        path: path.clone(),
        source,
    })?;
    if meta.len() > MAX_STYLE_FILE_BYTES {
        return Err(StyleLoadError::FileTooLarge {
            name: name.to_string(),
            bytes: meta.len(),
            max_bytes: MAX_STYLE_FILE_BYTES,
        });
    }

    let mut buffer = Vec::with_capacity(usize::try_from(meta.len()).unwrap_or(4096));
    file.by_ref()
        .take(MAX_STYLE_FILE_BYTES + 1)
        .read_to_end(&mut buffer)
        .map_err(|source| StyleLoadError::Io {
            path: path.clone(),
            source,
        })?;
    if buffer.len() as u64 > MAX_STYLE_FILE_BYTES {
        return Err(StyleLoadError::FileTooLarge {
            name: name.to_string(),
            bytes: buffer.len() as u64,
            max_bytes: MAX_STYLE_FILE_BYTES,
        });
    }

    let content = String::from_utf8(buffer).map_err(|error| StyleLoadError::InvalidProfile {
        name: name.to_string(),
        reason: format!("invalid UTF-8: {error}"),
    })?;
    Ok((path.display().to_string(), content))
}

fn deep_merge_table(base: &mut toml::Table, overlay: toml::Table) {
    for (key, value) in overlay {
        match (base.get_mut(&key), value) {
            (Some(toml::Value::Table(base_sub)), toml::Value::Table(overlay_sub)) => {
                deep_merge_table(base_sub, overlay_sub);
            }
            (_, new_value) => {
                base.insert(key, new_value);
            }
        }
    }
}

struct MatchedBreakpoint {
    name: String,
    max_cols: u16,
    table: toml::Table,
}

fn apply_breakpoints(table: &mut toml::Table, viewport_width: u16) -> Result<(), StyleLoadError> {
    let Some(layout_val) = table.get_mut("layout") else {
        return Ok(());
    };
    let Some(layout_tbl) = layout_val.as_table_mut() else {
        return Ok(());
    };
    let Some(bp_val) = layout_tbl.remove("breakpoints") else {
        return Ok(());
    };
    let Some(bp_tbl) = bp_val.as_table() else {
        return Err(StyleLoadError::BreakpointInvalid {
            name: "layout.breakpoints".to_string(),
            reason: "must be a table".to_string(),
        });
    };

    let mut matches = Vec::new();
    for (bp_name, value) in bp_tbl {
        let Some(entry) = value.as_table() else {
            return Err(StyleLoadError::BreakpointInvalid {
                name: bp_name.clone(),
                reason: "breakpoint entry must be a table".to_string(),
            });
        };
        let Some(raw_cols) = entry.get("max_cols").and_then(toml::Value::as_integer) else {
            return Err(StyleLoadError::BreakpointInvalid {
                name: bp_name.clone(),
                reason: "breakpoint must specify integer `max_cols`".to_string(),
            });
        };
        let Ok(max_cols) = u16::try_from(raw_cols) else {
            return Err(StyleLoadError::BreakpointInvalid {
                name: bp_name.clone(),
                reason: format!("`max_cols` {raw_cols} is outside 0..=65535"),
            });
        };

        if max_cols >= viewport_width {
            let mut stripped = entry.clone();
            stripped.remove("max_cols");
            matches.push(MatchedBreakpoint {
                name: bp_name.clone(),
                max_cols,
                table: stripped,
            });
        }
    }

    // Apply widest matching breakpoints first so the narrowest matching breakpoint wins.
    matches.sort_by(|a, b| {
        b.max_cols
            .cmp(&a.max_cols)
            .then_with(|| a.name.cmp(&b.name))
    });

    for matched in matches {
        apply_breakpoint_overlay(table, matched.table);
    }

    Ok(())
}

fn apply_breakpoint_overlay(root: &mut toml::Table, overlay: toml::Table) {
    for (key, value) in overlay {
        if ROOT_SECTIONS.contains(&key.as_str())
            && let toml::Value::Table(section_overlay) = value
        {
            match root.get_mut(&key) {
                Some(toml::Value::Table(existing)) => deep_merge_table(existing, section_overlay),
                _ => {
                    root.insert(key, toml::Value::Table(section_overlay));
                }
            }
            continue;
        }

        let layout = root
            .entry("layout")
            .or_insert_with(|| toml::Value::Table(toml::Table::new()));
        if let toml::Value::Table(layout_table) = layout {
            match (layout_table.get_mut(&key), value) {
                (Some(toml::Value::Table(existing)), toml::Value::Table(sub)) => {
                    deep_merge_table(existing, sub);
                }
                (_, val) => {
                    layout_table.insert(key, val);
                }
            }
        }
    }
}

fn resolve_vars_in_table(
    table: &mut toml::Table,
    root: &toml::Table,
) -> Result<(), StyleLoadError> {
    let keys: Vec<String> = table.keys().cloned().collect();
    for key in keys {
        if let Some(value) = table.get_mut(&key) {
            resolve_var_value(value, root)?;
        }
    }
    Ok(())
}

fn resolve_var_value(value: &mut toml::Value, root: &toml::Table) -> Result<(), StyleLoadError> {
    match value {
        toml::Value::String(raw) => {
            let trimmed = raw.trim();
            if let Some(target) = trimmed.strip_prefix('$') {
                let target = target.trim();
                if is_valid_var_ident(target) {
                    *value = resolve_var_reference(target, root)?;
                }
            }
            Ok(())
        }
        toml::Value::Array(items) => {
            for item in items {
                resolve_var_value(item, root)?;
            }
            Ok(())
        }
        toml::Value::Table(sub) => {
            let keys: Vec<String> = sub.keys().cloned().collect();
            for key in keys {
                if let Some(entry) = sub.get_mut(&key) {
                    resolve_var_value(entry, root)?;
                }
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn resolve_var_reference(target: &str, root: &toml::Table) -> Result<toml::Value, StyleLoadError> {
    let mut current_name = target.trim().to_string();
    let mut chain = Vec::new();

    loop {
        if current_name.is_empty() {
            return Err(StyleLoadError::UnresolvedVar(format!("${target}")));
        }
        if chain.contains(&current_name) {
            chain.push(current_name.clone());
            return Err(StyleLoadError::VarCycle {
                name: target.to_string(),
                chain,
            });
        }
        if chain.len() >= MAX_VAR_DEPTH {
            return Err(StyleLoadError::VarDepthExceeded {
                name: target.to_string(),
                max_depth: MAX_VAR_DEPTH,
            });
        }
        chain.push(current_name.clone());

        let found = lookup_token(&current_name, root)
            .ok_or_else(|| StyleLoadError::UnresolvedVar(format!("${current_name}")))?;

        match found {
            toml::Value::String(next) if next.trim().starts_with('$') => {
                let stripped = next.trim().strip_prefix('$').unwrap_or("").trim();
                if is_valid_var_ident(stripped) {
                    current_name = stripped.to_string();
                } else {
                    return Ok(found.clone());
                }
            }
            other => return Ok(other.clone()),
        }
    }
}

fn is_valid_var_ident(name: &str) -> bool {
    let mut parts = name.split('.');
    let Some(first) = parts.next() else {
        return false;
    };
    if !is_ident_segment(first) {
        return false;
    }
    parts.all(is_ident_segment)
}

fn is_ident_segment(segment: &str) -> bool {
    let mut chars = segment.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn lookup_token<'a>(name: &str, root: &'a toml::Table) -> Option<&'a toml::Value> {
    if !name.contains('.')
        && let Some(palette) = root.get("palette").and_then(toml::Value::as_table)
        && let Some(val) = palette.get(name)
    {
        return Some(val);
    }

    let mut current: Option<&toml::Value> = None;
    for (idx, segment) in name.split('.').enumerate() {
        current = if idx == 0 {
            root.get(segment)
        } else {
            current
                .and_then(toml::Value::as_table)
                .and_then(|t| t.get(segment))
        };
    }
    current
}

fn deserialize_profile(name: &str, table: toml::Table) -> Result<StyleLoadOutcome, StyleLoadError> {
    let mut unknown_keys = Vec::new();
    let mut profile: StyleProfile = serde_ignored::deserialize(toml::Value::Table(table), |path| {
        unknown_keys.push(path.to_string());
    })
    .map_err(|error| StyleLoadError::InvalidProfile {
        name: name.to_string(),
        reason: error.to_string(),
    })?;

    let mut warnings = Vec::new();
    for key in unknown_keys {
        warnings.push(format!("style `{name}`: unknown key `{key}`"));
    }
    if let Some(version) = profile.schema_version
        && version != 1
    {
        warnings.push(format!(
            "style `{name}`: unsupported schema_version {version}; expected 1"
        ));
    }

    profile.clamp(&mut warnings);

    Ok(StyleLoadOutcome { profile, warnings })
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use ratatui::style::Color;

    use super::*;
    use crate::style::BigTextOverflow;
    use crate::style::color::FadeCurve;
    use crate::style::{AnimationColorSpace, HighlightMode, LineChangeEffectKind};

    fn temp_styles_dir(label: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "lyrachi-styles-{label}-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn extends_inherits_parent_fields_without_overwriting_by_defaults() {
        let dir = temp_styles_dir("extends");
        fs::write(
            dir.join("child.toml"),
            r#"
extends = "builtin:dark"

[meta]
name = "dark-variant"

[highlight]
mode = "line"
"#,
        )
        .unwrap();

        let outcome = load_style("child", Some(&dir), 120).unwrap();
        assert!(outcome.warnings.is_empty(), "{:?}", outcome.warnings);
        // Inherited from builtin:dark instead of StyleProfile::default()
        assert_eq!(outcome.profile.palette.bg.0, Color::Rgb(0x1a, 0x1b, 0x26));
        assert_eq!(outcome.profile.lines.fade.curve, FadeCurve::EaseOut);
        assert!((outcome.profile.lines.fade.falloff - 0.35).abs() < 1e-6);
        assert_eq!(outcome.profile.layout.line_spacing, 1);
        // Overridden in child
        assert_eq!(outcome.profile.highlight.mode, HighlightMode::Line);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn narrowest_matching_breakpoint_wins() {
        let dir = temp_styles_dir("breakpoints");
        fs::write(
            dir.join("responsive.toml"),
            r#"
[layout]
visible_prev = 3
padding = [2, 4]

[layout.breakpoints.wide]
max_cols = 120
visible_prev = 2

[layout.breakpoints.medium]
max_cols = 80
visible_prev = 1

[layout.breakpoints.tiny]
max_cols = 45
visible_prev = 0
"#,
        )
        .unwrap();

        let at_140 = load_style("responsive", Some(&dir), 140).unwrap();
        assert_eq!(at_140.profile.layout.visible_prev, 3);

        let at_100 = load_style("responsive", Some(&dir), 100).unwrap();
        assert_eq!(at_100.profile.layout.visible_prev, 2);

        let at_60 = load_style("responsive", Some(&dir), 60).unwrap();
        assert_eq!(at_60.profile.layout.visible_prev, 1);

        let at_40 = load_style("responsive", Some(&dir), 40).unwrap();
        assert_eq!(at_40.profile.layout.visible_prev, 0);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_extends_cycles_and_depth_limit() {
        let dir = temp_styles_dir("cycles");
        fs::write(dir.join("a.toml"), "extends = \"b\"\n").unwrap();
        fs::write(dir.join("b.toml"), "extends = \"a\"\n").unwrap();

        let err = load_style("a", Some(&dir), 80).unwrap_err();
        assert!(matches!(err, StyleLoadError::ExtendsCycle { .. }), "{err}");

        for i in 0..=9 {
            let body = if i == 9 {
                "extends = \"builtin:default\"\n".to_string()
            } else {
                format!("extends = \"chain-{}\"\n", i + 1)
            };
            fs::write(dir.join(format!("chain-{i}.toml")), body).unwrap();
        }
        let depth_err = load_style("chain-0", Some(&dir), 80).unwrap_err();
        assert!(
            matches!(depth_err, StyleLoadError::ExtendsDepthExceeded { .. }),
            "{depth_err}"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_var_cycles_and_reports_serde_ignored_typos() {
        let dir = temp_styles_dir("vars-and-typos");
        fs::write(
            dir.join("var_cycle.toml"),
            r##"
[palette]
one = "$two"
two = "$one"
fg = "$one"
"##,
        )
        .unwrap();
        let var_err = load_style("var_cycle", Some(&dir), 80).unwrap_err();
        assert!(
            matches!(var_err, StyleLoadError::VarCycle { .. }),
            "{var_err}"
        );

        fs::write(
            dir.join("typos.toml"),
            r#"
[meta]
name = "typo-theme"
bogus_meta = 123

[layout]
visble_prev = 5
padding = [99, 1]
line_spacing = 99
"#,
        )
        .unwrap();
        let outcome = load_style("typos", Some(&dir), 80).unwrap();
        assert!(
            outcome
                .warnings
                .iter()
                .any(|w| w.contains("meta.bogus_meta") || w.contains("bogus_meta")),
            "{:?}",
            outcome.warnings
        );
        assert!(
            outcome
                .warnings
                .iter()
                .any(|w| w.contains("layout.visble_prev")),
            "{:?}",
            outcome.warnings
        );
        // Clamps enforced safely
        assert_eq!(outcome.profile.layout.padding.vertical, 32);
        assert_eq!(outcome.profile.layout.line_spacing, 4);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn literal_dollar_strings_are_not_mistaken_for_variables() {
        let dir = temp_styles_dir("literal-dollar");
        fs::write(
            dir.join("dollar.toml"),
            r#"
[palette]
accent = "cyan"

[lines.active]
fg = "$accent"
prefix = "$ "

"#,
        )
        .unwrap();

        let outcome = load_style("dollar", Some(&dir), 80).unwrap();
        assert!(outcome.warnings.is_empty(), "{:?}", outcome.warnings);
        assert_eq!(outcome.profile.lines.active.fg.0, Color::Cyan);
        assert_eq!(outcome.profile.lines.active.prefix, "$ ");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn security_sandbox_rejects_traversal_symlinks_and_oversized_files() {
        let dir = temp_styles_dir("security");
        let outside = temp_styles_dir("outside");
        let outside_file = outside.join("secret.toml");
        fs::write(&outside_file, "[meta]\nname = \"escape\"\n").unwrap();

        for traversal in ["../../etc/passwd", "/etc/passwd", "../secret", "sub/theme"] {
            let err = resolve_sandboxed_path(traversal, &dir).unwrap_err();
            assert!(
                matches!(err, StyleLoadError::SandboxViolation { .. }),
                "expected sandbox violation for `{traversal}`, got {err}"
            );
        }

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let link_path = dir.join("evil.toml");
            symlink(&outside_file, &link_path).unwrap();
            let err = resolve_sandboxed_path("evil", &dir).unwrap_err();
            assert!(
                matches!(err, StyleLoadError::SandboxViolation { .. }),
                "{err}"
            );
        }

        let big_file = dir.join("huge.toml");
        let oversized = "#".repeat((MAX_STYLE_FILE_BYTES as usize) + 128);
        fs::write(&big_file, oversized).unwrap();
        let big_err = load_style("huge", Some(&dir), 80).unwrap_err();
        assert!(
            matches!(big_err, StyleLoadError::FileTooLarge { .. }),
            "{big_err}"
        );

        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&outside);
    }

    #[test]
    fn loads_wipe_highlight_and_animation_sections() {
        let dir = temp_styles_dir("animation");
        fs::write(
            dir.join("animated.toml"),
            r#"
[highlight]
mode = "wipe"
trail = 5

[animation]
color_space = "hsv"
onset_pulse = true

[[animation.on_line_change]]
effect = "sweep"
duration_ms = 400
"#,
        )
        .unwrap();

        let outcome = load_style("animated", Some(&dir), 80).unwrap();
        assert!(outcome.warnings.is_empty(), "{:?}", outcome.warnings);
        assert_eq!(outcome.profile.highlight.mode, HighlightMode::Wipe);
        assert_eq!(outcome.profile.highlight.trail, 5);
        assert_eq!(
            outcome.profile.animation.color_space,
            AnimationColorSpace::Hsv
        );
        assert!(outcome.profile.animation.onset_pulse);
        assert_eq!(
            outcome.profile.animation.on_line_change[0].effect,
            LineChangeEffectKind::Sweep
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn parses_big_text_overflow_modes() {
        let dir = temp_styles_dir("big_text_overflow");
        for (value, expected) in [
            ("shrink", BigTextOverflow::Shrink),
            ("wrap", BigTextOverflow::Wrap),
            ("fallback_normal", BigTextOverflow::FallbackNormal),
            ("single_line", BigTextOverflow::SingleLine),
        ] {
            fs::write(
                dir.join("overflow.toml"),
                format!(
                    r#"
[big_text]
overflow = "{value}"
"#
                ),
            )
            .unwrap();

            let outcome = load_style("overflow", Some(&dir), 80).unwrap();
            assert!(outcome.warnings.is_empty(), "{:?}", outcome.warnings);
            assert_eq!(outcome.profile.big_text.overflow, expected);
        }

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn style_loads_synthetic_word_timing_from_meta_section() {
        let dir = temp_styles_dir("meta-synth");
        fs::write(
            dir.join("synth.toml"),
            r#"
[meta]
name = "synth-demo"
synthetic_word_timing = true
"#,
        )
        .unwrap();

        let outcome = load_style("synth", Some(&dir), 80).unwrap();
        assert!(outcome.warnings.is_empty(), "{:?}", outcome.warnings);
        assert!(outcome.profile.meta.synthetic_word_timing);

        let resolved = outcome.profile.resolve(
            "synth",
            crate::style::color::ColorCapability::TrueColor,
            None,
        );
        assert!(resolved.synthetic_word_timing);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn style_loads_and_clamps_max_chars_per_phrase() {
        let dir = temp_styles_dir("phrase-chars");
        fs::write(
            dir.join("phrase.toml"),
            r#"
[meta]
name = "phrase-demo"

[big_text]
max_chars_per_phrase = 24
"#,
        )
        .unwrap();

        let outcome = load_style("phrase", Some(&dir), 80).unwrap();
        assert!(outcome.warnings.is_empty(), "{:?}", outcome.warnings);
        assert_eq!(outcome.profile.big_text.max_chars_per_phrase, 24);

        let resolved = outcome.profile.resolve(
            "phrase",
            crate::style::color::ColorCapability::TrueColor,
            None,
        );
        assert_eq!(resolved.big_text_max_chars_per_phrase, 24);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn loads_instrumental_marker_and_warns_on_invisible_fallback() {
        let dir = temp_styles_dir("instrumental-marker");
        fs::write(
            dir.join("marker.toml"),
            r#"
[instrumental]
marker = "♪"
"#,
        )
        .unwrap();

        let outcome = load_style("marker", Some(&dir), 80).unwrap();
        assert!(outcome.warnings.is_empty(), "{:?}", outcome.warnings);
        assert_eq!(outcome.profile.instrumental.marker, '♪');

        fs::write(
            dir.join("marker.toml"),
            r#"
[instrumental]
marker = " "
"#,
        )
        .unwrap();

        let outcome = load_style("marker", Some(&dir), 80).unwrap();
        assert_eq!(
            outcome.profile.instrumental.marker,
            super::super::DEFAULT_INSTRUMENTAL_MARKER
        );
        assert!(
            outcome
                .warnings
                .iter()
                .any(|warning| warning.contains("instrumental.marker"))
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn all_builtin_presets_load_without_warnings() {
        for name in super::super::BUILTIN_PRESETS {
            let outcome = load_style(name, None, 80).unwrap();
            assert!(
                outcome.warnings.is_empty(),
                "builtin preset `{name}` had warnings: {:?}",
                outcome.warnings
            );
        }
    }
}

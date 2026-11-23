use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use clap::Parser;
use lyrachi::cli::{Args, Command, PipeFormat};

/// A temp base directory that removes itself on drop, retrying while Windows
/// still holds file locks from the spawned binary.
struct BaseDirGuard(PathBuf);

impl Drop for BaseDirGuard {
    fn drop(&mut self) {
        for _ in 0..20 {
            match std::fs::remove_dir_all(&self.0) {
                Ok(()) => break,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
                Err(_) => std::thread::sleep(std::time::Duration::from_millis(25)),
            }
        }
    }
}

/// Create `$BASE/{config,data,cache,state}` and return the root; tests point
/// `LYRACHI_HOME` at it so every platform resolves the same tree.
fn base_dir(name: &str) -> (std::path::PathBuf, BaseDirGuard) {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root =
        std::env::temp_dir().join(format!("lyrachi-cli-{name}-{}-{stamp}", std::process::id()));
    for dir in ["config", "data", "cache", "state"] {
        std::fs::create_dir_all(root.join(dir)).unwrap();
    }
    (root.clone(), BaseDirGuard(root))
}

#[test]
fn defaults_are_empty_and_unset() {
    let args = Args::try_parse_from(["lyrachi"]).expect("default args");

    assert!(args.player.is_none());
    assert!(args.local_dir.is_empty());
    assert!(args.offset_ms.is_none());
    assert!(args.lookahead_ms.is_none());
    assert!(!args.offline);
    assert!(!args.no_cache);
    assert!(args.config.is_none());
    assert!(args.log_file.is_none());
    assert!(!args.dump_events);
    assert!(args.command.is_none());
}

#[test]
fn parses_dump_events_and_offsets() {
    let args = Args::try_parse_from([
        "lyrachi",
        "--dump-events",
        "--player",
        "mpv",
        "--offset-ms",
        "-100",
        "--lookahead-ms",
        "80",
    ])
    .expect("args");

    assert_eq!(args.player.as_deref(), Some("mpv"));
    assert_eq!(args.offset_ms, Some(-100));
    assert_eq!(args.lookahead_ms, Some(80));
    assert!(args.dump_events);
}

#[test]
fn parses_no_cache_before_and_after_subcommand() {
    let before = Args::try_parse_from(["lyrachi", "--no-cache", "status"]).expect("args");
    assert!(before.no_cache);
    assert_eq!(before.command, Some(Command::Status { json: false }));

    let after = Args::try_parse_from(["lyrachi", "status", "--no-cache"]).expect("args");
    assert!(after.no_cache);
    assert_eq!(after.command, Some(Command::Status { json: false }));
}

#[test]
fn parses_paths_with_global_config_after_subcommand() {
    let args = Args::try_parse_from(["lyrachi", "paths", "--config", "/tmp/config.toml"])
        .expect("paths args");

    assert_eq!(args.command, Some(Command::Paths));
    assert_eq!(args.config.as_deref(), Some(Path::new("/tmp/config.toml")));
}

#[test]
fn paths_command_prints_resolved_paths_without_tui() {
    let (base, _guard) = base_dir("paths");
    let custom = base.join("config").join("custom.toml");
    std::fs::write(&custom, "").unwrap();

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_lyrachi"))
        .arg("paths")
        .arg("--config")
        .arg(&custom)
        .env("LYRACHI_HOME", &base)
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains(&format!("config: {}", custom.display())));
    assert!(stdout.contains(&format!(
        "styles: {}",
        base.join("config").join("styles").display()
    )));
    assert!(stdout.contains(&format!(
        "offsets: {}",
        base.join("data").join("offsets.json").display()
    )));
    assert!(stdout.contains(&format!(
        "cache: {}",
        base.join("cache").join("lyrics_v1.db").display()
    )));
    assert!(stdout.contains(&format!(
        "log: {}",
        base.join("state").join("lyrachi.log").display()
    )));
    assert!(stdout.contains(&format!("music: {}", base.join("Music").display())));
}

#[test]
fn parses_status_with_json_and_global_config_after_subcommand() {
    let args = Args::try_parse_from([
        "lyrachi",
        "status",
        "--json",
        "--config",
        "/tmp/config.toml",
    ])
    .expect("status args");

    assert_eq!(args.command, Some(Command::Status { json: true }));
    assert_eq!(args.config.as_deref(), Some(Path::new("/tmp/config.toml")));
}

#[test]
fn status_defaults_to_text_output() {
    let args = Args::try_parse_from(["lyrachi", "status"]).expect("status args");

    assert_eq!(args.command, Some(Command::Status { json: false }));
}

#[test]
fn parses_status_with_offline_after_subcommand() {
    let args = Args::try_parse_from(["lyrachi", "status", "--offline"]).expect("status args");

    assert_eq!(args.command, Some(Command::Status { json: false }));
    assert!(args.offline);
}

#[test]
fn status_json_prints_one_object_without_tui_or_backend() {
    let (base, _guard) = base_dir("status");
    // The allowlist selects only MPD, which is disabled by default, so no
    // backend exists on any platform and the probe fails deterministically.
    std::fs::write(
        base.join("config").join("config.toml"),
        "[media]\nbackends = [\"mpd\"]\n",
    )
    .unwrap();

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_lyrachi"))
        .args(["status", "--json", "--config"])
        .arg(base.join("config").join("config.toml"))
        .env("LYRACHI_HOME", &base)
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("status --json should print valid JSON");
    assert!(value.is_object());
    assert_eq!(value["backend_available"], false);
    assert!(value["player"].is_null());
    assert_eq!(value["playback"], "stopped");
    assert_eq!(value["offset"]["effective_ms"], -300);
}

#[test]
fn status_creates_the_cache_database_by_default() {
    let (base, _guard) = base_dir("cached");
    let db = base.join("cache").join("lyrics_v1.db");

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_lyrachi"))
        .args(["status", "--json"])
        .env("LYRACHI_HOME", &base)
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        db.exists(),
        "expected the cache database at {}",
        db.display()
    );
}

#[test]
fn no_cache_status_does_not_create_the_database() {
    let (base, _guard) = base_dir("nocache");
    let db = base.join("cache").join("lyrics_v1.db");

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_lyrachi"))
        .args(["status", "--json", "--no-cache"])
        .env("LYRACHI_HOME", &base)
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!db.exists(), "--no-cache must not create {}", db.display());
}

#[test]
fn parses_pipe_defaults_and_json_width() {
    let defaults = Args::try_parse_from(["lyrachi", "pipe"]).expect("pipe defaults");
    assert_eq!(
        defaults.command,
        Some(Command::Pipe {
            format: PipeFormat::Plain,
            max_width: None
        })
    );

    let json = Args::try_parse_from(["lyrachi", "pipe", "--format", "json", "--max-width", "42"])
        .expect("pipe json");
    assert_eq!(
        json.command,
        Some(Command::Pipe {
            format: PipeFormat::Json,
            max_width: Some(42)
        })
    );
}

#[test]
fn config_print_default_prints_valid_toml_template() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_lyrachi"))
        .args(["config", "print-default"])
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("schema_version = 1"));
    assert!(stdout.contains("[providers]"));
    assert!(stdout.contains("[ui]"));
}

#[test]
fn config_schema_outputs_valid_draft_2020_12_json_schema() {
    for kind in ["config", "style"] {
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_lyrachi"))
            .args(["config", "schema", "--kind", kind])
            .output()
            .unwrap();

        assert!(output.status.success());
        let stdout = String::from_utf8(output.stdout).unwrap();
        let schema: serde_json::Value = serde_json::from_str(&stdout).unwrap();
        assert_eq!(
            schema["$schema"], "https://json-schema.org/draft/2020-12/schema",
            "schema for {kind} failed"
        );
        assert!(schema["properties"].is_object());
    }
}

#[test]
fn config_check_detects_typos_in_config_and_style() {
    let root = std::env::temp_dir().join(format!("lyrachi-config-check-{}", std::process::id()));
    let config_dir = root.join("config");
    let styles_dir = config_dir.join("styles");
    std::fs::create_dir_all(&styles_dir).unwrap();

    // 1. Valid config and builtin style
    let valid_config = config_dir.join("valid.toml");
    std::fs::write(&valid_config, "[ui]\nstyle = \"builtin:dark\"\n").unwrap();

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_lyrachi"))
        .args([
            "config",
            "check",
            "--config",
            valid_config.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("OK:"), "{stdout}");

    // 2. Config with typo (unknown field rejected by deny_unknown_fields)
    let typo_config = config_dir.join("typo.toml");
    std::fs::write(&typo_config, "[ui]\ninvalid_setting = true\n").unwrap();

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_lyrachi"))
        .args(["config", "check", "--config", typo_config.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(!output.status.success());

    // 3. Style with typo (detected by serde_ignored and reported as warning)
    let style_path = styles_dir.join("custom.toml");
    std::fs::write(&style_path, "[lines.active]\nunknown_property = true\n").unwrap();
    let style_config = config_dir.join("style_check.toml");
    std::fs::write(&style_config, "[ui]\nstyle = \"custom\"\n").unwrap();

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_lyrachi"))
        .args([
            "config",
            "check",
            "--config",
            style_config.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("unknown_property"), "{stdout}");

    std::fs::remove_dir_all(root).unwrap();
}

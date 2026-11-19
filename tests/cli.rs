use std::path::Path;

use clap::Parser;
use lyrachi::cli::{Args, Command, PipeFormat};

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
    let root = std::env::temp_dir().join(format!("lyrachi-paths-{}", std::process::id()));
    let config_home = root.join("config");
    let data_home = root.join("data");
    let cache_home = root.join("cache");
    let state_home = root.join("state");
    let home = root.join("home");
    for dir in [&config_home, &data_home, &cache_home, &state_home, &home] {
        std::fs::create_dir_all(dir).unwrap();
    }
    std::fs::write(
        config_home.join("user-dirs.dirs"),
        "XDG_MUSIC_DIR=\"$HOME/Music\"\n",
    )
    .unwrap();
    let custom = root.join("custom.toml");
    std::fs::write(&custom, "").unwrap();

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_lyrachi"))
        .arg("paths")
        .arg("--config")
        .arg(&custom)
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_DATA_HOME", &data_home)
        .env("XDG_CACHE_HOME", &cache_home)
        .env("XDG_STATE_HOME", &state_home)
        .env_remove("XDG_MUSIC_DIR")
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
        custom.parent().unwrap().join("styles").display()
    )));
    assert!(stdout.contains(&format!(
        "offsets: {}",
        data_home.join("lyrachi/offsets.json").display()
    )));
    assert!(stdout.contains(&format!(
        "cache: {}",
        cache_home.join("lyrachi/lyrics_v1.db").display()
    )));
    assert!(stdout.contains(&format!(
        "log: {}",
        state_home.join("lyrachi/lyrachi.log").display()
    )));
    assert!(stdout.contains(&format!("music: {}", home.join("Music").display())));

    std::fs::remove_dir_all(root).unwrap();
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
fn status_json_prints_one_object_without_tui_or_dbus() {
    let root = std::env::temp_dir().join(format!("lyrachi-status-cli-{}", std::process::id()));
    let config_home = root.join("config");
    let data_home = root.join("data");
    let cache_home = root.join("cache");
    let state_home = root.join("state");
    let home = root.join("home");
    for dir in [&config_home, &data_home, &cache_home, &state_home, &home] {
        std::fs::create_dir_all(dir).unwrap();
    }
    let bus = root.join("no-bus");

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_lyrachi"))
        .args(["status", "--json"])
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_DATA_HOME", &data_home)
        .env("XDG_CACHE_HOME", &cache_home)
        .env("XDG_STATE_HOME", &state_home)
        .env(
            "DBUS_SESSION_BUS_ADDRESS",
            format!("unix:path={}", bus.display()),
        )
        .env_remove("XDG_MUSIC_DIR")
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
    assert!(value["player"].is_null());
    assert_eq!(value["playback"], "stopped");
    assert_eq!(value["offset"]["effective_ms"], 300);

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn no_cache_status_does_not_create_the_database() {
    let root = std::env::temp_dir().join(format!("lyrachi-status-nocache-{}", std::process::id()));
    let config_home = root.join("config");
    let data_home = root.join("data");
    let cache_home = root.join("cache");
    let state_home = root.join("state");
    let home = root.join("home");
    for dir in [&config_home, &data_home, &cache_home, &state_home, &home] {
        std::fs::create_dir_all(dir).unwrap();
    }

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_lyrachi"))
        .args(["status", "--json", "--no-cache"])
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_DATA_HOME", &data_home)
        .env("XDG_CACHE_HOME", &cache_home)
        .env("XDG_STATE_HOME", &state_home)
        .env(
            "DBUS_SESSION_BUS_ADDRESS",
            format!("unix:path={}", root.join("no-bus").display()),
        )
        .env_remove("XDG_MUSIC_DIR")
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!cache_home.join("lyrachi").join("lyrics_v1.db").exists());

    std::fs::remove_dir_all(root).unwrap();
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

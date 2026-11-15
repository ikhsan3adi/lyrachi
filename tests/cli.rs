use std::path::Path;

use clap::Parser;
use lyrachi::cli::{Args, Command};

#[test]
fn defaults_are_empty_and_unset() {
    let args = Args::try_parse_from(["lyrachi"]).expect("default args");

    assert!(args.player.is_none());
    assert!(args.local_dir.is_empty());
    assert!(args.offset_ms.is_none());
    assert!(args.lookahead_ms.is_none());
    assert!(!args.offline);
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

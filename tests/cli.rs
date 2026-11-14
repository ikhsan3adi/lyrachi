use clap::Parser;
use lyrachi::cli::Args;

#[test]
fn defaults_are_empty_and_zeroed() {
    let args = Args::try_parse_from(["lyrachi"]).expect("default args");

    assert!(args.player.is_none());
    assert!(args.local_dir.is_empty());
    assert_eq!(args.offset_ms, 0);
    assert_eq!(args.lookahead_ms, 0);
    assert!(args.log_file.is_none());
    assert!(!args.dump_events);
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
    assert_eq!(args.offset_ms, -100);
    assert_eq!(args.lookahead_ms, 80);
    assert!(args.dump_events);
}

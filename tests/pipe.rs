use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use lyrachi::app::pipe::{PipeOptions, run_events};
use lyrachi::cli::PipeFormat;
use lyrachi::config::RuntimeConfig;
use lyrachi::core::clock::PlaybackStatus;
use lyrachi::media::fake::FakeMediaSource;
use lyrachi::media::{BackendKind, MediaEvent, PlayerKind, TrackMeta};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

fn temp_dir(name: &str) -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "lyrachi-pipe-{name}-{}-{stamp}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn track() -> TrackMeta {
    TrackMeta {
        title: "Yellow".to_string(),
        artist: "Coldplay".to_string(),
        album: Some("Parachutes".to_string()),
        duration_ms: Some(10_000),
        track_id: None,
        url: None,
    }
}

fn start_events() -> Vec<(Duration, MediaEvent)> {
    vec![
        (
            Duration::ZERO,
            MediaEvent::PlayerChanged {
                id: "org.mpris.MediaPlayer2.spotify".to_string(),
                kind: PlayerKind::Native,
                backend: BackendKind::Mpris,
            },
        ),
        (Duration::ZERO, MediaEvent::TrackChanged(track())),
        (Duration::ZERO, MediaEvent::Status(PlaybackStatus::Playing)),
    ]
}

fn poll_events(first_at_ms: u64, step_ms: u64, positions: &[u64]) -> Vec<(Duration, MediaEvent)> {
    positions
        .iter()
        .enumerate()
        .map(|(index, pos_ms)| {
            (
                Duration::from_millis(first_at_ms + index as u64 * step_ms),
                MediaEvent::PositionPoll {
                    pos_ms: *pos_ms,
                    rtt: Duration::ZERO,
                    sample_age: None,
                },
            )
        })
        .collect()
}

async fn run_pipe(
    mut schedule: Vec<(Duration, MediaEvent)>,
    config: RuntimeConfig,
    offsets_file: PathBuf,
    options: PipeOptions,
) -> String {
    schedule.sort_by_key(|(at, _)| *at);
    let (tx, rx) = mpsc::channel(512);
    let source = FakeMediaSource::new(schedule);
    let mut output = Vec::new();
    let result = tokio::time::timeout(Duration::from_secs(5), async {
        let (_, result) = tokio::join!(
            source.run(tx, CancellationToken::new()),
            run_events(rx, config, offsets_file, &mut output, options)
        );
        result
    })
    .await
    .expect("pipe should finish");

    result.expect("pipe result");
    String::from_utf8(output).expect("utf-8 pipe output")
}

#[tokio::test]
async fn pipe_prints_once_per_active_line_despite_position_polls() {
    let dir = temp_dir("dedupe");
    let lyrics_dir = dir.join("music");
    std::fs::create_dir_all(&lyrics_dir).unwrap();
    std::fs::write(
        lyrics_dir.join("Coldplay - Yellow.lrc"),
        "[00:00.00]one\n[00:02.50]two\n[00:05.00]three\n[00:07.50]four\n",
    )
    .unwrap();

    let config = RuntimeConfig {
        network: false,
        local_dirs: vec![lyrics_dir],
        ..RuntimeConfig::default()
    };
    let positions = (0..=99).map(|index| index * 100).collect::<Vec<_>>();
    let mut schedule = start_events();
    schedule.extend(poll_events(300, 2, &positions));

    let output = run_pipe(
        schedule,
        config,
        dir.join("offsets.json"),
        PipeOptions {
            format: PipeFormat::Plain,
            max_width: None,
        },
    )
    .await;

    assert_eq!(output, "one\ntwo\nthree\nfour\n");

    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn pipe_json_output_is_one_valid_object_per_line() {
    let dir = temp_dir("json");
    let lyrics_dir = dir.join("music");
    std::fs::create_dir_all(&lyrics_dir).unwrap();
    std::fs::write(
        lyrics_dir.join("Coldplay - Yellow.lrc"),
        "[00:00.00]Look at the stars\n",
    )
    .unwrap();

    let config = RuntimeConfig {
        network: false,
        local_dirs: vec![lyrics_dir],
        ..RuntimeConfig::default()
    };
    let mut schedule = start_events();
    schedule.extend(poll_events(300, 2, &[0, 5_000]));

    let output = run_pipe(
        schedule,
        config,
        dir.join("offsets.json"),
        PipeOptions {
            format: PipeFormat::Json,
            max_width: Some(6),
        },
    )
    .await;
    let lines = output.lines().collect::<Vec<_>>();

    assert_eq!(lines.len(), 1, "{output}");
    let value: Value = serde_json::from_str(lines[0]).expect("valid JSON line");
    assert_eq!(value["text"], "Look a");
    assert_eq!(value["alt"], "playing");
    assert_eq!(value["class"], "line");
    assert!(
        value["progress"]
            .as_f64()
            .is_some_and(|progress| (0.0..=1.0).contains(&progress))
    );

    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn pipe_json_emits_when_playback_status_changes_on_the_same_line() {
    let dir = temp_dir("status");
    let lyrics_dir = dir.join("music");
    std::fs::create_dir_all(&lyrics_dir).unwrap();
    std::fs::write(
        lyrics_dir.join("Coldplay - Yellow.lrc"),
        "[00:00.00]Look at the stars\n",
    )
    .unwrap();

    let config = RuntimeConfig {
        network: false,
        local_dirs: vec![lyrics_dir],
        ..RuntimeConfig::default()
    };
    let mut schedule = start_events();
    schedule.extend(poll_events(300, 2, &[0, 100]));
    schedule.push((
        Duration::from_millis(500),
        MediaEvent::Status(PlaybackStatus::Paused),
    ));

    let output = run_pipe(
        schedule,
        config,
        dir.join("offsets.json"),
        PipeOptions {
            format: PipeFormat::Json,
            max_width: None,
        },
    )
    .await;
    let frames = output
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("valid JSON line"))
        .collect::<Vec<_>>();

    assert_eq!(frames.len(), 2, "{output}");
    assert_eq!(frames[0]["text"], frames[1]["text"]);
    assert_eq!(frames[0]["alt"], "playing");
    assert_eq!(frames[1]["alt"], "paused");
    assert_eq!(frames[0]["class"], "line");
    assert_eq!(frames[1]["class"], "line");

    std::fs::remove_dir_all(dir).unwrap();
}

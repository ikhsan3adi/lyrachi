use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use lyrachi::app::status::{collect_report, render_json, render_text};
use lyrachi::config::RuntimeConfig;
use lyrachi::core::clock::PlaybackStatus;
use lyrachi::media::fake::FakeMediaSource;
use lyrachi::media::{
    BackendKind, MediaEvent, MediaSnapshot, PlayerKind, PlayerSnapshot, TrackMeta,
};
use lyrachi::offsets::OffsetStore;
use lyrachi::providers::TrackQuery;
use lyrachi::providers::sanitize::encode_offset_key;
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

fn temp_dir(name: &str) -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "lyrachi-status-{name}-{}-{stamp}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn track(title: &str) -> TrackMeta {
    TrackMeta {
        title: title.to_string(),
        artist: "Coldplay".to_string(),
        album: Some("Parachutes".to_string()),
        duration_ms: Some(267_000),
        track_id: None,
        url: Some("file:///music/coldplay/yellow.flac".to_string()),
    }
}

#[tokio::test]
async fn fake_media_source_switches_tracks_seeks_and_pauses() {
    let (tx, mut rx) = mpsc::channel(16);
    let source = FakeMediaSource::new(vec![
        (
            Duration::ZERO,
            MediaEvent::PlayerChanged {
                id: "org.mpris.MediaPlayer2.spotify".to_string(),
                kind: PlayerKind::Native,
            },
        ),
        (Duration::ZERO, MediaEvent::TrackChanged(track("Trouble"))),
        (
            Duration::from_millis(1),
            MediaEvent::TrackChanged(track("Yellow")),
        ),
        (
            Duration::from_millis(2),
            MediaEvent::Seeked { pos_ms: 9_500 },
        ),
        (
            Duration::from_millis(2),
            MediaEvent::Status(PlaybackStatus::Paused),
        ),
    ]);

    let task = tokio::spawn(source.run(tx, CancellationToken::new()));
    let events = tokio::time::timeout(Duration::from_secs(1), async {
        let mut events = Vec::new();
        while let Some(event) = rx.recv().await {
            events.push(event);
        }
        events
    })
    .await
    .expect("fake events should arrive without D-Bus");
    task.await.expect("fake source task");

    assert_eq!(events.len(), 5);
    assert!(matches!(
        &events[1],
        MediaEvent::TrackChanged(track) if track.title == "Trouble"
    ));
    assert!(matches!(
        &events[2],
        MediaEvent::TrackChanged(track) if track.title == "Yellow"
    ));
    assert!(matches!(events[3], MediaEvent::Seeked { pos_ms: 9_500 }));
    assert!(matches!(
        events[4],
        MediaEvent::Status(PlaybackStatus::Paused)
    ));
}

#[tokio::test]
async fn status_report_resolves_local_fixture_and_keeps_global_plus_track_offsets() {
    let dir = temp_dir("fixture");
    let lyrics_dir = dir.join("music");
    std::fs::create_dir_all(&lyrics_dir).unwrap();
    std::fs::write(
        lyrics_dir.join("Coldplay - Yellow.lrc"),
        "[00:00.00]Look at the stars\n[00:04.50]Look how they shine for you\n[00:09.00]And everything you do\n",
    )
    .unwrap();

    let (tx, mut rx) = mpsc::channel(16);
    let source = FakeMediaSource::new(vec![
        (
            Duration::ZERO,
            MediaEvent::PlayerChanged {
                id: "org.mpris.MediaPlayer2.spotify".to_string(),
                kind: PlayerKind::Native,
            },
        ),
        (Duration::ZERO, MediaEvent::TrackChanged(track("Trouble"))),
        (
            Duration::from_millis(1),
            MediaEvent::TrackChanged(track("Yellow")),
        ),
        (
            Duration::from_millis(2),
            MediaEvent::Seeked { pos_ms: 9_500 },
        ),
        (
            Duration::from_millis(2),
            MediaEvent::Status(PlaybackStatus::Paused),
        ),
    ]);
    let task = tokio::spawn(source.run(tx, CancellationToken::new()));
    let mut snapshot = MediaSnapshot::default();
    while let Some(event) = rx.recv().await {
        match event {
            MediaEvent::PlayerChanged { id, kind } => {
                snapshot.player = Some(PlayerSnapshot { id, kind });
            }
            MediaEvent::TrackChanged(track) => snapshot.track = Some(track),
            MediaEvent::Seeked { pos_ms } => snapshot.position_ms = pos_ms,
            MediaEvent::Status(status) => snapshot.playback = status,
            _ => {}
        }
    }
    task.await.expect("fake source task");

    let expected_track = track("Yellow");
    let expected_key = encode_offset_key(&TrackQuery::from_meta(&expected_track, 0).canonical);
    let offsets_file = dir.join("offsets.json");
    let mut offsets = OffsetStore::load(&offsets_file);
    offsets.set(
        &TrackQuery::from_meta(snapshot.track.as_ref().unwrap(), 0).canonical,
        "Coldplay - Yellow",
        -200,
    );
    offsets.flush().unwrap();

    let config = RuntimeConfig {
        global_offset_ms: 100,
        network: false,
        local_dirs: vec![lyrics_dir],
        ..RuntimeConfig::default()
    };
    let report = collect_report(snapshot, &config, &offsets_file).await;

    assert_eq!(
        report.player.as_ref().unwrap().id,
        "org.mpris.MediaPlayer2.spotify"
    );
    assert_eq!(report.playback, "paused");
    assert_eq!(report.position_ms, 9_500);
    assert_eq!(report.track.as_ref().unwrap().title, "Yellow");
    assert_eq!(report.track.as_ref().unwrap().artist, "Coldplay");
    assert_eq!(
        report.track.as_ref().unwrap().album.as_deref(),
        Some("Parachutes")
    );
    assert_eq!(report.track.as_ref().unwrap().duration_ms, Some(267_000));
    assert_eq!(report.canonical_key.as_deref(), Some(expected_key.as_str()));

    let lyrics = report.lyrics.as_ref().unwrap();
    assert_eq!(lyrics.source, "local");
    assert_eq!(lyrics.granularity, "line");
    assert_eq!(lyrics.confidence, 1.0);
    assert_eq!(report.offset.global_ms, 100);
    assert_eq!(report.offset.track_ms, -200);
    assert_eq!(report.offset.effective_ms, -100);

    let backend = BackendKind::current();
    let backend_name = backend.as_str();
    let backend_available = backend.is_supported();
    let backend_state = if backend_available {
        "available"
    } else {
        "unavailable"
    };
    let expected = format!(
        "player: spotify (org.mpris.MediaPlayer2.spotify)\n\
         backend: {backend_name} ({backend_state})\n\
         playback: paused\n\
         title: Yellow\n\
         artist: Coldplay\n\
         album: Parachutes\n\
         duration_ms: 267000\n\
         url: file:///music/coldplay/yellow.flac\n\
         position_ms: 9500\n\
         lyrics_source: local\n\
         lyrics_granularity: line\n\
         lyrics_confidence: 1.000\n\
         canonical_key: {expected_key}\n\
         offset_global_ms: 100\n\
         offset_track_ms: -200\n\
         offset_effective_ms: -100\n\
         circuit_breaker_lrclib: closed\n\
         circuit_breaker_lyricsplus: closed\n"
    );
    assert_eq!(render_text(&report), expected);

    let json: Value = serde_json::from_str(&render_json(&report).unwrap()).unwrap();
    assert_eq!(json["backend"], backend.as_str());
    assert_eq!(json["backend_available"], backend_available);
    assert_eq!(json["player"]["name"], "spotify");
    assert_eq!(json["player"]["kind"], "native");
    assert_eq!(json["playback"], "paused");
    assert_eq!(json["track"]["title"], "Yellow");
    assert_eq!(json["track"]["duration_ms"], 267_000);
    assert_eq!(json["lyrics"]["source"], "local");
    assert_eq!(json["lyrics"]["granularity"], "line");
    assert_eq!(json["lyrics"]["confidence"], 1.0);
    assert_eq!(json["canonical_key"], expected_key);
    assert_eq!(json["offset"]["global_ms"], 100);
    assert_eq!(json["offset"]["track_ms"], -200);
    assert_eq!(json["offset"]["effective_ms"], -100);
    assert_eq!(json["circuit_breakers"][0]["provider"], "lrclib");
    assert_eq!(json["circuit_breakers"][0]["status"], "closed");
    assert_eq!(json["circuit_breakers"][1]["provider"], "lyricsplus");
    assert_eq!(json["circuit_breakers"][1]["status"], "closed");

    std::fs::remove_dir_all(dir).unwrap();
}

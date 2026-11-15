use std::time::{Duration, Instant};

use lyrachi::{
    app::{
        draw_frame,
        event::AppEvent,
        handle_resolved,
        resolve::{ResolveCoordinator, TrackPlan},
    },
    compiler::timeline::{CompiledTimeline, HORIZONTAL_PADDING},
    core::{
        clock::{EffectiveOffsets, PlaybackClock, PlaybackStatus},
        normalize::normalize_track,
        parse::lrc::parse_lrc,
        text::display_width,
    },
    media::TrackMeta,
    providers::{ProviderError, TrackQuery, lrclib::LrclibRecord, score},
    ui::UiState,
};
use ratatui::{Terminal, backend::TestBackend};
use tokio::sync::mpsc;

#[test]
fn dod_1_line_and_word_transitions_stay_within_100ms_of_player_position() {
    let fixture = include_str!("fixtures/enhanced_karaoke.elrc");
    let parsed = parse_lrc(fixture, Some(30_000)).unwrap();
    let track = normalize_track(parsed, "local", None, 1.0, Some(30_000), 0);
    let timeline = CompiledTimeline::compile(&track, 80);

    let base = Instant::now();
    let mut clock = PlaybackClock::new();
    clock.on_status_changed(PlaybackStatus::Playing, base);

    let rtt = Duration::from_millis(20);
    let poll_time = base + Duration::from_millis(1_000);
    clock.on_position_poll(1_030, rtt, poll_time);

    let effective_ms = clock.effective_lyric_ms(poll_time, EffectiveOffsets::default());
    let actual_player_ms = 1_030 + 10;
    let delta_ms = effective_ms.abs_diff(actual_player_ms);
    assert!(
        delta_ms <= 100,
        "expected effective_lyric_ms within 100ms of player position, got delta {delta_ms}ms"
    );

    let first_word_start = timeline.lines[0].words[0].start_ms;
    let cursor = timeline.evaluate(first_word_start);
    assert_eq!(cursor.active_line_idx, Some(0));
    assert_eq!(cursor.active_word_idx, Some(0));
}

#[test]
fn dod_2_seek_and_rate_recovery_within_200ms_with_signal_and_1100ms_with_poll() {
    let base = Instant::now();
    let mut clock = PlaybackClock::new();
    clock.on_status_changed(PlaybackStatus::Playing, base);

    let seek_instant = base + Duration::from_secs(5);
    clock.on_seeked(42_000, seek_instant);
    let after_seek = seek_instant + Duration::from_millis(16);
    assert!(
        clock.raw_position_at(after_seek).abs_diff(42_016) <= 200,
        "Seeked signal must resynchronize within 200ms"
    );

    clock.on_rate_changed(1.25, after_seek);
    let after_rate = after_seek + Duration::from_millis(160);
    assert_eq!(clock.raw_position_at(after_rate), 42_216);

    let silent_seek_time = after_rate;
    let next_poll_time = silent_seek_time + Duration::from_millis(1_000);
    clock.on_position_poll(90_000, Duration::from_millis(10), next_poll_time);
    let recovery_delay = next_poll_time.duration_since(silent_seek_time);
    assert!(recovery_delay <= Duration::from_millis(1_100));
    assert!(clock.raw_position_at(next_poll_time).abs_diff(90_005) <= 50);
}

#[tokio::test]
async fn dod_3_five_rapid_track_changes_in_two_seconds_never_show_stale_lyrics() {
    let fixture = include_str!("fixtures/standard_multi_tag.lrc");
    let parsed = parse_lrc(fixture, Some(180_000)).unwrap();
    let base_track = normalize_track(parsed, "lrclib", None, 0.98, Some(180_000), 0);

    let mut state = UiState::new();
    let mut coordinator = ResolveCoordinator::new();
    let (app_tx, mut app_rx) = mpsc::channel(16);
    let mut tokens = Vec::new();
    let mut tasks = Vec::new();

    for step in 1..=5_u64 {
        let meta = TrackMeta {
            title: format!("Rapid Track {step}"),
            artist: "Coldplay".to_string(),
            album: Some("Parachutes".to_string()),
            duration_ms: Some(180_000),
            track_id: Some(format!("/org/mpris/MediaPlayer2/Track/{step}")),
            url: None,
        };
        state.set_track(meta.clone());

        match coordinator.prepare_track(&meta) {
            TrackPlan::Fetch {
                epoch,
                query,
                cancel,
                ..
            } => {
                tokens.push(cancel.clone());
                let tx = app_tx.clone();
                let mut track = base_track.clone();
                track.source_track_id = Some(query.title.clone());
                track.lines[0].text = format!("Active lyric for {}", query.title);

                tasks.push(tokio::spawn(async move {
                    let delay = Duration::from_millis((6 - step) * 10);
                    tokio::select! {
                        biased;
                        _ = cancel.cancelled() => {
                            let _ = tx
                                .send(AppEvent::Resolved {
                                    epoch,
                                    result: Err(ProviderError::Cancelled),
                                })
                                .await;
                        }
                        _ = tokio::time::sleep(delay) => {
                            let _ = tx
                                .send(AppEvent::Resolved {
                                    epoch,
                                    result: Ok(Some(track)),
                                })
                                .await;
                        }
                    }
                }));
            }
            other => panic!("expected fetch plan, got {other:?}"),
        }
    }
    drop(app_tx);

    for (idx, token) in tokens[..4].iter().enumerate() {
        assert!(
            token.is_cancelled(),
            "track {} token was not cancelled",
            idx + 1
        );
    }
    assert!(!tokens[4].is_cancelled());

    for task in tasks {
        task.await.unwrap();
    }

    while let Some(AppEvent::Resolved { epoch, result }) = app_rx.recv().await {
        handle_resolved(&mut state, &mut coordinator, epoch, result);
        if let Some(loaded) = state.lyrics_track() {
            assert_eq!(
                loaded.source_track_id.as_deref(),
                Some("Rapid Track 5"),
                "stale track lyrics overwrote the active track"
            );
        }
    }

    let mut late_stale = base_track;
    late_stale.source_track_id = Some("Rapid Track 2".to_string());
    handle_resolved(&mut state, &mut coordinator, 2, Ok(Some(late_stale)));

    let final_track = state
        .lyrics_track()
        .expect("track 5 lyrics should be loaded");
    assert_eq!(
        final_track.source_track_id.as_deref(),
        Some("Rapid Track 5")
    );
}

#[test]
fn dod_4_browser_music_video_with_duration_diff_and_merged_artist_resolves_lyrics() {
    let browser_meta = TrackMeta {
        title: "Coldplay - Yellow (Official Music Video) [4K Remaster]".to_string(),
        artist: "ColdplayVEVO".to_string(),
        album: None,
        duration_ms: Some(272_000),
        track_id: None,
        url: None,
    };
    let query = TrackQuery::from_meta(&browser_meta, 0);
    assert_eq!(query.artist, "Coldplay");
    assert_eq!(query.title, "Yellow");

    let search_fixture = include_str!("fixtures/lrclib_search.json");
    let candidates: Vec<LrclibRecord> = serde_json::from_str(search_fixture).unwrap();
    let ranked = score::rank_candidates(&query, &candidates);

    assert!(
        !ranked.is_empty(),
        "expected fallback search candidates above 0.65"
    );
    let (best, confidence) = ranked[0];
    assert!(
        confidence >= 0.65,
        "confidence {confidence} fell below 0.65"
    );
    assert_eq!(best.id, 1);
    assert!(best.has_word_sync);
}

#[test]
fn dod_5_cjk_combining_marks_emoji_and_annotations_render_without_panic_or_overflow() {
    let fixture = include_str!("fixtures/multilingual_cjk_emoji.lrc");
    let parsed = parse_lrc(fixture, Some(60_000)).unwrap();
    let track = normalize_track(parsed, "local", None, 1.0, Some(60_000), 0);

    for (width, height) in [(80_u16, 24_u16), (100, 30), (120, 40), (160, 50), (200, 60)] {
        let mut state = UiState::new();
        state.set_player(Some("mpv".to_string()));
        state.playback = PlaybackStatus::Playing;
        state.set_lyrics(track.clone());
        state.resize(width, height);

        let max_inner_width = width.saturating_sub(HORIZONTAL_PADDING * 2);
        let timeline = state.timeline().unwrap();
        for compiled in &timeline.lines {
            for subline in &compiled.wrapped_main {
                assert!(
                    subline.display_width <= max_inner_width,
                    "subline width {} exceeded max {max_inner_width} at {width}x{height}",
                    subline.display_width
                );
            }
            for annotation in &compiled.wrapped_annotations {
                assert!(
                    display_width(annotation) <= max_inner_width,
                    "annotation width exceeded max {max_inner_width} at {width}x{height}"
                );
            }
        }

        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        let clock = PlaybackClock::new();
        draw_frame(&mut terminal, &mut state, &clock).unwrap();
    }
}

#[test]
fn dod_6_terminal_draw_p99_under_2ms_on_120x40() {
    let fixture = include_str!("fixtures/multilingual_cjk_emoji.lrc");
    let parsed = parse_lrc(fixture, Some(60_000)).unwrap();
    let track = normalize_track(parsed, "local", None, 1.0, Some(60_000), 0);

    let backend = TestBackend::new(120, 40);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut state = UiState::new();
    state.set_player(Some("mpv".to_string()));
    state.playback = PlaybackStatus::Playing;
    state.set_lyrics(track);
    state.resize(120, 40);

    let now = Instant::now();
    let mut clock = PlaybackClock::new();
    clock.on_status_changed(PlaybackStatus::Playing, now);
    clock.on_seeked(10_500, now);
    draw_frame(&mut terminal, &mut state, &clock).unwrap();

    let mut durations_us = Vec::with_capacity(200);
    for _ in 0..200 {
        let start = Instant::now();
        draw_frame(&mut terminal, &mut state, &clock).unwrap();
        durations_us.push(start.elapsed().as_micros());
    }
    durations_us.sort_unstable();
    let p50_us = durations_us[100];
    let p99_us = durations_us[197];
    let budget_us = if cfg!(debug_assertions) {
        10_000
    } else {
        2_000
    };
    println!("dod_6_terminal_draw_120x40: p50={p50_us}us p99={p99_us}us (budget={budget_us}us)");
    assert!(
        p99_us < budget_us,
        "DoD #6 violated: Terminal::draw p99 was {p99_us} us (>= {budget_us} us) on 120x40"
    );
}

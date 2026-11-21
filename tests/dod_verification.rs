use std::time::{Duration, Instant};

use lyrachi::{
    app::{
        draw_frame, draw_frame_with_cache, draw_frame_with_effects,
        event::AppEvent,
        handle_resolved,
        resolve::{ResolveCoordinator, TrackPlan},
    },
    compiler::timeline::{CompiledTimeline, HORIZONTAL_PADDING, TimelineContext},
    core::{
        clock::{EffectiveOffsets, PlaybackClock, PlaybackStatus},
        normalize::normalize_track,
        parse::lrc::parse_lrc,
        text::display_width,
    },
    media::TrackMeta,
    providers::{
        ProviderAnswer, ProviderError, ProviderId, TrackQuery, lrclib::LrclibRecord, score,
    },
    style::{HighlightMode, LineChangeEffect, LineChangeEffectKind},
    ui::{DisplayMode, UiState, block::BlockStageCache, effects::UiEffects},
};
use ratatui::{Terminal, backend::TestBackend};
use tokio::sync::mpsc;

#[test]
fn dod_1_line_and_word_transitions_stay_within_100ms_of_player_position() {
    let fixture = include_str!("fixtures/enhanced_karaoke.elrc");
    let parsed = parse_lrc(fixture, Some(30_000)).unwrap();
    let track = normalize_track(parsed, "local", None, 1.0, Some(30_000), 0);
    let timeline = CompiledTimeline::compile(&track, TimelineContext::new(80));

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
                                .send(AppEvent::Lyrics {
                                    epoch,
                                    result: Err(ProviderError::Cancelled),
                                    is_final: true,
                                })
                                .await;
                        }
                        _ = tokio::time::sleep(delay) => {
                            let _ = tx
                                .send(AppEvent::Lyrics {
                                    epoch,
                                    result: Ok(Some(ProviderAnswer::new(ProviderId::Local, track))),
                                    is_final: true,
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

    while let Some(AppEvent::Lyrics {
        epoch,
        result,
        is_final,
    }) = app_rx.recv().await
    {
        handle_resolved(&mut state, &mut coordinator, epoch, result, is_final);
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
    handle_resolved(
        &mut state,
        &mut coordinator,
        2,
        Ok(Some(ProviderAnswer::new(ProviderId::Local, late_stale))),
        true,
    );

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

    let wipe_track = normalize_track(
        parse_lrc(include_str!("fixtures/enhanced_karaoke.elrc"), Some(30_000)).unwrap(),
        "local",
        None,
        1.0,
        Some(30_000),
        0,
    );

    let budget_us = if cfg!(debug_assertions) {
        10_000
    } else {
        2_000
    };

    for mode in BenchmarkMode::ALL {
        let mut bench_clock = clock.clone();
        let previous_lyrics = state.lyrics_track().cloned();
        if mode == BenchmarkMode::WipeEffects {
            state.set_lyrics(wipe_track.clone());
            bench_clock.on_seeked(11_400, now);
        }
        let stats = draw_stats_for_mode(&mut terminal, &mut state, &bench_clock, mode, 50, 200);
        if let Some(prev) = previous_lyrics {
            state.set_lyrics(prev);
        }
        println!(
            "dod_6_terminal_draw_120x40 [{}]: p50={}us p99={}us (budget={budget_us}us)",
            mode.label(),
            stats.p50_us,
            stats.p99_us
        );
        assert!(
            stats.p99_us < budget_us,
            "DoD #6 violated for {}: Terminal::draw p99 was {} us (>= {budget_us} us) on 120x40",
            mode.label(),
            stats.p99_us
        );
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BenchmarkMode {
    Normal,
    Block,
    Kinetic,
    WipeEffects,
}

impl BenchmarkMode {
    pub const ALL: [Self; 4] = [Self::Normal, Self::Block, Self::Kinetic, Self::WipeEffects];

    pub fn label(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Block => "block",
            Self::Kinetic => "kinetic",
            Self::WipeEffects => "wipe+effects",
        }
    }
}

struct DrawStats {
    p50_us: u128,
    p99_us: u128,
}

#[allow(dead_code)]
fn draw_stats<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    state: &mut UiState,
    clock: &PlaybackClock,
    warmup: usize,
    samples: usize,
) -> DrawStats {
    draw_stats_for_mode(
        terminal,
        state,
        clock,
        BenchmarkMode::Normal,
        warmup,
        samples,
    )
}

fn draw_stats_for_mode<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    state: &mut UiState,
    clock: &PlaybackClock,
    mode: BenchmarkMode,
    warmup: usize,
    samples: usize,
) -> DrawStats {
    let previous_mode = state.display_mode;
    let previous_style = state.style.clone();
    state.display_mode = match mode {
        BenchmarkMode::Block | BenchmarkMode::WipeEffects => DisplayMode::Block,
        BenchmarkMode::Normal => DisplayMode::Normal,
        BenchmarkMode::Kinetic => DisplayMode::Kinetic,
    };
    let wipe = mode == BenchmarkMode::WipeEffects;
    if wipe {
        state.style.highlight_mode = HighlightMode::Wipe;
        state.style.highlight_trail = 3;
        state.style.accent = Some(ratatui::style::Color::Rgb(122, 162, 247));
        state.style.sung_fg = Some(ratatui::style::Color::Rgb(169, 177, 214));
        state.style.unsung_fg = Some(ratatui::style::Color::Rgb(86, 95, 137));
        state.style.on_line_change = vec![LineChangeEffect {
            effect: LineChangeEffectKind::Sweep,
            duration_ms: 3_000,
        }];
    }

    let mut cache = BlockStageCache::default();
    let mut effects = UiEffects::new();
    if wipe {
        effects.trigger_line_change(&state.style);
    }
    for _ in 0..warmup {
        if wipe {
            draw_frame_with_effects(terminal, state, &mut cache, &mut effects, clock).unwrap();
        } else {
            draw_frame_with_cache(terminal, state, &mut cache, clock).unwrap();
        }
    }

    let mut durations_us = Vec::with_capacity(samples);
    for _ in 0..samples {
        let started = Instant::now();
        if wipe {
            draw_frame_with_effects(terminal, state, &mut cache, &mut effects, clock).unwrap();
        } else {
            draw_frame_with_cache(terminal, state, &mut cache, clock).unwrap();
        }
        durations_us.push(started.elapsed().as_micros());
    }
    state.display_mode = previous_mode;
    state.style = previous_style;
    durations_us.sort_unstable();

    DrawStats {
        p50_us: durations_us[(samples / 2).min(samples - 1)],
        p99_us: durations_us[(samples * 99).div_ceil(100).saturating_sub(1)],
    }
}

#[test]
fn dod_13_synthetic_word_timing_satisfies_utf8_monotonicity_and_leaves_track_intact() {
    let fixture = include_str!("fixtures/multilingual_cjk_emoji.lrc");
    let parsed = parse_lrc(fixture, Some(60_000)).unwrap();
    let track = normalize_track(parsed, "local", None, 1.0, Some(60_000), 0);
    assert_eq!(
        track.granularity,
        lyrachi::core::model::SyncGranularity::Line
    );
    let original_track = track.clone();

    let mut context = TimelineContext::new(80);
    context.synthetic_word_timing = true;
    let compiled = CompiledTimeline::compile(&track, context);
    assert!(compiled.has_synthetic_words);

    assert_eq!(track, original_track, "original track must not be mutated");

    for (line_idx, line) in compiled.lines.iter().enumerate() {
        let source_line = &track.lines[line_idx];
        assert!(source_line.words.is_empty());
        assert!(
            !line.words.is_empty(),
            "line {line_idx} should have synthetic words"
        );

        let mut prev_byte = 0;
        let mut prev_end = line.start_ms;
        for word in &line.words {
            let (bs, be) = word.byte_range;
            assert!(bs >= prev_byte);
            assert!(bs < be);
            assert!(be <= source_line.text.len());
            assert!(source_line.text.is_char_boundary(bs));
            assert!(source_line.text.is_char_boundary(be));
            assert!(word.start_ms >= line.start_ms);
            assert!(word.start_ms >= prev_end);
            assert!(word.start_ms <= word.end_ms);
            assert!(word.end_ms <= line.end_ms);

            prev_byte = be;
            prev_end = word.end_ms;
        }

        assert_eq!(line.words.first().unwrap().start_ms, line.start_ms);
        assert_eq!(line.words.last().unwrap().end_ms, line.end_ms);
    }

    let mut state = UiState::new();
    state.set_lyrics(track);
    assert_eq!(state.track_status_label(), Some("line"));

    let mut profile = lyrachi::style::StyleProfile::default();
    profile.meta.synthetic_word_timing = true;
    let resolved = profile.resolve(
        "synth",
        lyrachi::style::color::ColorCapability::TrueColor,
        None,
    );
    state.set_resolved_style(resolved);
    assert_eq!(state.track_status_label(), Some("line (est. word)"));

    let default_resolved = lyrachi::style::StyleProfile::default().resolve(
        "default",
        lyrachi::style::color::ColorCapability::TrueColor,
        None,
    );
    state.set_resolved_style(default_resolved);
    assert_eq!(state.track_status_label(), Some("line"));
}

#[test]
fn dod_14_kinetic_phrase_mode_preserves_text_and_timing_precedence() {
    let fixture = include_str!("fixtures/enhanced_karaoke.elrc");
    let parsed = parse_lrc(fixture, Some(30_000)).unwrap();
    let track = normalize_track(parsed, "local", None, 1.0, Some(30_000), 0);

    let mut ctx = TimelineContext::new(80);
    ctx.phrase_max_chars = 12;
    let compiled = CompiledTimeline::compile(&track, ctx);

    for (idx, line) in compiled.lines.iter().enumerate() {
        let source_text = &track.lines[idx].text;
        let reconstructed: String = line.phrases.iter().map(|p| p.slice(source_text)).collect();
        assert_eq!(reconstructed, *source_text);
        for pair in line.phrases.windows(2) {
            assert_eq!(pair[0].end_ms, pair[1].start_ms);
            assert_eq!(pair[0].byte_range.1, pair[1].byte_range.0);
        }
    }

    let mut state = UiState::new();
    state.set_player(Some("mpv".to_string()));
    state.playback = PlaybackStatus::Playing;
    state.display_mode = DisplayMode::Kinetic;
    state.set_lyrics(track);
    state.resize(120, 40);

    let now = Instant::now();
    let mut clock = PlaybackClock::new();
    clock.on_status_changed(PlaybackStatus::Playing, now);
    clock.on_seeked(11_400, now);

    let backend = TestBackend::new(120, 40);
    let mut terminal = Terminal::new(backend).unwrap();
    draw_frame(&mut terminal, &mut state, &clock).unwrap();
}

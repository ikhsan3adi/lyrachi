use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use futures_util::StreamExt;
use ratatui::DefaultTerminal;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, debug_span, warn};

use crate::{
    config::RuntimeConfig,
    core::clock::{ClockState, EffectiveOffsets, PlaybackClock, PlaybackStatus},
    media::{MediaEvent, TrackMeta, mpris::arbiter},
    offsets::{DebouncedOffsetWriter, OffsetStore},
    providers::{ProviderError, ProviderResolver, TrackQuery},
    ui::{UiState, render},
};

pub mod event;
pub mod resolve;

use event::AppEvent;
use resolve::{ResolveCoordinator, TrackPlan};

pub async fn run(
    terminal: &mut DefaultTerminal,
    config: RuntimeConfig,
    warnings: Vec<String>,
    offsets_file: PathBuf,
) -> color_eyre::Result<()> {
    let resolver = match ProviderResolver::with_config(
        config.local_dirs.clone(),
        config.provider_order.clone(),
        config.network,
    ) {
        Ok(resolver) => Some(Arc::new(resolver)),
        Err(error) => {
            warn!(%error, "lyrics providers unavailable");
            None
        }
    };
    let mut state = UiState::new();
    state.global_offset_ms = config.global_offset_ms;
    state.lookahead_ms = config.lookahead_ms;
    state.offline = !config.network;
    state.config_warning = warnings.first().cloned();
    let mut clock = PlaybackClock::new();
    let mut coordinator = ResolveCoordinator::with_cache_capacity(config.l1_entries);
    let mut events = EventStream::new();
    let (media_tx, mut media_rx) = mpsc::channel(256);
    let (app_tx, mut app_rx) = mpsc::channel(64);
    let cancel = CancellationToken::new();
    let player_filter = config.player_filter.clone();
    let monitor_cancel = cancel.clone();
    let monitor_tx = media_tx.clone();
    let monitor = match zbus::Connection::session().await {
        Ok(conn) => Some(tokio::spawn(async move {
            if let Err(error) = arbiter::run(player_filter, conn, monitor_tx, monitor_cancel).await
            {
                warn!(%error, "MPRIS monitor stopped");
            }
        })),
        Err(error) => {
            warn!(%error, "session bus unavailable");
            None
        }
    };
    drop(media_tx);
    let mut offsets = DebouncedOffsetWriter::new(OffsetStore::load(&offsets_file));

    let loop_result: color_eyre::Result<()> = async {
        loop {
            let now = Instant::now();
            if let Err(error) = offsets.poll(now) {
                warn!(%error, "failed to persist lyric offset");
            }

            draw_frame(terminal, &mut state, &clock)?;

            let wait = next_wait(&state, &clock, now);
            let wait = offsets
                .time_until_flush(now)
                .map_or(wait, |flush_wait| wait.min(flush_wait));

            tokio::select! {
                event = events.next() => {
                    let Some(event) = event else {
                        break;
                    };
                    let event = event?;
                    if handle_input(&mut state, &mut coordinator, &mut offsets, event) {
                        break;
                    }
                }
                Some(event) = media_rx.recv() => {
                    if let MediaEvent::TrackChanged(meta) = &event {
                        let key = TrackQuery::from_meta(meta, 0).canonical;
                        let stored_offset = offsets.offset_for(&key);
                        coordinator.seed_user_offset(key, stored_offset);
                    }
                    handle_media(
                        event,
                        &mut state,
                        &mut clock,
                        &mut coordinator,
                        &app_tx,
                        resolver.as_ref(),
                        Instant::now(),
                    );
                }
                Some(event) = app_rx.recv() => {
                    let AppEvent::Resolved {
                        epoch: resolved_epoch,
                        result,
                    } = event;
                    handle_resolved(&mut state, &mut coordinator, resolved_epoch, result);
                }
                _ = tokio::time::sleep(wait) => {}
            }
        }

        Ok(())
    }
    .await;

    coordinator.cancel_active();
    cancel.cancel();
    if let Some(monitor) = monitor {
        monitor.abort();
        let _ = monitor.await;
    }

    let flush_result = offsets.flush();
    loop_result?;
    if let Err(error) = flush_result {
        warn!(%error, "failed to persist lyric offset on exit");
    }

    Ok(())
}

pub async fn run_dump_events(player_filter: Option<String>) -> color_eyre::Result<()> {
    let (tx, mut rx) = mpsc::channel(256);
    let cancel = CancellationToken::new();
    let monitor_cancel = cancel.clone();
    let conn = zbus::Connection::session().await?;
    let mut monitor =
        tokio::spawn(async move { arbiter::run(player_filter, conn, tx, monitor_cancel).await });
    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);

    loop {
        tokio::select! {
            _ = &mut ctrl_c => {
                cancel.cancel();
                break;
            }
            event = rx.recv() => {
                match event {
                    Some(event) => {
                        println!("{event:?}");
                        let _ = std::io::stdout().flush();
                    }
                    None => break,
                }
            }
            result = &mut monitor => {
                match result {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => eprintln!("lyrachi: MPRIS monitor failed: {error:?}"),
                    Err(error) => eprintln!("lyrachi: MPRIS monitor task failed: {error}"),
                }
                break;
            }
        }
    }

    cancel.cancel();
    monitor.abort();
    Ok(())
}

pub fn draw_frame<B: ratatui::backend::Backend>(
    terminal: &mut ratatui::Terminal<B>,
    state: &mut UiState,
    clock: &PlaybackClock,
) -> Result<(), B::Error> {
    let draw_started = Instant::now();
    let span = debug_span!(
        "terminal_draw",
        width = state.viewport_width,
        height = state.viewport_height,
        elapsed_us = tracing::field::Empty,
    );
    let _enter = span.enter();

    terminal.draw(|frame| {
        let area = frame.area();
        state.resize(area.width, area.height);
        let now = Instant::now();
        let effective_ms = clock.effective_lyric_ms(now, effective_offsets(state));
        render::render(frame, state, effective_ms);
    })?;

    let elapsed_us = u64::try_from(draw_started.elapsed().as_micros()).unwrap_or(u64::MAX);
    span.record("elapsed_us", elapsed_us);
    debug!(
        elapsed_us,
        width = state.viewport_width,
        height = state.viewport_height,
        "terminal frame drawn"
    );

    Ok(())
}

pub fn handle_media(
    event: MediaEvent,
    state: &mut UiState,
    clock: &mut PlaybackClock,
    coordinator: &mut ResolveCoordinator,
    app_tx: &mpsc::Sender<AppEvent>,
    resolver: Option<&Arc<ProviderResolver>>,
    now: Instant,
) {
    match event {
        MediaEvent::PlayerChanged { bus, .. } => {
            state.set_player(Some(player_label(&bus)));
        }
        MediaEvent::NoPlayer => {
            coordinator.cancel_active();
            clock.on_status_changed(PlaybackStatus::Stopped, now);
            state.set_no_player();
        }
        MediaEvent::TrackChanged(meta) => {
            clock.on_seeked(0, now);
            state.set_track(meta.clone());
            match coordinator.prepare_track(&meta) {
                TrackPlan::Unusable { .. } => {
                    state.set_not_found();
                }
                TrackPlan::Cached {
                    user_offset_ms,
                    track,
                    ..
                } => {
                    state.user_offset_ms = user_offset_ms;
                    state.set_lyrics(track);
                }
                TrackPlan::Fetch {
                    epoch: request_epoch,
                    user_offset_ms,
                    query,
                    cancel: request_cancel,
                } => {
                    state.user_offset_ms = user_offset_ms;
                    let Some(resolver) = resolver else {
                        state.set_error("Lyrics providers unavailable.".to_string());
                        return;
                    };
                    let resolver = Arc::clone(resolver);
                    let app_tx = app_tx.clone();
                    tokio::spawn(async move {
                        let result = resolver.resolve_query(&query, &request_cancel).await;
                        if matches!(result, Err(ProviderError::Cancelled)) {
                            return;
                        }
                        let _ = app_tx
                            .send(AppEvent::Resolved {
                                epoch: request_epoch,
                                result,
                            })
                            .await;
                    });
                }
            }
        }
        MediaEvent::NoTrack => {
            coordinator.cancel_active();
            clock.on_status_changed(PlaybackStatus::Stopped, now);
            state.set_no_track();
        }
        MediaEvent::Status(status) => {
            clock.on_status_changed(status, now);
            state.playback = status;
        }
        MediaEvent::Rate(rate) => clock.on_rate_changed(rate, now),
        MediaEvent::Seeked { pos_ms } => clock.on_seeked(pos_ms, now),
        MediaEvent::PositionPoll { pos_ms, rtt } => {
            clock.on_position_poll(pos_ms, rtt, now);
            let raw_ms = clock.raw_position_at(now);
            let effective_ms = clock.effective_lyric_ms(now, effective_offsets(state));
            let rtt_ms = u64::try_from(rtt.as_millis()).unwrap_or(u64::MAX);
            debug!(
                polled_pos_ms = pos_ms,
                rtt_ms,
                clock_raw_ms = raw_ms,
                effective_lyric_ms = effective_ms,
                "position poll synchronized"
            );
        }
    }
}

pub fn handle_resolved(
    state: &mut UiState,
    coordinator: &mut ResolveCoordinator,
    resolved_epoch: u64,
    result: Result<Option<crate::core::model::SyncedTrack>, ProviderError>,
) {
    match result {
        Ok(track) => {
            if let Some(outcome) = coordinator.complete_epoch(resolved_epoch, track) {
                match outcome {
                    Some(track) => {
                        state.user_offset_ms = track.user_offset_ms;
                        state.set_lyrics(track);
                    }
                    None => state.set_not_found(),
                }
            }
        }
        Err(ProviderError::Cancelled) => {}
        Err(error) => {
            if coordinator.complete_epoch(resolved_epoch, None).is_some() {
                state.set_error(error.to_string());
            }
        }
    }
}

fn handle_input(
    state: &mut UiState,
    coordinator: &mut ResolveCoordinator,
    offsets: &mut DebouncedOffsetWriter,
    event: Event,
) -> bool {
    let Event::Key(key) = event else {
        return false;
    };
    if key.kind != KeyEventKind::Press {
        return false;
    }

    match key.code {
        KeyCode::Char('q') => true,
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => true,
        KeyCode::Char('[') => {
            state.adjust_offset(-100);
            record_offset(state, coordinator, offsets);
            false
        }
        KeyCode::Char(']') => {
            state.adjust_offset(100);
            record_offset(state, coordinator, offsets);
            false
        }
        KeyCode::Char('\\') => {
            state.reset_offset();
            record_offset(state, coordinator, offsets);
            false
        }
        KeyCode::Up => {
            state.scroll_up();
            false
        }
        KeyCode::Down => {
            state.scroll_down();
            false
        }
        _ => false,
    }
}

fn record_offset(
    state: &UiState,
    coordinator: &ResolveCoordinator,
    offsets: &mut DebouncedOffsetWriter,
) {
    let (Some(track), Some(key)) = (state.track.as_ref(), coordinator.active_key()) else {
        return;
    };
    offsets.set(key, &track_label(track), state.user_offset_ms);
}

fn track_label(track: &TrackMeta) -> String {
    if track.artist.is_empty() {
        track.title.clone()
    } else {
        format!("{} - {}", track.artist, track.title)
    }
}

fn next_wait(state: &UiState, clock: &PlaybackClock, now: Instant) -> Duration {
    if !matches!(clock.state(), ClockState::Playing { .. }) {
        return Duration::from_secs(1);
    }
    let Some(timeline) = state.timeline() else {
        return Duration::from_secs(1);
    };
    if state.is_unsynced() {
        return Duration::from_secs(1);
    }

    let effective_ms = clock.effective_lyric_ms(now, effective_offsets(state));
    let cursor = timeline.evaluate(effective_ms);
    let next = timeline
        .next_boundary_ms(effective_ms)
        .unwrap_or_else(|| effective_ms.saturating_add(1_000));
    let media_delta_ms = next.saturating_sub(effective_ms).clamp(1, 1_000);
    let rate = clock.rate().max(0.01);
    let wall_ms = ((media_delta_ms as f64) / rate).round().clamp(1.0, 1_000.0) as u64;
    let boundary_wait = Duration::from_millis(wall_ms);

    if cursor.active_word_idx.is_some() {
        boundary_wait.min(Duration::from_millis(16))
    } else {
        boundary_wait
    }
}

fn effective_offsets(state: &UiState) -> EffectiveOffsets {
    EffectiveOffsets {
        intrinsic_ms: state
            .lyrics_track()
            .map(|track| track.intrinsic_offset_ms)
            .unwrap_or_default(),
        global_ms: state.global_offset_ms,
        player_ms: 0,
        user_ms: state.user_offset_ms,
        lookahead_ms: i64::try_from(state.lookahead_ms).unwrap_or(i64::MAX),
    }
}

fn player_label(bus: &str) -> String {
    let short = bus.strip_prefix("org.mpris.MediaPlayer2.").unwrap_or(bus);
    short.split('.').next().unwrap_or(short).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::model::{LineRole, LyricLine, SyncGranularity, SyncedTrack};

    fn track() -> SyncedTrack {
        SyncedTrack {
            source_provider: "test".to_string(),
            source_track_id: None,
            parser_version: 1,
            is_instrumental: false,
            granularity: SyncGranularity::Word,
            intrinsic_offset_ms: 0,
            user_offset_ms: 0,
            match_confidence: 1.0,
            lines: vec![LyricLine {
                start_ms: 1_000,
                end_ms: 2_000,
                role: LineRole::Primary,
                text: "word".to_string(),
                words: vec![crate::core::model::WordToken {
                    byte_range: (0, 4),
                    start_ms: 1_000,
                    end_ms: 2_000,
                    display_width: 4,
                }],
                annotations: Vec::new(),
            }],
        }
    }

    #[test]
    fn adaptive_wait_uses_word_and_line_boundaries() {
        let now = Instant::now();
        let mut clock = PlaybackClock::new();
        clock.on_status_changed(PlaybackStatus::Playing, now);
        let mut state = UiState::new();
        state.set_lyrics(track());

        assert_eq!(next_wait(&state, &clock, now), Duration::from_secs(1));
        assert_eq!(
            next_wait(&state, &clock, now + Duration::from_millis(1_100)),
            Duration::from_millis(16)
        );
        assert_eq!(
            next_wait(&state, &clock, now + Duration::from_millis(1_995)),
            Duration::from_millis(6)
        );

        clock.on_rate_changed(2.0, now);
        assert_eq!(next_wait(&state, &clock, now), Duration::from_millis(500));
    }

    #[test]
    fn paused_state_waits_up_to_one_second() {
        let now = Instant::now();
        let clock = PlaybackClock::new();
        let state = UiState::new();

        assert_eq!(next_wait(&state, &clock, now), Duration::from_secs(1));
    }

    #[test]
    fn handle_media_resets_clock_and_user_offset_on_track_and_player_loss() {
        let base = Instant::now();
        let mut clock = PlaybackClock::new();
        clock.on_status_changed(PlaybackStatus::Playing, base);
        let mut state = UiState::new();
        state.adjust_offset(300);
        let mut coordinator = ResolveCoordinator::new();
        let (app_tx, _app_rx) = mpsc::channel(4);

        let later = base + Duration::from_secs(12);
        assert_eq!(clock.raw_position_at(later), 12_000);

        handle_media(
            MediaEvent::TrackChanged(crate::media::TrackMeta {
                title: "Next".to_string(),
                artist: "Artist".to_string(),
                album: None,
                duration_ms: None,
                track_id: None,
                url: None,
            }),
            &mut state,
            &mut clock,
            &mut coordinator,
            &app_tx,
            None,
            later,
        );
        assert_eq!(coordinator.epoch(), 1);
        assert_eq!(state.user_offset_ms, 0);
        assert_eq!(clock.raw_position_at(later), 0);

        handle_media(
            MediaEvent::NoTrack,
            &mut state,
            &mut clock,
            &mut coordinator,
            &app_tx,
            None,
            later,
        );
        assert_eq!(coordinator.epoch(), 2);
        assert_eq!(clock.state(), &ClockState::Stopped);

        clock.on_status_changed(PlaybackStatus::Playing, later);
        state.adjust_offset(100);
        handle_media(
            MediaEvent::NoPlayer,
            &mut state,
            &mut clock,
            &mut coordinator,
            &app_tx,
            None,
            later,
        );
        assert_eq!(coordinator.epoch(), 3);
        assert_eq!(state.user_offset_ms, 0);
        assert_eq!(clock.state(), &ClockState::Stopped);
    }

    #[tokio::test]
    async fn rapid_track_skipping_cancels_in_flight_requests_and_discards_stale_epochs() {
        let mut state = UiState::new();
        let mut coordinator = ResolveCoordinator::new();
        let (app_tx, mut app_rx) = mpsc::channel(16);
        let mut tokens = Vec::new();
        let mut handles = Vec::new();

        for index in 1..=5_u64 {
            let meta = crate::media::TrackMeta {
                title: format!("Song {index}"),
                artist: "Coldplay".to_string(),
                album: Some("Parachutes".to_string()),
                duration_ms: Some(240_000),
                track_id: Some(format!("/track/{index}")),
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
                    let latency = Duration::from_millis((6 - index) * 15);
                    handles.push(tokio::spawn(async move {
                        let mut lrc = track();
                        lrc.source_track_id = Some(query.title.clone());
                        lrc.lines[0].text = format!("Lyrics for {}", query.title);

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
                            _ = tokio::time::sleep(latency) => {
                                let _ = tx
                                    .send(AppEvent::Resolved {
                                        epoch,
                                        result: Ok(Some(lrc)),
                                    })
                                    .await;
                            }
                        }
                    }));
                }
                other => panic!("expected fetch plan for track {index}, got {other:?}"),
            }
        }
        drop(app_tx);

        for token in &tokens[..4] {
            assert!(
                token.is_cancelled(),
                "earlier track token must be cancelled on next track"
            );
        }
        assert!(
            !tokens[4].is_cancelled(),
            "final track token must remain active"
        );

        for handle in handles {
            handle.await.unwrap();
        }

        while let Some(AppEvent::Resolved {
            epoch: resolved_epoch,
            result,
        }) = app_rx.recv().await
        {
            handle_resolved(&mut state, &mut coordinator, resolved_epoch, result);
        }

        // Even if a delayed response for epoch 1 arrives after epoch 5 resolved, it is ignored.
        let mut stale_track = track();
        stale_track.source_track_id = Some("Song 1".to_string());
        handle_resolved(&mut state, &mut coordinator, 1, Ok(Some(stale_track)));

        let loaded = state
            .lyrics_track()
            .expect("final track lyrics must be loaded");
        assert_eq!(loaded.source_track_id.as_deref(), Some("Song 5"));
        assert_eq!(loaded.lines[0].text, "Lyrics for Song 5");
    }

    #[test]
    fn draw_frame_p99_stays_under_two_milliseconds_at_120x40() {
        let backend = ratatui::backend::TestBackend::new(120, 40);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let mut state = UiState::new();
        state.set_player(Some("mpv".to_string()));
        state.playback = PlaybackStatus::Playing;
        state.set_lyrics(track());
        state.resize(120, 40);
        let mut clock = PlaybackClock::new();
        clock.on_status_changed(PlaybackStatus::Playing, Instant::now());
        draw_frame(&mut terminal, &mut state, &clock).unwrap();

        let mut samples_us = Vec::with_capacity(200);
        for _ in 0..200 {
            let started = Instant::now();
            draw_frame(&mut terminal, &mut state, &clock).unwrap();
            samples_us.push(started.elapsed().as_micros());
        }
        samples_us.sort_unstable();
        let p99_us = samples_us[197];
        let budget_us = if cfg!(debug_assertions) {
            10_000
        } else {
            2_000
        };
        assert!(
            p99_us < budget_us,
            "expected Terminal::draw p99 < {budget_us} us on 120x40, got {p99_us} us"
        );
    }

    #[test]
    fn player_labels_strip_bus_and_instance_suffix() {
        assert_eq!(
            player_label("org.mpris.MediaPlayer2.firefox.instance1"),
            "firefox"
        );
        assert_eq!(player_label("limusic"), "limusic");
    }
}

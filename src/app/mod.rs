use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use futures_util::StreamExt;
use ratatui::DefaultTerminal;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, debug_span, warn};

use crate::{
    cache::CacheHandle,
    config::RuntimeConfig,
    core::clock::{ClockState, EffectiveOffsets, PlaybackClock, PlaybackStatus},
    media::{MediaEvent, TrackMeta, mpris::arbiter, player_short_name},
    offsets::{DebouncedOffsetWriter, OffsetStore},
    prefs::{self, UiPrefs},
    providers::engine::EngineUpdate,
    providers::{ProviderAnswer, ProviderError, ProviderResolver, Tier, TrackQuery},
    ui::{UiState, render},
};

pub mod event;
pub mod pipe;
pub mod resolve;
pub mod status;

use event::AppEvent;
use resolve::{ResolveCoordinator, TrackPlan, resolve_with_updates};

#[derive(Clone, Copy)]
pub struct Resolution<'a> {
    pub resolver: Option<&'a Arc<ProviderResolver>>,
    pub cache: Option<&'a CacheHandle>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AnimationStatus {
    pub effects_active: bool,
    pub countdown_active: bool,
    pub ribbon_animating: bool,
}

impl AnimationStatus {
    pub fn needs_frame_tick(self) -> bool {
        self.effects_active || self.countdown_active || self.ribbon_animating
    }
}

#[derive(Debug, Default)]
pub struct AppRuntime {
    pub animation: AnimationStatus,
}

#[derive(Debug, Default)]
struct InputOutcome {
    quit: bool,
    refetch: Option<TrackPlan>,
}

impl InputOutcome {
    fn quit() -> Self {
        Self {
            quit: true,
            refetch: None,
        }
    }

    fn refetch(plan: TrackPlan) -> Self {
        Self {
            quit: false,
            refetch: Some(plan),
        }
    }
}

pub async fn run(
    terminal: &mut DefaultTerminal,
    config: RuntimeConfig,
    warnings: Vec<String>,
    offsets_file: PathBuf,
    config_file: PathBuf,
    cache: Option<CacheHandle>,
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
    state.configure_layout(config.fullscreen, config.show_header, config.show_status);
    state.set_ambiguous_width(config.ambiguous_width);
    state.set_terminal_unicode(config.unicode.is_full());
    refresh_hidden_status_banner(&mut state, Instant::now());
    let mut clock = PlaybackClock::new();
    let runtime = AppRuntime::default();
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

            let wait = next_wait(&state, &clock, now, runtime.animation);
            let wait = offsets
                .time_until_flush(now)
                .map_or(wait, |flush_wait| wait.min(flush_wait));
            let wait = state
                .time_until_banner_expiry(now)
                .map_or(wait, |banner_wait| wait.min(banner_wait));

            tokio::select! {
                event = events.next() => {
                    let Some(event) = event else {
                        break;
                    };
                    let event = event?;
                    let outcome = handle_input(
                        &mut state,
                        &mut coordinator,
                        &mut offsets,
                        Some(&config_file),
                        Instant::now(),
                        event,
                    );
                    if outcome.quit {
                        break;
                    }
                    if let Some(plan) = outcome.refetch {
                        apply_plan(
                            plan,
                            &mut state,
                            &app_tx,
                            Resolution {
                                resolver: resolver.as_ref(),
                                cache: cache.as_ref(),
                            },
                        );
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
                        Resolution {
                            resolver: resolver.as_ref(),
                            cache: cache.as_ref(),
                        },
                        Instant::now(),
                    );
                }
                Some(event) = app_rx.recv() => {
                    let AppEvent::Lyrics {
                        epoch: resolved_epoch,
                        result,
                        is_final,
                    } = event;
                    handle_resolved(
                        &mut state,
                        &mut coordinator,
                        resolved_epoch,
                        result,
                        is_final,
                    );
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
    if let Some(cache) = cache
        && let Err(error) = cache.shutdown().await
    {
        warn!(%error, "failed to shut down lyric cache");
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
        let now = Instant::now();
        state.expire_banner(now);
        state.resize(area.width, area.height);
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

fn apply_plan(
    plan: TrackPlan,
    state: &mut UiState,
    app_tx: &mpsc::Sender<AppEvent>,
    resolution: Resolution<'_>,
) {
    match plan {
        TrackPlan::Unusable { user_offset_ms, .. } => {
            state.user_offset_ms = user_offset_ms;
            state.refetching = false;
            state.set_not_found();
        }
        TrackPlan::Cached {
            user_offset_ms,
            answer,
            ..
        } => {
            state.user_offset_ms = user_offset_ms;
            state.refetching = false;
            state.set_lyrics(answer.track);
        }
        TrackPlan::Fetch {
            epoch: request_epoch,
            user_offset_ms,
            query,
            cancel: request_cancel,
            bypass_cache,
        } => {
            state.user_offset_ms = user_offset_ms;
            start_fetch(
                state,
                app_tx,
                resolution,
                request_epoch,
                query,
                request_cancel,
                bypass_cache,
            );
        }
    }
}

fn start_fetch(
    state: &mut UiState,
    app_tx: &mpsc::Sender<AppEvent>,
    resolution: Resolution<'_>,
    request_epoch: u64,
    query: TrackQuery,
    request_cancel: CancellationToken,
    bypass_cache: bool,
) {
    let Some(resolver) = resolution.resolver else {
        state.refetching = false;
        state.set_error("Lyrics providers unavailable.".to_string());
        return;
    };
    let resolver = Arc::clone(resolver);
    let cache = resolution.cache.cloned();
    let app_tx = app_tx.clone();
    tokio::spawn(async move {
        if bypass_cache
            && let Some(cache) = cache.as_ref()
            && let Err(error) = cache.invalidate_track(&query.canonical).await
        {
            warn!(%error, "failed to invalidate cached lyrics for refetch");
        }

        let (updates_tx, mut updates_rx) = mpsc::unbounded_channel::<EngineUpdate>();
        let forward_tx = app_tx.clone();
        let forward = tokio::spawn(async move {
            while let Some(update) = updates_rx.recv().await {
                if forward_tx
                    .send(AppEvent::Lyrics {
                        epoch: request_epoch,
                        result: update.result,
                        is_final: update.is_final,
                    })
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });
        let _ = resolve_with_updates(
            &resolver,
            cache.as_ref(),
            &query,
            &request_cancel,
            &updates_tx,
            bypass_cache,
        )
        .await;
        drop(updates_tx);
        let _ = forward.await;
    });
}

pub fn handle_media(
    event: MediaEvent,
    state: &mut UiState,
    clock: &mut PlaybackClock,
    coordinator: &mut ResolveCoordinator,
    app_tx: &mpsc::Sender<AppEvent>,
    resolution: Resolution<'_>,
    now: Instant,
) {
    match event {
        MediaEvent::PlayerChanged { bus, .. } => {
            state.set_player(Some(player_short_name(&bus).to_string()));
        }
        MediaEvent::NoPlayer => {
            coordinator.cancel_active();
            clock.on_status_changed(PlaybackStatus::Stopped, now);
            state.set_no_player();
        }
        MediaEvent::TrackChanged(meta) => {
            clock.on_seeked(0, now);
            state.set_track(meta.clone());
            let plan = coordinator.prepare_track(&meta);
            apply_plan(plan, state, app_tx, resolution);
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
    result: Result<Option<ProviderAnswer>, ProviderError>,
    is_final: bool,
) {
    match result {
        Ok(answer) => {
            let Some(outcome) = coordinator.complete_epoch(resolved_epoch, answer, is_final) else {
                return;
            };
            if is_final {
                state.refetching = false;
            }
            match outcome {
                Some(answer) => {
                    let next_tier = answer.tier;
                    let current = state.lyrics_track();
                    let current_tier = current.map(Tier::from_track);
                    let unchanged = current == Some(&answer.track);
                    let replace = !unchanged
                        && current_tier
                            .is_none_or(|tier| next_tier > tier || (is_final && next_tier == tier));
                    if replace {
                        state.user_offset_ms = answer.track.user_offset_ms;
                        state.set_lyrics(answer.track);
                    }
                }
                None => {
                    if is_final {
                        state.set_not_found();
                    }
                }
            }
        }
        Err(ProviderError::Cancelled) => {}
        Err(error) => {
            if coordinator
                .complete_epoch(resolved_epoch, None, true)
                .is_some()
            {
                state.refetching = false;
                state.set_error(error.to_string());
            }
        }
    }
}

fn handle_input(
    state: &mut UiState,
    coordinator: &mut ResolveCoordinator,
    offsets: &mut DebouncedOffsetWriter,
    config_file: Option<&Path>,
    now: Instant,
    event: Event,
) -> InputOutcome {
    let Event::Key(key) = event else {
        return InputOutcome::default();
    };
    if key.kind != KeyEventKind::Press {
        return InputOutcome::default();
    }

    match key.code {
        KeyCode::Char('q') => InputOutcome::quit(),
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => InputOutcome::quit(),
        KeyCode::Char('r') => match coordinator.force_refetch_current_track() {
            Some(plan) => {
                state.refetching = true;
                InputOutcome::refetch(plan)
            }
            None => {
                state.show_banner("No track to refetch", now);
                InputOutcome::default()
            }
        },
        KeyCode::Char('f') => {
            state.toggle_fullscreen();
            persist_layout(state, config_file, now);
            InputOutcome::default()
        }
        KeyCode::Char('t') => {
            state.toggle_header();
            persist_layout(state, config_file, now);
            InputOutcome::default()
        }
        KeyCode::Char('s') => {
            state.toggle_status();
            persist_layout(state, config_file, now);
            InputOutcome::default()
        }
        KeyCode::Char('[') => {
            state.adjust_offset(-100);
            record_offset(state, coordinator, offsets);
            InputOutcome::default()
        }
        KeyCode::Char(']') => {
            state.adjust_offset(100);
            record_offset(state, coordinator, offsets);
            InputOutcome::default()
        }
        KeyCode::Char('\\') => {
            state.reset_offset();
            record_offset(state, coordinator, offsets);
            InputOutcome::default()
        }
        KeyCode::Up => {
            state.scroll_up();
            InputOutcome::default()
        }
        KeyCode::Down => {
            state.scroll_down();
            InputOutcome::default()
        }
        _ => InputOutcome::default(),
    }
}

fn persist_layout(state: &mut UiState, config_file: Option<&Path>, now: Instant) {
    refresh_hidden_status_banner(state, now);

    let Some(config_file) = config_file else {
        return;
    };
    let (fullscreen, show_header, show_status) = state.persisted_layout();
    let prefs = UiPrefs {
        fullscreen,
        show_header,
        show_status,
    };
    if let Err(error) = prefs::save_ui_prefs(config_file, prefs) {
        let message = format!("Could not save preferences: {error}");
        state.config_warning = Some(message.clone());
        state.show_banner(message, now);
    }
}

fn refresh_hidden_status_banner(state: &mut UiState, now: Instant) {
    if state.show_status {
        state.clear_banner();
        return;
    }

    let message = state
        .config_warning
        .clone()
        .or_else(|| state.offline.then(|| "Offline mode".to_string()));
    if let Some(message) = message {
        state.show_banner(message, now);
    } else {
        state.clear_banner();
    }
}

fn record_offset(
    state: &UiState,
    coordinator: &mut ResolveCoordinator,
    offsets: &mut DebouncedOffsetWriter,
) {
    coordinator.record_user_offset(state.user_offset_ms);
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

fn next_wait(
    state: &UiState,
    clock: &PlaybackClock,
    now: Instant,
    animation: AnimationStatus,
) -> Duration {
    if animation.needs_frame_tick() {
        return Duration::from_millis(16);
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::model::{LineRole, LyricLine, SyncGranularity, SyncedTrack};
    use crate::providers::Provider;

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

        assert_eq!(
            next_wait(&state, &clock, now, AnimationStatus::default()),
            Duration::from_secs(1)
        );
        assert_eq!(
            next_wait(
                &state,
                &clock,
                now + Duration::from_millis(1_100),
                AnimationStatus::default()
            ),
            Duration::from_millis(16)
        );
        assert_eq!(
            next_wait(
                &state,
                &clock,
                now + Duration::from_millis(1_995),
                AnimationStatus::default()
            ),
            Duration::from_millis(6)
        );

        clock.on_rate_changed(2.0, now);
        assert_eq!(
            next_wait(&state, &clock, now, AnimationStatus::default()),
            Duration::from_millis(500)
        );
    }

    #[test]
    fn animation_status_requests_frame_ticks() {
        let now = Instant::now();
        let clock = PlaybackClock::new();
        let state = UiState::new();
        let animation = AnimationStatus {
            effects_active: true,
            ..AnimationStatus::default()
        };

        assert_eq!(
            next_wait(&state, &clock, now, animation),
            Duration::from_millis(16)
        );
        assert!(
            AnimationStatus {
                countdown_active: true,
                ..AnimationStatus::default()
            }
            .needs_frame_tick()
        );
        assert!(
            AnimationStatus {
                ribbon_animating: true,
                ..AnimationStatus::default()
            }
            .needs_frame_tick()
        );
        assert!(!AnimationStatus::default().needs_frame_tick());
    }

    #[test]
    fn paused_state_waits_up_to_one_second() {
        let now = Instant::now();
        let clock = PlaybackClock::new();
        let state = UiState::new();

        assert_eq!(
            next_wait(&state, &clock, now, AnimationStatus::default()),
            Duration::from_secs(1)
        );
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
            Resolution {
                resolver: None,
                cache: None,
            },
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
            Resolution {
                resolver: None,
                cache: None,
            },
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
            Resolution {
                resolver: None,
                cache: None,
            },
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
                                    .send(AppEvent::Lyrics {
                                        epoch,
                                        result: Err(ProviderError::Cancelled),
                                        is_final: true,
                                    })
                                    .await;
                            }
                            _ = tokio::time::sleep(latency) => {
                                let _ = tx
                                    .send(AppEvent::Lyrics {
                                        epoch,
                                        result: Ok(Some(ProviderAnswer::new(Provider::Local, lrc))),
                                        is_final: true,
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

        while let Some(AppEvent::Lyrics {
            epoch: resolved_epoch,
            result,
            is_final,
        }) = app_rx.recv().await
        {
            handle_resolved(
                &mut state,
                &mut coordinator,
                resolved_epoch,
                result,
                is_final,
            );
        }

        // Even if a delayed response for epoch 1 arrives after epoch 5 resolved, it is ignored.
        let mut stale_track = track();
        stale_track.source_track_id = Some("Song 1".to_string());
        handle_resolved(
            &mut state,
            &mut coordinator,
            1,
            Ok(Some(ProviderAnswer::new(Provider::Local, stale_track))),
            true,
        );

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
        for _ in 0..50 {
            draw_frame(&mut terminal, &mut state, &clock).unwrap();
        }

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
            player_short_name("org.mpris.MediaPlayer2.firefox.instance1"),
            "firefox"
        );
        assert_eq!(player_short_name("limusic"), "limusic");
    }

    #[test]
    fn tier_upgrade_preserves_clock_and_user_offset() {
        let base = Instant::now();
        let mut clock = PlaybackClock::new();
        clock.on_status_changed(PlaybackStatus::Playing, base);
        let mut state = UiState::new();
        let meta = crate::media::TrackMeta {
            title: "Yellow".to_string(),
            artist: "Coldplay".to_string(),
            album: Some("Parachutes".to_string()),
            duration_ms: Some(268_000),
            track_id: None,
            url: None,
        };
        state.set_track(meta.clone());
        let mut coordinator = ResolveCoordinator::new();
        let (epoch, cancel) = match coordinator.prepare_track(&meta) {
            TrackPlan::Fetch { epoch, cancel, .. } => (epoch, cancel),
            other => panic!("expected fetch plan, got {other:?}"),
        };
        coordinator.record_user_offset(-250);

        let mut line = track();
        line.granularity = SyncGranularity::Line;
        line.lines[0].words.clear();
        line.source_track_id = Some("line".to_string());
        handle_resolved(
            &mut state,
            &mut coordinator,
            epoch,
            Ok(Some(ProviderAnswer::new(Provider::Lrclib, line))),
            false,
        );

        assert_eq!(state.lyrics_track().map(Tier::from_track), Some(Tier::Line));
        assert_eq!(state.user_offset_ms, -250);
        assert_eq!(
            clock.raw_position_at(base + Duration::from_millis(500)),
            500
        );
        assert!(coordinator.cache().is_empty());

        let mut syllable = track();
        syllable.granularity = SyncGranularity::Syllable;
        syllable.source_track_id = Some("syllable".to_string());
        handle_resolved(
            &mut state,
            &mut coordinator,
            epoch,
            Ok(Some(ProviderAnswer::new(Provider::Lrclib, syllable))),
            true,
        );

        assert_eq!(
            state.lyrics_track().map(Tier::from_track),
            Some(Tier::Syllable)
        );
        assert_eq!(state.user_offset_ms, -250);
        assert_eq!(
            clock.raw_position_at(base + Duration::from_millis(500)),
            500
        );
        assert_eq!(coordinator.cache().len(), 1);
        assert!(!cancel.is_cancelled());
    }

    #[test]
    fn handle_input_records_offset_in_coordinator_and_preserves_across_resolution() {
        use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};

        let mut state = UiState::new();
        let mut coordinator = ResolveCoordinator::new();
        let mut offsets = DebouncedOffsetWriter::new(OffsetStore::in_memory());
        let meta = crate::media::TrackMeta {
            title: "Yellow".to_string(),
            artist: "Coldplay".to_string(),
            album: Some("Parachutes".to_string()),
            duration_ms: Some(268_000),
            track_id: None,
            url: None,
        };
        state.set_track(meta.clone());
        let plan = coordinator.prepare_track(&meta);
        let epoch = match plan {
            TrackPlan::Fetch { epoch, .. } => epoch,
            other => panic!("expected fetch plan, got {other:?}"),
        };

        let key_event = Event::Key(KeyEvent {
            code: KeyCode::Char('['),
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        });
        assert!(
            !handle_input(
                &mut state,
                &mut coordinator,
                &mut offsets,
                None,
                Instant::now(),
                key_event
            )
            .quit
        );
        assert_eq!(state.user_offset_ms, -100);

        let canonical = coordinator.active_key().unwrap().clone();
        assert_eq!(offsets.offset_for(&canonical), -100);

        let mut lrc = track();
        lrc.user_offset_ms = 0;
        let answer = ProviderAnswer::new(Provider::Local, lrc);
        handle_resolved(&mut state, &mut coordinator, epoch, Ok(Some(answer)), true);

        assert_eq!(state.user_offset_ms, -100);
        let loaded = state.lyrics_track().expect("lyrics loaded");
        assert_eq!(loaded.user_offset_ms, -100);

        let reset_event = Event::Key(KeyEvent {
            code: KeyCode::Char('\\'),
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        });
        assert!(
            !handle_input(
                &mut state,
                &mut coordinator,
                &mut offsets,
                None,
                Instant::now(),
                reset_event
            )
            .quit
        );
        assert_eq!(state.user_offset_ms, 0);
        assert_eq!(offsets.offset_for(&canonical), 0);
    }

    #[test]
    fn refetch_key_clears_l1_and_preserves_user_offset() {
        use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};

        let mut state = UiState::new();
        let mut coordinator = ResolveCoordinator::new();
        let mut offsets = DebouncedOffsetWriter::new(OffsetStore::in_memory());
        let meta = crate::media::TrackMeta {
            title: "Yellow".to_string(),
            artist: "Coldplay".to_string(),
            album: Some("Parachutes".to_string()),
            duration_ms: Some(268_000),
            track_id: None,
            url: None,
        };
        state.set_track(meta.clone());
        let epoch = match coordinator.prepare_track(&meta) {
            TrackPlan::Fetch { epoch, .. } => epoch,
            other => panic!("expected fetch plan, got {other:?}"),
        };
        coordinator.record_user_offset(-200);
        handle_resolved(
            &mut state,
            &mut coordinator,
            epoch,
            Ok(Some(ProviderAnswer::new(Provider::Local, track()))),
            true,
        );
        assert_eq!(coordinator.cache().len(), 1);
        assert_eq!(state.user_offset_ms, -200);

        let event = Event::Key(KeyEvent {
            code: KeyCode::Char('r'),
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        });
        let outcome = handle_input(
            &mut state,
            &mut coordinator,
            &mut offsets,
            None,
            Instant::now(),
            event,
        );

        assert!(!outcome.quit);
        assert!(state.refetching);
        assert!(coordinator.cache().is_empty());
        let refetch_epoch = match outcome.refetch.expect("r should produce a refetch plan") {
            TrackPlan::Fetch {
                epoch: refetch_epoch,
                user_offset_ms,
                query,
                cancel,
                bypass_cache,
            } => {
                assert_eq!(refetch_epoch, epoch + 1);
                assert_eq!(user_offset_ms, -200);
                assert_eq!(query.canonical, coordinator.active_key().unwrap().clone());
                assert!(!cancel.is_cancelled());
                assert!(bypass_cache);
                refetch_epoch
            }
            other => panic!("expected refetch plan, got {other:?}"),
        };

        handle_resolved(
            &mut state,
            &mut coordinator,
            refetch_epoch,
            Ok(Some(ProviderAnswer::new(Provider::Local, track()))),
            true,
        );
        assert!(!state.refetching);
        assert_eq!(coordinator.cache().len(), 1);
    }

    #[test]
    fn refetch_key_without_active_track_shows_banner_and_does_not_refetch() {
        use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};

        let mut state = UiState::new();
        let mut coordinator = ResolveCoordinator::new();
        let mut offsets = DebouncedOffsetWriter::new(OffsetStore::in_memory());
        let now = Instant::now();

        let event = Event::Key(KeyEvent {
            code: KeyCode::Char('r'),
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        });

        let outcome = handle_input(&mut state, &mut coordinator, &mut offsets, None, now, event);

        assert!(!outcome.quit);
        assert!(!state.refetching);
        assert!(outcome.refetch.is_none());
        assert_eq!(state.banner_message(), Some("No track to refetch"));
    }

    #[test]
    fn rapid_refetch_cancels_previous_refetch_epoch() {
        use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};

        let mut state = UiState::new();
        let mut coordinator = ResolveCoordinator::new();
        let mut offsets = DebouncedOffsetWriter::new(OffsetStore::in_memory());
        let meta = crate::media::TrackMeta {
            title: "Yellow".to_string(),
            artist: "Coldplay".to_string(),
            album: Some("Parachutes".to_string()),
            duration_ms: Some(268_000),
            track_id: None,
            url: None,
        };
        state.set_track(meta.clone());
        let _ = coordinator.prepare_track(&meta);
        coordinator.record_user_offset(150);

        let event = Event::Key(KeyEvent {
            code: KeyCode::Char('r'),
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        });

        let outcome_1 = handle_input(
            &mut state,
            &mut coordinator,
            &mut offsets,
            None,
            Instant::now(),
            event.clone(),
        );
        let (epoch_1, token_1) = match outcome_1.refetch.unwrap() {
            TrackPlan::Fetch { epoch, cancel, .. } => (epoch, cancel),
            other => panic!("expected fetch plan, got {other:?}"),
        };
        assert!(!token_1.is_cancelled());
        assert!(state.refetching);

        let outcome_2 = handle_input(
            &mut state,
            &mut coordinator,
            &mut offsets,
            None,
            Instant::now(),
            event,
        );
        let (epoch_2, token_2) = match outcome_2.refetch.unwrap() {
            TrackPlan::Fetch { epoch, cancel, .. } => (epoch, cancel),
            other => panic!("expected fetch plan, got {other:?}"),
        };
        assert!(token_1.is_cancelled());
        assert!(!token_2.is_cancelled());
        assert_eq!(epoch_2, epoch_1 + 1);
        assert!(state.refetching);

        handle_resolved(
            &mut state,
            &mut coordinator,
            epoch_1,
            Ok(Some(ProviderAnswer::new(Provider::Local, track()))),
            true,
        );
        assert!(state.refetching);
        assert!(coordinator.cache().is_empty());

        handle_resolved(
            &mut state,
            &mut coordinator,
            epoch_2,
            Ok(Some(ProviderAnswer::new(Provider::Local, track()))),
            true,
        );
        assert!(!state.refetching);
        assert_eq!(coordinator.cache().len(), 1);
        assert_eq!(state.user_offset_ms, 150);
    }

    #[test]
    fn layout_keys_toggle_and_persist_preferences() {
        use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};

        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "lyrachi-layout-keys-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let config_file = dir.join("config.toml");

        let mut state = UiState::new();
        let mut coordinator = ResolveCoordinator::new();
        let mut offsets = DebouncedOffsetWriter::new(OffsetStore::in_memory());
        let press = |state: &mut UiState,
                     coordinator: &mut ResolveCoordinator,
                     offsets: &mut DebouncedOffsetWriter,
                     code: char| {
            let event = Event::Key(KeyEvent {
                code: KeyCode::Char(code),
                modifiers: KeyModifiers::NONE,
                kind: KeyEventKind::Press,
                state: KeyEventState::NONE,
            });
            assert!(
                !handle_input(
                    state,
                    coordinator,
                    offsets,
                    Some(&config_file),
                    Instant::now(),
                    event
                )
                .quit
            );
        };

        press(&mut state, &mut coordinator, &mut offsets, 'f');
        assert!(state.is_fullscreen());
        let saved = std::fs::read_to_string(&config_file).unwrap();
        assert!(saved.contains("fullscreen = true"));
        assert!(saved.contains("show_header = true"));
        assert!(saved.contains("show_status = true"));

        press(&mut state, &mut coordinator, &mut offsets, 't');
        assert_eq!((state.show_header, state.show_status), (true, false));
        press(&mut state, &mut coordinator, &mut offsets, 's');
        assert_eq!((state.show_header, state.show_status), (true, true));

        state.config_warning = Some("invalid config: unknown key".to_string());
        press(&mut state, &mut coordinator, &mut offsets, 's');
        assert!(!state.show_status);
        assert_eq!(state.banner_message(), Some("invalid config: unknown key"));

        std::fs::remove_dir_all(dir).unwrap();
    }
}

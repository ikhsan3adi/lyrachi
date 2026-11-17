use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::event::{Event, EventStream};
use futures_util::StreamExt;
use ratatui::DefaultTerminal;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, debug_span, warn};

use crate::{
    cache::{CacheHandle, TrackPreference},
    config::RuntimeConfig,
    core::clock::{ClockState, EffectiveOffsets, PlaybackClock, PlaybackStatus},
    core::model::CanonicalTrackKey,
    media::{MediaEvent, TrackMeta, mpris::arbiter, player_short_name},
    offsets::{DebouncedOffsetWriter, OffsetStore},
    prefs::{self, UiPrefs},
    providers::engine::EngineUpdate,
    providers::{
        CandidateSummary, ProviderAnswer, ProviderError, ProviderResolver, Tier, TrackQuery,
    },
    ui::{CandidateMenuItem, ContextMenuAction, ContextMenuView, UiModal, UiState, render},
};

pub mod event;
pub mod keys;
pub mod pipe;
pub mod resolve;
pub mod status;

use event::AppEvent;
use keys::{KeyAction, KeyScope, resolve_key, search_input_char};
use resolve::{ResolveCoordinator, TrackPlan, resolve_with_updates};

const DEFAULT_PINNED_STYLE: &str = "builtin:default";

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
    menu_cancel: Option<CancellationToken>,
    menu_request_id: u64,
    candidates: Vec<CandidateSummary>,
    candidate_track_key: Option<CanonicalTrackKey>,
}

impl AppRuntime {
    pub fn cancel_menu_requests(&mut self) {
        if let Some(token) = self.menu_cancel.take() {
            token.cancel();
        }
        self.menu_request_id = self.menu_request_id.wrapping_add(1);
        self.candidates.clear();
        self.candidate_track_key = None;
    }

    fn next_menu_token(&mut self) -> (u64, CancellationToken) {
        if let Some(token) = self.menu_cancel.take() {
            token.cancel();
        }
        self.menu_request_id = self.menu_request_id.wrapping_add(1);
        let token = CancellationToken::new();
        self.menu_cancel = Some(token.clone());
        (self.menu_request_id, token)
    }
}

#[derive(Debug)]
enum MenuCommand {
    StartSearch {
        track_key: CanonicalTrackKey,
        query: TrackQuery,
    },
    HydrateCandidate {
        key: CanonicalTrackKey,
        summary: Box<CandidateSummary>,
    },
    ClearPinAndRefetch(TrackPlan),
    SetPinnedStyle {
        key: CanonicalTrackKey,
        style: Option<String>,
    },
    CancelMenu,
}

#[derive(Debug, Default)]
struct InputOutcome {
    quit: bool,
    refetch: Option<TrackPlan>,
    menu: Option<MenuCommand>,
}

impl InputOutcome {
    fn quit() -> Self {
        Self {
            quit: true,
            refetch: None,
            menu: None,
        }
    }

    fn refetch(plan: TrackPlan) -> Self {
        Self {
            quit: false,
            refetch: Some(plan),
            menu: None,
        }
    }

    fn menu(command: MenuCommand) -> Self {
        Self {
            quit: false,
            refetch: None,
            menu: Some(command),
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
    state.configure_ribbon(config.ribbon);
    state.set_ambiguous_width(config.ambiguous_width);
    state.set_terminal_unicode(config.unicode.is_full());
    refresh_hidden_status_banner(&mut state, Instant::now());
    let mut clock = PlaybackClock::new();
    let mut runtime = AppRuntime::default();
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

            runtime.animation.countdown_active = is_countdown_active(&state, &clock, now);
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
                    let outcome = handle_input_with_runtime(
                        &mut state,
                        Some(&mut runtime),
                        &mut coordinator,
                        &mut offsets,
                        Some(&config_file),
                        Instant::now(),
                        event,
                    );
                    if outcome.quit {
                        break;
                    }
                    let resolution = Resolution {
                        resolver: resolver.as_ref(),
                        cache: cache.as_ref(),
                    };
                    if let Some(plan) = outcome.refetch {
                        apply_plan(plan, &mut state, &app_tx, resolution);
                    }
                    if let Some(command) = outcome.menu {
                        apply_menu_command(
                            command,
                            &mut state,
                            &mut runtime,
                            &app_tx,
                            resolution,
                        );
                    }
                }
                Some(event) = media_rx.recv() => {
                    if let MediaEvent::TrackChanged(meta) = &event {
                        let key = TrackQuery::from_meta(meta, 0).canonical;
                        let stored_offset = offsets.offset_for(&key);
                        coordinator.seed_user_offset(key, stored_offset);
                    }
                    handle_media_with_runtime(
                        event,
                        &mut state,
                        &mut runtime,
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
                    handle_app_event(
                        event,
                        &mut state,
                        &mut runtime,
                        &mut coordinator,
                        cache.as_ref(),
                    );
                }
                _ = tokio::time::sleep(wait) => {}
            }
        }

        Ok(())
    }
    .await;

    runtime.cancel_menu_requests();
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
        state.expire_ribbon(now);
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
    apply_plan_with_pin(plan, state, app_tx, resolution, false);
}

fn apply_plan_with_pin(
    plan: TrackPlan,
    state: &mut UiState,
    app_tx: &mpsc::Sender<AppEvent>,
    resolution: Resolution<'_>,
    clear_pinned: bool,
) {
    match plan {
        TrackPlan::Unusable { user_offset_ms, .. } => {
            state.user_offset_ms = user_offset_ms;
            state.refetching = false;
            state.set_not_found();
        }
        TrackPlan::Cached {
            epoch,
            user_offset_ms,
            answer,
        } => {
            state.user_offset_ms = user_offset_ms;
            state.refetching = false;
            if let Some(meta) = state.track.as_ref()
                && let Some(cache) = resolution.cache
            {
                let key = TrackQuery::from_meta(meta, user_offset_ms).canonical;
                spawn_load_track_preference(cache, app_tx, epoch, key);
            }
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
            if !clear_pinned && let Some(cache) = resolution.cache {
                spawn_load_track_preference(cache, app_tx, request_epoch, query.canonical.clone());
            }
            start_fetch(
                state,
                app_tx,
                resolution,
                request_epoch,
                query,
                request_cancel,
                bypass_cache,
                clear_pinned,
            );
        }
    }
}

fn spawn_load_track_preference(
    cache: &CacheHandle,
    app_tx: &mpsc::Sender<AppEvent>,
    epoch: u64,
    key: CanonicalTrackKey,
) {
    let cache = cache.clone();
    let app_tx = app_tx.clone();
    tokio::spawn(async move {
        let Ok(Some(pref)) = cache.get_track_preference(&key).await else {
            return;
        };
        let pinned_source_label = if pref.pinned_payload_json.is_some() {
            pref.preferred_provider.clone()
        } else {
            None
        };
        let _ = app_tx
            .send(AppEvent::TrackPreferenceLoaded {
                epoch,
                key,
                pinned_source_label,
                pinned_style: pref.preferred_style,
            })
            .await;
    });
}

#[allow(clippy::too_many_arguments)]
fn start_fetch(
    state: &mut UiState,
    app_tx: &mpsc::Sender<AppEvent>,
    resolution: Resolution<'_>,
    request_epoch: u64,
    query: TrackQuery,
    request_cancel: CancellationToken,
    bypass_cache: bool,
    clear_pinned: bool,
) {
    if clear_pinned && let Some(cache) = resolution.cache.cloned() {
        let key = query.canonical.clone();
        tokio::spawn(async move {
            if let Err(error) = cache.clear_pinned_lyrics(&key).await {
                warn!(%error, "failed to clear pinned lyrics before refetch");
            }
        });
    }

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

#[allow(clippy::too_many_arguments)]
fn handle_media_with_runtime(
    event: MediaEvent,
    state: &mut UiState,
    runtime: &mut AppRuntime,
    clock: &mut PlaybackClock,
    coordinator: &mut ResolveCoordinator,
    app_tx: &mpsc::Sender<AppEvent>,
    resolution: Resolution<'_>,
    now: Instant,
) {
    if matches!(
        event,
        MediaEvent::NoPlayer | MediaEvent::TrackChanged(_) | MediaEvent::NoTrack
    ) {
        runtime.cancel_menu_requests();
    }
    handle_media(event, state, clock, coordinator, app_tx, resolution, now);
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

fn handle_app_event(
    event: AppEvent,
    state: &mut UiState,
    runtime: &mut AppRuntime,
    coordinator: &mut ResolveCoordinator,
    cache: Option<&CacheHandle>,
) {
    match event {
        AppEvent::Lyrics {
            epoch: resolved_epoch,
            result,
            is_final,
        } => {
            handle_resolved(state, coordinator, resolved_epoch, result, is_final);
        }
        AppEvent::TrackPreferenceLoaded {
            epoch,
            key,
            pinned_source_label,
            pinned_style,
        } => {
            handle_track_preference_loaded(
                state,
                coordinator,
                epoch,
                &key,
                pinned_source_label,
                pinned_style,
            );
        }
        AppEvent::SearchCandidates {
            request_id,
            key,
            result,
        } => {
            handle_search_candidates(state, runtime, coordinator, request_id, key, result);
        }
        AppEvent::CandidateHydrated {
            request_id,
            key,
            summary,
            result,
        } => {
            handle_candidate_hydrated(
                state,
                runtime,
                coordinator,
                request_id,
                key,
                &summary,
                result,
                cache,
            );
        }
    }
}

pub fn handle_track_preference_loaded(
    state: &mut UiState,
    coordinator: &ResolveCoordinator,
    epoch: u64,
    key: &CanonicalTrackKey,
    pinned_source_label: Option<String>,
    pinned_style: Option<String>,
) {
    if epoch != coordinator.epoch() || coordinator.active_key() != Some(key) {
        return;
    }
    state.pinned_source_label = pinned_source_label;
    state.pinned_style = pinned_style;
    state.sync_menu_preferences();
}

pub fn handle_search_candidates(
    state: &mut UiState,
    runtime: &mut AppRuntime,
    coordinator: &ResolveCoordinator,
    request_id: u64,
    key: CanonicalTrackKey,
    result: Result<Vec<CandidateSummary>, ProviderError>,
) {
    if request_id != runtime.menu_request_id || coordinator.active_key() != Some(&key) {
        return;
    }
    runtime.menu_cancel = None;
    match result {
        Ok(candidates) => {
            let active_provider = state.source_provider().map(str::to_owned);
            let active_track_id = state
                .lyrics_track()
                .and_then(|track| track.source_track_id.as_deref());
            let items = candidates
                .iter()
                .map(|cand| CandidateMenuItem {
                    provider_label: cand.provider.as_str().to_string(),
                    title: cand.title.clone(),
                    artist: cand.artist.clone(),
                    album: cand.album.clone(),
                    duration_ms: cand.duration_ms,
                    tier_label: cand.tier.as_str(),
                    score_pct: confidence_percent(cand.confidence),
                    is_active: active_provider.as_deref() == Some(cand.provider.as_str())
                        && match (active_track_id, cand.source_track_id.as_deref()) {
                            (Some(a), Some(b)) => a == b,
                            (None, None) => true,
                            _ => false,
                        },
                })
                .collect();
            runtime.candidates = candidates;
            runtime.candidate_track_key = Some(key);
            state.set_candidate_results(items, None);
        }
        Err(ProviderError::Cancelled) => {}
        Err(error) => {
            runtime.candidates.clear();
            state.set_candidate_results(Vec::new(), Some(format!("Search failed: {error}")));
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub fn handle_candidate_hydrated(
    state: &mut UiState,
    runtime: &mut AppRuntime,
    coordinator: &mut ResolveCoordinator,
    request_id: u64,
    key: CanonicalTrackKey,
    summary: &CandidateSummary,
    result: Result<Option<ProviderAnswer>, ProviderError>,
    cache: Option<&CacheHandle>,
) -> Option<(u64, ProviderAnswer)> {
    if request_id != runtime.menu_request_id || coordinator.active_key() != Some(&key) {
        return None;
    }
    runtime.menu_cancel = None;
    match result {
        Ok(Some(answer)) => {
            let Some((new_epoch, applied)) = coordinator.apply_manual_candidate(&key, answer)
            else {
                state.close_context_menu();
                runtime.cancel_menu_requests();
                return None;
            };
            state.user_offset_ms = applied.track.user_offset_ms;
            state.refetching = false;
            state.pinned_source_label = Some(applied.provider.as_str().to_string());
            state.set_lyrics(applied.track.clone());
            state.close_context_menu();
            runtime.cancel_menu_requests();

            if let Some(cache) = cache {
                let cache = cache.clone();
                let key = key.clone();
                let label = state
                    .track
                    .as_ref()
                    .map_or_else(|| summary.title.clone(), track_label);
                let answer_to_store = applied.clone();
                let source_track_id = summary.source_track_id.clone();
                let fallback_style = state.pinned_style.clone();
                tokio::spawn(async move {
                    let existing_style = cache
                        .get_track_preference(&key)
                        .await
                        .ok()
                        .flatten()
                        .and_then(|pref| pref.preferred_style)
                        .or(fallback_style);
                    match TrackPreference::with_pinned_track(
                        answer_to_store.provider,
                        source_track_id,
                        &answer_to_store.track,
                        existing_style,
                    ) {
                        Ok(pref) => {
                            if let Err(error) = cache.set_track_preference(&key, pref).await {
                                warn!(%error, "failed to persist pinned track preference");
                            }
                        }
                        Err(error) => {
                            warn!(%error, "failed to serialize pinned track preference");
                        }
                    }
                    if let Err(error) = cache.store(&key, &label, &answer_to_store).await {
                        warn!(%error, "failed to persist manual candidate to L2 cache");
                    }
                });
            }

            Some((new_epoch, applied))
        }
        Ok(None) => {
            state.set_candidate_hydrating(false, Some("Candidate has no lyrics.".to_string()));
            None
        }
        Err(ProviderError::Cancelled) => None,
        Err(error) => {
            state.set_candidate_hydrating(false, Some(format!("Failed to load: {error}")));
            None
        }
    }
}

fn confidence_percent(confidence: f32) -> u8 {
    if !confidence.is_finite() {
        return 0;
    }
    (confidence.clamp(0.0, 1.0) * 100.0).round() as u8
}

#[cfg(test)]
fn handle_input(
    state: &mut UiState,
    coordinator: &mut ResolveCoordinator,
    offsets: &mut DebouncedOffsetWriter,
    config_file: Option<&Path>,
    now: Instant,
    event: Event,
) -> InputOutcome {
    handle_input_with_runtime(state, None, coordinator, offsets, config_file, now, event)
}

fn handle_input_with_runtime(
    state: &mut UiState,
    runtime: Option<&mut AppRuntime>,
    coordinator: &mut ResolveCoordinator,
    offsets: &mut DebouncedOffsetWriter,
    config_file: Option<&Path>,
    now: Instant,
    event: Event,
) -> InputOutcome {
    let Event::Key(key) = event else {
        return InputOutcome::default();
    };
    let active_scope = match &state.modal {
        Some(UiModal::Help) => KeyScope::Help,
        Some(UiModal::ContextMenu(menu)) => match menu.view {
            ContextMenuView::SearchInput(_) => KeyScope::SearchInput,
            ContextMenuView::Actions { .. } | ContextMenuView::Candidates { .. } => {
                KeyScope::ContextMenu
            }
        },
        None => KeyScope::Global,
    };

    if active_scope == KeyScope::SearchInput {
        return handle_search_input_key(state, coordinator, now, key);
    }

    let Some(action) = resolve_key(active_scope, key) else {
        return InputOutcome::default();
    };

    match action {
        KeyAction::Quit => InputOutcome::quit(),
        KeyAction::Refetch => match coordinator.force_refetch_current_track() {
            Some(plan) => {
                state.refetching = true;
                InputOutcome::refetch(plan)
            }
            None => {
                state.show_banner("No track to refetch", now);
                InputOutcome::default()
            }
        },
        KeyAction::ToggleFullscreen => {
            state.toggle_fullscreen();
            persist_layout(state, config_file, now);
            InputOutcome::default()
        }
        KeyAction::ToggleHeader => {
            state.toggle_header();
            persist_layout(state, config_file, now);
            InputOutcome::default()
        }
        KeyAction::ToggleStatus => {
            state.toggle_status();
            persist_layout(state, config_file, now);
            InputOutcome::default()
        }
        KeyAction::ToggleRibbon => {
            state.toggle_ribbon(now);
            InputOutcome::default()
        }
        KeyAction::OffsetBack => {
            state.adjust_offset(-100);
            record_offset(state, coordinator, offsets);
            InputOutcome::default()
        }
        KeyAction::OffsetForward => {
            state.adjust_offset(100);
            record_offset(state, coordinator, offsets);
            InputOutcome::default()
        }
        KeyAction::OffsetReset => {
            state.reset_offset();
            record_offset(state, coordinator, offsets);
            InputOutcome::default()
        }
        KeyAction::ScrollUp => {
            state.scroll_up();
            InputOutcome::default()
        }
        KeyAction::ScrollDown => {
            state.scroll_down();
            InputOutcome::default()
        }
        KeyAction::ToggleHelp => {
            state.toggle_help();
            InputOutcome::menu(MenuCommand::CancelMenu)
        }
        KeyAction::CloseModal => {
            state.close_modal();
            InputOutcome::menu(MenuCommand::CancelMenu)
        }
        KeyAction::OpenContextMenu => {
            if !state.open_context_menu() {
                state.show_banner("No active track", now);
            }
            InputOutcome::default()
        }
        KeyAction::MenuUp => {
            if let Some(menu) = state.context_menu_mut() {
                menu.move_up();
            }
            InputOutcome::default()
        }
        KeyAction::MenuDown => {
            if let Some(menu) = state.context_menu_mut() {
                menu.move_down();
            }
            InputOutcome::default()
        }
        KeyAction::MenuSelect => handle_menu_select(state, runtime, coordinator, now),
        KeyAction::SearchSwitchField | KeyAction::SearchBackspace | KeyAction::SearchSubmit => {
            InputOutcome::default()
        }
    }
}

fn handle_search_input_key(
    state: &mut UiState,
    coordinator: &ResolveCoordinator,
    now: Instant,
    key: crossterm::event::KeyEvent,
) -> InputOutcome {
    match resolve_key(KeyScope::SearchInput, key) {
        Some(KeyAction::Quit) => InputOutcome::quit(),
        Some(KeyAction::CloseModal) => {
            state.close_modal();
            InputOutcome::menu(MenuCommand::CancelMenu)
        }
        Some(KeyAction::SearchSwitchField) => {
            if let Some(menu) = state.context_menu_mut()
                && let ContextMenuView::SearchInput(search) = &mut menu.view
            {
                search.switch_field();
            }
            InputOutcome::default()
        }
        Some(KeyAction::SearchBackspace) => {
            if let Some(menu) = state.context_menu_mut()
                && let ContextMenuView::SearchInput(search) = &mut menu.view
            {
                search.pop_char();
            }
            InputOutcome::default()
        }
        Some(KeyAction::SearchSubmit) => {
            let Some(menu) = state.context_menu() else {
                return InputOutcome::default();
            };
            let ContextMenuView::SearchInput(search) = &menu.view else {
                return InputOutcome::default();
            };
            let query = build_manual_search_query(
                &search.artist,
                &search.title,
                state.track.as_ref(),
                state.user_offset_ms,
            );
            if !query.is_usable() {
                state.show_banner("Enter a song title to search", now);
                return InputOutcome::default();
            }
            let Some(track_key) = coordinator.active_key().cloned() else {
                state.close_modal();
                state.show_banner("No active track", now);
                return InputOutcome::menu(MenuCommand::CancelMenu);
            };
            let query_label = format_query_label(&query);
            state.open_candidate_loading(query_label);
            InputOutcome::menu(MenuCommand::StartSearch { track_key, query })
        }
        _ => {
            if let Some(ch) = search_input_char(key)
                && let Some(menu) = state.context_menu_mut()
                && let ContextMenuView::SearchInput(search) = &mut menu.view
            {
                search.push_char(ch);
            }
            InputOutcome::default()
        }
    }
}

fn handle_menu_select(
    state: &mut UiState,
    runtime: Option<&mut AppRuntime>,
    coordinator: &mut ResolveCoordinator,
    now: Instant,
) -> InputOutcome {
    enum Selection {
        Action(ContextMenuAction),
        Candidate(usize),
    }

    let selection = match state.context_menu() {
        Some(menu) => match menu.view {
            ContextMenuView::Actions { .. } => menu.selected_action().map(Selection::Action),
            ContextMenuView::Candidates { .. } => {
                menu.selected_candidate_index().map(Selection::Candidate)
            }
            ContextMenuView::SearchInput(_) => None,
        },
        None => None,
    };

    let Some(selection) = selection else {
        return InputOutcome::default();
    };

    match selection {
        Selection::Action(ContextMenuAction::SelectCandidate) => {
            let Some(track_key) = coordinator.active_key().cloned() else {
                state.close_modal();
                state.show_banner("No active track", now);
                return InputOutcome::menu(MenuCommand::CancelMenu);
            };
            let Some(query) = coordinator.active_query().cloned().or_else(|| {
                state
                    .track
                    .as_ref()
                    .map(|meta| TrackQuery::from_meta(meta, state.user_offset_ms))
            }) else {
                state.close_modal();
                state.show_banner("No active track", now);
                return InputOutcome::menu(MenuCommand::CancelMenu);
            };
            let label = format_query_label(&query);
            state.open_candidate_loading(label);
            InputOutcome::menu(MenuCommand::StartSearch { track_key, query })
        }
        Selection::Action(ContextMenuAction::EditSearchQuery) => {
            let (artist, title) = match state.track.as_ref() {
                Some(track) => (track.artist.clone(), track.title.clone()),
                None => match coordinator.active_query() {
                    Some(query) => (query.artist.clone(), query.title.clone()),
                    None => (String::new(), String::new()),
                },
            };
            state.open_search_input(artist, title);
            InputOutcome::default()
        }
        Selection::Action(ContextMenuAction::ResetPreferredSource) => {
            state.pinned_source_label = None;
            state.close_modal();
            match coordinator.force_refetch_current_track() {
                Some(plan) => {
                    state.refetching = true;
                    state.show_banner("Preferred source reset", now);
                    InputOutcome::menu(MenuCommand::ClearPinAndRefetch(plan))
                }
                None => {
                    state.show_banner("No track to reset", now);
                    InputOutcome::menu(MenuCommand::CancelMenu)
                }
            }
        }
        Selection::Action(ContextMenuAction::TogglePinStyle) => {
            let next_style = if state.pinned_style.is_some() {
                None
            } else {
                Some(DEFAULT_PINNED_STYLE.to_string())
            };
            state.pinned_style = next_style.clone();
            state.sync_menu_preferences();
            if next_style.is_some() {
                state.show_banner("Pinned style: builtin:default", now);
            } else {
                state.show_banner("Unpinned track style", now);
            }
            state.close_modal();
            match coordinator.active_key().cloned() {
                Some(key) => InputOutcome::menu(MenuCommand::SetPinnedStyle {
                    key,
                    style: next_style,
                }),
                None => InputOutcome::menu(MenuCommand::CancelMenu),
            }
        }
        Selection::Candidate(index) => {
            let Some(rt) = runtime else {
                return InputOutcome::default();
            };
            let Some(summary) = rt.candidates.get(index).cloned() else {
                return InputOutcome::default();
            };
            let Some(key) = rt
                .candidate_track_key
                .clone()
                .or_else(|| coordinator.active_key().cloned())
            else {
                state.close_modal();
                state.show_banner("No active track", now);
                return InputOutcome::menu(MenuCommand::CancelMenu);
            };
            state.set_candidate_hydrating(true, None);
            InputOutcome::menu(MenuCommand::HydrateCandidate {
                key,
                summary: Box::new(summary),
            })
        }
    }
}

fn apply_menu_command(
    command: MenuCommand,
    state: &mut UiState,
    runtime: &mut AppRuntime,
    app_tx: &mpsc::Sender<AppEvent>,
    resolution: Resolution<'_>,
) {
    match command {
        MenuCommand::CancelMenu => {
            runtime.cancel_menu_requests();
        }
        MenuCommand::StartSearch { track_key, query } => {
            let Some(resolver) = resolution.resolver else {
                runtime.cancel_menu_requests();
                state.set_candidate_results(
                    Vec::new(),
                    Some("Lyrics providers unavailable.".to_string()),
                );
                return;
            };
            let (request_id, cancel) = runtime.next_menu_token();
            let resolver = Arc::clone(resolver);
            let app_tx = app_tx.clone();
            tokio::spawn(async move {
                let result = resolver.search_candidates(&query, &cancel).await;
                let _ = app_tx
                    .send(AppEvent::SearchCandidates {
                        request_id,
                        key: track_key,
                        result,
                    })
                    .await;
            });
        }
        MenuCommand::HydrateCandidate { key, summary } => {
            let Some(resolver) = resolution.resolver else {
                runtime.cancel_menu_requests();
                state.set_candidate_hydrating(
                    false,
                    Some("Lyrics providers unavailable.".to_string()),
                );
                return;
            };
            let (request_id, cancel) = runtime.next_menu_token();
            let resolver = Arc::clone(resolver);
            let app_tx = app_tx.clone();
            tokio::spawn(async move {
                let result = resolver.hydrate_candidate(&summary, &cancel).await;
                let _ = app_tx
                    .send(AppEvent::CandidateHydrated {
                        request_id,
                        key,
                        summary,
                        result,
                    })
                    .await;
            });
        }
        MenuCommand::ClearPinAndRefetch(plan) => {
            runtime.cancel_menu_requests();
            apply_plan_with_pin(plan, state, app_tx, resolution, true);
        }
        MenuCommand::SetPinnedStyle { key, style } => {
            runtime.cancel_menu_requests();
            let Some(cache) = resolution.cache.cloned() else {
                return;
            };
            tokio::spawn(async move {
                let mut pref = cache
                    .get_track_preference(&key)
                    .await
                    .ok()
                    .flatten()
                    .unwrap_or_default();
                pref.preferred_style = style;
                pref.updated_at = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |dur| i64::try_from(dur.as_secs()).unwrap_or(i64::MAX));
                if let Err(error) = cache.set_track_preference(&key, pref).await {
                    warn!(%error, "failed to update pinned style preference");
                }
            });
        }
    }
}

fn build_manual_search_query(
    artist: &str,
    title: &str,
    active_track: Option<&TrackMeta>,
    user_offset_ms: i64,
) -> TrackQuery {
    let meta = TrackMeta {
        title: title.trim().to_string(),
        artist: artist.trim().to_string(),
        album: None,
        duration_ms: active_track.and_then(|track| track.duration_ms),
        track_id: active_track.and_then(|track| track.track_id.clone()),
        url: None,
    };
    TrackQuery::from_meta(&meta, user_offset_ms)
}

fn format_query_label(query: &TrackQuery) -> String {
    if query.artist.is_empty() {
        query.title.clone()
    } else {
        format!("{} - {}", query.artist, query.title)
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
        ribbon: state.ribbon_mode,
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

fn is_countdown_active(state: &UiState, clock: &PlaybackClock, now: Instant) -> bool {
    if state.is_modal_open() || !matches!(clock.state(), ClockState::Playing { .. }) {
        return false;
    }
    if state.is_unsynced() || state.is_instrumental() {
        return false;
    }
    let Some(timeline) = state.timeline() else {
        return false;
    };
    let effective_ms = clock.effective_lyric_ms(now, effective_offsets(state));
    timeline
        .evaluate_for_status(effective_ms, state.playback)
        .waiting
        .is_some()
}

fn next_wait(
    state: &UiState,
    clock: &PlaybackClock,
    now: Instant,
    animation: AnimationStatus,
) -> Duration {
    if state.is_modal_open() {
        return Duration::from_secs(1);
    }
    let wait = base_next_wait(state, clock, now, animation);
    state
        .time_until_ribbon_expiry(now)
        .map_or(wait, |ribbon_wait| wait.min(ribbon_wait))
}

fn base_next_wait(
    state: &UiState,
    clock: &PlaybackClock,
    now: Instant,
    animation: AnimationStatus,
) -> Duration {
    if state.is_modal_open() {
        return Duration::from_secs(1);
    }
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
    let cursor = timeline.evaluate_for_status(effective_ms, state.playback);
    let next = timeline
        .next_boundary_ms(effective_ms)
        .unwrap_or_else(|| effective_ms.saturating_add(1_000));
    let media_delta_ms = next.saturating_sub(effective_ms).clamp(1, 1_000);
    let rate = clock.rate().max(0.01);
    let wall_ms = ((media_delta_ms as f64) / rate).round().clamp(1.0, 1_000.0) as u64;
    let boundary_wait = Duration::from_millis(wall_ms);

    if cursor.active_word_idx.is_some() || cursor.waiting.is_some() {
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

    #[test]
    fn help_modal_routes_keys_and_freezes_background_ticks() {
        use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};

        let mut state = UiState::new();
        let mut gap_track = track();
        gap_track.lines[0].start_ms = 6_000;
        gap_track.lines[0].end_ms = 8_000;
        gap_track.lines[0].words[0].start_ms = 6_000;
        gap_track.lines[0].words[0].end_ms = 8_000;
        state.set_lyrics(gap_track);
        state.playback = PlaybackStatus::Playing;

        let now = Instant::now();
        let mut clock = PlaybackClock::new();
        clock.on_status_changed(PlaybackStatus::Playing, now);

        assert!(is_countdown_active(&state, &clock, now));
        assert_eq!(
            next_wait(&state, &clock, now, AnimationStatus::default()),
            Duration::from_millis(16)
        );

        let mut coordinator = ResolveCoordinator::new();
        let mut offsets = DebouncedOffsetWriter::new(OffsetStore::in_memory());
        let key_event = |code: KeyCode, modifiers: KeyModifiers| {
            Event::Key(KeyEvent {
                code,
                modifiers,
                kind: KeyEventKind::Press,
                state: KeyEventState::NONE,
            })
        };

        // '?' opens Help modal
        let outcome = handle_input(
            &mut state,
            &mut coordinator,
            &mut offsets,
            None,
            now,
            key_event(KeyCode::Char('?'), KeyModifiers::SHIFT),
        );
        assert!(!outcome.quit);
        assert_eq!(state.modal, Some(UiModal::Help));
        assert!(!is_countdown_active(&state, &clock, now));
        assert_eq!(
            next_wait(
                &state,
                &clock,
                now,
                AnimationStatus {
                    effects_active: true,
                    countdown_active: true,
                    ribbon_animating: true,
                }
            ),
            Duration::from_secs(1)
        );

        // 'q' is blocked while Help modal is open
        let blocked_quit = handle_input(
            &mut state,
            &mut coordinator,
            &mut offsets,
            None,
            now,
            key_event(KeyCode::Char('q'), KeyModifiers::NONE),
        );
        assert!(!blocked_quit.quit);
        assert_eq!(state.modal, Some(UiModal::Help));

        // Esc closes Help modal
        handle_input(
            &mut state,
            &mut coordinator,
            &mut offsets,
            None,
            now,
            key_event(KeyCode::Esc, KeyModifiers::NONE),
        );
        assert_eq!(state.modal, None);
        assert!(is_countdown_active(&state, &clock, now));

        // Re-open with '?' and close again with '?'
        handle_input(
            &mut state,
            &mut coordinator,
            &mut offsets,
            None,
            now,
            key_event(KeyCode::Char('?'), KeyModifiers::NONE),
        );
        assert_eq!(state.modal, Some(UiModal::Help));
        handle_input(
            &mut state,
            &mut coordinator,
            &mut offsets,
            None,
            now,
            key_event(KeyCode::Char('?'), KeyModifiers::NONE),
        );
        assert_eq!(state.modal, None);
    }

    #[test]
    fn tab_toggles_ribbon_and_next_wait_tracks_expiry_when_paused_or_idle() {
        use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};

        let mut state = UiState::new();
        let mut clock = PlaybackClock::new();
        let mut coordinator = ResolveCoordinator::new();
        let mut offsets = DebouncedOffsetWriter::new(OffsetStore::in_memory());
        let t0 = Instant::now();
        let key_event = |code: KeyCode, modifiers: KeyModifiers| {
            Event::Key(KeyEvent {
                code,
                modifiers,
                kind: KeyEventKind::Press,
                state: KeyEventState::NONE,
            })
        };

        state.configure_ribbon(crate::config::RibbonMode::Auto);
        assert_eq!(state.ribbon_placement(), None);

        // Tab opens ribbon for 3 seconds while Idle (Stopped)
        handle_input(
            &mut state,
            &mut coordinator,
            &mut offsets,
            None,
            t0,
            key_event(KeyCode::Tab, KeyModifiers::NONE),
        );
        assert_eq!(
            state.ribbon_placement(),
            Some(crate::ui::RibbonPlacement::Top)
        );

        let wait_near_expiry = next_wait(
            &state,
            &clock,
            t0 + Duration::from_millis(2_750),
            AnimationStatus::default(),
        );
        assert_eq!(wait_near_expiry, Duration::from_millis(250));

        // Same while Paused
        clock.on_status_changed(PlaybackStatus::Paused, t0 + Duration::from_millis(2_800));
        let wait_paused = next_wait(
            &state,
            &clock,
            t0 + Duration::from_millis(2_800),
            AnimationStatus::default(),
        );
        assert_eq!(wait_paused, Duration::from_millis(200));

        state.expire_ribbon(t0 + Duration::from_secs(3));
        assert_eq!(state.ribbon_placement(), None);
    }

    #[test]
    fn search_input_isolates_global_hotkeys_and_edits_buffer() {
        use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};

        let mut state = UiState::new();
        let mut runtime = AppRuntime::default();
        let mut coordinator = ResolveCoordinator::new();
        let mut offsets = DebouncedOffsetWriter::new(OffsetStore::in_memory());
        let now = Instant::now();
        let meta = TrackMeta {
            title: "Yellow".to_string(),
            artist: "Coldplay".to_string(),
            album: None,
            duration_ms: Some(267_000),
            track_id: Some("track-1".to_string()),
            url: None,
        };
        let _ = coordinator.prepare_track(&meta);
        state.set_track(meta);
        state.open_search_input(String::new(), "Initial".to_string());

        let key_event = |code: KeyCode, modifiers: KeyModifiers| {
            Event::Key(KeyEvent {
                code,
                modifiers,
                kind: KeyEventKind::Press,
                state: KeyEventState::NONE,
            })
        };

        let text = "qrs mw?cS[]\\";
        for ch in text.chars() {
            let mods = if ch.is_ascii_uppercase() {
                KeyModifiers::SHIFT
            } else {
                KeyModifiers::NONE
            };
            let outcome = handle_input_with_runtime(
                &mut state,
                Some(&mut runtime),
                &mut coordinator,
                &mut offsets,
                None,
                now,
                key_event(KeyCode::Char(ch), mods),
            );
            assert!(!outcome.quit);
            assert!(outcome.refetch.is_none());
            assert!(outcome.menu.is_none());
        }

        assert_eq!(state.user_offset_ms, 0);
        assert!(state.show_header);
        assert!(state.show_status);
        assert!(!state.refetching);

        let search = match &state.context_menu().unwrap().view {
            ContextMenuView::SearchInput(search) => search,
            other => panic!("expected SearchInput, got {other:?}"),
        };
        assert_eq!(search.artist, text);

        // Up switches back to Artist, Down switches to Title, Tab switches
        handle_input_with_runtime(
            &mut state,
            Some(&mut runtime),
            &mut coordinator,
            &mut offsets,
            None,
            now,
            key_event(KeyCode::Down, KeyModifiers::NONE),
        );
        let search = match &state.context_menu().unwrap().view {
            ContextMenuView::SearchInput(search) => search,
            other => panic!("expected SearchInput, got {other:?}"),
        };
        assert_eq!(search.active_field, crate::ui::SearchField::Title);

        handle_input_with_runtime(
            &mut state,
            Some(&mut runtime),
            &mut coordinator,
            &mut offsets,
            None,
            now,
            key_event(KeyCode::Up, KeyModifiers::NONE),
        );
        let search = match &state.context_menu().unwrap().view {
            ContextMenuView::SearchInput(search) => search,
            other => panic!("expected SearchInput, got {other:?}"),
        };
        assert_eq!(search.active_field, crate::ui::SearchField::Artist);

        // Tab switches to Title field, Backspace edits Title, Enter submits StartSearch
        handle_input_with_runtime(
            &mut state,
            Some(&mut runtime),
            &mut coordinator,
            &mut offsets,
            None,
            now,
            key_event(KeyCode::Tab, KeyModifiers::NONE),
        );
        handle_input_with_runtime(
            &mut state,
            Some(&mut runtime),
            &mut coordinator,
            &mut offsets,
            None,
            now,
            key_event(KeyCode::Backspace, KeyModifiers::NONE),
        );
        let submit = handle_input_with_runtime(
            &mut state,
            Some(&mut runtime),
            &mut coordinator,
            &mut offsets,
            None,
            now,
            key_event(KeyCode::Enter, KeyModifiers::NONE),
        );
        match submit.menu {
            Some(MenuCommand::StartSearch { query, .. }) => {
                assert_eq!(query.title, "Initia");
            }
            other => panic!("expected StartSearch, got {other:?}"),
        }
    }

    #[test]
    fn manual_candidate_selection_bumps_epoch_and_rejects_stale_lyrics() {
        let mut state = UiState::new();
        let mut runtime = AppRuntime::default();
        let mut coordinator = ResolveCoordinator::new();
        let meta = TrackMeta {
            title: "Yellow".to_string(),
            artist: "Coldplay".to_string(),
            album: None,
            duration_ms: Some(267_000),
            track_id: Some("track-1".to_string()),
            url: None,
        };
        state.set_track(meta.clone());
        let (stale_epoch, key) = match coordinator.prepare_track(&meta) {
            TrackPlan::Fetch { epoch, query, .. } => (epoch, query.canonical),
            other => panic!("expected Fetch plan, got {other:?}"),
        };

        let mut manual_track = track();
        manual_track.source_provider = "lyricsplus".to_string();
        manual_track.source_track_id = Some("lp-99".to_string());
        let manual_answer =
            ProviderAnswer::new(crate::providers::Provider::LyricsPlus, manual_track);
        let summary = CandidateSummary {
            provider: crate::providers::Provider::LyricsPlus,
            source_track_id: Some("lp-99".to_string()),
            title: "Yellow".to_string(),
            artist: Some("Coldplay".to_string()),
            album: None,
            duration_ms: Some(267_000),
            tier: Tier::Word,
            confidence: 0.99,
            local_path: None,
            browse_hint: None,
            query_duration_ms: Some(267_000),
            query_user_offset_ms: 0,
            hydrated: Some(manual_answer.clone()),
        };

        state.open_candidate_loading("Coldplay - Yellow".to_string());
        let (req_id, _token) = runtime.next_menu_token();
        handle_search_candidates(
            &mut state,
            &mut runtime,
            &coordinator,
            req_id,
            key.clone(),
            Ok(vec![summary.clone()]),
        );
        assert_eq!(runtime.candidates.len(), 1);

        let (hydrate_req_id, _hydrate_token) = runtime.next_menu_token();
        let (new_epoch, _applied) = handle_candidate_hydrated(
            &mut state,
            &mut runtime,
            &mut coordinator,
            hydrate_req_id,
            key,
            &summary,
            Ok(Some(manual_answer)),
            None,
        )
        .expect("manual candidate should apply");
        assert!(new_epoch > stale_epoch);
        assert_eq!(state.modal, None);
        assert_eq!(state.source_provider(), Some("lyricsplus"));
        assert_eq!(state.pinned_source_label.as_deref(), Some("lyricsplus"));

        // Delayed response from previous resolve epoch must be discarded
        let mut stale_track = track();
        stale_track.source_provider = "lrclib".to_string();
        stale_track.source_track_id = Some("stale-1".to_string());
        let stale_answer = ProviderAnswer::new(crate::providers::Provider::Lrclib, stale_track);
        handle_resolved(
            &mut state,
            &mut coordinator,
            stale_epoch,
            Ok(Some(stale_answer)),
            true,
        );
        assert_eq!(state.source_provider(), Some("lyricsplus"));
    }

    #[test]
    fn menu_requests_cancel_on_esc_and_track_change() {
        use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};

        let mut state = UiState::new();
        let mut runtime = AppRuntime::default();
        let mut coordinator = ResolveCoordinator::new();
        let mut clock = PlaybackClock::new();
        let mut offsets = DebouncedOffsetWriter::new(OffsetStore::in_memory());
        let (app_tx, _app_rx) = mpsc::channel(8);
        let now = Instant::now();

        let meta_a = TrackMeta {
            title: "Track A".to_string(),
            artist: "Artist A".to_string(),
            album: None,
            duration_ms: Some(180_000),
            track_id: None,
            url: None,
        };
        let meta_b = TrackMeta {
            title: "Track B".to_string(),
            artist: "Artist B".to_string(),
            album: None,
            duration_ms: Some(200_000),
            track_id: None,
            url: None,
        };
        state.set_track(meta_a.clone());
        let _ = coordinator.prepare_track(&meta_a);
        state.open_candidate_loading("Artist A - Track A".to_string());
        let (_req_1, token_1) = runtime.next_menu_token();

        let esc = Event::Key(KeyEvent {
            code: KeyCode::Esc,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        });
        let esc_outcome = handle_input_with_runtime(
            &mut state,
            Some(&mut runtime),
            &mut coordinator,
            &mut offsets,
            None,
            now,
            esc,
        );
        assert!(matches!(esc_outcome.menu, Some(MenuCommand::CancelMenu)));
        runtime.cancel_menu_requests();
        assert!(token_1.is_cancelled());
        assert_eq!(state.modal, None);

        // Now start a search on Track A and switch to Track B
        state.open_candidate_loading("Artist A - Track A".to_string());
        let (req_2, token_2) = runtime.next_menu_token();
        let key_a = coordinator.active_key().cloned().unwrap();
        handle_media_with_runtime(
            MediaEvent::TrackChanged(meta_b),
            &mut state,
            &mut runtime,
            &mut clock,
            &mut coordinator,
            &app_tx,
            Resolution {
                resolver: None,
                cache: None,
            },
            now,
        );
        assert!(token_2.is_cancelled());
        assert_eq!(state.modal, None);

        // Late candidate list for Track A with req_2 must be ignored
        handle_search_candidates(
            &mut state,
            &mut runtime,
            &coordinator,
            req_2,
            key_a,
            Ok(Vec::new()),
        );
        assert_eq!(state.modal, None);
    }

    #[test]
    fn context_menu_reset_source_and_toggle_style_pin() {
        use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};

        let mut state = UiState::new();
        let mut runtime = AppRuntime::default();
        let mut coordinator = ResolveCoordinator::new();
        let mut offsets = DebouncedOffsetWriter::new(OffsetStore::in_memory());
        let now = Instant::now();
        let meta = TrackMeta {
            title: "Yellow".to_string(),
            artist: "Coldplay".to_string(),
            album: None,
            duration_ms: Some(267_000),
            track_id: None,
            url: None,
        };
        state.set_track(meta.clone());
        let _ = coordinator.prepare_track(&meta);
        state.pinned_source_label = Some("lrclib".to_string());

        let press = |code: KeyCode| {
            Event::Key(KeyEvent {
                code,
                modifiers: KeyModifiers::NONE,
                kind: KeyEventKind::Press,
                state: KeyEventState::NONE,
            })
        };

        // Open ContextMenu with 'c', move down 3x to TogglePinStyle, Enter
        handle_input_with_runtime(
            &mut state,
            Some(&mut runtime),
            &mut coordinator,
            &mut offsets,
            None,
            now,
            press(KeyCode::Char('c')),
        );
        for _ in 0..3 {
            handle_input_with_runtime(
                &mut state,
                Some(&mut runtime),
                &mut coordinator,
                &mut offsets,
                None,
                now,
                press(KeyCode::Down),
            );
        }
        let pin_out = handle_input_with_runtime(
            &mut state,
            Some(&mut runtime),
            &mut coordinator,
            &mut offsets,
            None,
            now,
            press(KeyCode::Enter),
        );
        assert_eq!(state.pinned_style.as_deref(), Some(DEFAULT_PINNED_STYLE));
        assert!(matches!(
            pin_out.menu,
            Some(MenuCommand::SetPinnedStyle { style: Some(_), .. })
        ));

        // Re-open with Enter, move down 2x to ResetPreferredSource, Enter
        handle_input_with_runtime(
            &mut state,
            Some(&mut runtime),
            &mut coordinator,
            &mut offsets,
            None,
            now,
            press(KeyCode::Enter),
        );
        for _ in 0..2 {
            handle_input_with_runtime(
                &mut state,
                Some(&mut runtime),
                &mut coordinator,
                &mut offsets,
                None,
                now,
                press(KeyCode::Down),
            );
        }
        let reset_out = handle_input_with_runtime(
            &mut state,
            Some(&mut runtime),
            &mut coordinator,
            &mut offsets,
            None,
            now,
            press(KeyCode::Enter),
        );
        assert_eq!(state.pinned_source_label, None);
        assert!(state.refetching);
        assert!(matches!(
            reset_out.menu,
            Some(MenuCommand::ClearPinAndRefetch(_))
        ));
    }

    #[test]
    fn lyricsplus_candidate_marked_active_when_source_track_id_is_none() {
        let mut state = UiState::new();
        let mut runtime = AppRuntime::default();
        let mut coordinator = ResolveCoordinator::new();
        let meta = TrackMeta {
            title: "Yellow".to_string(),
            artist: "Coldplay".to_string(),
            album: None,
            duration_ms: Some(267_000),
            track_id: None,
            url: None,
        };
        state.set_track(meta.clone());
        let key = match coordinator.prepare_track(&meta) {
            TrackPlan::Fetch { query, .. } => query.canonical,
            other => panic!("expected Fetch plan, got {other:?}"),
        };

        let mut lp_track = track();
        lp_track.source_provider = "lyricsplus".to_string();
        lp_track.source_track_id = None;
        state.set_lyrics(lp_track);

        let summary = CandidateSummary {
            provider: crate::providers::Provider::LyricsPlus,
            source_track_id: None,
            title: "Yellow".to_string(),
            artist: Some("Coldplay".to_string()),
            album: None,
            duration_ms: Some(267_000),
            tier: Tier::Word,
            confidence: 0.99,
            local_path: None,
            browse_hint: None,
            query_duration_ms: Some(267_000),
            query_user_offset_ms: 0,
            hydrated: None,
        };

        state.open_candidate_loading("Coldplay - Yellow".to_string());
        let (req_id, _token) = runtime.next_menu_token();
        handle_search_candidates(
            &mut state,
            &mut runtime,
            &coordinator,
            req_id,
            key,
            Ok(vec![summary]),
        );

        let menu = state.context_menu().expect("context menu open");
        match &menu.view {
            ContextMenuView::Candidates { items, .. } => {
                assert_eq!(items.len(), 1);
                assert!(
                    items[0].is_active,
                    "LyricsPlus candidate with None track ID should be marked is_active: true"
                );
            }
            other => panic!("expected Candidates view, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn reset_preferred_source_clears_pinned_preference_in_l2_cache() {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let db_path = std::env::temp_dir().join(format!(
            "lyrachi-m32-reset-{}-{stamp}.db",
            std::process::id()
        ));
        let cache = CacheHandle::open(&db_path).unwrap();

        let mut state = UiState::new();
        let mut runtime = AppRuntime::default();
        let mut coordinator = ResolveCoordinator::new();
        let mut offsets = DebouncedOffsetWriter::new(OffsetStore::in_memory());
        let (app_tx, mut app_rx) = mpsc::channel(16);
        let now = Instant::now();
        let meta = TrackMeta {
            title: "Yellow".to_string(),
            artist: "Coldplay".to_string(),
            album: None,
            duration_ms: Some(267_000),
            track_id: None,
            url: None,
        };
        state.set_track(meta.clone());
        let key = match coordinator.prepare_track(&meta) {
            TrackPlan::Fetch { query, .. } => query.canonical,
            other => panic!("expected Fetch plan, got {other:?}"),
        };

        let answer = ProviderAnswer::new(crate::providers::Provider::Lrclib, track());
        let pref = TrackPreference::with_pinned_track(
            crate::providers::Provider::Lrclib,
            Some("123".to_string()),
            &answer.track,
            Some("builtin:dark".to_string()),
        )
        .unwrap();
        cache.set_track_preference(&key, pref).await.unwrap();
        state.pinned_source_label = Some("lrclib".to_string());
        state.pinned_style = Some("builtin:dark".to_string());

        let press = |code: crossterm::event::KeyCode| {
            Event::Key(crossterm::event::KeyEvent {
                code,
                modifiers: crossterm::event::KeyModifiers::NONE,
                kind: crossterm::event::KeyEventKind::Press,
                state: crossterm::event::KeyEventState::NONE,
            })
        };

        handle_input_with_runtime(
            &mut state,
            Some(&mut runtime),
            &mut coordinator,
            &mut offsets,
            None,
            now,
            press(crossterm::event::KeyCode::Char('c')),
        );
        for _ in 0..2 {
            handle_input_with_runtime(
                &mut state,
                Some(&mut runtime),
                &mut coordinator,
                &mut offsets,
                None,
                now,
                press(crossterm::event::KeyCode::Down),
            );
        }
        let outcome = handle_input_with_runtime(
            &mut state,
            Some(&mut runtime),
            &mut coordinator,
            &mut offsets,
            None,
            now,
            press(crossterm::event::KeyCode::Enter),
        );
        assert_eq!(state.pinned_source_label, None);
        assert_eq!(state.pinned_style.as_deref(), Some("builtin:dark"));

        let command = outcome.menu.expect("expected menu command");
        apply_menu_command(
            command,
            &mut state,
            &mut runtime,
            &app_tx,
            Resolution {
                resolver: None,
                cache: Some(&cache),
            },
        );

        tokio::time::sleep(Duration::from_millis(100)).await;

        let stored = cache.get_track_preference(&key).await.unwrap();
        let stored_pref = stored.expect("preference row with preserved style must exist");
        assert_eq!(stored_pref.pinned_payload_json, None);
        assert_eq!(stored_pref.preferred_provider, None);
        assert_eq!(stored_pref.preferred_style.as_deref(), Some("builtin:dark"));

        while let Ok(event) = app_rx.try_recv() {
            handle_app_event(
                event,
                &mut state,
                &mut runtime,
                &mut coordinator,
                Some(&cache),
            );
        }
        assert_eq!(state.pinned_source_label, None);

        cache.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&db_path);
        let _ = std::fs::remove_file(format!("{}-wal", db_path.display()));
        let _ = std::fs::remove_file(format!("{}-shm", db_path.display()));
    }
}

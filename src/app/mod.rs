use std::io::Write as _;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use futures_util::StreamExt;
use ratatui::DefaultTerminal;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::warn;

use crate::{
    cli::Args,
    core::clock::{ClockState, EffectiveOffsets, PlaybackClock, PlaybackStatus},
    media::{MediaEvent, mpris::arbiter},
    providers::{ProviderError, ProviderResolver},
    ui::{UiState, render},
};

pub mod event;
pub mod resolve;

use event::AppEvent;

pub async fn run(terminal: &mut DefaultTerminal, args: Args) -> color_eyre::Result<()> {
    let resolver = match ProviderResolver::new(args.local_dir.clone()) {
        Ok(resolver) => Some(Arc::new(resolver)),
        Err(error) => {
            warn!(%error, "lyrics providers unavailable");
            None
        }
    };
    let mut state = UiState::new();
    state.global_offset_ms = args.offset_ms;
    state.lookahead_ms = args.lookahead_ms;
    let mut clock = PlaybackClock::new();
    let mut epoch = 0u64;
    let mut events = EventStream::new();
    let (media_tx, mut media_rx) = mpsc::channel(256);
    let (app_tx, mut app_rx) = mpsc::channel(64);
    let cancel = CancellationToken::new();
    let player_filter = args.player.clone();
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

    loop {
        terminal.draw(|frame| {
            let area = frame.area();
            state.resize(area.width, area.height);
            let now = Instant::now();
            let effective_ms = clock.effective_lyric_ms(now, effective_offsets(&state));
            render::render(frame, &state, effective_ms);
        })?;

        let wait = next_wait(&state, &clock, Instant::now());

        tokio::select! {
            event = events.next() => {
                let Some(event) = event else {
                    break;
                };
                let event = event?;
                if handle_input(&mut state, event) {
                    break;
                }
            }
            Some(event) = media_rx.recv() => {
                handle_media(
                    event,
                    &mut state,
                    &mut clock,
                    &mut epoch,
                    &app_tx,
                    resolver.as_ref(),
                    Instant::now(),
                );
            }
            Some(event) = app_rx.recv() => {
                if let AppEvent::Resolved {
                    epoch: resolved_epoch,
                    result,
                } = event
                    && resolved_epoch == epoch
                {
                    apply_result(&mut state, result);
                }
            }
            _ = tokio::time::sleep(wait) => {}
        }
    }

    cancel.cancel();
    if let Some(monitor) = monitor {
        monitor.abort();
        let _ = monitor.await;
    }

    Ok(())
}

pub async fn run_dump_events(args: Args) -> color_eyre::Result<()> {
    let (tx, mut rx) = mpsc::channel(256);
    let cancel = CancellationToken::new();
    let monitor_cancel = cancel.clone();
    let player_filter = args.player.clone();
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

fn handle_media(
    event: MediaEvent,
    state: &mut UiState,
    clock: &mut PlaybackClock,
    epoch: &mut u64,
    app_tx: &mpsc::Sender<AppEvent>,
    resolver: Option<&Arc<ProviderResolver>>,
    now: Instant,
) {
    match event {
        MediaEvent::PlayerChanged { bus, .. } => {
            state.set_player(Some(player_label(&bus)));
        }
        MediaEvent::NoPlayer => {
            *epoch = epoch.wrapping_add(1);
            clock.on_status_changed(PlaybackStatus::Stopped, now);
            state.set_no_player();
        }
        MediaEvent::TrackChanged(meta) => {
            *epoch = epoch.wrapping_add(1);
            clock.on_seeked(0, now);
            state.set_track(meta.clone());
            let Some(resolver) = resolver else {
                state.set_error("Lyrics providers unavailable.".to_string());
                return;
            };
            let resolver = Arc::clone(resolver);
            let app_tx = app_tx.clone();
            let request_epoch = *epoch;
            tokio::spawn(async move {
                let result = resolver.resolve(&meta, 0).await;
                let _ = app_tx
                    .send(AppEvent::Resolved {
                        epoch: request_epoch,
                        result,
                    })
                    .await;
            });
        }
        MediaEvent::NoTrack => {
            *epoch = epoch.wrapping_add(1);
            clock.on_status_changed(PlaybackStatus::Stopped, now);
            state.set_no_track();
        }
        MediaEvent::Status(status) => {
            clock.on_status_changed(status, now);
            state.playback = status;
        }
        MediaEvent::Rate(rate) => clock.on_rate_changed(rate, now),
        MediaEvent::Seeked { pos_ms } => clock.on_seeked(pos_ms, now),
        MediaEvent::PositionPoll { pos_ms, rtt } => clock.on_position_poll(pos_ms, rtt, now),
    }
}

fn apply_result(
    state: &mut UiState,
    result: Result<Option<crate::core::model::SyncedTrack>, ProviderError>,
) {
    match result {
        Ok(Some(track)) => state.set_lyrics(track),
        Ok(None) => state.set_not_found(),
        Err(error) => state.set_error(error.to_string()),
    }
}

fn handle_input(state: &mut UiState, event: Event) -> bool {
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
            false
        }
        KeyCode::Char(']') => {
            state.adjust_offset(100);
            false
        }
        KeyCode::Char('\\') => {
            state.reset_offset();
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
        let mut epoch = 0u64;
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
            }),
            &mut state,
            &mut clock,
            &mut epoch,
            &app_tx,
            None,
            later,
        );
        assert_eq!(epoch, 1);
        assert_eq!(state.user_offset_ms, 0);
        assert_eq!(clock.raw_position_at(later), 0);

        handle_media(
            MediaEvent::NoTrack,
            &mut state,
            &mut clock,
            &mut epoch,
            &app_tx,
            None,
            later,
        );
        assert_eq!(epoch, 2);
        assert_eq!(clock.state(), &ClockState::Stopped);

        clock.on_status_changed(PlaybackStatus::Playing, later);
        state.adjust_offset(100);
        handle_media(
            MediaEvent::NoPlayer,
            &mut state,
            &mut clock,
            &mut epoch,
            &app_tx,
            None,
            later,
        );
        assert_eq!(epoch, 3);
        assert_eq!(state.user_offset_ms, 0);
        assert_eq!(clock.state(), &ClockState::Stopped);
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

use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use color_eyre::Result;
use serde::Serialize;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::cache::CacheHandle;
use crate::cli::PipeFormat;
use crate::config::RuntimeConfig;
use crate::core::clock::{ClockState, PlaybackClock};
use crate::core::model::SyncGranularity;
use crate::core::text::truncate_to_width;
use crate::media::MediaEvent;
use crate::media::mpris::arbiter;
use crate::offsets::OffsetStore;
use crate::providers::{ProviderResolver, TrackQuery};
use crate::ui::{LyricsView, UiState};

use super::event::AppEvent;
use super::{Resolution, effective_offsets, handle_media, handle_resolved};

const IDLE_WAIT: Duration = Duration::from_secs(1);
const MAX_WAIT_MS: u64 = 1_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PipeOptions {
    pub format: PipeFormat,
    pub max_width: Option<u16>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PipeFrame {
    pub text: String,
    pub alt: String,
    pub class: String,
    pub progress: f32,
}

impl PipeFrame {
    fn same_output(&self, other: &Self, format: PipeFormat) -> bool {
        self.text == other.text
            && match format {
                PipeFormat::Plain => true,
                PipeFormat::Json => self.alt == other.alt && self.class == other.class,
            }
    }
}

pub async fn run(
    config: RuntimeConfig,
    offsets_file: PathBuf,
    options: PipeOptions,
    cache: Option<CacheHandle>,
) -> Result<()> {
    let player_filter = config.player_filter.clone();
    let (tx, rx) = mpsc::channel(256);
    let cancel = CancellationToken::new();
    let monitor_cancel = cancel.clone();
    let connection = zbus::Connection::session().await?;
    let monitor = tokio::spawn(async move {
        if let Err(error) = arbiter::run(player_filter, connection, tx, monitor_cancel).await {
            warn!(%error, "MPRIS monitor stopped");
        }
    });
    let stdout = io::stdout();
    let mut output = stdout.lock();
    let result = tokio::select! {
        _ = shutdown_signal() => Ok(()),
        result = run_events_with_cache(rx, config, offsets_file, &mut output, options, cache) => result,
    };

    cancel.cancel();
    monitor.abort();
    let _ = monitor.await;
    let _ = output.flush();
    result
}

pub async fn run_events<W: Write>(
    media_rx: mpsc::Receiver<MediaEvent>,
    config: RuntimeConfig,
    offsets_file: PathBuf,
    output: W,
    options: PipeOptions,
) -> Result<()> {
    run_events_with_cache(media_rx, config, offsets_file, output, options, None).await
}

async fn run_events_with_cache<W: Write>(
    mut media_rx: mpsc::Receiver<MediaEvent>,
    config: RuntimeConfig,
    offsets_file: PathBuf,
    mut output: W,
    options: PipeOptions,
    cache: Option<CacheHandle>,
) -> Result<()> {
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
    state.viewport_width = options.max_width.unwrap_or(u16::MAX);
    let mut clock = PlaybackClock::new();
    let mut coordinator =
        super::resolve::ResolveCoordinator::with_cache_capacity(config.l1_entries);
    let offsets = OffsetStore::load(&offsets_file);
    let (app_tx, mut app_rx) = mpsc::channel(64);
    let mut last_output: Option<PipeFrame> = None;

    loop {
        let now = Instant::now();
        let wait = pipe_wait(&state, &clock, now);
        tokio::select! {
            event = media_rx.recv() => {
                let Some(event) = event else {
                    break;
                };
                apply_media(
                    event,
                    &mut state,
                    &mut clock,
                    &mut coordinator,
                    &app_tx,
                    Resolution {
                        resolver: resolver.as_ref(),
                        cache: cache.as_ref(),
                    },
                    &offsets,
                );
                while let Ok(event) = media_rx.try_recv() {
                    apply_media(
                        event,
                        &mut state,
                        &mut clock,
                        &mut coordinator,
                        &app_tx,
                        Resolution {
                            resolver: resolver.as_ref(),
                            cache: cache.as_ref(),
                        },
                        &offsets,
                    );
                }
            }
            Some(event) = app_rx.recv() => {
                let AppEvent::Lyrics {
                    epoch,
                    result,
                    is_final,
                } = event;
                handle_resolved(&mut state, &mut coordinator, epoch, result, is_final);
            }
            _ = tokio::time::sleep(wait) => {}
        }

        if !emit_current(
            &state,
            &clock,
            Instant::now(),
            options,
            &mut last_output,
            &mut output,
        )? {
            break;
        }
    }

    coordinator.cancel_active();
    if let Some(cache) = cache
        && let Err(error) = cache.shutdown().await
    {
        warn!(%error, "failed to shut down lyric cache");
    }
    Ok(())
}

fn apply_media(
    event: MediaEvent,
    state: &mut UiState,
    clock: &mut PlaybackClock,
    coordinator: &mut super::resolve::ResolveCoordinator,
    app_tx: &mpsc::Sender<AppEvent>,
    resolution: Resolution<'_>,
    offsets: &OffsetStore,
) {
    if let MediaEvent::TrackChanged(meta) = &event {
        let query = TrackQuery::from_meta(meta, 0);
        coordinator.seed_user_offset(
            query.canonical.clone(),
            offsets.offset_for(&query.canonical),
        );
    }
    handle_media(
        event,
        state,
        clock,
        coordinator,
        app_tx,
        resolution,
        Instant::now(),
    );
}

fn emit_current<W: Write>(
    state: &UiState,
    clock: &PlaybackClock,
    now: Instant,
    options: PipeOptions,
    last_output: &mut Option<PipeFrame>,
    output: &mut W,
) -> io::Result<bool> {
    let Some(frame) = current_frame(state, clock, now, options.max_width) else {
        return Ok(true);
    };
    if last_output
        .as_ref()
        .is_some_and(|previous| frame.same_output(previous, options.format))
    {
        return Ok(true);
    }

    match write_frame(output, &frame, options.format) {
        Ok(()) => {
            *last_output = Some(frame);
            Ok(true)
        }
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => Ok(false),
        Err(error) => Err(error),
    }
}

fn write_frame<W: Write>(output: &mut W, frame: &PipeFrame, format: PipeFormat) -> io::Result<()> {
    let line = match format {
        PipeFormat::Plain => frame.text.clone(),
        PipeFormat::Json => serde_json::to_string(frame)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?,
    };
    output.write_all(line.as_bytes())?;
    output.write_all(b"\n")?;
    output.flush()
}

fn current_frame(
    state: &UiState,
    clock: &PlaybackClock,
    now: Instant,
    max_width: Option<u16>,
) -> Option<PipeFrame> {
    let alt = clock.state().status().as_str().to_string();

    match &state.lyrics {
        LyricsView::Loading => None,
        LyricsView::Idle => {
            let text = if state.player.is_some() {
                "No track playing"
            } else {
                "No player"
            };
            Some(status_frame(text, alt, max_width))
        }
        LyricsView::NotFound => Some(status_frame("No lyrics found", alt, max_width)),
        LyricsView::Error(_) => Some(status_frame("Lyrics unavailable", alt, max_width)),
        LyricsView::Loaded { track, timeline } => {
            if track.lines.is_empty() {
                return Some(status_frame("No lyrics found", alt, max_width));
            }
            if track.is_instrumental {
                return Some(PipeFrame {
                    text: fit_text("[Instrumental]", max_width),
                    alt,
                    class: "instrumental".to_string(),
                    progress: 0.0,
                });
            }
            if track.granularity == SyncGranularity::Unsynced {
                return track.lines.first().map(|line| PipeFrame {
                    text: fit_text(&line.text, max_width),
                    alt,
                    class: "unsynced".to_string(),
                    progress: 0.0,
                });
            }

            let effective_ms = clock.effective_lyric_ms(now, effective_offsets(state));
            let cursor = timeline.evaluate(effective_ms);
            let line = track.lines.get(cursor.active_line_idx?)?;
            let progress = if cursor.progress.is_finite() {
                cursor.progress.clamp(0.0, 1.0)
            } else {
                0.0
            };

            Some(PipeFrame {
                text: fit_text(&line.text, max_width),
                alt,
                class: track.status_label().to_string(),
                progress,
            })
        }
    }
}

fn status_frame(text: &str, alt: String, max_width: Option<u16>) -> PipeFrame {
    PipeFrame {
        text: fit_text(text, max_width),
        alt,
        class: "no-lyrics".to_string(),
        progress: 0.0,
    }
}

fn fit_text(text: &str, max_width: Option<u16>) -> String {
    let text = max_width.map_or(text, |max_width| truncate_to_width(text, max_width));
    text.trim_end().to_string()
}

fn pipe_wait(state: &UiState, clock: &PlaybackClock, now: Instant) -> Duration {
    if !matches!(clock.state(), ClockState::Playing { .. }) {
        return IDLE_WAIT;
    }
    let Some(timeline) = state.timeline() else {
        return IDLE_WAIT;
    };
    if state.is_unsynced() {
        return IDLE_WAIT;
    }

    let effective_ms = clock.effective_lyric_ms(now, effective_offsets(state));
    let cursor = timeline.evaluate(effective_ms);
    let next_start = timeline
        .lines
        .iter()
        .find(|line| line.start_ms > effective_ms)
        .map(|line| line.start_ms);
    let current_end = cursor
        .active_line_idx
        .and_then(|index| timeline.lines.get(index))
        .and_then(|line| line.end_ms.checked_add(1));
    let next = match (next_start, current_end) {
        (Some(left), Some(right)) => Some(left.min(right)),
        (Some(next), None) | (None, Some(next)) => Some(next),
        (None, None) => None,
    };
    let Some(next) = next else {
        return IDLE_WAIT;
    };

    let media_delta_ms = next.saturating_sub(effective_ms).clamp(1, MAX_WAIT_MS);
    let rate = clock.rate().max(0.01);
    let wall_ms = ((media_delta_ms as f64) / rate)
        .round()
        .clamp(1.0, MAX_WAIT_MS as f64) as u64;
    Duration::from_millis(wall_ms)
}

#[cfg(unix)]
async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};

    match signal(SignalKind::terminate()) {
        Ok(mut terminate) => {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = terminate.recv() => {}
            }
        }
        Err(error) => {
            debug!(%error, "failed to listen for SIGTERM");
            let _ = tokio::signal::ctrl_c().await;
        }
    }
}

#[cfg(not(unix))]
async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;
    use crate::core::model::{LineRole, LyricLine, SyncedTrack};

    struct BrokenPipe;

    impl Write for BrokenPipe {
        fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "closed"))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn track() -> SyncedTrack {
        SyncedTrack {
            source_provider: "local".to_string(),
            source_track_id: None,
            parser_version: 1,
            is_instrumental: false,
            granularity: SyncGranularity::Line,
            intrinsic_offset_ms: 0,
            user_offset_ms: 0,
            match_confidence: 1.0,
            lines: vec![LyricLine {
                start_ms: 0,
                end_ms: 5_000,
                role: LineRole::Primary,
                text: "世界abc".to_string(),
                words: Vec::new(),
                annotations: Vec::new(),
            }],
        }
    }

    #[test]
    fn truncates_by_display_width_without_splitting_graphemes() {
        let mut state = UiState::new();
        state.set_lyrics(track());
        let frame = current_frame(&state, &PlaybackClock::new(), Instant::now(), Some(5)).unwrap();

        assert_eq!(frame.text, "世界a");
    }

    #[tokio::test]
    async fn broken_pipe_exits_cleanly() {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("lyrachi-pipe-epipe-{}-{stamp}", std::process::id()));
        let (tx, rx) = mpsc::channel(4);
        tx.send(MediaEvent::NoTrack).await.unwrap();
        drop(tx);
        let options = PipeOptions {
            format: PipeFormat::Plain,
            max_width: None,
        };

        let result = run_events(
            rx,
            RuntimeConfig::default(),
            dir.join("offsets.json"),
            BrokenPipe,
            options,
        )
        .await;

        assert!(result.is_ok());
    }
}

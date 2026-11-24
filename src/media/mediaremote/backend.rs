//! MediaRemote backend: a single event-driven `media-control stream`
//! subprocess that reports whatever macOS treats as Now Playing.
//!
//! The binary is optional. When it is missing or the health check fails,
//! `start_monitor` returns `SessionUnavailable` so the composite falls back
//! to AppleScript. A stream that dies mid-run is restarted with backoff and
//! `NoPlayer` is emitted so the UI does not keep a stale track.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::core::clock::PlaybackStatus;
use crate::media::composite::{ExponentialBackoff, jitter_seed, player_matches};
use crate::media::diff::{PollState, diff};
use crate::media::mediaremote::parse::{self, Outcome, Poll};
use crate::media::mediaremote::stream::{
    GET_ARGS, NextLine, ProcessFactory, StreamFactory, StreamSource, capture_frames, find_binary,
    run_once,
};
use crate::media::{BackendKind, BoxFuture, MediaBackend, MediaError, MediaEvent, MediaSnapshot};

const HEALTH_TIMEOUT: Duration = Duration::from_secs(3);
const RECV_SLICE: Duration = Duration::from_millis(200);
const BACKOFF_INITIAL: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(30);
const DEBUG_FRAMES: usize = 5;

pub struct MediaRemoteBackend;

impl MediaBackend for MediaRemoteBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::MediaRemote
    }

    fn probe<'a>(
        &'a self,
        player_filter: Option<&'a str>,
    ) -> BoxFuture<'a, Result<MediaSnapshot, MediaError>> {
        Box::pin(async move {
            let filter = player_filter.map(str::to_string);
            tokio::task::spawn_blocking(move || probe_sync(filter.as_deref()))
                .await
                .map_err(|error| {
                    MediaError::SessionUnavailable(format!(
                        "mediaremote probe task failed: {error}"
                    ))
                })?
        })
    }

    fn start_monitor<'a>(
        &'a self,
        player_filter: Option<String>,
        tx: mpsc::Sender<MediaEvent>,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<JoinHandle<()>, MediaError>> {
        Box::pin(async move {
            let binary = tokio::task::spawn_blocking(health_check)
                .await
                .map_err(|error| {
                    MediaError::SessionUnavailable(format!(
                        "mediaremote health task failed: {error}"
                    ))
                })??;
            let factory = ProcessFactory { binary };
            Ok(tokio::spawn(async move {
                let result = tokio::task::spawn_blocking(move || {
                    monitor_loop(
                        player_filter,
                        tx,
                        cancel,
                        &factory,
                        BACKOFF_INITIAL,
                        BACKOFF_MAX,
                    )
                })
                .await;
                if let Err(error) = result {
                    tracing::warn!(%error, "mediaremote monitor stopped");
                }
            }))
        })
    }
}

fn unavailable(hint: &str) -> MediaError {
    MediaError::SessionUnavailable(format!(
        "{hint}; install it with `brew install media-control`"
    ))
}

fn health_check() -> Result<PathBuf, MediaError> {
    let binary = find_binary().ok_or_else(|| unavailable("media-control not found in PATH"))?;
    let output = run_once(&binary, GET_ARGS, HEALTH_TIMEOUT).map_err(|error| {
        MediaError::SessionUnavailable(format!("media-control get failed: {error}"))
    })?;
    validate_get(&output)?;
    Ok(binary)
}

fn validate_get(output: &str) -> Result<(), MediaError> {
    serde_json::from_str::<serde_json::Value>(output).map_err(|error| {
        MediaError::SessionUnavailable(format!("media-control get returned invalid JSON: {error}"))
    })?;
    Ok(())
}

fn probe_sync(filter: Option<&str>) -> Result<MediaSnapshot, MediaError> {
    let binary = find_binary().ok_or_else(|| unavailable("media-control not found in PATH"))?;
    let output = run_once(&binary, GET_ARGS, HEALTH_TIMEOUT).map_err(|error| {
        MediaError::SessionUnavailable(format!("media-control get failed: {error}"))
    })?;
    snapshot_from_get(&output, filter)
}

fn snapshot_from_get(output: &str, filter: Option<&str>) -> Result<MediaSnapshot, MediaError> {
    let payload = serde_json::from_str::<Option<parse::Payload>>(output).map_err(|error| {
        MediaError::SessionUnavailable(format!("media-control get returned invalid JSON: {error}"))
    })?;
    let empty = MediaSnapshot {
        backend: BackendKind::MediaRemote,
        ..MediaSnapshot::default()
    };
    let Some(poll) = payload.as_ref().and_then(parse::poll_from_payload) else {
        return Ok(empty);
    };
    if !player_matches(&poll.player.id, filter) {
        return Ok(empty);
    }
    Ok(MediaSnapshot {
        player: Some(poll.player),
        track: Some(poll.track),
        playback: poll.playback,
        position_ms: poll.position_ms,
        backend: BackendKind::MediaRemote,
    })
}

fn monitor_loop(
    filter: Option<String>,
    tx: mpsc::Sender<MediaEvent>,
    cancel: CancellationToken,
    factory: &dyn StreamFactory,
    backoff_initial: Duration,
    backoff_max: Duration,
) {
    let mut previous = PollState::default();
    let mut backoff = ExponentialBackoff::new(backoff_initial, backoff_max);

    while !cancel.is_cancelled() {
        match factory.spawn() {
            Ok(mut source) => {
                if !consume_stream(
                    source.as_mut(),
                    filter.as_deref(),
                    &tx,
                    &cancel,
                    &mut previous,
                    &mut backoff,
                ) {
                    source.shutdown();
                    return;
                }
                source.shutdown();
            }
            Err(error) => tracing::warn!(%error, "mediaremote: stream failed to start"),
        }

        if previous.player.is_some() {
            previous = PollState::default();
            if tx.blocking_send(MediaEvent::NoPlayer).is_err() {
                return;
            }
        }

        if cancel.is_cancelled() {
            break;
        }
        sleep_slices(backoff.next_delay_with_jitter(jitter_seed()), &cancel);
    }
}

/// Drain one stream source. Returns false when the event channel closed.
fn consume_stream(
    source: &mut dyn StreamSource,
    filter: Option<&str>,
    tx: &mpsc::Sender<MediaEvent>,
    cancel: &CancellationToken,
    previous: &mut PollState,
    backoff: &mut ExponentialBackoff,
) -> bool {
    loop {
        if cancel.is_cancelled() {
            return true;
        }
        match source.next_line(RECV_SLICE) {
            NextLine::Timeout => {}
            NextLine::Closed => return true,
            NextLine::Line(line) => match parse::parse_line(&line) {
                Outcome::Skip => {}
                Outcome::NoPlayer => {
                    backoff.reset();
                    if !finish_without_player(previous, tx) {
                        return false;
                    }
                }
                Outcome::Poll(poll) => {
                    backoff.reset();
                    if !player_matches(&poll.player.id, filter) {
                        if !finish_without_player(previous, tx) {
                            return false;
                        }
                        continue;
                    }
                    if !emit_poll(&poll, previous, tx) {
                        return false;
                    }
                }
            },
        }
    }
}

fn finish_without_player(previous: &mut PollState, tx: &mpsc::Sender<MediaEvent>) -> bool {
    if previous.player.is_none() {
        return true;
    }
    *previous = PollState::default();
    tx.blocking_send(MediaEvent::NoPlayer).is_ok()
}

fn emit_poll(poll: &Poll, previous: &mut PollState, tx: &mpsc::Sender<MediaEvent>) -> bool {
    let next = PollState {
        player: Some(poll.player.clone()),
        track: Some(poll.track.clone()),
        playback: poll.playback,
        position_ms: poll.position_ms,
        rate: poll.rate,
        sampled_at: Instant::now(),
    };
    for event in diff(previous, &next) {
        if tx.blocking_send(event).is_err() {
            return false;
        }
    }
    if next.playback == PlaybackStatus::Playing
        && tx
            .blocking_send(MediaEvent::PositionPoll {
                pos_ms: next.position_ms,
                rtt: Duration::ZERO,
                sample_age: poll.sample_age,
            })
            .is_err()
    {
        return false;
    }
    *previous = next;
    true
}

fn sleep_slices(wait: Duration, cancel: &CancellationToken) {
    let mut remaining = wait;
    while !remaining.is_zero() && !cancel.is_cancelled() {
        let slice = remaining.min(Duration::from_millis(50));
        std::thread::sleep(slice);
        remaining = remaining.saturating_sub(slice);
    }
}

/// Raw diagnostic for `lyrachi debug mediaremote`.
pub struct DebugDump {
    pub binary: Option<PathBuf>,
    pub get: Option<String>,
    pub get_error: Option<String>,
    pub frames: Vec<String>,
}

pub fn debug_dump() -> DebugDump {
    let binary = find_binary();
    let (get, get_error) = match binary.as_deref() {
        Some(path) => match run_once(path, GET_ARGS, HEALTH_TIMEOUT) {
            Ok(output) => (Some(output), None),
            Err(error) => (None, Some(error)),
        },
        None => (None, Some("media-control not found in PATH".to_string())),
    };
    let frames = binary
        .as_deref()
        .map(|path| capture_frames(path, DEBUG_FRAMES, Duration::from_secs(3)))
        .unwrap_or_default();
    DebugDump {
        binary,
        get,
        get_error,
        frames,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use super::*;

    fn frame(title: &str, position_ms: u64, playing: bool) -> String {
        format!(
            r#"{{"type":"data","diff":false,"payload":{{"title":"{title}","artist":"Artist","bundleIdentifier":"com.spotify.client","playing":{playing},"playbackRate":1,"elapsedTimeMicros":{},"durationMicros":225000000}}}}"#,
            position_ms * 1_000
        )
    }

    struct ScriptedSource {
        lines: VecDeque<String>,
        shutdowns: Arc<AtomicUsize>,
    }

    impl StreamSource for ScriptedSource {
        fn next_line(&mut self, _wait: Duration) -> NextLine {
            self.lines
                .pop_front()
                .map_or(NextLine::Closed, NextLine::Line)
        }

        fn shutdown(&mut self) {
            self.shutdowns.fetch_add(1, Ordering::Relaxed);
        }
    }

    struct FakeFactory {
        scripts: Mutex<VecDeque<Vec<String>>>,
        spawns: Arc<AtomicUsize>,
        shutdowns: Arc<AtomicUsize>,
    }

    impl FakeFactory {
        fn new(scripts: Vec<Vec<String>>) -> Self {
            Self {
                scripts: Mutex::new(scripts.into()),
                spawns: Arc::new(AtomicUsize::new(0)),
                shutdowns: Arc::new(AtomicUsize::new(0)),
            }
        }

        fn spawns(&self) -> usize {
            self.spawns.load(Ordering::Relaxed)
        }

        fn shutdowns(&self) -> usize {
            self.shutdowns.load(Ordering::Relaxed)
        }
    }

    impl StreamFactory for FakeFactory {
        fn spawn(&self) -> Result<Box<dyn StreamSource>, String> {
            self.spawns.fetch_add(1, Ordering::Relaxed);
            let script = self.scripts.lock().unwrap().pop_front().unwrap_or_default();
            Ok(Box::new(ScriptedSource {
                lines: script.into(),
                shutdowns: self.shutdowns.clone(),
            }))
        }
    }

    fn run_monitor(
        factory: Arc<FakeFactory>,
        filter: Option<&str>,
        cancel: CancellationToken,
    ) -> (mpsc::Receiver<MediaEvent>, std::thread::JoinHandle<()>) {
        let (tx, rx) = mpsc::channel(64);
        let filter = filter.map(str::to_string);
        let handle = std::thread::spawn(move || {
            monitor_loop(
                filter,
                tx,
                cancel,
                factory.as_ref(),
                Duration::from_millis(5),
                Duration::from_millis(20),
            );
        });
        (rx, handle)
    }

    async fn recv_with_timeout(rx: &mut mpsc::Receiver<MediaEvent>) -> Option<MediaEvent> {
        tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .ok()
            .flatten()
    }

    async fn drain(rx: &mut mpsc::Receiver<MediaEvent>, window: Duration) -> Vec<MediaEvent> {
        let deadline = tokio::time::Instant::now() + window;
        let mut events = Vec::new();
        while let Some(remaining) = deadline.checked_duration_since(tokio::time::Instant::now()) {
            match tokio::time::timeout(remaining, rx.recv()).await {
                Ok(Some(event)) => events.push(event),
                Ok(None) | Err(_) => break,
            }
        }
        events
    }

    #[tokio::test]
    async fn stream_polls_emit_events_and_no_player_on_exit() {
        let factory = Arc::new(FakeFactory::new(vec![vec![frame("Song", 45_000, true)]]));
        let cancel = CancellationToken::new();
        let (mut rx, handle) = run_monitor(factory.clone(), None, cancel.clone());

        let first = recv_with_timeout(&mut rx).await.expect("player changed");
        assert!(matches!(
            first,
            MediaEvent::PlayerChanged { ref id, .. } if id == "spotify"
        ));
        let second = recv_with_timeout(&mut rx).await.expect("track changed");
        assert!(matches!(&second, MediaEvent::TrackChanged(track) if track.title == "Song"));
        let third = recv_with_timeout(&mut rx).await.expect("status");
        assert!(matches!(third, MediaEvent::Status(PlaybackStatus::Playing)));
        let fourth = recv_with_timeout(&mut rx).await.expect("position");
        match fourth {
            MediaEvent::PositionPoll {
                pos_ms,
                rtt,
                sample_age,
            } => {
                assert_eq!(pos_ms, 45_000);
                assert_eq!(rtt, Duration::ZERO);
                assert_eq!(sample_age, None);
            }
            other => panic!("expected position poll, got {other:?}"),
        }

        let no_player = recv_with_timeout(&mut rx).await.expect("no player on exit");
        assert_eq!(no_player, MediaEvent::NoPlayer);
        let restart_deadline = Instant::now() + Duration::from_secs(2);
        while factory.spawns() < 2 && Instant::now() < restart_deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(factory.spawns() >= 2, "stream must restart after exit");
        assert!(factory.shutdowns() >= 1);

        cancel.cancel();
        let _ = handle.join();
    }

    #[tokio::test]
    async fn filter_mismatch_never_announces_the_player() {
        let factory = Arc::new(FakeFactory::new(vec![vec![frame("Song", 10_000, true)]]));
        let cancel = CancellationToken::new();
        let (mut rx, handle) = run_monitor(factory.clone(), Some("mpv"), cancel.clone());

        let events = drain(&mut rx, Duration::from_millis(150)).await;
        assert!(
            events
                .iter()
                .all(|event| !matches!(event, MediaEvent::PlayerChanged { .. })),
            "{events:?}"
        );

        cancel.cancel();
        let _ = handle.join();
    }

    #[tokio::test]
    async fn forward_seek_between_frames_is_reported() {
        let factory = Arc::new(FakeFactory::new(vec![vec![
            frame("Song", 10_000, true),
            frame("Song", 90_000, true),
        ]]));
        let cancel = CancellationToken::new();
        let (mut rx, handle) = run_monitor(factory.clone(), None, cancel.clone());

        let events = drain(&mut rx, Duration::from_millis(200)).await;
        assert!(events.iter().any(|event| matches!(
            event,
            MediaEvent::Seeked { pos_ms } if *pos_ms == 90_000
        )));

        cancel.cancel();
        let _ = handle.join();
    }

    #[tokio::test]
    async fn paused_frames_emit_status_without_position_polls() {
        let factory = Arc::new(FakeFactory::new(vec![vec![frame("Song", 10_000, false)]]));
        let cancel = CancellationToken::new();
        let (mut rx, handle) = run_monitor(factory.clone(), None, cancel.clone());

        let events = drain(&mut rx, Duration::from_millis(200)).await;
        assert!(
            events
                .iter()
                .any(|event| matches!(event, MediaEvent::Status(PlaybackStatus::Paused)))
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, MediaEvent::PositionPoll { .. }))
        );

        cancel.cancel();
        let _ = handle.join();
    }

    #[tokio::test]
    async fn cancelled_monitor_does_not_spawn() {
        let factory = Arc::new(FakeFactory::new(vec![]));
        let cancel = CancellationToken::new();
        cancel.cancel();
        let (_rx, handle) = run_monitor(factory.clone(), None, cancel);
        handle.join().unwrap();
        assert_eq!(factory.spawns(), 0);
    }

    #[test]
    fn get_output_maps_to_a_snapshot() {
        let output = r#"{"title":"Song","artist":"Artist","bundleIdentifier":"com.apple.Music","playing":true,"playbackRate":1,"elapsedTimeMicros":45000000,"durationMicros":225000000}"#;
        let snapshot = snapshot_from_get(output, None).expect("snapshot");
        assert_eq!(
            snapshot.player.as_ref().map(|player| player.id.as_str()),
            Some("music")
        );
        assert_eq!(
            snapshot.track.as_ref().map(|track| track.title.as_str()),
            Some("Song")
        );
        assert_eq!(snapshot.playback, PlaybackStatus::Playing);
        assert_eq!(snapshot.position_ms, 45_000);
        assert_eq!(snapshot.backend, BackendKind::MediaRemote);
    }

    #[test]
    fn get_null_or_filtered_maps_to_an_empty_snapshot() {
        for output in ["null", "{}"] {
            let snapshot = snapshot_from_get(output, None).expect("snapshot");
            assert!(snapshot.player.is_none(), "{output}");
            assert_eq!(snapshot.backend, BackendKind::MediaRemote);
        }

        let filtered = snapshot_from_get(
            r#"{"title":"Song","bundleIdentifier":"com.spotify.client","playing":true}"#,
            Some("mpv"),
        )
        .expect("snapshot");
        assert!(filtered.player.is_none());
    }

    #[test]
    fn invalid_get_json_is_a_session_error() {
        assert!(validate_get("not json").is_err());
        assert!(validate_get("null").is_ok());
        assert!(validate_get("{}").is_ok());
        assert!(snapshot_from_get("not json", None).is_err());
    }
}

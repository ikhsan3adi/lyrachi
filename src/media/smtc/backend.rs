//! Polling Windows SMTC backend: reads media sessions on a dedicated MTA
//! thread and forwards changes as media events.

use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant, SystemTime};

use tokio::sync::mpsc::Sender;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::debug;
use windows::Media::Control::{
    GlobalSystemMediaTransportControlsSession as SmtcSession,
    GlobalSystemMediaTransportControlsSessionManager as SmtcManager,
    GlobalSystemMediaTransportControlsSessionMediaProperties as SmtcMediaProperties,
    GlobalSystemMediaTransportControlsSessionPlaybackStatus,
};
use windows::core::HSTRING;

use super::com::spawn_mta;
use super::util::{self, RawPlaybackStatus, SessionCandidate};
use crate::core::clock::PlaybackStatus;
use crate::media::backend::{BoxFuture, MediaBackend};
use crate::media::diff::{PollState, diff};
use crate::media::{
    BackendKind, MediaError, MediaEvent, MediaSnapshot, PlayerKind, PlayerSnapshot, TrackMeta,
};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
const MEDIA_PROPERTIES_TIMEOUT: Duration = Duration::from_millis(500);
const PLAYING_POLL_INTERVAL: Duration = Duration::from_millis(500);
const IDLE_POLL_INTERVAL: Duration = Duration::from_millis(1_500);

/// Polling backend over `GlobalSystemMediaTransportControlsSessionManager`.
#[derive(Debug, Default, Clone)]
pub struct SmtcBackend;

impl MediaBackend for SmtcBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Smtc
    }

    fn probe<'a>(
        &'a self,
        player_filter: Option<&'a str>,
    ) -> BoxFuture<'a, Result<MediaSnapshot, MediaError>> {
        Box::pin(probe(player_filter))
    }

    fn start_monitor<'a>(
        &'a self,
        player_filter: Option<String>,
        tx: Sender<MediaEvent>,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<JoinHandle<()>, MediaError>> {
        Box::pin(async move { start_monitor(player_filter, tx, cancel).await })
    }
}

/// One-shot snapshot of the currently selected session, for `lyrachi status`.
pub async fn probe(player_filter: Option<&str>) -> Result<MediaSnapshot, MediaError> {
    let filter = player_filter.map(str::to_string);
    let (tx, rx) = tokio::sync::oneshot::channel();
    spawn_mta("lyrachi-smtc-probe", move |init| {
        let _ = tx.send(match init {
            Ok(()) => collect_snapshot(filter.as_deref()),
            Err(error) => Err(error),
        });
    })?;
    match rx.await {
        Ok(result) => result,
        Err(_) => Err(MediaError::SessionUnavailable(
            "SMTC probe thread failed".to_string(),
        )),
    }
}

/// Dump every live SMTC session, for `lyrachi debug smtc`.
pub async fn dump_sessions() -> Result<Vec<SessionDump>, MediaError> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    spawn_mta("lyrachi-smtc-dump", move |init| {
        let _ = tx.send(match init {
            Ok(()) => collect_dumps(),
            Err(error) => Err(error),
        });
    })?;
    match rx.await {
        Ok(result) => result,
        Err(_) => Err(MediaError::SessionUnavailable(
            "SMTC dump thread failed".to_string(),
        )),
    }
}

/// Start polling SMTC sessions and forwarding events until `cancel` fires.
pub async fn start_monitor(
    player_filter: Option<String>,
    tx: Sender<MediaEvent>,
    cancel: CancellationToken,
) -> Result<JoinHandle<()>, MediaError> {
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let (wake_tx, wake_rx) = mpsc::channel::<()>();
    let (done_tx, done_rx) = tokio::sync::oneshot::channel::<()>();
    let thread_cancel = cancel.clone();

    spawn_mta("lyrachi-smtc", move |init| {
        match init {
            Err(error) => {
                let _ = ready_tx.send(Err(error));
            }
            Ok(()) => match request_manager() {
                Ok(manager) => {
                    let _ = ready_tx.send(Ok(()));
                    monitor_loop(
                        manager,
                        player_filter.as_deref(),
                        &tx,
                        &wake_rx,
                        &thread_cancel,
                    );
                }
                Err(error) => {
                    let _ = ready_tx.send(Err(error));
                }
            },
        }
        let _ = done_tx.send(());
    })?;

    match ready_rx.await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => return Err(error),
        Err(_) => {
            return Err(MediaError::SessionUnavailable(
                "SMTC monitor thread failed".to_string(),
            ));
        }
    }

    Ok(tokio::spawn(async move {
        let mut done_rx = done_rx;
        tokio::select! {
            _ = cancel.cancelled() => {
                let _ = wake_tx.send(());
            }
            _ = &mut done_rx => return,
        }
        let _ = done_rx.await;
    }))
}

/// Human-readable summary of one SMTC session, printed by `lyrachi debug smtc`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionDump {
    pub aumid: String,
    pub status: &'static str,
    pub title: Option<String>,
    pub position_ms: Option<u64>,
    pub end_ms: Option<u64>,
    /// Age of the timeline sample, from `LastUpdatedTime`.
    pub last_updated_age: Option<Duration>,
    /// Whether the player reports a usable timeline at all.
    pub timeline_reported: bool,
}

fn request_manager() -> Result<SmtcManager, MediaError> {
    let op = SmtcManager::RequestAsync().map_err(|error| {
        MediaError::SessionUnavailable(format!(
            "SMTC is unavailable (requires Windows 10 1809 or newer): {error}"
        ))
    })?;
    let (tx, rx) = mpsc::channel();
    op.when(move |result| {
        let _ = tx.send(result);
    })
    .map_err(|error| MediaError::SessionUnavailable(format!("SMTC callback failed: {error}")))?;
    match rx.recv_timeout(REQUEST_TIMEOUT) {
        Ok(Ok(manager)) => Ok(manager),
        Ok(Err(error)) => Err(MediaError::SessionUnavailable(format!(
            "SMTC is unavailable: {error}"
        ))),
        Err(_) => Err(MediaError::SessionUnavailable(
            "SMTC manager request timed out".to_string(),
        )),
    }
}

fn media_properties_with_timeout(session: &SmtcSession) -> Option<SmtcMediaProperties> {
    let op = session.TryGetMediaPropertiesAsync().ok()?;
    let (tx, rx) = mpsc::channel();
    op.when(move |result| {
        let _ = tx.send(result);
    })
    .ok()?;
    match rx.recv_timeout(MEDIA_PROPERTIES_TIMEOUT) {
        Ok(Ok(properties)) => Some(properties),
        Ok(Err(error)) => {
            debug!(%error, "failed to read SMTC media properties");
            None
        }
        Err(_) => {
            debug!("reading SMTC media properties timed out");
            None
        }
    }
}

fn raw_status(
    status: GlobalSystemMediaTransportControlsSessionPlaybackStatus,
) -> RawPlaybackStatus {
    match status {
        GlobalSystemMediaTransportControlsSessionPlaybackStatus::Closed => {
            RawPlaybackStatus::Closed
        }
        GlobalSystemMediaTransportControlsSessionPlaybackStatus::Opened => {
            RawPlaybackStatus::Opened
        }
        GlobalSystemMediaTransportControlsSessionPlaybackStatus::Changing => {
            RawPlaybackStatus::Changing
        }
        GlobalSystemMediaTransportControlsSessionPlaybackStatus::Stopped => {
            RawPlaybackStatus::Stopped
        }
        GlobalSystemMediaTransportControlsSessionPlaybackStatus::Playing => {
            RawPlaybackStatus::Playing
        }
        GlobalSystemMediaTransportControlsSessionPlaybackStatus::Paused => {
            RawPlaybackStatus::Paused
        }
        _ => RawPlaybackStatus::Stopped,
    }
}

/// Read the playback status and rate with a single `GetPlaybackInfo` call.
/// The COM round-trip dominates the cost, so no caller asks twice per tick.
fn read_playback(session: &SmtcSession) -> (RawPlaybackStatus, f64) {
    session
        .GetPlaybackInfo()
        .ok()
        .map_or((RawPlaybackStatus::Stopped, 1.0), |info| {
            let status = info
                .PlaybackStatus()
                .ok()
                .map(raw_status)
                .unwrap_or(RawPlaybackStatus::Stopped);
            let rate = info
                .PlaybackRate()
                .ok()
                .and_then(|reference| reference.Value().ok())
                .map(util::safe_rate)
                .unwrap_or(1.0);
            (status, rate)
        })
}

struct TimelineSample {
    position_ms: u64,
    duration_ms: Option<u64>,
    sample_age: Option<Duration>,
}

fn read_timeline(session: &SmtcSession) -> Option<TimelineSample> {
    let timeline = session.GetTimelineProperties().ok()?;
    let position_ms = util::ticks_to_ms(timeline.Position().ok()?.Duration);
    let duration_ms = {
        let start = timeline.StartTime().ok()?.Duration;
        let end = timeline.EndTime().ok()?.Duration;
        util::timeline_duration_ms(start, end)
    };
    let sample_age = timeline
        .LastUpdatedTime()
        .ok()
        .map(|updated| updated.UniversalTime)
        .and_then(|universal_time| util::sample_age(universal_time, SystemTime::now()));
    Some(TimelineSample {
        position_ms,
        duration_ms,
        sample_age,
    })
}

enum TrackRead {
    Track(TrackMeta),
    Empty,
    Unavailable,
}

fn read_track(session: &SmtcSession, duration_ms: Option<u64>) -> TrackRead {
    let Some(properties) = media_properties_with_timeout(session) else {
        return TrackRead::Unavailable;
    };
    let Some(title) = trimmed(properties.Title()) else {
        return TrackRead::Empty;
    };
    let artist = trimmed(properties.Artist())
        .or_else(|| trimmed(properties.Subtitle()))
        .or_else(|| trimmed(properties.AlbumArtist()))
        .unwrap_or_default();
    TrackRead::Track(TrackMeta {
        title,
        artist,
        album: trimmed(properties.AlbumTitle()),
        duration_ms,
        track_id: None,
        url: None,
    })
}

fn trimmed(value: windows::core::Result<HSTRING>) -> Option<String> {
    value
        .ok()
        .map(|value| value.to_string().trim().to_string())
        .filter(|value| !value.is_empty())
}

struct SessionInfo {
    candidate: SessionCandidate,
    session: SmtcSession,
}

/// Enumerate sessions, falling back to `GetCurrentSession()` when enumeration
/// comes back empty.
fn collect_candidates(manager: &SmtcManager) -> Vec<SessionInfo> {
    let current_id = manager
        .GetCurrentSession()
        .ok()
        .and_then(|session| session.SourceAppUserModelId().ok())
        .map(|aumid| aumid.to_string());

    let mut sessions = Vec::new();
    if let Ok(vector) = manager.GetSessions()
        && let Ok(size) = vector.Size()
    {
        for index in 0..size {
            let Ok(session) = vector.GetAt(index) else {
                continue;
            };
            let Ok(aumid) = session.SourceAppUserModelId() else {
                continue;
            };
            let aumid = aumid.to_string();
            let (status, _) = read_playback(&session);
            let candidate = SessionCandidate {
                status,
                current: current_id
                    .as_deref()
                    .is_some_and(|current| current == aumid),
                id: aumid,
            };
            sessions.push(SessionInfo { candidate, session });
        }
    }

    if sessions.is_empty()
        && let Ok(session) = manager.GetCurrentSession()
        && let Ok(aumid) = session.SourceAppUserModelId()
    {
        let aumid = aumid.to_string();
        let (status, _) = read_playback(&session);
        let candidate = SessionCandidate {
            status,
            current: true,
            id: aumid,
        };
        sessions.push(SessionInfo { candidate, session });
    }

    sessions
}

fn selected<'a>(
    sessions: &'a [SessionInfo],
    player_filter: Option<&str>,
) -> Option<&'a SessionInfo> {
    let best = util::select_session(sessions.iter().map(|info| &info.candidate), player_filter)?;
    sessions.iter().find(|info| &info.candidate == best)
}

#[derive(Default)]
struct MonitorState {
    player: Option<PlayerSnapshot>,
    /// Previous poll, handed to `media::diff` on the next tick.
    previous: PollState,
    /// Last reported status, kept only as the hold value while a session
    /// reports `Changing`.
    status: Option<PlaybackStatus>,
    emitted_track: Option<TrackMeta>,
    pending_track: Option<TrackMeta>,
    pending_empty: bool,
    last_position_ms: u64,
    held_position: Option<u64>,
}

impl MonitorState {
    fn reset(&mut self) {
        let rate = self.previous.rate;
        *self = Self::default();
        // The clock keeps its rate across players, so the next diff must
        // still see the old rate and announce a correction when the new
        // player runs at a different speed.
        self.previous.rate = rate;
    }
}

/// Backward position jumps larger than this are held for one tick before
/// being trusted, so a bogus timeline sample cannot snap the clock to zero.
const BACKWARD_JUMP_MS: u64 = 5_000;

/// A held sample is confirmed as a real seek when the next tick lands this
/// close to it.
const CONFIRM_WINDOW_MS: u64 = 1_500;

/// Gate a playing-position sample against backward glitches. Returns the
/// position to emit, or `None` when the sample is held for confirmation.
fn accept_position(state: &mut MonitorState, position: u64, track_changed: bool) -> Option<u64> {
    if track_changed || position + BACKWARD_JUMP_MS >= state.last_position_ms {
        state.held_position = None;
        return Some(position);
    }
    match state.held_position {
        // The suspicious value repeats or advances normally: a real seek.
        Some(held) if position.abs_diff(held) <= CONFIRM_WINDOW_MS => {
            state.held_position = None;
            Some(position)
        }
        // Still suspicious: keep holding at the newest sample.
        Some(_) => {
            state.held_position = Some(position);
            None
        }
        None => {
            state.held_position = Some(position);
            None
        }
    }
}

/// Keep the previously emitted duration when a fresh read comes back without
/// one, so a timeline glitch cannot masquerade as a track change.
fn with_pinned_duration(track: &mut TrackMeta, previous: Option<&TrackMeta>) {
    if track.duration_ms.is_none() {
        track.duration_ms = previous.and_then(|previous| previous.duration_ms);
    }
}

fn monitor_loop(
    manager: SmtcManager,
    player_filter: Option<&str>,
    tx: &Sender<MediaEvent>,
    wake_rx: &mpsc::Receiver<()>,
    cancel: &CancellationToken,
) {
    let mut state = MonitorState::default();
    while let Some(interval) = tick(&manager, player_filter, &mut state, tx) {
        match wake_rx.recv_timeout(interval) {
            Ok(()) => {}
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        if cancel.is_cancelled() {
            break;
        }
    }
}

/// Clear per-player tracking and adopt a new player. `reset` keeps the old
/// rate so `diff` still announces a correction when the new player runs at a
/// different speed.
fn adopt_player(state: &mut MonitorState, id: &str) {
    state.reset();
    state.player = Some(PlayerSnapshot {
        id: id.to_string(),
        kind: PlayerKind::from_id(id),
        backend: BackendKind::Smtc,
    });
}

/// Read every session once, emit events for changes, and return the next poll
/// interval. `None` means the event channel closed and the loop should stop.
fn tick(
    manager: &SmtcManager,
    player_filter: Option<&str>,
    state: &mut MonitorState,
    tx: &Sender<MediaEvent>,
) -> Option<Duration> {
    let started = Instant::now();
    let sessions = collect_candidates(manager);
    let Some(info) = selected(&sessions, player_filter) else {
        if state.player.is_some() {
            state.reset();
            send(tx, MediaEvent::NoPlayer)?;
        }
        return Some(IDLE_POLL_INTERVAL);
    };

    if state
        .player
        .as_ref()
        .is_none_or(|player| player.id != info.candidate.id)
    {
        adopt_player(state, &info.candidate.id);
    }

    let timeline = read_timeline(&info.session);
    let (raw, rate) = read_playback(&info.session);
    let track = read_track(&info.session, timeline.as_ref().and_then(|t| t.duration_ms));

    apply_poll(
        state,
        timeline.as_ref(),
        (raw, rate),
        track,
        started.elapsed(),
        Instant::now(),
        tx,
    )
}

/// Fold one poll of session reads into the monitor state and emit the
/// transition events. Split from `tick` so the transition matrix stays
/// testable without a live WinRT session: `rtt` feeds `PositionPoll` and
/// `now` seeds `PollState::sampled_at`. The playback tuple is `read_playback`'s
/// (status, rate) pair.
fn apply_poll(
    state: &mut MonitorState,
    timeline: Option<&TimelineSample>,
    playback: (RawPlaybackStatus, f64),
    track_read: TrackRead,
    rtt: Duration,
    now: Instant,
    tx: &Sender<MediaEvent>,
) -> Option<Duration> {
    let (raw_status, rate) = playback;
    let status = util::map_status(raw_status).unwrap_or_else(|| {
        // `Changing` or an unknown value: hold the previous status, or assume
        // stopped on the very first read.
        state.status.unwrap_or(PlaybackStatus::Stopped)
    });

    let mut track_changed = false;
    match track_read {
        TrackRead::Track(mut track) => {
            state.pending_empty = false;
            with_pinned_duration(&mut track, state.emitted_track.as_ref());
            if state.emitted_track.as_ref() != Some(&track) {
                // The first track of a session goes out immediately; later
                // changes wait one tick so transient metadata flicker is
                // filtered out.
                if state.emitted_track.is_none() || state.pending_track.as_ref() == Some(&track) {
                    state.pending_track = None;
                    state.emitted_track = Some(track);
                    track_changed = true;
                } else {
                    state.pending_track = Some(track);
                }
            }
        }
        TrackRead::Empty => {
            // One empty read may be transient; only a repeated one counts.
            // `diff` re-announces only when the track actually leaves, so no
            // extra latch is needed here.
            if state.pending_empty && state.emitted_track.is_some() {
                state.emitted_track = None;
                state.pending_track = None;
            }
            state.pending_empty = true;
        }
        TrackRead::Unavailable => {
            // Timed out or failed read: keep the last known track.
        }
    }

    // Gate the position sample once per tick; both the transition anchor and
    // the periodic poll consume the same verdict. A held sample falls back to
    // the last accepted position so `diff` sees a stale poll, not a seek.
    let accepted =
        timeline.and_then(|timeline| accept_position(state, timeline.position_ms, track_changed));
    let position_ms = accepted.unwrap_or(state.last_position_ms);
    if let Some(position) = accepted {
        state.last_position_ms = position;
    }

    let next = PollState {
        player: state.player.clone(),
        track: state.emitted_track.clone(),
        playback: status,
        position_ms,
        rate,
        sampled_at: now,
    };

    // The single event path: diff decides what changed, including seeks, so
    // the old manual Seeked branches cannot double-emit anymore.
    for event in diff(&state.previous, &next) {
        send(tx, event)?;
    }

    if status == PlaybackStatus::Playing
        && let (Some(timeline), Some(position)) = (timeline, accepted)
    {
        send(
            tx,
            MediaEvent::PositionPoll {
                pos_ms: position,
                rtt,
                sample_age: timeline.sample_age,
            },
        )?;
    }

    state.previous = next;
    state.status = Some(status);

    Some(match status {
        PlaybackStatus::Playing => PLAYING_POLL_INTERVAL,
        _ => IDLE_POLL_INTERVAL,
    })
}

fn send(tx: &Sender<MediaEvent>, event: MediaEvent) -> Option<()> {
    tx.blocking_send(event).ok()
}

fn collect_snapshot(filter: Option<&str>) -> Result<MediaSnapshot, MediaError> {
    let manager = request_manager()?;
    let sessions = collect_candidates(&manager);
    let Some(info) = selected(&sessions, filter) else {
        return Ok(MediaSnapshot::default());
    };
    let timeline = read_timeline(&info.session);
    let track = match read_track(&info.session, timeline.as_ref().and_then(|t| t.duration_ms)) {
        TrackRead::Track(track) => Some(track),
        _ => None,
    };
    Ok(MediaSnapshot {
        player: Some(PlayerSnapshot {
            id: info.candidate.id.clone(),
            kind: PlayerKind::from_id(&info.candidate.id),
            backend: BackendKind::Smtc,
        }),
        track,
        playback: util::map_status(info.candidate.status).unwrap_or(PlaybackStatus::Stopped),
        position_ms: timeline.as_ref().map_or(0, |t| t.position_ms),
        backend: BackendKind::Smtc,
    })
}

fn collect_dumps() -> Result<Vec<SessionDump>, MediaError> {
    let manager = request_manager()?;
    let sessions = collect_candidates(&manager);
    Ok(sessions
        .iter()
        .map(|info| {
            let timeline = read_timeline(&info.session);
            let title = match read_track(&info.session, None) {
                TrackRead::Track(track) => Some(track.title),
                _ => None,
            };
            SessionDump {
                aumid: info.candidate.id.clone(),
                status: info.candidate.status.as_str(),
                title,
                position_ms: timeline.as_ref().map(|t| t.position_ms),
                end_ms: timeline.as_ref().and_then(|t| t.duration_ms),
                last_updated_age: timeline.as_ref().and_then(|t| t.sample_age),
                timeline_reported: !util::timeline_is_unreported(
                    info.candidate.status,
                    timeline.as_ref().map_or(0, |t| t.position_ms),
                    timeline.as_ref().and_then(|t| t.duration_ms),
                ),
            }
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(title: &str, duration_ms: Option<u64>) -> TrackMeta {
        TrackMeta {
            title: title.to_string(),
            artist: "Artist".to_string(),
            album: None,
            duration_ms,
            track_id: None,
            url: None,
        }
    }

    #[test]
    fn normal_advance_is_always_accepted() {
        let mut state = MonitorState {
            last_position_ms: 90_000,
            ..MonitorState::default()
        };
        assert_eq!(accept_position(&mut state, 90_500, false), Some(90_500));
        assert_eq!(state.held_position, None);
    }

    #[test]
    fn backward_jump_is_held_then_confirmed() {
        let mut state = MonitorState {
            last_position_ms: 90_000,
            ..MonitorState::default()
        };

        // First suspicious sample: held, nothing emitted.
        assert_eq!(accept_position(&mut state, 30_000, false), None);
        assert_eq!(state.held_position, Some(30_000));

        // Next tick continues from the held value: a real seek.
        assert_eq!(accept_position(&mut state, 30_500, false), Some(30_500));
        assert_eq!(state.held_position, None);
    }

    #[test]
    fn recovered_sample_discards_the_hold() {
        let mut state = MonitorState {
            last_position_ms: 90_000,
            ..MonitorState::default()
        };

        assert_eq!(accept_position(&mut state, 0, false), None);
        // The glitch is gone and the timeline is back at the frozen value.
        assert_eq!(accept_position(&mut state, 90_400, false), Some(90_400));
        assert_eq!(state.held_position, None);
    }

    #[test]
    fn a_track_change_bypasses_the_guard() {
        let mut state = MonitorState {
            last_position_ms: 90_000,
            ..MonitorState::default()
        };
        // A new track legitimately starts near zero.
        assert_eq!(accept_position(&mut state, 400, true), Some(400));
        assert_eq!(state.held_position, None);
    }

    #[test]
    fn a_still_suspicious_sample_updates_the_hold() {
        let mut state = MonitorState {
            last_position_ms: 90_000,
            ..MonitorState::default()
        };

        assert_eq!(accept_position(&mut state, 0, false), None);
        // A different far-behind value outside the confirm window: keep
        // holding at the newest sample.
        assert_eq!(accept_position(&mut state, 60_000, false), None);
        assert_eq!(state.held_position, Some(60_000));

        // Once the samples advance consistently, the seek is confirmed.
        assert_eq!(accept_position(&mut state, 60_500, false), Some(60_500));
        assert_eq!(state.held_position, None);
    }

    #[test]
    fn missing_duration_falls_back_to_the_emitted_one() {
        let previous = track("Yellow", Some(219_381));
        let mut glitched = track("Yellow", None);
        with_pinned_duration(&mut glitched, Some(&previous));
        assert_eq!(glitched.duration_ms, Some(219_381));

        // A fresh real duration still goes through.
        let mut updated = track("Yellow", Some(225_000));
        with_pinned_duration(&mut updated, Some(&previous));
        assert_eq!(updated.duration_ms, Some(225_000));

        // No previous duration: nothing to pin.
        let mut bare = track("Yellow", None);
        with_pinned_duration(&mut bare, None);
        assert_eq!(bare.duration_ms, None);
    }

    fn at(base: Instant, ms: u64) -> Instant {
        base + Duration::from_millis(ms)
    }

    fn timeline(position_ms: u64) -> TimelineSample {
        TimelineSample {
            position_ms,
            duration_ms: Some(200_000),
            sample_age: None,
        }
    }

    /// A monitor one tick after a settled playing poll, so transitions under
    /// test start from a known steady state.
    fn settled_monitor(position_ms: u64, at: Instant) -> MonitorState {
        let emitted = Some(track("Song", Some(200_000)));
        let player = Some(PlayerSnapshot {
            id: "player".to_string(),
            kind: PlayerKind::Native,
            backend: BackendKind::Smtc,
        });
        MonitorState {
            player: player.clone(),
            emitted_track: emitted.clone(),
            previous: PollState {
                player,
                track: emitted,
                playback: PlaybackStatus::Playing,
                position_ms,
                rate: 1.0,
                sampled_at: at,
            },
            last_position_ms: position_ms,
            ..MonitorState::default()
        }
    }

    fn drain(rx: &mut tokio::sync::mpsc::Receiver<MediaEvent>) -> Vec<MediaEvent> {
        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }
        events
    }

    #[test]
    fn a_large_forward_jump_emits_exactly_one_seek() {
        let base = Instant::now();
        let mut state = settled_monitor(10_000, base);
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);

        // Two seconds of playback, then the timeline reports a minute in.
        let sample = timeline(60_000);
        let interval = apply_poll(
            &mut state,
            Some(&sample),
            (RawPlaybackStatus::Playing, 1.0),
            TrackRead::Track(track("Song", Some(200_000))),
            Duration::ZERO,
            at(base, 2_000),
            &tx,
        );

        assert_eq!(interval, Some(PLAYING_POLL_INTERVAL));
        let events = drain(&mut rx);
        assert_eq!(events.len(), 2, "one seek plus its poll: {events:?}");
        assert!(matches!(events[0], MediaEvent::Seeked { pos_ms: 60_000 }));
        assert!(matches!(
            events[1],
            MediaEvent::PositionPoll {
                pos_ms: 60_000,
                sample_age: None,
                ..
            }
        ));
    }

    #[test]
    fn a_confirmed_backward_jump_emits_exactly_one_seek() {
        let base = Instant::now();
        let mut state = settled_monitor(90_000, base);
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);

        // Tick 1: the suspicious sample is held, and diff sees an unchanged
        // position (stale poll), not a seek.
        let held = timeline(30_000);
        let interval = apply_poll(
            &mut state,
            Some(&held),
            (RawPlaybackStatus::Playing, 1.0),
            TrackRead::Track(track("Song", Some(200_000))),
            Duration::ZERO,
            at(base, 500),
            &tx,
        );
        assert_eq!(interval, Some(PLAYING_POLL_INTERVAL));
        assert!(drain(&mut rx).is_empty(), "a held sample stays silent");

        // Tick 2: the sample repeats just past the held value: a real seek,
        // announced once even though the old code had two Seeked paths.
        let confirmed = timeline(30_500);
        apply_poll(
            &mut state,
            Some(&confirmed),
            (RawPlaybackStatus::Playing, 1.0),
            TrackRead::Track(track("Song", Some(200_000))),
            Duration::ZERO,
            at(base, 1_000),
            &tx,
        );
        let events = drain(&mut rx);
        assert_eq!(events.len(), 2, "one seek plus its poll: {events:?}");
        assert!(matches!(events[0], MediaEvent::Seeked { pos_ms: 30_500 }));
        assert!(matches!(
            events[1],
            MediaEvent::PositionPoll { pos_ms: 30_500, .. }
        ));
    }

    #[test]
    fn a_track_change_reanchors_without_a_seek() {
        let base = Instant::now();
        let mut state = settled_monitor(100_000, base);
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let next_track = track("Next", Some(210_000));

        // Tick 1: fresh metadata waits one tick (flicker filter); only the
        // routine position poll flows, no track or seek event.
        apply_poll(
            &mut state,
            Some(&timeline(100_500)),
            (RawPlaybackStatus::Playing, 1.0),
            TrackRead::Track(next_track.clone()),
            Duration::ZERO,
            at(base, 500),
            &tx,
        );
        let flicker = drain(&mut rx);
        assert_eq!(flicker.len(), 1, "poll only: {flicker:?}");
        assert!(matches!(
            flicker[0],
            MediaEvent::PositionPoll {
                pos_ms: 100_500,
                ..
            }
        ));

        // Tick 2: the metadata repeats, the track goes out, and the new
        // track's near-zero position must not masquerade as a seek.
        apply_poll(
            &mut state,
            Some(&timeline(400)),
            (RawPlaybackStatus::Playing, 1.0),
            TrackRead::Track(next_track),
            Duration::ZERO,
            at(base, 1_000),
            &tx,
        );
        let events = drain(&mut rx);
        assert_eq!(events.len(), 2, "track plus its poll: {events:?}");
        assert!(matches!(&events[0], MediaEvent::TrackChanged(t) if t.title == "Next"));
        assert!(matches!(
            events[1],
            MediaEvent::PositionPoll { pos_ms: 400, .. }
        ));
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, MediaEvent::Seeked { .. }))
        );
    }

    #[test]
    fn pausing_at_the_same_position_emits_only_status() {
        let base = Instant::now();
        let mut state = settled_monitor(50_000, base);
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);

        let interval = apply_poll(
            &mut state,
            Some(&timeline(50_000)),
            (RawPlaybackStatus::Paused, 1.0),
            TrackRead::Track(track("Song", Some(200_000))),
            Duration::ZERO,
            at(base, 300),
            &tx,
        );

        assert_eq!(interval, Some(IDLE_POLL_INTERVAL));
        let events = drain(&mut rx);
        assert_eq!(events.len(), 1, "status only: {events:?}");
        assert!(matches!(
            events[0],
            MediaEvent::Status(PlaybackStatus::Paused)
        ));
    }

    #[test]
    fn a_seek_while_paused_is_detected() {
        let base = Instant::now();
        let mut state = settled_monitor(50_000, base);
        state.previous.playback = PlaybackStatus::Paused;
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);

        apply_poll(
            &mut state,
            Some(&timeline(80_000)),
            (RawPlaybackStatus::Paused, 1.0),
            TrackRead::Track(track("Song", Some(200_000))),
            Duration::ZERO,
            at(base, 1_000),
            &tx,
        );

        let events = drain(&mut rx);
        assert_eq!(
            events.len(),
            1,
            "seek only, no poll while paused: {events:?}"
        );
        assert!(matches!(events[0], MediaEvent::Seeked { pos_ms: 80_000 }));
    }

    #[test]
    fn position_poll_only_flows_while_playing_and_carries_sample_age() {
        let base = Instant::now();
        let mut state = settled_monitor(10_000, base);
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);

        let mut sample = timeline(10_500);
        sample.sample_age = Some(Duration::from_millis(120));
        apply_poll(
            &mut state,
            Some(&sample),
            (RawPlaybackStatus::Playing, 1.0),
            TrackRead::Track(track("Song", Some(200_000))),
            Duration::ZERO,
            at(base, 500),
            &tx,
        );
        let events = drain(&mut rx);
        assert_eq!(events.len(), 1, "poll only: {events:?}");
        assert!(matches!(
            events[0],
            MediaEvent::PositionPoll {
                pos_ms: 10_500,
                sample_age: Some(_),
                ..
            }
        ));

        // The same sample while paused produces nothing at all.
        let mut paused = settled_monitor(10_000, base);
        paused.previous.playback = PlaybackStatus::Paused;
        apply_poll(
            &mut paused,
            Some(&sample),
            (RawPlaybackStatus::Paused, 1.0),
            TrackRead::Track(track("Song", Some(200_000))),
            Duration::ZERO,
            at(base, 500),
            &tx,
        );
        assert!(drain(&mut rx).is_empty(), "paused emits no poll");
    }

    #[test]
    fn a_player_swap_announces_the_new_player_exactly_once() {
        let base = Instant::now();
        let mut state = settled_monitor(30_000, base);
        // The old player ran slow; the clock keeps that rate across the swap,
        // so the diff must still announce the correction.
        state.previous.rate = 0.75;
        adopt_player(&mut state, "Other");
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);

        apply_poll(
            &mut state,
            Some(&timeline(45_000)),
            (RawPlaybackStatus::Playing, 1.0),
            TrackRead::Track(track("Song", Some(200_000))),
            Duration::ZERO,
            at(base, 500),
            &tx,
        );

        let events = drain(&mut rx);
        assert_eq!(
            events.len(),
            5,
            "player, track, status, rate, poll: {events:?}"
        );
        assert!(matches!(&events[0], MediaEvent::PlayerChanged { id, .. } if id == "Other"));
        assert!(matches!(events[1], MediaEvent::TrackChanged(_)));
        assert!(matches!(
            events[2],
            MediaEvent::Status(PlaybackStatus::Playing)
        ));
        assert!(matches!(events[3], MediaEvent::Rate(rate) if rate == 1.0));
        assert!(matches!(events[4], MediaEvent::PositionPoll { .. }));
    }
}

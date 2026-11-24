//! macOS backend: one-shot `osascript` polls for Music.app and Spotify.
//!
//! Everything here is plain process handling, so the module compiles and is
//! tested on every platform; `media::macos` is only wired into
//! `default_backend` on macOS. Player detection goes through `pgrep` so a
//! poll without any running player never sends an Apple event (and never
//! triggers the Automation consent dialog).

use std::io::Read;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::core::clock::PlaybackStatus;
use crate::media::diff::{PollState, diff};
use crate::media::macos::parse::{
    self, DENIED_REPROBE, FailureClass, IDLE_POLL, PlayerId, RowState, classify_code, parse_output,
    poll_interval, select_player,
};
use crate::media::macos::script::script_for;
use crate::media::{BackendKind, BoxFuture, MediaBackend, MediaError, MediaEvent, MediaSnapshot};

const PGREP: &str = "/usr/bin/pgrep";
const OSASCRIPT: &str = "/usr/bin/osascript";
const PGREP_TIMEOUT: Duration = Duration::from_secs(2);
/// Budget once a player is known to work; the script-level Apple event
/// timeout is far longer but only matters while a dialog is open.
const READY_TIMEOUT: Duration = Duration::from_secs(5);
/// Outlasts the script's own `with timeout` so the script, not the watchdog,
/// reports the Apple event timeout as an error row.
const CONSENT_TIMEOUT: Duration = Duration::from_secs(125);
/// `lyrachi status` must never hang on a pending consent dialog.
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);
/// Age of the position sample at read time. The script reads position last,
/// so this is small and roughly constant; verify on real hardware via
/// `lyrachi debug applescript` before trusting it.
const SAMPLE_AGE: Duration = Duration::from_millis(50);
/// Warn once every N stale (repeated) position polls while playing.
const STALE_WARN_EVERY: u32 = 5;

pub(crate) struct RawOutput {
    pub stdout: String,
    pub stderr: String,
}

#[derive(Debug)]
pub(crate) enum FailureKind {
    Cancelled,
    Timeout,
    Spawn(std::io::Error),
    Exit(i32),
}

#[derive(Debug)]
pub(crate) struct ScriptFailure {
    pub kind: FailureKind,
    pub stderr: String,
}

/// Every external process (pgrep, osascript) goes through this trait so the
/// whole poll loop is testable without spawning anything.
pub(crate) trait ProcessRunner: Send + Sync {
    fn run(
        &self,
        program: &str,
        args: &[&str],
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<RawOutput, ScriptFailure>;
}

/// Runs a child to completion with watchdog and cancellation. The child is
/// always killed and reaped, stderr is drained on a dedicated thread so the
/// pipe cannot fill and deadlock, and stdout is collected after exit.
fn run_process(
    program: &str,
    args: &[&str],
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<RawOutput, ScriptFailure> {
    let spawn_error = |error| ScriptFailure {
        kind: FailureKind::Spawn(error),
        stderr: String::new(),
    };
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(spawn_error)?;

    let mut stderr_pipe = child.stderr.take().expect("stderr is piped");
    let stderr_thread = thread::spawn(move || {
        let mut buffer = String::new();
        let _ = stderr_pipe.read_to_string(&mut buffer);
        buffer
    });

    let started = Instant::now();
    let outcome = loop {
        if cancel.is_cancelled() {
            break Err(FailureKind::Cancelled);
        }
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) => {
                if started.elapsed() >= timeout {
                    break Err(FailureKind::Timeout);
                }
                thread::sleep(Duration::from_millis(25));
            }
            Err(error) => break Err(FailureKind::Spawn(error)),
        }
    };

    let _ = child.kill();
    let _ = child.wait();
    let stdout = child
        .stdout
        .take()
        .map(|mut pipe| {
            let mut buffer = Vec::new();
            let _ = pipe.read_to_end(&mut buffer);
            String::from_utf8_lossy(&buffer).into_owned()
        })
        .unwrap_or_default();
    let stderr = stderr_thread.join().unwrap_or_default();

    match outcome {
        Ok(status) if status.success() => Ok(RawOutput { stdout, stderr }),
        Ok(status) => Err(ScriptFailure {
            kind: FailureKind::Exit(status.code().unwrap_or(-1)),
            stderr,
        }),
        Err(kind) => Err(ScriptFailure { kind, stderr }),
    }
}

pub(crate) struct OsascriptRunner;

impl ProcessRunner for OsascriptRunner {
    fn run(
        &self,
        program: &str,
        args: &[&str],
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<RawOutput, ScriptFailure> {
        run_process(program, args, timeout, cancel)
    }
}

fn pgrep_running(
    runner: &dyn ProcessRunner,
    process: &str,
    cancel: &CancellationToken,
) -> Result<bool, ScriptFailure> {
    match runner.run(PGREP, &["-x", process], PGREP_TIMEOUT, cancel) {
        Ok(output) => Ok(!output.stdout.is_empty()),
        // pgrep exits 1 when nothing matches; that is an answer, not a failure.
        Err(failure) => match failure.kind {
            FailureKind::Exit(1) => Ok(false),
            _ => Err(failure),
        },
    }
}

fn detect_players(
    runner: &dyn ProcessRunner,
    cancel: &CancellationToken,
) -> Result<Vec<PlayerId>, ScriptFailure> {
    let mut players = Vec::new();
    // Script order and tie-break order: Music first, then Spotify.
    if pgrep_running(runner, "Music", cancel)? {
        players.push(PlayerId::Music);
    }
    if pgrep_running(runner, "Spotify", cancel)? {
        players.push(PlayerId::Spotify);
    }
    Ok(players)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum PermissionState {
    #[default]
    Unknown,
    WaitingConsent,
    Ready,
    Denied,
}

struct PlayerRuntime {
    permission: PermissionState,
    /// When `Denied`, the next cheap reprobe is scheduled at this instant.
    reprobe_at: Option<Instant>,
    denied_warned: bool,
}

impl Default for PlayerRuntime {
    fn default() -> Self {
        Self {
            permission: PermissionState::Unknown,
            reprobe_at: None,
            denied_warned: false,
        }
    }
}

#[derive(Default)]
struct PlayerRuntimes {
    music: PlayerRuntime,
    spotify: PlayerRuntime,
}

impl PlayerRuntimes {
    fn peek(&self, player: PlayerId) -> &PlayerRuntime {
        match player {
            PlayerId::Music => &self.music,
            PlayerId::Spotify => &self.spotify,
        }
    }

    fn get(&mut self, player: PlayerId) -> &mut PlayerRuntime {
        match player {
            PlayerId::Music => &mut self.music,
            PlayerId::Spotify => &mut self.spotify,
        }
    }
}

#[derive(Default)]
struct LoopState {
    previous: PollState,
    runtimes: PlayerRuntimes,
    stale_polls: u32,
}

trait EventSink {
    /// `false` means the channel closed and the loop must stop.
    fn send(&mut self, event: MediaEvent) -> bool;
}

struct ChannelSink<'a>(&'a mpsc::Sender<MediaEvent>);

impl EventSink for ChannelSink<'_> {
    fn send(&mut self, event: MediaEvent) -> bool {
        self.0.blocking_send(event).is_ok()
    }
}

fn permission_hint(class: FailureClass) -> &'static str {
    match class {
        FailureClass::NotPermitted => {
            "Automation permission denied; grant it in System Settings > \
             Privacy & Security > Automation for this terminal app"
        }
        FailureClass::ConsentRequired | FailureClass::TimedOut => {
            "waiting for the macOS Automation consent dialog"
        }
        FailureClass::TrackUnreadable => "the player reported no readable track",
        FailureClass::AppNotRunning => "the player is not running",
        FailureClass::Other => "unexpected AppleScript failure",
    }
}

/// Read the running players once, emit events for changes, and return the
/// next poll interval. `None` means the event channel closed and the loop
/// should stop.
fn tick(
    filter: Option<&str>,
    state: &mut LoopState,
    runner: &dyn ProcessRunner,
    sink: &mut dyn EventSink,
    cancel: &CancellationToken,
) -> Option<Duration> {
    let started = Instant::now();
    let detected = match detect_players(runner, cancel) {
        Ok(players) => players,
        Err(failure) => {
            tracing::warn!(error = ?failure.kind, "applescript: player detection failed");
            return Some(IDLE_POLL);
        }
    };

    let requested: Vec<PlayerId> = detected
        .iter()
        .copied()
        .filter(|player| {
            player_matches_id(*player, filter)
                && match state.runtimes.get(*player).permission {
                    PermissionState::Denied => state
                        .runtimes
                        .get(*player)
                        .reprobe_at
                        .is_none_or(|at| Instant::now().duration_since(at) >= DENIED_REPROBE),
                    _ => true,
                }
        })
        .collect();

    if requested.is_empty() {
        return finish_without_player(&detected, state, sink);
    }

    let script = script_for(&requested);
    let waiting_for_consent = requested.iter().any(|player| {
        matches!(
            state.runtimes.get(*player).permission,
            PermissionState::Unknown | PermissionState::WaitingConsent
        )
    });
    let timeout = if waiting_for_consent {
        CONSENT_TIMEOUT
    } else {
        READY_TIMEOUT
    };

    let mut result = runner.run(OSASCRIPT, &["-e", script.as_str()], timeout, cancel);
    // One immediate retry for a timeout while every player is Ready; during
    // first consent a timeout means the dialog is open and must not be
    // retried into a second dialog.
    let all_ready = requested
        .iter()
        .all(|player| state.runtimes.get(*player).permission == PermissionState::Ready);
    if all_ready
        && matches!(
            result,
            Err(ScriptFailure {
                kind: FailureKind::Timeout,
                ..
            })
        )
    {
        tracing::warn!("applescript: poll timed out; retrying once");
        result = runner.run(OSASCRIPT, &["-e", script.as_str()], timeout, cancel);
    }
    let output = match result {
        Ok(output) => output,
        Err(failure) => return handle_script_failure(failure),
    };

    let rows = parse_output(&output.stdout);
    update_permissions(state, &rows);
    advance(filter, state, sink, rows, started)?;
    Some(backoff_wait(&detected, state))
}

/// How long to sleep after a poll: the selected player's cadence, or a
/// denied-player reprobe cadence when nothing is playable.
fn backoff_wait(detected: &[PlayerId], state: &LoopState) -> Duration {
    if state.previous.player.is_some() {
        return poll_interval(state.previous.playback.into_row_state());
    }
    idle_wait(detected, state)
}

fn idle_wait(detected: &[PlayerId], state: &LoopState) -> Duration {
    let blocked = detected
        .iter()
        .any(|player| state.runtimes.peek(*player).permission == PermissionState::Denied);
    let waiting = detected.iter().any(|player| {
        matches!(
            state.runtimes.peek(*player).permission,
            PermissionState::Unknown | PermissionState::WaitingConsent
        )
    });
    if blocked && !waiting {
        DENIED_REPROBE
    } else {
        IDLE_POLL
    }
}

fn player_matches_id(player: PlayerId, filter: Option<&str>) -> bool {
    crate::media::composite::player_matches(player.as_str(), filter)
}

fn update_permissions(state: &mut LoopState, rows: &[parse::ParsedRow]) {
    for row in rows {
        let runtime = state.runtimes.get(row.player);
        match row.state {
            RowState::Error(code) => match classify_code(code) {
                FailureClass::NotPermitted => {
                    runtime.permission = PermissionState::Denied;
                    runtime.reprobe_at = Some(Instant::now());
                    if !runtime.denied_warned {
                        runtime.denied_warned = true;
                        tracing::warn!(
                            player = row.player.as_str(),
                            "applescript: {}",
                            permission_hint(FailureClass::NotPermitted)
                        );
                    }
                }
                FailureClass::ConsentRequired => {
                    runtime.permission = PermissionState::WaitingConsent;
                }
                FailureClass::TimedOut => {
                    if runtime.permission == PermissionState::Unknown {
                        runtime.permission = PermissionState::WaitingConsent;
                    }
                }
                FailureClass::AppNotRunning => {}
                FailureClass::TrackUnreadable | FailureClass::Other => {}
            },
            _ => {
                runtime.permission = PermissionState::Ready;
                runtime.denied_warned = false;
            }
        }
    }
}

fn handle_script_failure(failure: ScriptFailure) -> Option<Duration> {
    match failure.kind {
        FailureKind::Cancelled => None,
        FailureKind::Timeout => {
            // A timeout with no Ready player means the consent dialog is
            // still open; keep waiting on the idle cadence.
            tracing::warn!("applescript: poll timed out");
            Some(IDLE_POLL)
        }
        FailureKind::Spawn(error) => {
            tracing::warn!(%error, "applescript: could not spawn helper process");
            Some(IDLE_POLL)
        }
        FailureKind::Exit(_) => {
            let class = parse::classify_failure(&failure.stderr);
            tracing::warn!(
                class = ?class,
                "applescript: {}",
                permission_hint(class)
            );
            Some(IDLE_POLL)
        }
    }
}

/// No usable player this poll: reset the tracked state, emit `NoPlayer` on
/// the transition, and back off.
fn finish_without_player(
    detected: &[PlayerId],
    state: &mut LoopState,
    sink: &mut dyn EventSink,
) -> Option<Duration> {
    let had_player = state.previous.player.is_some();
    state.previous = PollState::default();
    state.stale_polls = 0;
    if had_player && !sink.send(MediaEvent::NoPlayer) {
        return None;
    }
    Some(idle_wait(detected, state))
}

#[allow(clippy::too_many_arguments)]
fn advance(
    filter: Option<&str>,
    state: &mut LoopState,
    sink: &mut dyn EventSink,
    rows: Vec<parse::ParsedRow>,
    started: Instant,
) -> Option<()> {
    let selected = select_player(&rows, filter);
    let next = match selected {
        Some(row) if row.state.status().is_some() => {
            let (position_ms, _) = row.normalized();
            PollState {
                player: Some(row.player.snapshot(BackendKind::AppleScript)),
                track: row.track(),
                playback: row.state.status().expect("status checked"),
                position_ms,
                rate: 1.0,
                sampled_at: Instant::now(),
            }
        }
        _ => PollState::default(),
    };

    if next.playback == PlaybackStatus::Playing
        && state.previous.playback == PlaybackStatus::Playing
        && state.previous.player == next.player
        && next.position_ms == state.previous.position_ms
    {
        state.stale_polls += 1;
        if state.stale_polls.is_multiple_of(STALE_WARN_EVERY) {
            tracing::warn!(
                "applescript: player position has not advanced for {} polls",
                state.stale_polls
            );
        }
    } else {
        state.stale_polls = 0;
    }

    for event in diff(&state.previous, &next) {
        if !sink.send(event) {
            return None;
        }
    }
    if next.playback == PlaybackStatus::Playing
        && !sink.send(MediaEvent::PositionPoll {
            pos_ms: next.position_ms,
            rtt: started.elapsed(),
            sample_age: Some(SAMPLE_AGE),
        })
    {
        return None;
    }

    state.previous = next;
    Some(())
}

impl PlaybackStatus {
    fn into_row_state(self) -> RowState {
        match self {
            PlaybackStatus::Playing => RowState::Playing,
            PlaybackStatus::Paused => RowState::Paused,
            PlaybackStatus::Stopped => RowState::Stopped,
        }
    }
}

fn sleep_slices(wait: Duration, cancel: &CancellationToken) {
    let mut remaining = wait;
    while !remaining.is_zero() && !cancel.is_cancelled() {
        let slice = remaining.min(Duration::from_millis(50));
        thread::sleep(slice);
        remaining = remaining.saturating_sub(slice);
    }
}

fn monitor_loop(
    filter: Option<String>,
    tx: mpsc::Sender<MediaEvent>,
    cancel: CancellationToken,
    runner: &dyn ProcessRunner,
) {
    let mut state = LoopState::default();
    let mut sink = ChannelSink(&tx);
    loop {
        if cancel.is_cancelled() {
            break;
        }
        let Some(wait) = tick(filter.as_deref(), &mut state, runner, &mut sink, &cancel) else {
            break;
        };
        sleep_slices(wait, &cancel);
    }
}

fn snapshot_from_row(row: &parse::ParsedRow) -> MediaSnapshot {
    let (position_ms, _) = row.normalized();
    MediaSnapshot {
        player: Some(row.player.snapshot(BackendKind::AppleScript)),
        track: row.track(),
        playback: row.state.status().unwrap_or(PlaybackStatus::Stopped),
        position_ms,
        backend: BackendKind::AppleScript,
    }
}

fn probe_sync(
    filter: Option<&str>,
    runner: &dyn ProcessRunner,
) -> Result<MediaSnapshot, MediaError> {
    let cancel = CancellationToken::new();
    let players = detect_players(runner, &cancel).map_err(|failure| {
        MediaError::SessionUnavailable(format!("applescript probe failed: {:?}", failure.kind))
    })?;
    if players.is_empty() {
        return Ok(MediaSnapshot {
            backend: BackendKind::AppleScript,
            ..MediaSnapshot::default()
        });
    }

    let requested: Vec<PlayerId> = players
        .into_iter()
        .filter(|player| player_matches_id(*player, filter))
        .collect();
    if requested.is_empty() {
        return Ok(MediaSnapshot {
            backend: BackendKind::AppleScript,
            ..MediaSnapshot::default()
        });
    }

    let script = script_for(&requested);
    let output = runner
        .run(OSASCRIPT, &["-e", script.as_str()], PROBE_TIMEOUT, &cancel)
        .map_err(|failure| {
            if let FailureKind::Exit(_) = failure.kind {
                tracing::debug!(stderr = %failure.stderr, "applescript probe stderr");
            }
            MediaError::SessionUnavailable(
                "applescript: probe failed or the Automation consent dialog is pending".to_string(),
            )
        })?;

    let rows = parse_output(&output.stdout);
    match select_player(&rows, filter) {
        Some(row) if row.state.status().is_some() => Ok(snapshot_from_row(row)),
        Some(row) => {
            let code = match row.state {
                RowState::Error(code) => code,
                _ => 0,
            };
            let class = classify_code(code);
            Err(MediaError::SessionUnavailable(format!(
                "{}: {}",
                row.player.as_str(),
                permission_hint(class)
            )))
        }
        None => Ok(MediaSnapshot {
            backend: BackendKind::AppleScript,
            ..MediaSnapshot::default()
        }),
    }
}

/// Raw diagnostic for `lyrachi debug applescript`: what pgrep saw, the exact
/// script sent, the raw rows with separators intact, and the parsed result
/// with the unit heuristic applied.
pub struct DebugDump {
    pub detected: Vec<PlayerId>,
    pub script: Option<String>,
    pub raw_stdout: Option<String>,
    pub stderr: Option<String>,
    pub rows: Vec<parse::ParsedRow>,
    pub elapsed: Duration,
    pub failure: Option<String>,
}

pub fn debug_dump() -> DebugDump {
    let cancel = CancellationToken::new();
    let runner = OsascriptRunner;
    let mut failure = None;
    let detected = match detect_players(&runner, &cancel) {
        Ok(players) => players,
        Err(err) => {
            failure = Some(format!("player detection failed: {:?}", err.kind));
            Vec::new()
        }
    };
    if detected.is_empty() {
        return DebugDump {
            detected,
            script: None,
            raw_stdout: None,
            stderr: None,
            rows: Vec::new(),
            elapsed: Duration::ZERO,
            failure,
        };
    }

    let script = script_for(&detected);
    let started = Instant::now();
    match runner.run(
        OSASCRIPT,
        &["-e", script.as_str()],
        CONSENT_TIMEOUT,
        &cancel,
    ) {
        Ok(output) => {
            let rows = parse_output(&output.stdout);
            let stderr = (!output.stderr.is_empty()).then_some(output.stderr);
            DebugDump {
                detected,
                script: Some(script),
                raw_stdout: Some(output.stdout),
                stderr,
                rows,
                elapsed: started.elapsed(),
                failure,
            }
        }
        Err(err) => DebugDump {
            detected,
            script: Some(script),
            raw_stdout: None,
            stderr: (!err.stderr.is_empty()).then_some(err.stderr),
            rows: Vec::new(),
            elapsed: started.elapsed(),
            failure: Some(format!("{:?}", err.kind)),
        },
    }
}

pub struct MacosBackend;

impl MediaBackend for MacosBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::AppleScript
    }

    fn probe<'a>(
        &'a self,
        player_filter: Option<&'a str>,
    ) -> BoxFuture<'a, Result<MediaSnapshot, MediaError>> {
        Box::pin(async move {
            let filter = player_filter.map(str::to_string);
            tokio::task::spawn_blocking(move || probe_sync(filter.as_deref(), &OsascriptRunner))
                .await
                .map_err(|error| {
                    MediaError::SessionUnavailable(format!(
                        "applescript probe task failed: {error}"
                    ))
                })
                .and_then(std::convert::identity)
        })
    }

    fn start_monitor<'a>(
        &'a self,
        player_filter: Option<String>,
        tx: mpsc::Sender<MediaEvent>,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<JoinHandle<()>, MediaError>> {
        Box::pin(async move {
            Ok(tokio::spawn(async move {
                let result = tokio::task::spawn_blocking(move || {
                    monitor_loop(player_filter, tx, cancel, &OsascriptRunner)
                })
                .await;
                if let Err(error) = result {
                    tracing::warn!(%error, "applescript monitor stopped");
                }
            }))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    struct FixtureRunner {
        responses: Mutex<VecDeque<Result<String, (i32, String)>>>,
        calls: Mutex<Vec<(String, Duration)>>,
        args: Mutex<Vec<Vec<String>>>,
    }

    impl FixtureRunner {
        fn new(responses: Vec<Result<String, (i32, String)>>) -> Self {
            Self {
                responses: Mutex::new(responses.into()),
                calls: Mutex::new(Vec::new()),
                args: Mutex::new(Vec::new()),
            }
        }

        fn calls(&self) -> Vec<(String, Duration)> {
            self.calls.lock().unwrap().clone()
        }

        fn recorded_args(&self) -> Vec<Vec<String>> {
            self.args.lock().unwrap().clone()
        }
    }

    impl ProcessRunner for FixtureRunner {
        fn run(
            &self,
            program: &str,
            args: &[&str],
            timeout: Duration,
            _cancel: &CancellationToken,
        ) -> Result<RawOutput, ScriptFailure> {
            self.calls
                .lock()
                .unwrap()
                .push((program.to_string(), timeout));
            self.args
                .lock()
                .unwrap()
                .push(args.iter().map(|arg| arg.to_string()).collect());
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("fixture exhausted")
                .map(|stdout| RawOutput {
                    stdout,
                    stderr: String::new(),
                })
                .map_err(|(code, stderr)| ScriptFailure {
                    kind: FailureKind::Exit(code),
                    stderr,
                })
        }
    }

    struct VecSink(Vec<MediaEvent>);

    impl EventSink for VecSink {
        fn send(&mut self, event: MediaEvent) -> bool {
            self.0.push(event);
            true
        }
    }

    const FS: char = parse::FIELD_SEP;
    const RS: char = parse::ROW_SEP;

    fn music_row(state: &str, position: &str, duration: &str, title: &str) -> String {
        format!(
            "music{FS}{state}{FS}{position}{FS}{duration}{FS}{FS}{title}{FS}Artist{FS}Album{FS}id1"
        )
    }

    fn spotify_row(state: &str, position: &str, duration: &str, title: &str) -> String {
        format!(
            "spotify{FS}{state}{FS}{position}{FS}{duration}{FS}{FS}{title}{FS}Artist{FS}{FS}id2"
        )
    }

    fn error_row(player: &str, code: i32) -> String {
        format!("{player}{FS}error{FS}{code}{FS}{FS}{FS}{FS}{FS}{FS}{FS}")
    }

    fn pgrep(hit: bool) -> Result<String, (i32, String)> {
        if hit {
            Ok("123\n".to_string())
        } else {
            Err((1, String::new()))
        }
    }

    fn script_output(body: String) -> Result<String, (i32, String)> {
        Ok(format!("{body}\n"))
    }

    fn tick_events(
        state: &mut LoopState,
        runner: &dyn ProcessRunner,
        filter: Option<&str>,
    ) -> (Vec<MediaEvent>, Option<Duration>) {
        let mut sink = VecSink(Vec::new());
        let wait = tick(filter, state, runner, &mut sink, &CancellationToken::new());
        (sink.0, wait)
    }

    #[test]
    fn first_poll_announces_player_track_status_and_position() {
        let runner = FixtureRunner::new(vec![
            pgrep(true),
            pgrep(false),
            script_output(music_row("playing", "45", "225", "Song")),
        ]);
        let mut state = LoopState::default();
        let (events, wait) = tick_events(&mut state, &runner, None);
        assert!(matches!(
            events[0],
            MediaEvent::PlayerChanged { ref id, .. } if id == "music"
        ));
        assert!(matches!(&events[1], MediaEvent::TrackChanged(track) if track.title == "Song"));
        assert!(matches!(
            events[2],
            MediaEvent::Status(PlaybackStatus::Playing)
        ));
        match events[3] {
            MediaEvent::PositionPoll {
                pos_ms,
                rtt,
                sample_age,
            } => {
                assert_eq!(pos_ms, 45_000);
                assert!(rtt > Duration::ZERO);
                assert_eq!(sample_age, Some(SAMPLE_AGE));
            }
            _ => panic!("expected position poll, got {:?}", events[3]),
        }
        assert_eq!(events.len(), 4);
        assert_eq!(wait, Some(parse::PLAYING_POLL));
    }

    #[test]
    fn selects_spotify_when_music_is_paused() {
        let runner = FixtureRunner::new(vec![
            pgrep(true),
            pgrep(true),
            script_output(format!(
                "{}{RS}{}",
                music_row("paused", "30", "225", "Song"),
                spotify_row("playing", "45000", "225000", "Other")
            )),
        ]);
        let mut state = LoopState::default();
        let (events, _) = tick_events(&mut state, &runner, None);
        assert!(matches!(
            events[0],
            MediaEvent::PlayerChanged { ref id, .. } if id == "spotify"
        ));
        assert!(matches!(&events[1], MediaEvent::TrackChanged(track) if track.title == "Other"));
    }

    #[test]
    fn player_filter_excludes_both_players_without_a_script() {
        let runner = FixtureRunner::new(vec![pgrep(true), pgrep(false)]);
        let mut state = LoopState::default();
        let (events, wait) = tick_events(&mut state, &runner, Some("mpv"));
        assert!(events.is_empty());
        assert_eq!(wait, Some(IDLE_POLL));
        assert_eq!(runner.calls().len(), 2, "only pgrep calls expected");
    }

    #[test]
    fn denied_player_backs_off_and_reprobes() {
        let runner = FixtureRunner::new(vec![
            pgrep(true),
            pgrep(false),
            script_output(music_row("playing", "10", "200", "Song")),
            pgrep(true),
            pgrep(false),
            script_output(error_row("music", -1743)),
            // Reprobe tick: pgrep only, no script until the cadence elapses.
            pgrep(true),
            pgrep(false),
        ]);
        let mut state = LoopState::default();
        tick_events(&mut state, &runner, None);
        let (events, wait) = tick_events(&mut state, &runner, None);
        assert_eq!(events, vec![MediaEvent::NoPlayer]);
        assert_eq!(wait, Some(DENIED_REPROBE));

        let (_, wait) = tick_events(&mut state, &runner, None);
        assert_eq!(wait, Some(DENIED_REPROBE));
        let calls = runner.calls();
        assert_eq!(calls.len(), 8, "no second script call before the reprobe");
    }

    #[test]
    fn consent_timeout_switches_to_waiting_and_keeps_long_budget() {
        let runner = FixtureRunner::new(vec![
            pgrep(true),
            pgrep(false),
            script_output(error_row("music", -1712)),
            pgrep(true),
            pgrep(false),
            script_output(music_row("playing", "10", "200", "Song")),
        ]);
        let mut state = LoopState::default();
        let (_, first_wait) = tick_events(&mut state, &runner, None);
        assert_eq!(first_wait, Some(IDLE_POLL));

        let (_, second_wait) = tick_events(&mut state, &runner, None);
        assert_eq!(second_wait, Some(parse::PLAYING_POLL));
        let script_calls: Vec<(String, Duration)> = runner
            .calls()
            .into_iter()
            .filter(|(program, _)| program == OSASCRIPT)
            .collect();
        assert_eq!(script_calls.len(), 2);
        assert_eq!(script_calls[0].1, CONSENT_TIMEOUT);
        // Still waiting for consent on the second poll: the long budget holds.
        assert_eq!(script_calls[1].1, CONSENT_TIMEOUT);
    }

    #[test]
    fn ready_players_poll_with_the_short_budget() {
        let runner = FixtureRunner::new(vec![
            pgrep(true),
            pgrep(false),
            script_output(music_row("playing", "10", "200", "Song")),
            pgrep(true),
            pgrep(false),
            script_output(music_row("playing", "11", "200", "Song")),
        ]);
        let mut state = LoopState::default();
        tick_events(&mut state, &runner, None);
        tick_events(&mut state, &runner, None);
        let script_calls: Vec<Duration> = runner
            .calls()
            .into_iter()
            .filter(|(program, _)| program == OSASCRIPT)
            .map(|(_, timeout)| timeout)
            .collect();
        assert_eq!(script_calls, vec![CONSENT_TIMEOUT, READY_TIMEOUT]);
    }

    #[test]
    fn seek_between_polls_is_emitted() {
        let runner = FixtureRunner::new(vec![
            pgrep(true),
            pgrep(false),
            script_output(music_row("playing", "10", "200", "Song")),
            pgrep(true),
            pgrep(false),
            script_output(music_row("playing", "90", "200", "Song")),
        ]);
        let mut state = LoopState::default();
        tick_events(&mut state, &runner, None);
        let (events, _) = tick_events(&mut state, &runner, None);
        assert!(matches!(events[0], MediaEvent::Seeked { pos_ms: 90_000 }));
    }

    #[test]
    fn track_change_does_not_emit_seek() {
        let runner = FixtureRunner::new(vec![
            pgrep(true),
            pgrep(false),
            script_output(music_row("playing", "10", "200", "Song")),
            pgrep(true),
            pgrep(false),
            script_output(music_row("playing", "0", "180", "Next")),
        ]);
        let mut state = LoopState::default();
        tick_events(&mut state, &runner, None);
        let (events, _) = tick_events(&mut state, &runner, None);
        assert!(matches!(&events[0], MediaEvent::TrackChanged(track) if track.title == "Next"));
        assert_eq!(events.len(), 2, "TrackChanged + PositionPoll only");
        assert!(
            events
                .iter()
                .all(|event| !matches!(event, MediaEvent::Seeked { .. }))
        );
    }

    #[test]
    fn stale_position_polls_emit_no_seek() {
        let runner = FixtureRunner::new(vec![
            pgrep(true),
            pgrep(false),
            script_output(music_row("playing", "30", "200", "Song")),
            pgrep(true),
            pgrep(false),
            script_output(music_row("playing", "30", "200", "Song")),
        ]);
        let mut state = LoopState::default();
        tick_events(&mut state, &runner, None);
        let (events, _) = tick_events(&mut state, &runner, None);
        assert_eq!(events.len(), 1, "only the PositionPoll");
        assert!(matches!(
            events[0],
            MediaEvent::PositionPoll { pos_ms: 30_000, .. }
        ));
    }

    #[test]
    fn disappearing_player_resets_state() {
        let runner = FixtureRunner::new(vec![
            pgrep(true),
            pgrep(false),
            script_output(music_row("playing", "10", "200", "Song")),
            pgrep(false),
            pgrep(false),
        ]);
        let mut state = LoopState::default();
        tick_events(&mut state, &runner, None);
        let (events, wait) = tick_events(&mut state, &runner, None);
        assert_eq!(events, vec![MediaEvent::NoPlayer]);
        assert_eq!(wait, Some(IDLE_POLL));
    }

    #[test]
    fn cancelled_token_stops_the_loop_immediately() {
        let runner = FixtureRunner::new(vec![]);
        let (tx, _rx) = mpsc::channel(8);
        let cancel = CancellationToken::new();
        cancel.cancel();
        let start = Instant::now();
        monitor_loop(None, tx, cancel, &runner);
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn run_process_times_out_and_kills_the_child() {
        let cancel = CancellationToken::new();
        let result = run_process("/bin/sleep", &["30"], Duration::from_millis(150), &cancel);
        assert!(matches!(
            result,
            Err(ScriptFailure {
                kind: FailureKind::Timeout,
                ..
            })
        ));
    }

    #[test]
    fn run_process_honours_cancellation() {
        let cancel = CancellationToken::new();
        let killer = cancel.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(100));
            killer.cancel();
        });
        let result = run_process("/bin/sleep", &["30"], Duration::from_secs(30), &cancel);
        assert!(matches!(
            result,
            Err(ScriptFailure {
                kind: FailureKind::Cancelled,
                ..
            })
        ));
    }

    #[test]
    fn run_process_reports_spawn_failures() {
        let cancel = CancellationToken::new();
        let result = run_process(
            "/nonexistent/lyrachi-binary",
            &[],
            Duration::from_secs(1),
            &cancel,
        );
        assert!(matches!(
            result,
            Err(ScriptFailure {
                kind: FailureKind::Spawn(_),
                ..
            })
        ));
    }

    #[test]
    fn run_process_collects_stdout_and_stderr() {
        let cancel = CancellationToken::new();
        let output = run_process(
            "/bin/sh",
            &["-c", "echo out; echo err >&2"],
            Duration::from_secs(5),
            &cancel,
        )
        .expect("sh succeeds");
        assert!(output.stdout.starts_with("out"));
        assert!(output.stderr.starts_with("err"));
    }

    #[test]
    fn tick_passes_script_to_osascript_via_e_flag() {
        let runner = FixtureRunner::new(vec![
            pgrep(true),
            pgrep(false),
            script_output(music_row("playing", "10", "200", "Song")),
        ]);
        let mut state = LoopState::default();
        tick_events(&mut state, &runner, None);

        let args = runner.recorded_args();
        let script_args = args.last().expect("osascript call");
        assert_eq!(script_args.first().map(String::as_str), Some("-e"));
        assert!(
            script_args
                .get(1)
                .is_some_and(|script| script.contains("Music"))
        );
    }

    #[test]
    fn probe_passes_script_to_osascript_via_e_flag() {
        let runner = FixtureRunner::new(vec![
            pgrep(true),
            pgrep(false),
            script_output(music_row("playing", "10", "200", "Song")),
        ]);
        let snapshot = probe_sync(None, &runner).unwrap();
        assert_eq!(
            snapshot.player.as_ref().map(|player| player.id.as_str()),
            Some("music")
        );

        let args = runner.recorded_args();
        let script_args = args.last().expect("osascript call");
        assert_eq!(script_args.first().map(String::as_str), Some("-e"));
    }

    #[test]
    fn probe_script_only_targets_the_filtered_player() {
        // Both players running, but only Spotify is requested.
        let runner = FixtureRunner::new(vec![
            pgrep(true),
            pgrep(true),
            script_output(spotify_row("playing", "45000", "225000", "Other")),
        ]);
        let snapshot = probe_sync(Some("spotify"), &runner).unwrap();
        assert_eq!(
            snapshot.player.as_ref().map(|player| player.id.as_str()),
            Some("spotify")
        );

        let args = runner.recorded_args();
        let script = args
            .last()
            .and_then(|args| args.get(1))
            .expect("script argument");
        assert!(script.contains("Spotify"));
        assert!(!script.contains("Music"), "Music must not be scripted");
    }
}

use std::collections::HashMap;
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::debug;

use super::backend::{BoxFuture, MediaBackend};
use super::composite::{ExponentialBackoff, jitter_seed};
use super::diff::{PollState, diff};
use super::{
    BackendKind, MediaError, MediaEvent, MediaSnapshot, MpdConfig, PlayerKind, PlayerSnapshot,
    TrackMeta,
};
use crate::core::clock::PlaybackStatus;

const PLAYING_POLL: Duration = Duration::from_millis(500);
const IDLE_POLL: Duration = Duration::from_millis(2_000);

#[derive(Debug, Clone)]
pub struct MpdBackend {
    config: MpdConfig,
}

impl MpdBackend {
    pub fn new(config: MpdConfig) -> Self {
        Self { config }
    }
}

impl MediaBackend for MpdBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Mpd
    }

    fn probe<'a>(
        &'a self,
        player_filter: Option<&'a str>,
    ) -> BoxFuture<'a, Result<MediaSnapshot, MediaError>> {
        Box::pin(async move {
            if !super::composite::player_matches("mpd", player_filter) {
                return Ok(MediaSnapshot {
                    backend: BackendKind::Mpd,
                    ..MediaSnapshot::default()
                });
            }
            let mut connection = MpdConnection::connect(&self.config).await?;
            let status = connection.request("status\n", self.config.timeout).await?;
            let song = connection
                .request("currentsong\n", self.config.timeout)
                .await?;
            Ok(snapshot_from(&status, &song))
        })
    }

    fn start_monitor<'a>(
        &'a self,
        player_filter: Option<String>,
        tx: mpsc::Sender<MediaEvent>,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<JoinHandle<()>, MediaError>> {
        let config = self.config.clone();
        Box::pin(async move {
            Ok(tokio::spawn(async move {
                run_supervisor(config, player_filter, tx, cancel).await;
            }))
        })
    }
}

async fn run_supervisor(
    config: MpdConfig,
    player_filter: Option<String>,
    tx: mpsc::Sender<MediaEvent>,
    cancel: CancellationToken,
) {
    let child = cancel.child_token();
    let (wake_tx, wake_rx) = mpsc::channel(1);
    let mut poll = tokio::spawn(run_poll(
        config.clone(),
        player_filter,
        tx,
        wake_rx,
        child.clone(),
    ));
    let mut idle = tokio::spawn(run_idle(
        config,
        wake_tx.clone(),
        child.clone(),
        cancel.clone(),
    ));

    tokio::select! {
        _ = cancel.cancelled() => {}
        _ = &mut poll => {}
        _ = &mut idle => {}
    }

    child.cancel();
    poll.abort();
    idle.abort();
}

async fn run_idle(
    config: MpdConfig,
    wake_tx: mpsc::Sender<()>,
    child: CancellationToken,
    parent: CancellationToken,
) {
    let mut backoff = ExponentialBackoff::default();
    loop {
        tokio::select! {
            _ = parent.cancelled() => return,
            _ = child.cancelled() => return,
            _ = async {
                match MpdConnection::connect(&config).await {
                    Ok(mut connection) => {
                        backoff.reset();
                        loop {
                            tokio::select! {
                                _ = parent.cancelled() => return,
                                _ = child.cancelled() => return,
                                result = async {
                                    connection.send("idle player\n").await?;
                                    connection.read_until_ok().await
                                } => {
                                    if let Err(error) = result {
                                        debug!(%error, "mpd idle connection ended");
                                        break;
                                    }
                                    let _ = wake_tx.try_send(());
                                }
                            }
                        }
                    }
                    Err(error) => debug!(%error, "mpd idle connection failed"),
                }
                sleep_or_cancel(backoff.next_delay_with_jitter(jitter_seed()), &parent).await;
            } => {}
        }
    }
}

async fn run_poll(
    config: MpdConfig,
    player_filter: Option<String>,
    tx: mpsc::Sender<MediaEvent>,
    mut wake_rx: mpsc::Receiver<()>,
    cancel: CancellationToken,
) {
    let mut backoff = ExponentialBackoff::default();
    loop {
        if cancel.is_cancelled() {
            return;
        }

        let mut announced = false;
        match MpdConnection::connect(&config).await {
            Ok(mut connection) => {
                backoff.reset();
                announced = poll_connected(
                    &config,
                    &mut connection,
                    player_filter.as_deref(),
                    &tx,
                    &mut wake_rx,
                    &cancel,
                )
                .await;
            }
            Err(error) => debug!(%error, "mpd poll connection failed"),
        }

        if announced {
            let _ = tx.send(MediaEvent::NoPlayer).await;
            let _ = tx.send(MediaEvent::NoTrack).await;
            let _ = tx.send(MediaEvent::Status(PlaybackStatus::Stopped)).await;
        }

        sleep_or_cancel(backoff.next_delay_with_jitter(jitter_seed()), &cancel).await;
    }
}

async fn poll_connected(
    config: &MpdConfig,
    connection: &mut MpdConnection,
    player_filter: Option<&str>,
    tx: &mpsc::Sender<MediaEvent>,
    wake_rx: &mut mpsc::Receiver<()>,
    cancel: &CancellationToken,
) -> bool {
    let mut previous: Option<PollState> = None;
    let mut announced = false;

    loop {
        let started = Instant::now();
        let status = match connection.request("status\n", config.timeout).await {
            Ok(status) => status,
            Err(error) => {
                debug!(%error, "mpd status request failed");
                return announced;
            }
        };
        let song = match connection.request("currentsong\n", config.timeout).await {
            Ok(song) => song,
            Err(error) => {
                debug!(%error, "mpd currentsong request failed");
                return announced;
            }
        };
        let rtt = started.elapsed();
        let parsed = parse_state(&status, &song);
        let current = poll_state(&parsed, Instant::now());

        if super::composite::player_matches("mpd", player_filter)
            && emit_changes(previous.as_ref(), &current, rtt, tx).await
        {
            announced = true;
        }

        previous = Some(current);
        let wait = if parsed.playback == PlaybackStatus::Playing {
            PLAYING_POLL
        } else {
            IDLE_POLL
        };

        tokio::select! {
            _ = cancel.cancelled() => return announced,
            _ = tokio::time::sleep(wait) => {}
            Some(()) = wake_rx.recv() => {}
        }
    }
}

async fn sleep_or_cancel(delay: Duration, cancel: &CancellationToken) {
    tokio::select! {
        _ = cancel.cancelled() => {}
        _ = tokio::time::sleep(delay) => {}
    }
}

fn poll_state(current: &MpdState, sampled_at: Instant) -> PollState {
    PollState {
        player: Some(PlayerSnapshot {
            id: "mpd".to_string(),
            kind: PlayerKind::Native,
            backend: BackendKind::Mpd,
        }),
        track: current.track.clone(),
        playback: current.playback,
        position_ms: current.position_ms,
        rate: 1.0,
        sampled_at,
    }
}

async fn emit_changes(
    previous: Option<&PollState>,
    current: &PollState,
    rtt: Duration,
    tx: &mpsc::Sender<MediaEvent>,
) -> bool {
    let fresh = PollState::default();
    let mut announced = false;

    for event in diff(previous.unwrap_or(&fresh), current) {
        announced |= matches!(event, MediaEvent::PlayerChanged { .. });
        let _ = tx.send(event).await;
    }

    if current.track.is_some() {
        let _ = tx
            .send(MediaEvent::PositionPoll {
                pos_ms: current.position_ms,
                rtt,
                sample_age: None,
            })
            .await;
    }

    announced
}

#[derive(Debug, Clone, PartialEq)]
struct MpdState {
    track: Option<TrackMeta>,
    playback: PlaybackStatus,
    position_ms: u64,
}

fn snapshot_from(
    status: &HashMap<String, String>,
    song: &HashMap<String, String>,
) -> MediaSnapshot {
    let state = parse_state(status, song);
    let player = state.track.is_some().then_some(super::PlayerSnapshot {
        id: "mpd".to_string(),
        kind: PlayerKind::Native,
        backend: BackendKind::Mpd,
    });
    MediaSnapshot {
        player,
        track: state.track,
        playback: state.playback,
        position_ms: state.position_ms,
        backend: BackendKind::Mpd,
    }
}

fn parse_state(status: &HashMap<String, String>, song: &HashMap<String, String>) -> MpdState {
    let playback = match status.get("state").map(String::as_str) {
        Some("play") => PlaybackStatus::Playing,
        Some("pause") => PlaybackStatus::Paused,
        _ => PlaybackStatus::Stopped,
    };
    let position_ms = status
        .get("elapsed")
        .and_then(|value| parse_seconds_ms(value))
        .or_else(|| {
            status
                .get("time")
                .and_then(|value| value.split_once(':'))
                .and_then(|(elapsed, _)| parse_seconds_ms(elapsed))
        })
        .unwrap_or(0);
    let track = track_from(song);

    MpdState {
        track,
        playback,
        position_ms,
    }
}

fn track_from(song: &HashMap<String, String>) -> Option<TrackMeta> {
    let title = song
        .get("Title")
        .or_else(|| song.get("title"))
        .or_else(|| song.get("Name"))
        .or_else(|| song.get("name"))
        .or_else(|| song.get("file"))
        .filter(|value| !value.trim().is_empty())?;
    let duration_ms = song
        .get("duration")
        .or_else(|| song.get("Time"))
        .and_then(|value| parse_seconds_ms(value));
    let track_id = song.get("Id").or_else(|| song.get("songid")).cloned();

    Some(TrackMeta {
        title: title.clone(),
        artist: song
            .get("Artist")
            .or_else(|| song.get("artist"))
            .cloned()
            .unwrap_or_default(),
        album: song
            .get("Album")
            .or_else(|| song.get("album"))
            .filter(|value| !value.trim().is_empty())
            .cloned(),
        duration_ms,
        track_id,
        url: None,
    })
}

fn parse_seconds_ms(value: &str) -> Option<u64> {
    let seconds = value.trim().parse::<f64>().ok()?;
    if !seconds.is_finite() || seconds < 0.0 {
        return None;
    }
    Some((seconds * 1_000.0).round() as u64)
}

struct MpdConnection {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
}

impl MpdConnection {
    async fn connect(config: &MpdConfig) -> Result<Self, MediaError> {
        let stream = tokio::time::timeout(
            config.timeout,
            TcpStream::connect((config.host.as_str(), config.port)),
        )
        .await
        .map_err(|_| MediaError::SessionUnavailable("mpd connection timed out".to_string()))?
        .map_err(|error| MediaError::SessionUnavailable(format!("mpd connect: {error}")))?;
        stream
            .set_nodelay(true)
            .map_err(|error| MediaError::SessionUnavailable(format!("mpd socket: {error}")))?;
        let (reader, writer) = stream.into_split();
        Ok(Self {
            reader: BufReader::new(reader),
            writer,
        })
    }

    async fn send(&mut self, command: &str) -> Result<(), MediaError> {
        self.writer
            .write_all(command.as_bytes())
            .await
            .map_err(|error| MediaError::SessionUnavailable(format!("mpd write: {error}")))?;
        self.writer
            .flush()
            .await
            .map_err(|error| MediaError::SessionUnavailable(format!("mpd flush: {error}")))
    }

    async fn request(
        &mut self,
        command: &str,
        timeout: Duration,
    ) -> Result<HashMap<String, String>, MediaError> {
        tokio::time::timeout(timeout, async {
            self.send(command).await?;
            self.read_until_ok().await
        })
        .await
        .map_err(|_| MediaError::SessionUnavailable("mpd request timed out".to_string()))?
    }

    async fn read_until_ok(&mut self) -> Result<HashMap<String, String>, MediaError> {
        let mut values = HashMap::new();
        let mut line = String::new();
        loop {
            line.clear();
            let read =
                self.reader.read_line(&mut line).await.map_err(|error| {
                    MediaError::SessionUnavailable(format!("mpd read: {error}"))
                })?;
            if read == 0 {
                return Err(MediaError::SessionUnavailable(
                    "mpd connection closed".to_string(),
                ));
            }
            let trimmed = line.trim_end_matches(['\r', '\n']);
            if trimmed == "OK" {
                return Ok(values);
            }
            if trimmed.starts_with("ACK ") {
                return Err(MediaError::SessionUnavailable(format!(
                    "mpd command failed: {trimmed}"
                )));
            }
            if trimmed.starts_with("changed: ") {
                continue;
            }
            if let Some((key, value)) = trimmed.split_once(": ") {
                values.insert(key.to_string(), value.to_string());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::TcpListener;

    use super::*;

    #[derive(Debug, Clone)]
    struct MockState {
        state: String,
        elapsed: f64,
        title: String,
        artist: String,
        album: String,
        duration: f64,
        songid: u32,
        connections: usize,
        status_requests: usize,
        idle_requests: usize,
    }

    impl Default for MockState {
        fn default() -> Self {
            Self {
                state: "play".to_string(),
                elapsed: 1.25,
                title: "Yellow".to_string(),
                artist: "Coldplay".to_string(),
                album: "Parachutes".to_string(),
                duration: 267.0,
                songid: 1,
                connections: 0,
                status_requests: 0,
                idle_requests: 0,
            }
        }
    }

    async fn spawn_mock() -> (u16, Arc<Mutex<MockState>>, JoinHandle<()>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let state = Arc::new(Mutex::new(MockState::default()));
        let server_state = state.clone();
        let server = tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let state = server_state.clone();
                tokio::spawn(async move {
                    state.lock().unwrap().connections += 1;
                    let (reader, mut writer) = stream.into_split();
                    let mut lines = BufReader::new(reader).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        match line.trim() {
                            "idle player" => {
                                state.lock().unwrap().idle_requests += 1;
                                tokio::time::sleep(Duration::from_millis(20)).await;
                                let _ = writer.write_all(b"changed: player\nOK\n").await;
                            }
                            "status" => {
                                let snapshot = {
                                    let mut state = state.lock().unwrap();
                                    state.status_requests += 1;
                                    state.clone()
                                };
                                let response = format!(
                                    "volume: 80\nrepeat: 0\nrandom: 0\nsingle: 0\nconsume: 0\nplaylist: 1\nplaylistlength: 1\nstate: {}\nsong: 0\nsongid: {}\ntime: {}:{}\nelapsed: {}\nduration: {}\nOK\n",
                                    snapshot.state,
                                    snapshot.songid,
                                    snapshot.elapsed,
                                    snapshot.duration,
                                    snapshot.elapsed,
                                    snapshot.duration
                                );
                                let _ = writer.write_all(response.as_bytes()).await;
                            }
                            "currentsong" => {
                                let snapshot = state.lock().unwrap().clone();
                                let response = format!(
                                    "file: music/{}.flac\nArtist: {}\nAlbum: {}\nTitle: {}\nduration: {}\nsongid: {}\nOK\n",
                                    snapshot.songid,
                                    snapshot.artist,
                                    snapshot.album,
                                    snapshot.title,
                                    snapshot.duration,
                                    snapshot.songid
                                );
                                let _ = writer.write_all(response.as_bytes()).await;
                            }
                            "close" => break,
                            _ => {
                                let _ = writer.write_all(b"ACK unknown command\n").await;
                            }
                        }
                    }
                });
            }
        });
        (port, state, server)
    }

    async fn collect_events(
        rx: &mut mpsc::Receiver<MediaEvent>,
        duration: Duration,
    ) -> Vec<MediaEvent> {
        let mut events = Vec::new();
        let deadline = tokio::time::Instant::now() + duration;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return events;
            }
            match tokio::time::timeout(remaining, rx.recv()).await {
                Ok(Some(event)) => events.push(event),
                Ok(None) | Err(_) => return events,
            }
        }
    }

    #[tokio::test]
    async fn idle_and_poll_connections_report_track_state_and_seek() {
        let (port, state, server) = spawn_mock().await;
        let backend = MpdBackend::new(MpdConfig {
            enabled: true,
            host: "127.0.0.1".to_string(),
            port,
            timeout: Duration::from_millis(500),
        });
        let (tx, mut rx) = mpsc::channel(128);
        let cancel = CancellationToken::new();
        let handle = backend
            .start_monitor(None, tx, cancel.clone())
            .await
            .unwrap();

        let initial = collect_events(&mut rx, Duration::from_millis(150)).await;
        assert!(initial.iter().any(|event| matches!(
            event,
            MediaEvent::PlayerChanged { id, backend: BackendKind::Mpd, .. } if id == "mpd"
        )));
        assert!(initial.iter().any(|event| matches!(
            event,
            MediaEvent::TrackChanged(track) if track.title == "Yellow" && track.artist == "Coldplay"
        )));
        assert!(
            initial
                .iter()
                .any(|event| matches!(event, MediaEvent::Status(PlaybackStatus::Playing)))
        );

        {
            let mut state = state.lock().unwrap();
            state.state = "pause".to_string();
            state.elapsed = 42.5;
            state.title = "Trouble".to_string();
            state.songid = 2;
        }

        let updated = collect_events(&mut rx, Duration::from_millis(300)).await;
        assert!(updated.iter().any(|event| matches!(
            event,
            MediaEvent::TrackChanged(track) if track.title == "Trouble"
        )));
        assert!(
            updated
                .iter()
                .any(|event| matches!(event, MediaEvent::Status(PlaybackStatus::Paused)))
        );
        assert!(updated.iter().any(|event| matches!(
            event,
            MediaEvent::PositionPoll { pos_ms, .. } if *pos_ms >= 42_000
        )));

        let snapshot = state.lock().unwrap().clone();
        assert!(snapshot.connections >= 2);
        assert!(snapshot.idle_requests > 0);
        assert!(snapshot.status_requests >= 2);

        cancel.cancel();
        let _ = handle.await;
        server.abort();
    }

    #[tokio::test]
    async fn forward_seek_is_reported_without_a_track_change() {
        let (port, state, server) = spawn_mock().await;
        let backend = MpdBackend::new(MpdConfig {
            enabled: true,
            host: "127.0.0.1".to_string(),
            port,
            timeout: Duration::from_millis(500),
        });
        let (tx, mut rx) = mpsc::channel(128);
        let cancel = CancellationToken::new();
        let handle = backend
            .start_monitor(None, tx, cancel.clone())
            .await
            .unwrap();

        let initial = collect_events(&mut rx, Duration::from_millis(150)).await;
        assert!(initial.iter().any(
            |event| matches!(event, MediaEvent::PositionPoll { pos_ms, .. } if *pos_ms >= 1_000)
        ));

        state.lock().unwrap().elapsed = 120.0;

        let updated = collect_events(&mut rx, Duration::from_millis(900)).await;
        assert!(updated.iter().any(|event| matches!(
            event,
            MediaEvent::Seeked { pos_ms } if *pos_ms >= 120_000
        )));
        assert!(
            !updated
                .iter()
                .any(|event| matches!(event, MediaEvent::TrackChanged(_)))
        );

        cancel.cancel();
        let _ = handle.await;
        server.abort();
    }
}

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::future::join_all;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::warn;

use super::backend::{BoxFuture, MediaBackend};
use super::{
    BackendKind, MediaError, MediaEvent, MediaSnapshot, PlayerSnapshot, TrackMeta,
    player_short_name,
};
use crate::core::clock::PlaybackStatus;

#[derive(Debug, Clone)]
pub struct ExponentialBackoff {
    initial: Duration,
    max: Duration,
    current: Duration,
}

impl ExponentialBackoff {
    pub fn new(initial: Duration, max: Duration) -> Self {
        Self {
            initial,
            max,
            current: initial,
        }
    }

    pub fn next_delay(&mut self) -> Duration {
        let delay = self.current;
        self.current = (self.current.saturating_mul(2)).min(self.max);
        delay
    }

    pub fn reset(&mut self) {
        self.current = self.initial;
    }

    pub fn current(&self) -> Duration {
        self.current
    }
}

impl Default for ExponentialBackoff {
    fn default() -> Self {
        Self::new(Duration::from_millis(500), Duration::from_secs(10))
    }
}

pub struct CompositeBackend {
    backends: Vec<Arc<dyn MediaBackend>>,
}

impl CompositeBackend {
    pub fn new(backends: Vec<Arc<dyn MediaBackend>>) -> Self {
        Self { backends }
    }

    pub fn backends(&self) -> &[Arc<dyn MediaBackend>] {
        &self.backends
    }
}

impl MediaBackend for CompositeBackend {
    fn kind(&self) -> BackendKind {
        self.backends
            .first()
            .map(|backend| backend.kind())
            .unwrap_or_else(BackendKind::current)
    }

    fn probe<'a>(
        &'a self,
        player_filter: Option<&'a str>,
    ) -> BoxFuture<'a, Result<MediaSnapshot, MediaError>> {
        Box::pin(async move {
            if self.backends.is_empty() {
                return Err(MediaError::Unsupported);
            }

            let probe_futs = self
                .backends
                .iter()
                .map(|b| b.probe(player_filter))
                .collect::<Vec<_>>();

            let results = join_all(probe_futs).await;

            let mut best_snapshot: Option<MediaSnapshot> = None;
            let mut first_error: Option<MediaError> = None;

            for result in results {
                match result {
                    Ok(snapshot) => {
                        if snapshot
                            .player
                            .as_ref()
                            .is_some_and(|p| !matches_filter(&p.id, player_filter))
                        {
                            continue;
                        }

                        match &best_snapshot {
                            None => best_snapshot = Some(snapshot),
                            Some(current) => {
                                let curr_status_rank = current
                                    .player
                                    .as_ref()
                                    .map_or(0, |_| playback_rank(current.playback));
                                let new_status_rank = snapshot
                                    .player
                                    .as_ref()
                                    .map_or(0, |_| playback_rank(snapshot.playback));

                                if new_status_rank > curr_status_rank {
                                    best_snapshot = Some(snapshot);
                                } else if new_status_rank == curr_status_rank {
                                    let curr_priority = current
                                        .player
                                        .as_ref()
                                        .map_or(255, |p| p.backend.priority_rank());
                                    let new_priority = snapshot
                                        .player
                                        .as_ref()
                                        .map_or(255, |p| p.backend.priority_rank());

                                    if new_priority < curr_priority {
                                        best_snapshot = Some(snapshot);
                                    }
                                }
                            }
                        }
                    }
                    Err(err) => {
                        if first_error.is_none() {
                            first_error = Some(err);
                        }
                    }
                }
            }

            if let Some(snapshot) = best_snapshot {
                Ok(snapshot)
            } else if let Some(err) = first_error {
                Err(err)
            } else {
                Ok(MediaSnapshot::default())
            }
        })
    }

    fn start_monitor<'a>(
        &'a self,
        player_filter: Option<String>,
        tx: mpsc::Sender<MediaEvent>,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<JoinHandle<()>, MediaError>> {
        Box::pin(async move {
            if self.backends.is_empty() {
                return Err(MediaError::Unsupported);
            }

            let (internal_tx, internal_rx) = mpsc::channel::<(usize, BackendKind, MediaEvent)>(256);
            let mut successful_backends = 0;
            let mut first_error = None;

            for (idx, backend) in self.backends.iter().enumerate() {
                let (backend_tx, mut backend_rx) = mpsc::channel::<MediaEvent>(64);
                let backend_kind = backend.kind();
                let b_tx = internal_tx.clone();
                let b_cancel = cancel.clone();

                match backend
                    .start_monitor(player_filter.clone(), backend_tx, b_cancel)
                    .await
                {
                    Ok(_handle) => {
                        successful_backends += 1;
                        tokio::spawn(async move {
                            while let Some(event) = backend_rx.recv().await {
                                if b_tx.send((idx, backend_kind, event)).await.is_err() {
                                    break;
                                }
                            }
                        });
                    }
                    Err(err) => {
                        warn!(
                            backend = %backend_kind.as_str(),
                            %err,
                            "composite: backend monitor failed to start"
                        );
                        if first_error.is_none() {
                            first_error = Some(err);
                        }
                    }
                }
            }

            if successful_backends == 0 {
                return Err(first_error.unwrap_or(MediaError::Unsupported));
            }

            let num_backends = self.backends.len();
            let arbiter_task = tokio::spawn(async move {
                run_composite_arbiter(num_backends, player_filter, internal_rx, tx, cancel).await;
            });

            Ok(arbiter_task)
        })
    }
}

#[derive(Debug, Clone)]
struct BackendPlayerState {
    backend: BackendKind,
    player: Option<PlayerSnapshot>,
    track: Option<TrackMeta>,
    playback: PlaybackStatus,
    rate: f64,
    last_pos_ms: u64,
    last_active_at: Instant,
}

impl BackendPlayerState {
    fn new(backend: BackendKind, now: Instant) -> Self {
        Self {
            backend,
            player: None,
            track: None,
            playback: PlaybackStatus::Stopped,
            rate: 1.0,
            last_pos_ms: 0,
            last_active_at: now,
        }
    }
}

async fn run_composite_arbiter(
    num_backends: usize,
    player_filter: Option<String>,
    mut rx: mpsc::Receiver<(usize, BackendKind, MediaEvent)>,
    tx: mpsc::Sender<MediaEvent>,
    cancel: CancellationToken,
) {
    let now = Instant::now();
    let mut states: Vec<BackendPlayerState> = (0..num_backends)
        .map(|_| BackendPlayerState::new(BackendKind::Unsupported, now))
        .collect();

    let mut current_winner: Option<usize> = None;

    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            msg = rx.recv() => {
                let Some((idx, backend_kind, event)) = msg else {
                    break;
                };

                if idx >= states.len() {
                    continue;
                }

                let now = Instant::now();
                states[idx].backend = backend_kind;

                match &event {
                    MediaEvent::PlayerChanged { id, kind, .. } => {
                        if matches_filter(id, player_filter.as_deref()) {
                            states[idx].player = Some(PlayerSnapshot {
                                id: id.clone(),
                                kind: *kind,
                                backend: backend_kind,
                            });
                            states[idx].last_active_at = now;
                        } else {
                            states[idx].player = None;
                            states[idx].track = None;
                            states[idx].playback = PlaybackStatus::Stopped;
                        }
                    }
                    MediaEvent::NoPlayer => {
                        states[idx].player = None;
                        states[idx].track = None;
                        states[idx].playback = PlaybackStatus::Stopped;
                        states[idx].last_active_at = now;
                    }
                    MediaEvent::TrackChanged(meta) => {
                        states[idx].track = Some(meta.clone());
                        states[idx].last_active_at = now;
                    }
                    MediaEvent::NoTrack => {
                        states[idx].track = None;
                        states[idx].last_active_at = now;
                    }
                    MediaEvent::Status(status) => {
                        states[idx].playback = *status;
                        states[idx].last_active_at = now;
                    }
                    MediaEvent::Rate(rate) => {
                        states[idx].rate = *rate;
                        states[idx].last_active_at = now;
                    }
                    MediaEvent::Seeked { pos_ms } => {
                        states[idx].last_pos_ms = *pos_ms;
                        states[idx].last_active_at = now;
                    }
                    MediaEvent::PositionPoll { pos_ms, .. } => {
                        states[idx].last_pos_ms = *pos_ms;
                    }
                }

                let new_winner = select_winner(&states);

                if new_winner != current_winner {
                    current_winner = new_winner;
                    if let Some(w) = current_winner {
                        let w_state = &states[w];
                        if let Some(player) = &w_state.player {
                            let _ = tx
                                .send(MediaEvent::PlayerChanged {
                                    id: player.id.clone(),
                                    kind: player.kind,
                                    backend: player.backend,
                                })
                                .await;
                        }
                        if let Some(track) = &w_state.track {
                            let _ = tx.send(MediaEvent::TrackChanged(track.clone())).await;
                        } else {
                            let _ = tx.send(MediaEvent::NoTrack).await;
                        }
                        let _ = tx.send(MediaEvent::Status(w_state.playback)).await;
                        let _ = tx.send(MediaEvent::Rate(w_state.rate)).await;
                        let _ = tx.send(MediaEvent::Seeked {
                            pos_ms: w_state.last_pos_ms,
                        }).await;
                    } else {
                        let _ = tx.send(MediaEvent::NoPlayer).await;
                        let _ = tx.send(MediaEvent::NoTrack).await;
                        let _ = tx.send(MediaEvent::Status(PlaybackStatus::Stopped)).await;
                    }
                } else if current_winner == Some(idx) {
                    let _ = tx.send(event).await;
                }
            }
        }
    }
}

fn select_winner(states: &[BackendPlayerState]) -> Option<usize> {
    let mut best: Option<(usize, &BackendPlayerState)> = None;

    for (idx, state) in states.iter().enumerate() {
        if state.player.is_none() {
            continue;
        }

        match best {
            None => best = Some((idx, state)),
            Some((_best_idx, best_state)) => {
                let curr_status_rank = playback_rank(best_state.playback);
                let new_status_rank = playback_rank(state.playback);

                let should_replace = match new_status_rank.cmp(&curr_status_rank) {
                    std::cmp::Ordering::Greater => true,
                    std::cmp::Ordering::Less => false,
                    std::cmp::Ordering::Equal => {
                        let curr_priority = best_state.backend.priority_rank();
                        let new_priority = state.backend.priority_rank();
                        match new_priority.cmp(&curr_priority) {
                            std::cmp::Ordering::Less => true,
                            std::cmp::Ordering::Greater => false,
                            std::cmp::Ordering::Equal => {
                                state.last_active_at > best_state.last_active_at
                            }
                        }
                    }
                };

                if should_replace {
                    best = Some((idx, state));
                }
            }
        }
    }

    best.map(|(idx, _)| idx)
}

fn playback_rank(status: PlaybackStatus) -> u8 {
    match status {
        PlaybackStatus::Playing => 2,
        PlaybackStatus::Paused => 1,
        PlaybackStatus::Stopped => 0,
    }
}

fn matches_filter(player_id: &str, filter: Option<&str>) -> bool {
    let Some(filter) = filter else {
        return true;
    };
    let filter = filter.trim().to_ascii_lowercase();
    if filter.is_empty() {
        return true;
    }
    let id_lower = player_id.to_ascii_lowercase();
    let short_lower = player_short_name(player_id).to_ascii_lowercase();
    id_lower.contains(&filter) || short_lower.contains(&filter)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::PlayerKind;
    use crate::media::fake::FakeBackend;

    fn sample_track(title: &str) -> TrackMeta {
        TrackMeta {
            title: title.to_string(),
            artist: "Artist".to_string(),
            album: Some("Album".to_string()),
            duration_ms: Some(200_000),
            track_id: None,
            url: None,
        }
    }

    #[tokio::test]
    async fn playing_beats_paused_across_backends() {
        let (tx, mut rx) = mpsc::channel(32);
        let cancel = CancellationToken::new();

        let mpris_events = vec![
            (
                Duration::ZERO,
                MediaEvent::PlayerChanged {
                    id: "spotify".to_string(),
                    kind: PlayerKind::Native,
                    backend: BackendKind::Mpris,
                },
            ),
            (Duration::ZERO, MediaEvent::Status(PlaybackStatus::Paused)),
            (
                Duration::ZERO,
                MediaEvent::TrackChanged(sample_track("Spotify Track")),
            ),
        ];

        let mpd_events = vec![
            (
                Duration::from_millis(5),
                MediaEvent::PlayerChanged {
                    id: "mpd".to_string(),
                    kind: PlayerKind::Native,
                    backend: BackendKind::Mpd,
                },
            ),
            (
                Duration::from_millis(5),
                MediaEvent::Status(PlaybackStatus::Playing),
            ),
            (
                Duration::from_millis(5),
                MediaEvent::TrackChanged(sample_track("MPD Track")),
            ),
        ];

        let b1 = Arc::new(FakeBackend::new(BackendKind::Mpris, mpris_events));
        let b2 = Arc::new(FakeBackend::new(BackendKind::Mpd, mpd_events));
        let composite = CompositeBackend::new(vec![b1, b2]);

        let handle = composite
            .start_monitor(None, tx, cancel.clone())
            .await
            .expect("composite monitor");

        let mut received = Vec::new();
        let timeout = tokio::time::sleep(Duration::from_millis(150));
        tokio::pin!(timeout);

        loop {
            tokio::select! {
                _ = &mut timeout => break,
                Some(event) = rx.recv() => {
                    received.push(event);
                }
            }
        }

        cancel.cancel();
        let _ = handle.await;

        let last_player = received
            .iter()
            .filter_map(|e| match e {
                MediaEvent::PlayerChanged { id, backend, .. } => Some((id.clone(), *backend)),
                _ => None,
            })
            .next_back();

        assert_eq!(
            last_player,
            Some(("mpd".to_string(), BackendKind::Mpd)),
            "MPD (playing) must defeat MPRIS (paused)"
        );
    }

    #[tokio::test]
    async fn native_os_beats_browser_at_same_playing_status() {
        let (tx, mut rx) = mpsc::channel(32);
        let cancel = CancellationToken::new();

        let mpris_events = vec![
            (
                Duration::ZERO,
                MediaEvent::PlayerChanged {
                    id: "spotify".to_string(),
                    kind: PlayerKind::Native,
                    backend: BackendKind::Mpris,
                },
            ),
            (Duration::ZERO, MediaEvent::Status(PlaybackStatus::Playing)),
        ];

        let web_events = vec![
            (
                Duration::ZERO,
                MediaEvent::PlayerChanged {
                    id: "chrome".to_string(),
                    kind: PlayerKind::Browser,
                    backend: BackendKind::WebNowPlaying,
                },
            ),
            (Duration::ZERO, MediaEvent::Status(PlaybackStatus::Playing)),
        ];

        let b1 = Arc::new(FakeBackend::new(BackendKind::Mpris, mpris_events));
        let b2 = Arc::new(FakeBackend::new(BackendKind::WebNowPlaying, web_events));
        let composite = CompositeBackend::new(vec![b1, b2]);

        let handle = composite
            .start_monitor(None, tx, cancel.clone())
            .await
            .expect("composite monitor");

        let mut received = Vec::new();
        let timeout = tokio::time::sleep(Duration::from_millis(150));
        tokio::pin!(timeout);

        loop {
            tokio::select! {
                _ = &mut timeout => break,
                Some(event) = rx.recv() => {
                    received.push(event);
                }
            }
        }

        cancel.cancel();
        let _ = handle.await;

        let active_players: Vec<_> = received
            .iter()
            .filter_map(|e| match e {
                MediaEvent::PlayerChanged { id, backend, .. } => Some((id.as_str(), *backend)),
                _ => None,
            })
            .collect();

        assert_eq!(
            active_players.last().copied(),
            Some(("spotify", BackendKind::Mpris)),
            "Native OS (priority 0) must beat Browser (priority 2) when both playing"
        );
    }

    #[tokio::test]
    async fn explicit_filter_restricts_arbitration_to_matching_backend() {
        let (tx, mut rx) = mpsc::channel(32);
        let cancel = CancellationToken::new();

        let mpris_events = vec![
            (
                Duration::ZERO,
                MediaEvent::PlayerChanged {
                    id: "spotify".to_string(),
                    kind: PlayerKind::Native,
                    backend: BackendKind::Mpris,
                },
            ),
            (Duration::ZERO, MediaEvent::Status(PlaybackStatus::Playing)),
        ];

        let mpd_events = vec![
            (
                Duration::ZERO,
                MediaEvent::PlayerChanged {
                    id: "mpd".to_string(),
                    kind: PlayerKind::Native,
                    backend: BackendKind::Mpd,
                },
            ),
            (Duration::ZERO, MediaEvent::Status(PlaybackStatus::Playing)),
        ];

        let b1 = Arc::new(FakeBackend::new(BackendKind::Mpris, mpris_events));
        let b2 = Arc::new(FakeBackend::new(BackendKind::Mpd, mpd_events));
        let composite = CompositeBackend::new(vec![b1, b2]);

        let handle = composite
            .start_monitor(Some("mpd".to_string()), tx, cancel.clone())
            .await
            .expect("composite monitor");

        let mut received = Vec::new();
        let timeout = tokio::time::sleep(Duration::from_millis(150));
        tokio::pin!(timeout);

        loop {
            tokio::select! {
                _ = &mut timeout => break,
                Some(event) = rx.recv() => {
                    received.push(event);
                }
            }
        }

        cancel.cancel();
        let _ = handle.await;

        let players: Vec<_> = received
            .iter()
            .filter_map(|e| match e {
                MediaEvent::PlayerChanged { id, .. } => Some(id.as_str()),
                _ => None,
            })
            .collect();

        assert!(players.contains(&"mpd"));
        assert!(
            !players.contains(&"spotify"),
            "spotify should have been filtered out"
        );
    }

    #[tokio::test]
    async fn composite_probe_picks_best_snapshot() {
        let snap1 = MediaSnapshot {
            player: Some(PlayerSnapshot {
                id: "spotify".to_string(),
                kind: PlayerKind::Native,
                backend: BackendKind::Mpris,
            }),
            track: Some(sample_track("Spotify Track")),
            playback: PlaybackStatus::Paused,
            position_ms: 1_000,
            backend: BackendKind::Mpris,
        };

        let snap2 = MediaSnapshot {
            player: Some(PlayerSnapshot {
                id: "mpd".to_string(),
                kind: PlayerKind::Native,
                backend: BackendKind::Mpd,
            }),
            track: Some(sample_track("MPD Track")),
            playback: PlaybackStatus::Playing,
            position_ms: 5_000,
            backend: BackendKind::Mpd,
        };

        let b1 = Arc::new(FakeBackend::new(BackendKind::Mpris, vec![]).with_snapshot(snap1));
        let b2 = Arc::new(FakeBackend::new(BackendKind::Mpd, vec![]).with_snapshot(snap2));
        let composite = CompositeBackend::new(vec![b1, b2]);

        let best = composite.probe(None).await.expect("probe should succeed");
        assert_eq!(
            best.player.as_ref().map(|p| p.id.as_str()),
            Some("mpd"),
            "probe should pick playing MPD over paused Spotify"
        );
    }
}

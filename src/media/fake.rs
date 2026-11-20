use std::time::Duration;

use tokio::sync::mpsc::{self, Sender};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::backend::{BoxFuture, MediaBackend};
use super::{BackendKind, MediaError, MediaEvent, MediaSnapshot};

#[derive(Debug, Clone, PartialEq)]
pub struct FakeMediaSource {
    schedule: Vec<(Duration, MediaEvent)>,
}

impl FakeMediaSource {
    pub fn new(mut schedule: Vec<(Duration, MediaEvent)>) -> Self {
        schedule.sort_by_key(|(at, _)| *at);
        Self { schedule }
    }

    pub fn schedule(&self) -> &[(Duration, MediaEvent)] {
        &self.schedule
    }

    pub async fn run(self, tx: Sender<MediaEvent>, cancel: CancellationToken) {
        let started = tokio::time::Instant::now();

        for (at, event) in self.schedule {
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = tokio::time::sleep_until(started + at) => {}
            }
            if tx.send(event).await.is_err() {
                return;
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct FakeBackend {
    pub kind: BackendKind,
    pub schedule: Vec<(Duration, MediaEvent)>,
    pub initial_snapshot: Option<MediaSnapshot>,
}

impl FakeBackend {
    pub fn new(kind: BackendKind, schedule: Vec<(Duration, MediaEvent)>) -> Self {
        Self {
            kind,
            schedule,
            initial_snapshot: None,
        }
    }

    pub fn with_snapshot(mut self, snapshot: MediaSnapshot) -> Self {
        self.initial_snapshot = Some(snapshot);
        self
    }
}

impl MediaBackend for FakeBackend {
    fn kind(&self) -> BackendKind {
        self.kind
    }

    fn probe<'a>(
        &'a self,
        _player_filter: Option<&'a str>,
    ) -> BoxFuture<'a, Result<MediaSnapshot, MediaError>> {
        let snapshot = self.initial_snapshot.clone().unwrap_or(MediaSnapshot {
            backend: self.kind,
            ..Default::default()
        });
        Box::pin(async move { Ok(snapshot) })
    }

    fn start_monitor<'a>(
        &'a self,
        _player_filter: Option<String>,
        tx: mpsc::Sender<MediaEvent>,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<JoinHandle<()>, MediaError>> {
        let source = FakeMediaSource::new(self.schedule.clone());
        Box::pin(async move {
            Ok(tokio::spawn(async move {
                source.run(tx, cancel).await;
            }))
        })
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::sync::mpsc;

    use super::*;
    use crate::core::clock::PlaybackStatus;
    use crate::media::{PlayerKind, TrackMeta};

    fn track(title: &str) -> TrackMeta {
        TrackMeta {
            title: title.to_string(),
            artist: "Coldplay".to_string(),
            album: Some("Parachutes".to_string()),
            duration_ms: Some(267_000),
            track_id: None,
            url: None,
        }
    }

    #[tokio::test]
    async fn schedules_track_switch_seek_and_pause_without_dbus() {
        let (tx, mut rx) = mpsc::channel(16);
        let source = FakeMediaSource::new(vec![
            (
                Duration::ZERO,
                MediaEvent::PlayerChanged {
                    id: "org.mpris.MediaPlayer2.spotify".to_string(),
                    kind: PlayerKind::Native,
                    backend: BackendKind::Mpris,
                },
            ),
            (Duration::ZERO, MediaEvent::TrackChanged(track("Yellow"))),
            (
                Duration::from_millis(1),
                MediaEvent::TrackChanged(track("Trouble")),
            ),
            (
                Duration::from_millis(2),
                MediaEvent::Seeked { pos_ms: 4_500 },
            ),
            (
                Duration::from_millis(2),
                MediaEvent::Status(PlaybackStatus::Paused),
            ),
        ]);

        let task = tokio::spawn(source.run(tx, CancellationToken::new()));
        let events = tokio::time::timeout(Duration::from_secs(1), async {
            let mut events = Vec::new();
            while let Some(event) = rx.recv().await {
                events.push(event);
            }
            events
        })
        .await
        .expect("fake media events should arrive");

        task.await.expect("fake media source task");

        assert_eq!(events.len(), 5);
        assert!(matches!(
            &events[1],
            MediaEvent::TrackChanged(track) if track.title == "Yellow"
        ));
        assert!(matches!(
            &events[2],
            MediaEvent::TrackChanged(track) if track.title == "Trouble"
        ));
        assert!(matches!(events[3], MediaEvent::Seeked { pos_ms: 4_500 }));
        assert!(matches!(
            events[4],
            MediaEvent::Status(PlaybackStatus::Paused)
        ));
    }
}

use std::time::Instant;

use crate::core::clock::PlaybackStatus;
use crate::media::{MediaEvent, PlayerSnapshot, TrackMeta};

/// A position sample deviating more than this from the extrapolated
/// expectation counts as a seek (m44-review:39).
pub const SEEK_THRESHOLD_MS: u64 = 700;

/// One polling snapshot of a single player, taken by any backend that polls
/// (AppleScript now, MPD later). `rate` is kept for backends that report it;
/// the AppleScript path is always 1.0.
#[derive(Debug, Clone, PartialEq)]
pub struct PollState {
    pub player: Option<PlayerSnapshot>,
    pub track: Option<TrackMeta>,
    pub playback: PlaybackStatus,
    pub position_ms: u64,
    pub rate: f64,
    pub sampled_at: Instant,
}

impl Default for PollState {
    fn default() -> Self {
        Self {
            player: None,
            track: None,
            playback: PlaybackStatus::Stopped,
            position_ms: 0,
            rate: 1.0,
            sampled_at: Instant::now(),
        }
    }
}

/// Compute the transition events between two polls. `PositionPoll` is not
/// emitted here: it carries transport-specific `rtt`/`sample_age` and stays
/// with the caller.
///
/// Contract: elapsed time comes from `sampled_at` on both states (never a
/// wall-clock `now`), extrapolation only happens out of `Playing`, a track
/// change re-anchors the clock through `TrackChanged` instead of emitting a
/// redundant `Seeked`, and a repeated identical position while playing is a
/// stale poll, not a seek.
pub fn diff(previous: &PollState, next: &PollState) -> Vec<MediaEvent> {
    let mut events = Vec::new();

    match (&previous.player, &next.player) {
        (Some(_), None) => {
            events.push(MediaEvent::NoPlayer);
            return events;
        }
        (None, None) => return events,
        (_, Some(player)) => {
            if previous.player.as_ref() != Some(player) {
                events.push(MediaEvent::PlayerChanged {
                    id: player.id.clone(),
                    kind: player.kind,
                    backend: player.backend,
                });
            }
        }
    }

    let same_player = previous.player == next.player;
    let track_changed = previous.track != next.track;
    if track_changed {
        match &next.track {
            Some(track) => events.push(MediaEvent::TrackChanged(track.clone())),
            None => events.push(MediaEvent::NoTrack),
        }
    }

    if previous.playback != next.playback {
        events.push(MediaEvent::Status(next.playback));
    }

    if previous.rate != next.rate {
        events.push(MediaEvent::Rate(next.rate));
    }

    if !same_player || track_changed {
        return events;
    }

    if let Some(pos_ms) = seek_position(previous, next) {
        events.push(MediaEvent::Seeked { pos_ms });
    }

    events
}

/// Returns the seek target when `next` deviates from the expected position,
/// or `None` when the sample is ordinary (including a stale repeated value).
/// Expectation extrapolates only out of `Playing`; out of `Paused` it is the
/// frozen position, so a paused seek or a resume after a paused seek is still
/// detected.
fn seek_position(previous: &PollState, next: &PollState) -> Option<u64> {
    if !matches!(
        next.playback,
        PlaybackStatus::Playing | PlaybackStatus::Paused
    ) {
        return None;
    }
    if !matches!(
        previous.playback,
        PlaybackStatus::Playing | PlaybackStatus::Paused
    ) {
        return None;
    }
    // A poll that repeats the previous position verbatim is a stale sample
    // (Spotify during crossfade), not a seek to the past.
    if next.position_ms == previous.position_ms {
        return None;
    }

    let elapsed_ms = next
        .sampled_at
        .checked_duration_since(previous.sampled_at)
        .unwrap_or_default()
        .as_millis() as f64;
    let advance = matches!(previous.playback, PlaybackStatus::Playing);
    let expected = previous.position_ms as f64
        + if advance {
            elapsed_ms * previous.rate
        } else {
            0.0
        };
    let deviation = (next.position_ms as f64 - expected).abs();

    (deviation > SEEK_THRESHOLD_MS as f64).then_some(next.position_ms)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::{BackendKind, PlayerKind};

    fn player(id: &str) -> Option<PlayerSnapshot> {
        Some(PlayerSnapshot {
            id: id.to_string(),
            kind: PlayerKind::Native,
            backend: BackendKind::AppleScript,
        })
    }

    fn track(title: &str) -> TrackMeta {
        TrackMeta {
            title: title.to_string(),
            artist: "Artist".to_string(),
            album: None,
            duration_ms: Some(200_000),
            track_id: None,
            url: None,
        }
    }

    fn state(player_id: Option<&str>, at: Instant) -> PollState {
        PollState {
            player: player_id.and_then(player),
            track: player_id.map(|_| track("Song")),
            playback: PlaybackStatus::Stopped,
            position_ms: 0,
            rate: 1.0,
            sampled_at: at,
        }
    }

    #[test]
    fn identical_states_emit_nothing() {
        let at = Instant::now();
        let mut previous = state(Some("music"), at);
        previous.playback = PlaybackStatus::Playing;
        previous.position_ms = 5_000;
        let next = previous.clone();
        assert!(diff(&previous, &next).is_empty());
    }

    #[test]
    fn disappearing_player_emits_no_player_only() {
        let at = Instant::now();
        let previous = state(Some("music"), at);
        let next = state(None, at);
        assert_eq!(diff(&previous, &next), vec![MediaEvent::NoPlayer]);
    }

    #[test]
    fn appearing_player_emits_player_track_and_status() {
        let at = Instant::now();
        let previous = state(None, at);
        let mut next = state(Some("music"), at);
        next.playback = PlaybackStatus::Playing;
        let events = diff(&previous, &next);
        assert!(matches!(
            events.first(),
            Some(MediaEvent::PlayerChanged { id, .. }) if id == "music"
        ));
        assert!(matches!(&events[1], MediaEvent::TrackChanged(_)));
        assert!(matches!(
            events[2],
            MediaEvent::Status(PlaybackStatus::Playing)
        ));
        assert_eq!(events.len(), 3);
    }

    #[test]
    fn changing_player_reemits_track_without_seek() {
        let at = Instant::now();
        let mut previous = state(Some("music"), at);
        previous.playback = PlaybackStatus::Playing;
        previous.position_ms = 60_000;
        let mut next = state(Some("spotify"), at);
        next.playback = PlaybackStatus::Playing;
        next.track = Some(track("Other"));
        next.position_ms = 120_000;
        let events = diff(&previous, &next);
        assert!(matches!(
            events.first(),
            Some(MediaEvent::PlayerChanged { id, .. }) if id == "spotify"
        ));
        assert!(
            events
                .iter()
                .all(|event| !matches!(event, MediaEvent::Seeked { .. }))
        );
    }

    #[test]
    fn track_change_reanchors_without_seek() {
        let at = Instant::now();
        let mut previous = state(Some("music"), at);
        previous.playback = PlaybackStatus::Playing;
        previous.position_ms = 100_000;
        let mut next = previous.clone();
        next.sampled_at = at;
        next.track = Some(track("Next"));
        next.position_ms = 0;
        let events = diff(&previous, &next);
        assert!(matches!(&events[0], MediaEvent::TrackChanged(_)));
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn status_change_emits_status() {
        let at = Instant::now();
        let mut previous = state(Some("music"), at);
        previous.playback = PlaybackStatus::Playing;
        let mut next = previous.clone();
        next.playback = PlaybackStatus::Paused;
        assert_eq!(
            diff(&previous, &next),
            vec![MediaEvent::Status(PlaybackStatus::Paused)]
        );
    }

    #[test]
    fn rate_change_emits_rate() {
        let at = Instant::now();
        let mut previous = state(Some("music"), at);
        previous.playback = PlaybackStatus::Playing;
        let mut next = previous.clone();
        next.rate = 0.5;
        assert_eq!(diff(&previous, &next), vec![MediaEvent::Rate(0.5)]);
    }

    #[test]
    fn forward_seek_beyond_threshold_is_detected() {
        let at = Instant::now();
        let mut previous = state(Some("music"), at);
        previous.playback = PlaybackStatus::Playing;
        previous.position_ms = 10_000;
        let mut next = previous.clone();
        next.sampled_at = at + std::time::Duration::from_millis(1_000);
        next.position_ms = 60_000;
        assert_eq!(
            diff(&previous, &next),
            vec![MediaEvent::Seeked { pos_ms: 60_000 }]
        );
    }

    #[test]
    fn normal_advance_within_threshold_emits_nothing() {
        let at = Instant::now();
        let mut previous = state(Some("music"), at);
        previous.playback = PlaybackStatus::Playing;
        previous.position_ms = 10_000;
        let mut next = previous.clone();
        next.sampled_at = at + std::time::Duration::from_millis(1_000);
        next.position_ms = 11_000;
        assert!(diff(&previous, &next).is_empty());
    }

    #[test]
    fn repeated_position_while_playing_is_stale_not_seek() {
        let at = Instant::now();
        let mut previous = state(Some("spotify"), at);
        previous.playback = PlaybackStatus::Playing;
        previous.position_ms = 30_000;
        let mut next = previous.clone();
        next.sampled_at = at + std::time::Duration::from_millis(1_000);
        assert!(diff(&previous, &next).is_empty());
    }

    #[test]
    fn seek_from_paused_uses_frozen_expectation() {
        let at = Instant::now();
        let mut previous = state(Some("music"), at);
        previous.playback = PlaybackStatus::Paused;
        previous.position_ms = 10_000;
        let mut next = previous.clone();
        next.sampled_at = at + std::time::Duration::from_secs(30);
        next.playback = PlaybackStatus::Playing;
        next.position_ms = 90_000;
        assert_eq!(
            diff(&previous, &next),
            vec![
                MediaEvent::Status(PlaybackStatus::Playing),
                MediaEvent::Seeked { pos_ms: 90_000 },
            ]
        );
    }

    #[test]
    fn paused_seek_is_detected() {
        let at = Instant::now();
        let mut previous = state(Some("music"), at);
        previous.playback = PlaybackStatus::Paused;
        previous.position_ms = 10_000;
        let mut next = previous.clone();
        next.sampled_at = at + std::time::Duration::from_secs(1);
        next.position_ms = 50_000;
        assert_eq!(
            diff(&previous, &next),
            vec![MediaEvent::Seeked { pos_ms: 50_000 }]
        );
    }

    #[test]
    fn pausing_after_a_seek_is_detected() {
        let at = Instant::now();
        let mut previous = state(Some("music"), at);
        previous.playback = PlaybackStatus::Playing;
        previous.position_ms = 10_000;
        let mut next = previous.clone();
        next.sampled_at = at + std::time::Duration::from_millis(500);
        next.playback = PlaybackStatus::Paused;
        next.position_ms = 50_000;
        assert_eq!(
            diff(&previous, &next),
            vec![
                MediaEvent::Status(PlaybackStatus::Paused),
                MediaEvent::Seeked { pos_ms: 50_000 },
            ]
        );
    }

    #[test]
    fn stopping_never_reports_seek() {
        let at = Instant::now();
        let mut previous = state(Some("music"), at);
        previous.playback = PlaybackStatus::Playing;
        previous.position_ms = 50_000;
        let mut next = previous.clone();
        next.playback = PlaybackStatus::Stopped;
        next.position_ms = 0;
        let events = diff(&previous, &next);
        assert!(matches!(
            events[0],
            MediaEvent::Status(PlaybackStatus::Stopped)
        ));
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn dropped_track_emits_no_track() {
        let at = Instant::now();
        let previous = state(Some("music"), at);
        let mut next = state(Some("music"), at);
        next.track = None;
        assert_eq!(diff(&previous, &next), vec![MediaEvent::NoTrack]);
    }
}

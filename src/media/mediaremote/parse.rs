//! Pure parsing for `media-control` JSON output.
//!
//! We always pass `--micros`, so times arrive as integer microseconds and
//! `timestampEpochMicros` gives the sample age directly. Unknown fields are
//! ignored so a minor adapter release cannot take the monitor down.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Deserialize;

use crate::core::clock::PlaybackStatus;
use crate::media::{BackendKind, PlayerKind, PlayerSnapshot, TrackMeta};

/// A timestamp older than this means `--micros` was ignored (seconds are
/// reported instead), so the sample age cannot be trusted.
const MAX_TRUSTED_AGE: Duration = Duration::from_secs(60);

#[derive(Debug, Default, Deserialize)]
pub struct Payload {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub artist: Option<String>,
    #[serde(default)]
    pub album: Option<String>,
    #[serde(default, rename = "bundleIdentifier")]
    pub bundle_identifier: Option<String>,
    #[serde(default, rename = "parentApplicationBundleIdentifier")]
    pub parent_bundle_identifier: Option<String>,
    #[serde(default)]
    pub playing: Option<bool>,
    #[serde(default, rename = "playbackRate")]
    pub playback_rate: Option<f64>,
    #[serde(default, rename = "elapsedTimeMicros")]
    pub elapsed_micros: Option<u64>,
    #[serde(default, rename = "durationMicros")]
    pub duration_micros: Option<u64>,
    #[serde(default, rename = "timestampEpochMicros")]
    pub timestamp_micros: Option<u64>,
    #[serde(default, rename = "uniqueIdentifier")]
    pub unique_identifier: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Frame {
    #[serde(default, rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    diff: bool,
    #[serde(default)]
    payload: Option<Payload>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Poll {
    pub player: PlayerSnapshot,
    pub track: TrackMeta,
    pub playback: PlaybackStatus,
    pub position_ms: u64,
    pub rate: f64,
    pub sample_age: Option<Duration>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Poll(Box<Poll>),
    NoPlayer,
    /// A diff frame (we request `--no-diff`) or unparsable JSON.
    Skip,
}

pub fn parse_line(line: &str) -> Outcome {
    let Ok(frame) = serde_json::from_str::<Frame>(line) else {
        return Outcome::Skip;
    };
    if frame.diff || frame.kind.as_deref().is_some_and(|kind| kind != "data") {
        return Outcome::Skip;
    }
    frame
        .payload
        .as_ref()
        .and_then(poll_from_payload)
        .map_or(Outcome::NoPlayer, |poll| Outcome::Poll(Box::new(poll)))
}

pub fn poll_from_payload(payload: &Payload) -> Option<Poll> {
    let bundle = payload
        .parent_bundle_identifier
        .as_deref()
        .or(payload.bundle_identifier.as_deref())?;
    let title = payload
        .title
        .as_deref()
        .filter(|title| !title.trim().is_empty())?;
    let id = player_id(bundle);
    let playback = if payload.playing.unwrap_or(false) {
        PlaybackStatus::Playing
    } else {
        PlaybackStatus::Paused
    };
    let rate = payload
        .playback_rate
        .filter(|rate| rate.is_finite() && *rate > 0.0)
        .unwrap_or(1.0);

    Some(Poll {
        player: PlayerSnapshot {
            kind: PlayerKind::from_id(&id),
            id,
            backend: BackendKind::MediaRemote,
        },
        track: TrackMeta {
            title: title.to_string(),
            artist: payload.artist.clone().unwrap_or_default(),
            album: payload
                .album
                .clone()
                .filter(|album| !album.trim().is_empty()),
            duration_ms: payload
                .duration_micros
                .filter(|duration| *duration > 0)
                .map(|duration| duration / 1_000),
            track_id: payload.unique_identifier.clone(),
            url: None,
        },
        playback,
        position_ms: payload.elapsed_micros.unwrap_or(0) / 1_000,
        rate,
        sample_age: sample_age(payload.timestamp_micros),
    })
}

fn sample_age(timestamp_micros: Option<u64>) -> Option<Duration> {
    let timestamp = timestamp_micros?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_micros() as u64;
    let age = Duration::from_micros(now.saturating_sub(timestamp));
    (age < MAX_TRUSTED_AGE).then_some(age)
}

/// Known bundle identifiers map to the short ids used elsewhere; anything
/// else falls back to its last dotted segment.
pub fn player_id(bundle: &str) -> String {
    let mapped = match bundle {
        "com.apple.Music" => "music",
        "com.spotify.client" => "spotify",
        "com.apple.Safari" | "com.apple.WebKit.GPU" => "safari",
        "com.google.Chrome" => "chrome",
        "com.microsoft.edgemac" => "msedge",
        "com.apple.podcasts" => "podcasts",
        _ => bundle.rsplit('.').next().unwrap_or(bundle),
    };
    mapped.to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(payload: &str, diff: bool) -> String {
        format!(r#"{{"type":"data","diff":{diff},"payload":{payload}}}"#)
    }

    fn full_payload() -> &'static str {
        r#"{
            "title": "Song",
            "artist": "Artist",
            "album": "Album",
            "bundleIdentifier": "com.spotify.client",
            "playing": true,
            "playbackRate": 1,
            "elapsedTimeMicros": 45000000,
            "durationMicros": 225000000,
            "timestampEpochMicros": 1700000000000000,
            "uniqueIdentifier": "abc"
        }"#
    }

    fn poll(payload: &str) -> Poll {
        match parse_line(&frame(payload, false)) {
            Outcome::Poll(poll) => *poll,
            other => panic!("expected poll, got {other:?}"),
        }
    }

    fn recent_payload() -> String {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_micros() as u64;
        full_payload().replace(
            "\"timestampEpochMicros\": 1700000000000000",
            &format!("\"timestampEpochMicros\": {now}"),
        )
    }

    #[test]
    fn parses_a_full_payload_in_microseconds() {
        let parsed = poll(&recent_payload());
        assert_eq!(parsed.player.id, "spotify");
        assert_eq!(parsed.player.kind, PlayerKind::Native);
        assert_eq!(parsed.player.backend, BackendKind::MediaRemote);
        assert_eq!(parsed.track.title, "Song");
        assert_eq!(parsed.track.artist, "Artist");
        assert_eq!(parsed.track.album.as_deref(), Some("Album"));
        assert_eq!(parsed.track.duration_ms, Some(225_000));
        assert_eq!(parsed.track.track_id.as_deref(), Some("abc"));
        assert_eq!(parsed.playback, PlaybackStatus::Playing);
        assert_eq!(parsed.position_ms, 45_000);
        assert_eq!(parsed.rate, 1.0);
        assert!(parsed.sample_age.is_some());
    }

    #[test]
    fn playing_false_maps_to_paused() {
        let parsed = poll(&full_payload().replace("\"playing\": true", "\"playing\": false"));
        assert_eq!(parsed.playback, PlaybackStatus::Paused);
    }

    #[test]
    fn playback_rate_is_kept_when_valid() {
        let parsed = poll(&full_payload().replace("\"playbackRate\": 1", "\"playbackRate\": 0.5"));
        assert_eq!(parsed.rate, 0.5);
        let paused = poll(&full_payload().replace("\"playbackRate\": 1", "\"playbackRate\": 0"));
        assert_eq!(paused.rate, 1.0);
    }

    #[test]
    fn empty_or_null_payload_means_no_player() {
        assert_eq!(parse_line(&frame("{}", false)), Outcome::NoPlayer);
        assert_eq!(parse_line(&frame("null", false)), Outcome::NoPlayer);
    }

    #[test]
    fn diff_frames_and_bad_json_are_skipped() {
        assert_eq!(parse_line(&frame(full_payload(), true)), Outcome::Skip);
        assert_eq!(parse_line("not json"), Outcome::Skip);
        assert_eq!(parse_line(""), Outcome::Skip);
        assert_eq!(
            parse_line(r#"{"type":"heartbeat","diff":false,"payload":null}"#),
            Outcome::Skip
        );
    }

    #[test]
    fn unknown_fields_are_tolerated() {
        let payload = full_payload().replace(
            "\"title\"",
            "\"brandNewField\": {\"nested\": [1, 2]}, \"title\"",
        );
        assert_eq!(poll(&payload).track.title, "Song");
    }

    #[test]
    fn parent_bundle_wins_and_unknown_bundles_fall_back() {
        let webkit = poll(
            r#"{"title":"Video","bundleIdentifier":"com.apple.WebKit.GPU","parentApplicationBundleIdentifier":"com.apple.Safari","playing":true}"#,
        );
        assert_eq!(webkit.player.id, "safari");
        assert_eq!(webkit.player.kind, PlayerKind::Browser);

        let unknown =
            poll(r#"{"title":"Song","bundleIdentifier":"com.example.NicePlayer","playing":true}"#);
        assert_eq!(unknown.player.id, "niceplayer");
        assert_eq!(unknown.player.kind, PlayerKind::Unknown);
    }

    #[test]
    fn missing_title_means_no_player() {
        assert_eq!(
            parse_line(&frame(
                r#"{"bundleIdentifier":"com.spotify.client","playing":true}"#,
                false
            )),
            Outcome::NoPlayer
        );
    }

    #[test]
    fn optional_fields_degrade_cleanly() {
        let parsed = poll(
            r#"{"title":"Song","bundleIdentifier":"com.apple.Music","playing":true,"album":"  ","durationMicros":0,"elapsedTimeMicros":0}"#,
        );
        assert_eq!(parsed.track.artist, "");
        assert_eq!(parsed.track.album, None);
        assert_eq!(parsed.track.duration_ms, None);
        assert_eq!(parsed.position_ms, 0);
        assert_eq!(parsed.sample_age, None);
        assert_eq!(parsed.player.id, "music");
    }

    #[test]
    fn untrusted_timestamps_drop_the_sample_age() {
        let ancient = poll(&full_payload().replace(
            "\"timestampEpochMicros\": 1700000000000000",
            "\"timestampEpochMicros\": 1700000000",
        ));
        assert_eq!(ancient.sample_age, None);

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_micros() as u64;
        let future = poll(&full_payload().replace(
            "\"timestampEpochMicros\": 1700000000000000",
            &format!("\"timestampEpochMicros\": {}", now + 5_000_000),
        ));
        assert_eq!(future.sample_age, Some(Duration::ZERO));
    }

    #[test]
    fn known_bundles_map_to_short_ids() {
        assert_eq!(player_id("com.apple.Music"), "music");
        assert_eq!(player_id("com.spotify.client"), "spotify");
        assert_eq!(player_id("com.google.Chrome"), "chrome");
        assert_eq!(player_id("com.microsoft.edgemac"), "msedge");
        assert_eq!(player_id("org.example.App"), "app");
        assert_eq!(player_id("plain"), "plain");
    }
}

//! Pure mapping helpers for the SMTC backend. Kept free of the `windows`
//! crate so their tests run on every platform.

use std::time::{Duration, SystemTime};

use crate::core::clock::PlaybackStatus;

/// Ticks between the WinRT epoch (1601-01-01) and the Unix epoch.
const TICKS_BETWEEN_1601_AND_EPOCH: i64 = 11_644_473_600_000_000;

const TICKS_PER_MS: i64 = 10_000;

/// Durations beyond this are treated as bogus metadata, mirroring the MPRIS backend.
const MAX_DURATION_MS: u64 = 3 * 60 * 60 * 1_000;

/// Raw SMTC playback status, mirroring
/// `GlobalSystemMediaTransportControlsSessionPlaybackStatus`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RawPlaybackStatus {
    Closed,
    Opened,
    Changing,
    Stopped,
    Playing,
    Paused,
}

impl RawPlaybackStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Closed => "closed",
            Self::Opened => "opened",
            Self::Changing => "changing",
            Self::Stopped => "stopped",
            Self::Playing => "playing",
            Self::Paused => "paused",
        }
    }
}

/// Map a raw SMTC status onto the app-level status. `Changing` yields `None`
/// so callers can hold their previous status instead of flickering.
pub fn map_status(status: RawPlaybackStatus) -> Option<PlaybackStatus> {
    match status {
        RawPlaybackStatus::Playing => Some(PlaybackStatus::Playing),
        RawPlaybackStatus::Paused => Some(PlaybackStatus::Paused),
        RawPlaybackStatus::Changing => None,
        RawPlaybackStatus::Stopped | RawPlaybackStatus::Opened | RawPlaybackStatus::Closed => {
            Some(PlaybackStatus::Stopped)
        }
    }
}

/// Selection rank among concurrent SMTC sessions; lower wins.
pub fn session_rank(status: RawPlaybackStatus) -> u8 {
    match status {
        RawPlaybackStatus::Playing => 0,
        RawPlaybackStatus::Paused => 1,
        RawPlaybackStatus::Closed
        | RawPlaybackStatus::Opened
        | RawPlaybackStatus::Changing
        | RawPlaybackStatus::Stopped => 2,
    }
}

/// Convert 100 ns ticks (the WinRT `TimeSpan`/`DateTime` unit) to whole
/// milliseconds, flooring; negative spans clamp to zero.
pub fn ticks_to_ms(ticks: i64) -> u64 {
    ticks.div_euclid(TICKS_PER_MS).max(0) as u64
}

/// Convert a WinRT `DateTime` (`UniversalTime`, 100 ns ticks since 1601) to a
/// Unix instant. `None` for zero, negative, or overflowing values.
pub fn universal_time_to_system_time(universal_time: i64) -> Option<SystemTime> {
    if universal_time <= 0 {
        return None;
    }
    let unix_ticks = universal_time.checked_sub(TICKS_BETWEEN_1601_AND_EPOCH)?;
    if unix_ticks < 0 {
        return None;
    }
    SystemTime::UNIX_EPOCH.checked_add(Duration::from_millis(ticks_to_ms(unix_ticks)))
}

/// Age of a media position sample given its `LastUpdatedTime` (100 ns ticks
/// since 1601). `None` when the timestamp is missing or invalid; future
/// timestamps (clock skew) report an age of zero.
pub fn sample_age(last_updated_time: i64, now: SystemTime) -> Option<Duration> {
    let updated = universal_time_to_system_time(last_updated_time)?;
    Some(now.duration_since(updated).unwrap_or(Duration::ZERO))
}

/// Track duration from the timeline span, capped like the MPRIS backend;
/// `None` when the span is empty or bogus.
pub fn timeline_duration_ms(start_ticks: i64, end_ticks: i64) -> Option<u64> {
    let span = end_ticks.checked_sub(start_ticks)?;
    (span > 0)
        .then(|| ticks_to_ms(span))
        .filter(|duration_ms| *duration_ms <= MAX_DURATION_MS)
}

/// Sanitize a playback rate reported by a player: non-finite or non-positive
/// values fall back to 1.0.
pub fn safe_rate(rate: f64) -> f64 {
    if rate.is_finite() && rate > 0.0 {
        rate
    } else {
        1.0
    }
}

/// Whether a session looks like the player never reports a timeline: active
/// playback with a zero position and no duration. Browsers only carry a
/// position when the website pushes `mediaSession.setPositionState`, so such
/// sessions offer no usable position at all.
pub fn timeline_is_unreported(
    status: RawPlaybackStatus,
    position_ms: u64,
    duration_ms: Option<u64>,
) -> bool {
    duration_ms.is_none()
        && position_ms == 0
        && matches!(
            status,
            RawPlaybackStatus::Playing | RawPlaybackStatus::Paused
        )
}

/// A SMTC session reduced to what session selection needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionCandidate {
    /// Raw AUMID, e.g. `SpotifyAB.SpotifyMusic_zpdnkdrmxy60w!SpotifyMusic`.
    pub id: String,
    pub status: RawPlaybackStatus,
    /// Whether this is the session `GetCurrentSession()` reports.
    pub current: bool,
}

/// Case-insensitive match against the raw AUMID or its short name, so
/// `--player spotify` matches both `Spotify.exe` and the packaged AUMID.
pub fn player_matches(aumid: &str, filter: &str) -> bool {
    aumid.eq_ignore_ascii_case(filter)
        || crate::media::player_short_name(aumid).eq_ignore_ascii_case(filter)
}

/// Pick the session to follow: Playing over Paused over everything else,
/// preferring the system's current session on ties, then the AUMID.
pub fn select_session<'a>(
    candidates: impl IntoIterator<Item = &'a SessionCandidate>,
    player_filter: Option<&str>,
) -> Option<&'a SessionCandidate> {
    candidates
        .into_iter()
        .filter(|candidate| {
            player_filter.is_none_or(|filter| player_matches(&candidate.id, filter))
        })
        .min_by(|left, right| {
            session_rank(left.status)
                .cmp(&session_rank(right.status))
                .then_with(|| usize::from(right.current).cmp(&usize::from(left.current)))
                .then_with(|| left.id.cmp(&right.id))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn candidate(id: &str, status: RawPlaybackStatus) -> SessionCandidate {
        SessionCandidate {
            id: id.to_string(),
            status,
            current: false,
        }
    }

    #[test]
    fn maps_statuses_onto_app_levels() {
        assert_eq!(
            map_status(RawPlaybackStatus::Playing),
            Some(PlaybackStatus::Playing)
        );
        assert_eq!(
            map_status(RawPlaybackStatus::Paused),
            Some(PlaybackStatus::Paused)
        );
        assert_eq!(
            map_status(RawPlaybackStatus::Stopped),
            Some(PlaybackStatus::Stopped)
        );
        assert_eq!(
            map_status(RawPlaybackStatus::Opened),
            Some(PlaybackStatus::Stopped)
        );
        assert_eq!(
            map_status(RawPlaybackStatus::Closed),
            Some(PlaybackStatus::Stopped)
        );
        assert_eq!(map_status(RawPlaybackStatus::Changing), None);
    }

    #[test]
    fn ranks_playing_over_paused_over_the_rest() {
        assert!(session_rank(RawPlaybackStatus::Playing) < session_rank(RawPlaybackStatus::Paused));
        assert!(session_rank(RawPlaybackStatus::Paused) < session_rank(RawPlaybackStatus::Stopped));
        assert_eq!(
            session_rank(RawPlaybackStatus::Changing),
            session_rank(RawPlaybackStatus::Closed)
        );
    }

    #[test]
    fn converts_ticks_to_whole_milliseconds() {
        assert_eq!(ticks_to_ms(10_000), 1);
        assert_eq!(ticks_to_ms(9_999), 0);
        assert_eq!(ticks_to_ms(25_000_000), 2_500);
        assert_eq!(ticks_to_ms(-1), 0);
        assert_eq!(ticks_to_ms(-25_000_000), 0);
    }

    #[test]
    fn converts_universal_time_across_the_1601_epoch() {
        let offset_ticks = TICKS_BETWEEN_1601_AND_EPOCH;
        assert_eq!(universal_time_to_system_time(0), None);
        assert_eq!(universal_time_to_system_time(-1), None);
        assert_eq!(universal_time_to_system_time(offset_ticks - 1), None);
        assert_eq!(
            universal_time_to_system_time(offset_ticks),
            Some(SystemTime::UNIX_EPOCH)
        );
        let five_seconds = offset_ticks + 5 * TICKS_PER_MS * 1_000;
        assert_eq!(
            universal_time_to_system_time(five_seconds),
            Some(SystemTime::UNIX_EPOCH + Duration::from_secs(5))
        );
    }

    #[test]
    fn measures_sample_age_from_last_updated_time() {
        let now = SystemTime::now();
        let unix_ms = now
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        let universal =
            |age_ms: i64| (unix_ms + age_ms) * TICKS_PER_MS + TICKS_BETWEEN_1601_AND_EPOCH;

        assert_eq!(sample_age(0, now), None);
        // `now` carries sub-millisecond precision the fixture loses, so the
        // zero-age case is only exact within a millisecond.
        let fresh = sample_age(universal(0), now).unwrap();
        assert!(fresh < Duration::from_millis(2), "{fresh:?}");

        let age = sample_age(universal(-5_000), now).unwrap();
        assert!(
            age >= Duration::from_secs(5) && age <= Duration::from_secs(6),
            "{age:?}"
        );

        // Future timestamp (clock skew) clamps to zero.
        assert_eq!(sample_age(universal(60_000), now), Some(Duration::ZERO));
    }

    #[test]
    fn derives_duration_from_the_timeline_span() {
        assert_eq!(timeline_duration_ms(0, 2_250_000_000), Some(225_000));
        assert_eq!(timeline_duration_ms(1_000, 1_000), None);
        assert_eq!(timeline_duration_ms(2_000, 1_000), None);
        // Longer than three hours is treated as bogus metadata.
        assert_eq!(
            timeline_duration_ms(0, (4 * 60 * 60 * 1_000) as i64 * TICKS_PER_MS),
            None
        );
    }

    #[test]
    fn falls_back_to_unit_rate_for_bogus_values() {
        assert_eq!(safe_rate(1.0), 1.0);
        assert_eq!(safe_rate(0.5), 0.5);
        assert_eq!(safe_rate(2.0), 2.0);
        assert_eq!(safe_rate(0.0), 1.0);
        assert_eq!(safe_rate(-1.0), 1.0);
        assert_eq!(safe_rate(f64::NAN), 1.0);
        assert_eq!(safe_rate(f64::INFINITY), 1.0);
    }

    #[test]
    fn flags_sessions_whose_timeline_is_never_reported() {
        assert!(timeline_is_unreported(RawPlaybackStatus::Playing, 0, None));
        assert!(timeline_is_unreported(RawPlaybackStatus::Paused, 0, None));
        // A real position or duration means the timeline works.
        assert!(!timeline_is_unreported(
            RawPlaybackStatus::Playing,
            40_000,
            None
        ));
        assert!(!timeline_is_unreported(
            RawPlaybackStatus::Playing,
            0,
            Some(200_000)
        ));
        // Idle sessions legitimately have an empty timeline.
        assert!(!timeline_is_unreported(RawPlaybackStatus::Stopped, 0, None));
    }

    #[test]
    fn selects_the_playing_session() {
        let candidates = [
            candidate("AppA_p1!App", RawPlaybackStatus::Paused),
            candidate("AppB_p2!App", RawPlaybackStatus::Playing),
        ];
        assert_eq!(select_session(&candidates, None).unwrap().id, "AppB_p2!App");
    }

    #[test]
    fn prefers_the_current_session_on_status_ties() {
        let mut paused_a = candidate("AppA_p1!App", RawPlaybackStatus::Paused);
        let mut paused_b = candidate("AppB_p2!App", RawPlaybackStatus::Paused);
        paused_b.current = true;
        let candidates = [paused_a.clone(), paused_b.clone()];
        assert_eq!(select_session(&candidates, None).unwrap().id, paused_b.id);

        // An explicit playing session still beats the current paused one.
        paused_a.status = RawPlaybackStatus::Playing;
        let candidates = [paused_a, paused_b];
        assert_eq!(select_session(&candidates, None).unwrap().id, "AppA_p1!App");
    }

    #[test]
    fn breaks_remaining_ties_by_aumid() {
        let candidates = [
            candidate("AppB_p2!App", RawPlaybackStatus::Playing),
            candidate("AppA_p1!App", RawPlaybackStatus::Playing),
        ];
        assert_eq!(select_session(&candidates, None).unwrap().id, "AppA_p1!App");
    }

    #[test]
    fn filters_by_aumid_and_short_name() {
        let candidates = [
            candidate("Spotify.exe", RawPlaybackStatus::Playing),
            candidate(
                "Microsoft.ZuneMusic_8wekyb3d8bbwe!Microsoft.ZuneMusic",
                RawPlaybackStatus::Paused,
            ),
        ];

        assert_eq!(
            select_session(&candidates, Some("spotify")).unwrap().id,
            "Spotify.exe"
        );
        assert_eq!(
            select_session(&candidates, Some("media-player"))
                .unwrap()
                .id,
            "Microsoft.ZuneMusic_8wekyb3d8bbwe!Microsoft.ZuneMusic"
        );
        assert_eq!(
            select_session(
                &candidates,
                Some("Microsoft.ZuneMusic_8wekyb3d8bbwe!Microsoft.ZuneMusic")
            )
            .unwrap()
            .id,
            "Microsoft.ZuneMusic_8wekyb3d8bbwe!Microsoft.ZuneMusic"
        );
        assert_eq!(select_session(&candidates, Some("vlc")), None);
    }
}

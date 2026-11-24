//! Pure parsing and classification for one-shot `osascript` output.

use std::time::Duration;

use crate::core::clock::PlaybackStatus;
use crate::media::composite::player_matches;
use crate::media::{BackendKind, PlayerKind, PlayerSnapshot, TrackMeta};

/// Field and row separators chosen over tab/newline so the AppleScript side
/// needs no sanitising pass; Rust strips any residual control characters.
pub const FIELD_SEP: char = '\u{1f}';
pub const ROW_SEP: char = '\u{1e}';
const FIELD_COUNT: usize = 9;

/// Spawning `osascript` is expensive, so playing polls are slower than
/// MPD/SMTC (500 ms) and `PlaybackClock` extrapolates the difference.
pub const PLAYING_POLL: Duration = Duration::from_millis(1_000);
pub const IDLE_POLL: Duration = Duration::from_millis(2_000);
/// After a TCC denial, reprobe this often to pick up a System Settings
/// change without ever re-triggering the consent dialog.
pub const DENIED_REPROBE: Duration = Duration::from_millis(30_000);

/// Durations above this raw value cannot be whole seconds for a music track
/// (one hour), so the pair is read as milliseconds. A podcast longer than an
/// hour reported in seconds would be misread; Spotify desktop reports ms.
const SECONDS_AMBIGUITY_CAP: u64 = 3_600;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayerId {
    Music,
    Spotify,
}

impl PlayerId {
    pub fn from_name(raw: &str) -> Option<Self> {
        match raw {
            "music" => Some(Self::Music),
            "spotify" => Some(Self::Spotify),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Music => "music",
            Self::Spotify => "spotify",
        }
    }

    pub fn snapshot(self, backend: BackendKind) -> PlayerSnapshot {
        PlayerSnapshot {
            id: self.as_str().to_string(),
            kind: PlayerKind::Native,
            backend,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowState {
    Playing,
    Paused,
    Stopped,
    /// The player's tell block failed; the raw Apple event code is kept for
    /// the TCC state machine.
    Error(i32),
}

impl RowState {
    pub fn status(self) -> Option<PlaybackStatus> {
        match self {
            Self::Playing => Some(PlaybackStatus::Playing),
            Self::Paused => Some(PlaybackStatus::Paused),
            Self::Stopped => Some(PlaybackStatus::Stopped),
            Self::Error(_) => None,
        }
    }

    fn rank(self) -> u8 {
        match self {
            Self::Playing => 0,
            Self::Paused => 1,
            Self::Stopped => 2,
            Self::Error(_) => 3,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ParsedRow {
    pub player: PlayerId,
    pub state: RowState,
    pub position_raw: Option<u64>,
    pub duration_raw: Option<u64>,
    pub duration_time: Option<String>,
    pub title: String,
    pub artist: String,
    pub album: Option<String>,
    pub track_id: Option<String>,
}

impl ParsedRow {
    /// Normalised `(position_ms, duration_ms)`.
    pub fn normalized(&self) -> (u64, Option<u64>) {
        normalize_ms(
            self.position_raw,
            self.duration_raw,
            self.duration_time.as_deref(),
        )
    }

    pub fn track(&self) -> Option<TrackMeta> {
        if self.title.is_empty() || self.artist.is_empty() {
            return None;
        }
        let (_, duration_ms) = self.normalized();
        Some(TrackMeta {
            title: self.title.clone(),
            artist: self.artist.clone(),
            album: self.album.clone(),
            duration_ms,
            track_id: self.track_id.clone(),
            url: None,
        })
    }
}

/// Parse a whole script output into valid rows, skipping malformed ones.
pub fn parse_output(raw: &str) -> Vec<ParsedRow> {
    raw.split(ROW_SEP)
        .filter(|line| !line.is_empty())
        .filter_map(|line| match parse_row(line) {
            Some(row) => Some(row),
            None => {
                tracing::warn!("applescript: discarding malformed player row");
                None
            }
        })
        .collect()
}

/// Fields: player, state, position_raw, duration_raw, duration_time, title,
/// artist, album, track_id. Error rows carry the Apple event code in the
/// position field and nothing else.
pub fn parse_row(line: &str) -> Option<ParsedRow> {
    let fields: Vec<&str> = line.splitn(FIELD_COUNT, FIELD_SEP).collect();
    if fields.len() != FIELD_COUNT {
        return None;
    }
    let fields: Vec<String> = fields
        .iter()
        .map(|field| field.chars().filter(|c| !c.is_control()).collect())
        .collect();
    let player = PlayerId::from_name(&fields[0])?;

    let state = match fields[1].as_str() {
        "playing" => RowState::Playing,
        "paused" => RowState::Paused,
        "stopped" => RowState::Stopped,
        "error" => RowState::Error(fields[2].parse::<i32>().ok()?),
        _ => return None,
    };

    if let RowState::Error(code) = state {
        return Some(ParsedRow {
            player,
            state: RowState::Error(code),
            position_raw: None,
            duration_raw: None,
            duration_time: None,
            title: String::new(),
            artist: String::new(),
            album: None,
            track_id: None,
        });
    }

    let position_raw = parse_count(&fields[2])?;
    let duration_raw = parse_count(&fields[3])?;
    let duration_time = (!fields[4].is_empty()).then_some(fields[4].clone());

    Some(ParsedRow {
        player,
        state,
        position_raw,
        duration_raw,
        duration_time,
        title: fields[5].clone(),
        artist: fields[6].clone(),
        album: (!fields[7].is_empty()).then_some(fields[7].clone()),
        track_id: (!fields[8].is_empty()).then_some(fields[8].clone()),
    })
}

/// Empty means absent; a non-empty value that is not a whole number makes the
/// whole row malformed.
fn parse_count(field: &str) -> Option<Option<u64>> {
    if field.is_empty() {
        return Some(None);
    }
    field.parse::<u64>().ok().map(Some)
}

/// Resolve the seconds-vs-milliseconds question. The explicit `MM:SS` string
/// from Music carries no unit doubt and wins over the raw duration. A raw
/// duration above [`SECONDS_AMBIGUITY_CAP`] marks the pair as milliseconds;
/// otherwise a position far beyond the duration-as-seconds marks a mixed
/// pair (position in ms only). With no duration reference at all, position
/// is read as seconds.
pub fn normalize_ms(
    position_raw: Option<u64>,
    duration_raw: Option<u64>,
    duration_time: Option<&str>,
) -> (u64, Option<u64>) {
    let time_ms = duration_time
        .and_then(parse_time_string)
        .filter(|ms| *ms > 0);
    let duration_ms = match time_ms {
        Some(ms) => Some(ms),
        None => duration_raw.filter(|raw| *raw > 0).map(|raw| {
            if raw > SECONDS_AMBIGUITY_CAP {
                raw
            } else {
                raw * 1000
            }
        }),
    };

    let position_ms = match position_raw {
        None => 0,
        Some(raw) => {
            let is_ms = match (duration_raw, time_ms) {
                (Some(d), _) if d > SECONDS_AMBIGUITY_CAP => true,
                (Some(d), _) if d > 0 => raw > d * 105 / 100,
                (None, Some(tms)) => raw > tms * 105 / 1000,
                _ => false,
            };
            if is_ms { raw } else { raw.saturating_mul(1000) }
        }
    };

    (position_ms, duration_ms)
}

/// Parse `MM:SS` or `H:MM:SS` into milliseconds.
pub fn parse_time_string(raw: &str) -> Option<u64> {
    let parts: Vec<u64> = raw
        .split(':')
        .map(|part| part.parse().ok())
        .collect::<Option<_>>()?;
    match parts.as_slice() {
        [minutes, seconds] => Some((minutes * 60 + seconds) * 1000),
        [hours, minutes, seconds] => Some((hours * 3600 + minutes * 60 + seconds) * 1000),
        _ => None,
    }
}

/// Pick the row the TUI should follow: best playback status wins, and the
/// stable script order (Music before Spotify) breaks ties. Error rows rank
/// last so a denied player never shadows a working one.
pub fn select_player<'a>(rows: &'a [ParsedRow], filter: Option<&str>) -> Option<&'a ParsedRow> {
    rows.iter()
        .filter(|row| player_matches(row.player.as_str(), filter))
        .min_by_key(|row| row.state.rank())
}

/// Poll interval for a selected row's state.
pub fn poll_interval(state: RowState) -> Duration {
    match state {
        RowState::Playing => PLAYING_POLL,
        RowState::Paused | RowState::Stopped | RowState::Error(_) => IDLE_POLL,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureClass {
    NotPermitted,
    ConsentRequired,
    TimedOut,
    TrackUnreadable,
    AppNotRunning,
    Other,
}

/// Classify an Apple event error number. Messages are localised, numbers are
/// not; text matching is only a fallback for stderr without a code.
pub fn classify_code(code: i32) -> FailureClass {
    match code {
        -1743 => FailureClass::NotPermitted,
        -1744 => FailureClass::ConsentRequired,
        -1712 => FailureClass::TimedOut,
        -1728 => FailureClass::TrackUnreadable,
        -600 | -10814 => FailureClass::AppNotRunning,
        _ => FailureClass::Other,
    }
}

pub fn classify_failure(stderr: &str) -> FailureClass {
    const CODES: &[(&str, FailureClass)] = &[
        ("(-1743)", FailureClass::NotPermitted),
        ("(-1744)", FailureClass::ConsentRequired),
        ("(-1712)", FailureClass::TimedOut),
        ("(-1728)", FailureClass::TrackUnreadable),
        ("(-600)", FailureClass::AppNotRunning),
        ("(-10814)", FailureClass::AppNotRunning),
    ];
    for (needle, class) in CODES {
        if stderr.contains(needle) {
            return *class;
        }
    }
    let lower = stderr.to_ascii_lowercase();
    if lower.contains("not authorized") || lower.contains("not allowed") {
        FailureClass::NotPermitted
    } else if lower.contains("timed out") {
        FailureClass::TimedOut
    } else {
        FailureClass::Other
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::{BackendKind, PlayerKind};

    const FS: char = FIELD_SEP;

    fn row(line: &str) -> ParsedRow {
        parse_row(line).expect("valid row")
    }

    fn music_playing(position: &str, duration: &str, time: &str) -> ParsedRow {
        row(&format!(
            "music{FS}playing{FS}{position}{FS}{duration}{FS}{time}{FS}Title{FS}Artist{FS}Album{FS}42"
        ))
    }

    #[test]
    fn parses_a_full_music_row() {
        let parsed = music_playing("45", "225", "3:45");
        assert_eq!(parsed.player, PlayerId::Music);
        assert_eq!(parsed.state, RowState::Playing);
        assert_eq!(parsed.position_raw, Some(45));
        assert_eq!(parsed.duration_raw, Some(225));
        assert_eq!(parsed.duration_time.as_deref(), Some("3:45"));
        assert_eq!(parsed.title, "Title");
        assert_eq!(parsed.track_id.as_deref(), Some("42"));
        let (position_ms, duration_ms) = parsed.normalized();
        assert_eq!(position_ms, 45_000);
        assert_eq!(duration_ms, Some(225_000));
    }

    #[test]
    fn music_stopped_row_has_empty_fields() {
        let parsed = row("music\x1fstopped\x1f\x1f\x1f\x1f\x1f\x1f\x1f\x1f");
        assert_eq!(parsed.state, RowState::Stopped);
        assert_eq!(parsed.position_raw, None);
        assert_eq!(parsed.track(), None);
    }

    #[test]
    fn music_duration_falls_back_to_time_string() {
        let parsed = music_playing("45", "", "3:45");
        let (position_ms, duration_ms) = parsed.normalized();
        assert_eq!(position_ms, 45_000);
        assert_eq!(duration_ms, Some(225_000));
    }

    #[test]
    fn long_time_string_parses_hours() {
        assert_eq!(parse_time_string("1:02:03"), Some(3_723_000));
        assert_eq!(parse_time_string("3:45"), Some(225_000));
        assert_eq!(parse_time_string("nope"), None);
        assert_eq!(parse_time_string("1:2"), Some(62_000));
        assert_eq!(parse_time_string("1:2:3:4"), None);
    }

    #[test]
    fn spotify_millisecond_pair_is_detected() {
        let parsed = row(&format!(
            "spotify{FS}playing{FS}45000{FS}225000{FS}{FS}Title{FS}Artist{FS}{FS}id1"
        ));
        let (position_ms, duration_ms) = parsed.normalized();
        assert_eq!(position_ms, 45_000);
        assert_eq!(duration_ms, Some(225_000));
    }

    #[test]
    fn spotify_second_pair_is_detected() {
        let parsed = row(&format!(
            "spotify{FS}playing{FS}45{FS}225{FS}{FS}Title{FS}Artist{FS}{FS}id1"
        ));
        let (position_ms, duration_ms) = parsed.normalized();
        assert_eq!(position_ms, 45_000);
        assert_eq!(duration_ms, Some(225_000));
    }

    #[test]
    fn mixed_pair_reads_position_as_milliseconds() {
        let (position_ms, duration_ms) = normalize_ms(Some(45_000), Some(225), None);
        assert_eq!(position_ms, 45_000);
        assert_eq!(duration_ms, Some(225_000));
    }

    #[test]
    fn position_without_duration_is_read_as_seconds() {
        let (position_ms, duration_ms) = normalize_ms(Some(45), None, None);
        assert_eq!(position_ms, 45_000);
        assert_eq!(duration_ms, None);
    }

    #[test]
    fn zero_duration_maps_to_none() {
        let parsed = music_playing("45", "0", "");
        assert_eq!(parsed.normalized(), (45_000, None));
    }

    #[test]
    fn control_characters_are_stripped_from_text() {
        let parsed = row(&format!(
            "music{FS}playing{FS}45{FS}225{FS}{FS}Ti\u{7}tle\u{1e}x{FS}Artist{FS}{FS}42"
        ));
        assert_eq!(parsed.title, "Titlex");
    }

    #[test]
    fn malformed_rows_are_rejected() {
        assert!(parse_row("music\x1fplaying").is_none());
        assert!(
            parse_row(&format!(
                "player{FS}playing{FS}1{FS}2{FS}{FS}t{FS}a{FS}{FS}i"
            ))
            .is_none()
        );
        assert!(parse_row(&format!("music{FS}weird{FS}1{FS}2{FS}{FS}t{FS}a{FS}{FS}i")).is_none());
        assert!(
            parse_row(&format!(
                "music{FS}error{FS}notanumber{FS}{FS}{FS}{FS}{FS}{FS}{FS}"
            ))
            .is_none()
        );
        assert!(
            parse_row(&format!(
                "music{FS}playing{FS}NaN{FS}225{FS}{FS}t{FS}a{FS}{FS}i"
            ))
            .is_none()
        );
    }

    #[test]
    fn error_rows_carry_the_apple_event_code() {
        let parsed = row(&format!(
            "music{FS}error{FS}-1743{FS}{FS}{FS}{FS}{FS}{FS}{FS}"
        ));
        assert_eq!(parsed.state, RowState::Error(-1743));
        assert_eq!(parsed.track(), None);
        assert_eq!(parsed.state.status(), None);
    }

    #[test]
    fn parse_output_skips_malformed_and_empty_rows() {
        let valid_music = format!("music{FS}playing{FS}45{FS}225{FS}{FS}A{FS}B{FS}{FS}1");
        let valid_spotify = format!("spotify{FS}playing{FS}10{FS}200{FS}{FS}C{FS}D{FS}{FS}2");
        let raw = format!("{valid_music}{ROW_SEP}bad{FS}row{ROW_SEP}{valid_spotify}");
        let rows = parse_output(&raw);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].player, PlayerId::Music);
        assert_eq!(rows[1].player, PlayerId::Spotify);
        assert!(parse_output("").is_empty());
    }

    #[test]
    fn selection_prefers_playing_then_script_order() {
        let music = music_playing("45", "225", "");
        let mut paused = music.clone();
        paused.state = RowState::Paused;
        let spotify = row(&format!(
            "spotify{FS}playing{FS}10{FS}200{FS}{FS}C{FS}D{FS}{FS}2"
        ));

        let rows = vec![paused, spotify.clone()];
        assert_eq!(
            select_player(&rows, None).unwrap().player,
            PlayerId::Spotify
        );

        let rows = vec![music.clone(), spotify.clone()];
        assert_eq!(select_player(&rows, None).unwrap().player, PlayerId::Music);

        let rows = vec![music.clone(), spotify];
        assert_eq!(
            select_player(&rows, Some("spot")).unwrap().player,
            PlayerId::Spotify
        );
        assert!(select_player(&rows, Some("mpv")).is_none());
    }

    #[test]
    fn intervals_follow_playback() {
        assert_eq!(poll_interval(RowState::Playing), PLAYING_POLL);
        assert_eq!(poll_interval(RowState::Paused), IDLE_POLL);
        assert_eq!(poll_interval(RowState::Stopped), IDLE_POLL);
        assert_eq!(poll_interval(RowState::Error(-1743)), IDLE_POLL);
    }

    #[test]
    fn error_codes_classify_by_number() {
        assert_eq!(classify_code(-1743), FailureClass::NotPermitted);
        assert_eq!(classify_code(-1744), FailureClass::ConsentRequired);
        assert_eq!(classify_code(-1712), FailureClass::TimedOut);
        assert_eq!(classify_code(-1728), FailureClass::TrackUnreadable);
        assert_eq!(classify_code(-600), FailureClass::AppNotRunning);
        assert_eq!(classify_code(-10814), FailureClass::AppNotRunning);
        assert_eq!(classify_code(-1), FailureClass::Other);
    }

    #[test]
    fn stderr_classifies_by_code_first_then_text() {
        let stderr =
            "execution error: Music got an error: Not authorized to send Apple events. (-1743)";
        assert_eq!(classify_failure(stderr), FailureClass::NotPermitted);
        assert_eq!(
            classify_failure("execution error: Timed out. (-1712)"),
            FailureClass::TimedOut
        );
        assert_eq!(
            classify_failure("Not authorized to send Apple events."),
            FailureClass::NotPermitted
        );
        assert_eq!(
            classify_failure("xerró una Temporització (-1712)"),
            FailureClass::TimedOut
        );
        assert_eq!(classify_failure("something broke"), FailureClass::Other);
    }

    #[test]
    fn player_snapshots_are_native() {
        let snapshot = PlayerId::Music.snapshot(BackendKind::AppleScript);
        assert_eq!(snapshot.id, "music");
        assert_eq!(snapshot.kind, PlayerKind::Native);
    }
}

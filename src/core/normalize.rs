use tracing::warn;

use crate::core::model::{
    Annotation, AnnotationKind, LineRole, LyricLine, PARSER_VERSION, SyncGranularity, SyncedTrack,
    WordToken, is_intra_word_join,
};
use crate::core::parse::{ParsedLine, ParsedTrack, ParsedWord};
use crate::core::text::{clamp_estimate, display_width, grapheme_count, trim_range};

const ANNOTATION_WINDOW_MS: u64 = 20;
const INSTRUMENTAL_GAP_MS: u64 = 8_000;
const MIN_LINE_MS: u64 = 2_000;
const MAX_LINE_MS: u64 = 6_000;
const MS_PER_GRAPHEME: u64 = 120;
const MIN_WORD_MS: u64 = 300;
const MAX_WORD_MS: u64 = 2_500;
const WORD_MS_PER_GRAPHEME: u64 = 150;
const INSTRUMENTAL_FALLBACK_MS: u64 = 5_000;
pub const MAX_LINES: usize = 2_000;
pub const MAX_WORDS: usize = 16_000;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NormalizeError {
    #[error("lyrics contain {0} lines, limit is {MAX_LINES}")]
    TooManyLines(usize),
    #[error("lyrics contain {0} word tokens, limit is {MAX_WORDS}")]
    TooManyWords(usize),
}

pub fn validate_parsed_limits(parsed: &ParsedTrack) -> Result<(), NormalizeError> {
    if parsed.lines.len() > MAX_LINES {
        return Err(NormalizeError::TooManyLines(parsed.lines.len()));
    }

    let words = parsed
        .lines
        .iter()
        .map(|line| line.text.split_whitespace().count().max(line.words.len()))
        .sum();

    if words > MAX_WORDS {
        return Err(NormalizeError::TooManyWords(words));
    }

    Ok(())
}

fn empty_track(
    source_provider: String,
    source_track_id: Option<String>,
    match_confidence: f32,
    intrinsic_offset_ms: i64,
    user_offset_ms: i64,
) -> SyncedTrack {
    SyncedTrack {
        source_provider,
        source_track_id,
        parser_version: PARSER_VERSION,
        is_instrumental: false,
        granularity: SyncGranularity::Unsynced,
        intrinsic_offset_ms,
        user_offset_ms,
        match_confidence,
        lines: Vec::new(),
    }
}

#[derive(Debug, Clone)]
struct MergedLine {
    start_ms: u64,
    end_ms: Option<u64>,
    text: String,
    words: Vec<ParsedWord>,
    annotations: Vec<Annotation>,
    explicit_last_word_end_ms: Option<u64>,
}

pub fn normalize_track(
    parsed: ParsedTrack,
    source_provider: impl Into<String>,
    source_track_id: Option<String>,
    match_confidence: f32,
    track_duration_ms: Option<u64>,
    user_offset_ms: i64,
) -> SyncedTrack {
    if let Err(error) = validate_parsed_limits(&parsed) {
        warn!(%error, "lyrics rejected by safety limits");
        return empty_track(
            source_provider.into(),
            source_track_id,
            match_confidence,
            parsed.intrinsic_offset_ms,
            user_offset_ms,
        );
    }

    normalize_track_unchecked(
        parsed,
        source_provider,
        source_track_id,
        match_confidence,
        track_duration_ms,
        user_offset_ms,
    )
}

pub fn normalize_track_checked(
    parsed: ParsedTrack,
    source_provider: impl Into<String>,
    source_track_id: Option<String>,
    match_confidence: f32,
    track_duration_ms: Option<u64>,
    user_offset_ms: i64,
) -> Result<SyncedTrack, NormalizeError> {
    validate_parsed_limits(&parsed)?;
    Ok(normalize_track_unchecked(
        parsed,
        source_provider,
        source_track_id,
        match_confidence,
        track_duration_ms,
        user_offset_ms,
    ))
}

fn normalize_track_unchecked(
    parsed: ParsedTrack,
    source_provider: impl Into<String>,
    source_track_id: Option<String>,
    match_confidence: f32,
    track_duration_ms: Option<u64>,
    user_offset_ms: i64,
) -> SyncedTrack {
    let duration = track_duration_ms.or(parsed.track_duration_ms);
    let source_provider = source_provider.into();

    if parsed.is_instrumental {
        return normalize_instrumental(
            source_provider,
            source_track_id,
            match_confidence,
            duration,
            user_offset_ms,
        );
    }

    let mut merged = merge_annotations(parsed.lines);
    merged.retain(|line| !line.text.trim().is_empty() || !line.words.is_empty());

    if merged.is_empty() {
        return SyncedTrack {
            source_provider,
            source_track_id,
            parser_version: PARSER_VERSION,
            is_instrumental: false,
            granularity: SyncGranularity::Unsynced,
            intrinsic_offset_ms: parsed.intrinsic_offset_ms,
            user_offset_ms,
            match_confidence,
            lines: Vec::new(),
        };
    }

    let mut lines = Vec::with_capacity(merged.len() + 2);

    for index in 0..merged.len() {
        let line = &merged[index];
        let next_start = merged.get(index + 1).map(|next| next.start_ms);
        let (mut end_ms, needs_break) = resolve_line_end(line, next_start, duration, line.end_ms);
        let words = resolve_words(line, end_ms);
        if let Some(last_word) = words.last() {
            end_ms = end_ms.max(last_word.end_ms);
        }

        lines.push(LyricLine {
            start_ms: line.start_ms,
            end_ms,
            role: LineRole::Primary,
            text: line.text.clone(),
            words,
            annotations: line.annotations.clone(),
        });

        if needs_break && let Some(next_start) = next_start {
            let break_start = end_ms.saturating_add(1);
            let break_end = next_start.saturating_sub(1);
            if break_start <= break_end {
                lines.push(instrumental_line(break_start, break_end));
            }
        }
    }

    let granularity = if lines.iter().any(|line| {
        line.words
            .windows(2)
            .any(|pair| is_intra_word_join(&line.text, &pair[0], &pair[1]))
    }) {
        SyncGranularity::Syllable
    } else if lines.iter().any(|line| !line.words.is_empty()) {
        SyncGranularity::Word
    } else {
        SyncGranularity::Line
    };

    SyncedTrack {
        source_provider,
        source_track_id,
        parser_version: PARSER_VERSION,
        is_instrumental: false,
        granularity,
        intrinsic_offset_ms: parsed.intrinsic_offset_ms,
        user_offset_ms,
        match_confidence,
        lines,
    }
}

pub fn normalize_plain(
    plain: &str,
    source_provider: impl Into<String>,
    source_track_id: Option<String>,
    match_confidence: f32,
    user_offset_ms: i64,
) -> SyncedTrack {
    let line_count = plain.lines().count();
    let word_count = plain.split_whitespace().count();

    if line_count > MAX_LINES {
        warn!(
            line_count,
            limit = MAX_LINES,
            "plain lyrics rejected by safety limits"
        );
        return empty_track(
            source_provider.into(),
            source_track_id,
            match_confidence,
            0,
            user_offset_ms,
        );
    }

    if word_count > MAX_WORDS {
        warn!(
            word_count,
            limit = MAX_WORDS,
            "plain lyrics rejected by safety limits"
        );
        return empty_track(
            source_provider.into(),
            source_track_id,
            match_confidence,
            0,
            user_offset_ms,
        );
    }

    let lines = plain
        .lines()
        .filter_map(|line| {
            let text = line.trim_end();
            if text.trim().is_empty() {
                None
            } else {
                Some(LyricLine {
                    start_ms: 0,
                    end_ms: 0,
                    role: LineRole::Primary,
                    text: text.to_string(),
                    words: Vec::new(),
                    annotations: Vec::new(),
                })
            }
        })
        .collect();

    SyncedTrack {
        source_provider: source_provider.into(),
        source_track_id,
        parser_version: PARSER_VERSION,
        is_instrumental: false,
        granularity: SyncGranularity::Unsynced,
        intrinsic_offset_ms: 0,
        user_offset_ms,
        match_confidence,
        lines,
    }
}

pub fn normalize_instrumental(
    source_provider: impl Into<String>,
    source_track_id: Option<String>,
    match_confidence: f32,
    duration_ms: Option<u64>,
    user_offset_ms: i64,
) -> SyncedTrack {
    let end_ms = duration_ms.unwrap_or(INSTRUMENTAL_FALLBACK_MS).max(1_000);

    SyncedTrack {
        source_provider: source_provider.into(),
        source_track_id,
        parser_version: PARSER_VERSION,
        is_instrumental: true,
        granularity: SyncGranularity::Line,
        intrinsic_offset_ms: 0,
        user_offset_ms,
        match_confidence,
        lines: vec![instrumental_line(0, end_ms)],
    }
}

fn instrumental_line(start_ms: u64, end_ms: u64) -> LyricLine {
    LyricLine {
        start_ms,
        end_ms,
        role: LineRole::InstrumentalBreak,
        text: "[Instrumental]".to_string(),
        words: Vec::new(),
        annotations: Vec::new(),
    }
}

fn merge_annotations(mut lines: Vec<ParsedLine>) -> Vec<MergedLine> {
    lines.sort_by_key(|line| {
        (
            line.start_ms,
            line.text.trim().is_empty() && line.words.is_empty(),
        )
    });

    let mut merged = Vec::with_capacity(lines.len());
    let mut index = 0usize;

    while index < lines.len() {
        let current = &lines[index];

        if current.text.trim().is_empty() && current.words.is_empty() {
            index += 1;
            continue;
        }

        let mut end_ms = current.end_ms;
        let mut annotations = Vec::new();
        let mut next_index = index + 1;

        while next_index < lines.len()
            && lines[next_index].start_ms.saturating_sub(current.start_ms) <= ANNOTATION_WINDOW_MS
        {
            let next = &lines[next_index];

            if next.text.trim().is_empty() && next.words.is_empty() {
                if end_ms.is_none() && next.start_ms > current.start_ms {
                    end_ms = Some(next.start_ms);
                }
            } else if !next.text.trim().is_empty() {
                annotations.push(Annotation {
                    kind: annotation_kind(&current.text, &next.text),
                    text: next.text.trim().to_string(),
                });
            }

            next_index += 1;
        }

        while next_index < lines.len()
            && lines[next_index].text.trim().is_empty()
            && lines[next_index].words.is_empty()
        {
            if end_ms.is_none() && lines[next_index].start_ms > current.start_ms {
                end_ms = Some(lines[next_index].start_ms);
            }
            next_index += 1;
        }

        merged.push(MergedLine {
            start_ms: current.start_ms,
            end_ms,
            text: current.text.clone(),
            words: current.words.clone(),
            annotations,
            explicit_last_word_end_ms: current.explicit_last_word_end_ms,
        });

        index = next_index;
    }

    merged
}

fn resolve_line_end(
    line: &MergedLine,
    next_start: Option<u64>,
    duration_ms: Option<u64>,
    provided_end: Option<u64>,
) -> (u64, bool) {
    let max_end = next_start.map_or(u64::MAX, |next| next.saturating_sub(1).max(line.start_ms));

    if let Some(end_ms) = provided_end {
        let end_ms = end_ms.clamp(line.start_ms, max_end);
        let needs_break =
            next_start.is_some_and(|next| next > end_ms.saturating_add(INSTRUMENTAL_GAP_MS));
        return (end_ms, needs_break);
    }

    let word_hint = word_end_hint(line).unwrap_or(line.start_ms);

    match next_start {
        Some(next) if next.saturating_sub(line.start_ms) > INSTRUMENTAL_GAP_MS => {
            let estimated = line.start_ms.saturating_add(clamp_estimate(
                grapheme_count(&line.text),
                MS_PER_GRAPHEME,
                MIN_LINE_MS,
                MAX_LINE_MS,
            ));
            let end_ms = estimated.max(word_hint).clamp(line.start_ms, max_end);
            let needs_break = next > end_ms.saturating_add(INSTRUMENTAL_GAP_MS);
            (end_ms, needs_break)
        }
        Some(_) => (max_end, false),
        None => {
            let end_ms = duration_ms
                .unwrap_or(line.start_ms.saturating_add(INSTRUMENTAL_FALLBACK_MS))
                .max(line.start_ms.saturating_add(1_000))
                .max(word_hint);
            (end_ms, false)
        }
    }
}

fn word_end_hint(line: &MergedLine) -> Option<u64> {
    let mut hint = line.explicit_last_word_end_ms;
    for (index, word) in line.words.iter().enumerate() {
        let word_end = if let Some(end_ms) = word.end_ms {
            end_ms
        } else if index + 1 < line.words.len() {
            line.words[index + 1].start_ms
        } else if let Some(explicit_end_ms) = line.explicit_last_word_end_ms {
            explicit_end_ms
        } else {
            let (start, end) = word.byte_range;
            let graphemes = if start < end
                && end <= line.text.len()
                && line.text.is_char_boundary(start)
                && line.text.is_char_boundary(end)
            {
                let (trimmed_start, trimmed_end) = trim_range(&line.text, start, end);
                grapheme_count(&line.text[trimmed_start..trimmed_end])
            } else {
                0
            };
            word.start_ms.saturating_add(clamp_estimate(
                graphemes,
                WORD_MS_PER_GRAPHEME,
                MIN_WORD_MS,
                MAX_WORD_MS,
            ))
        };
        hint = Some(hint.map_or(word_end, |current| current.max(word_end)));
    }
    hint
}

fn resolve_words(line: &MergedLine, line_end_ms: u64) -> Vec<WordToken> {
    if line.words.is_empty() {
        return Vec::new();
    }

    let valid_words: Vec<(usize, usize, &ParsedWord)> = line
        .words
        .iter()
        .filter_map(|word| {
            let (start, end) = word.byte_range;
            if start >= end
                || end > line.text.len()
                || !line.text.is_char_boundary(start)
                || !line.text.is_char_boundary(end)
            {
                warn!(start, end, "skipping invalid word byte range");
                return None;
            }
            let (trimmed_start, trimmed_end) = trim_range(&line.text, start, end);
            (trimmed_start < trimmed_end).then_some((trimmed_start, trimmed_end, word))
        })
        .collect();

    let mut tokens = Vec::with_capacity(valid_words.len());
    let mut cursor_ms = line.start_ms;

    for (index, &(start, end, word)) in valid_words.iter().enumerate() {
        let word_start = word
            .start_ms
            .max(cursor_ms)
            .clamp(line.start_ms, line_end_ms);
        let word_end = if let Some(end_ms) = word.end_ms {
            end_ms.max(word_start)
        } else if let Some(&(_, _, next)) = valid_words.get(index + 1) {
            next.start_ms.max(word_start)
        } else if let Some(explicit_end_ms) = line.explicit_last_word_end_ms {
            explicit_end_ms.max(word_start)
        } else if line.end_ms.is_some() {
            line_end_ms
        } else {
            word_start
                .saturating_add(clamp_estimate(
                    grapheme_count(&line.text[start..end]),
                    WORD_MS_PER_GRAPHEME,
                    MIN_WORD_MS,
                    MAX_WORD_MS,
                ))
                .min(line_end_ms)
        };
        let max_word_end = valid_words
            .get(index + 1)
            .map_or(line_end_ms, |&(_, _, next)| {
                next.start_ms.max(word_start).min(line_end_ms)
            });
        let word_end = word_end.clamp(word_start, max_word_end);
        cursor_ms = word_end;

        tokens.push(WordToken {
            byte_range: (start, end),
            start_ms: word_start,
            end_ms: word_end,
            display_width: display_width(&line.text[start..end]),
        });
    }

    tokens
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScriptClass {
    Cjk,
    Latin,
    Other,
}

fn annotation_kind(current: &str, next: &str) -> AnnotationKind {
    if has_non_latin(current) && is_ascii_latin(next) {
        AnnotationKind::Romanization
    } else if script_class(current) == script_class(next) {
        AnnotationKind::Secondary
    } else {
        AnnotationKind::Translation
    }
}

fn script_class(text: &str) -> ScriptClass {
    if has_non_latin(text) {
        ScriptClass::Cjk
    } else if text
        .chars()
        .any(|character| character.is_ascii_alphabetic())
    {
        ScriptClass::Latin
    } else {
        ScriptClass::Other
    }
}

fn is_ascii_latin(text: &str) -> bool {
    !text.is_empty()
        && text.is_ascii()
        && text
            .chars()
            .any(|character| character.is_ascii_alphabetic())
}

fn has_non_latin(text: &str) -> bool {
    text.chars().any(|character| {
        matches!(
            character as u32,
            0x3040..=0x30FF
                | 0x3400..=0x4DBF
                | 0x4E00..=0x9FFF
                | 0xAC00..=0xD7AF
                | 0x1100..=0x11FF
                | 0x20000..=0x2FA1F
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed_line(start_ms: u64, text: &str) -> ParsedLine {
        ParsedLine {
            start_ms,
            end_ms: None,
            text: text.to_string(),
            words: Vec::new(),
            explicit_last_word_end_ms: None,
        }
    }

    fn normalize_lines(lines: Vec<ParsedLine>, duration: Option<u64>) -> SyncedTrack {
        normalize_track(
            ParsedTrack {
                lines,
                ..ParsedTrack::default()
            },
            "fixture",
            None,
            1.0,
            duration,
            0,
        )
    }

    #[test]
    fn rejects_tracks_over_line_limit() {
        let lines = (0..=MAX_LINES)
            .map(|index| parsed_line(index as u64 * 1_000, "line"))
            .collect();
        let parsed = ParsedTrack {
            lines,
            ..ParsedTrack::default()
        };

        assert!(matches!(
            validate_parsed_limits(&parsed),
            Err(NormalizeError::TooManyLines(count)) if count == MAX_LINES + 1
        ));
        let track = normalize_track(parsed, "fixture", None, 1.0, None, 0);
        assert!(track.lines.is_empty());
    }

    #[test]
    fn rejects_tracks_over_word_limit() {
        let text = vec!["word"; 81].join(" ");
        let lines = (0..200)
            .map(|index| parsed_line(index as u64 * 1_000, &text))
            .collect();
        let parsed = ParsedTrack {
            lines,
            ..ParsedTrack::default()
        };

        assert!(matches!(
            validate_parsed_limits(&parsed),
            Err(NormalizeError::TooManyWords(count)) if count == 16_200
        ));
    }

    #[test]
    fn rejects_plain_lyrics_over_word_limit() {
        let plain = vec!["word"; MAX_WORDS + 1].join(" ");
        let track = normalize_plain(&plain, "fixture", None, 1.0, 0);

        assert!(track.lines.is_empty());
    }

    #[test]
    fn merges_same_time_translations_and_empty_markers() {
        let track = normalize_lines(
            vec![
                parsed_line(1_000, "夜に駆ける"),
                parsed_line(1_001, "Yoru ni kakeru"),
                parsed_line(1_002, "Racing into the night"),
                parsed_line(1_003, ""),
                parsed_line(3_000, "Next"),
            ],
            Some(10_000),
        );

        assert_eq!(track.lines.len(), 2);
        assert_eq!(track.lines[0].end_ms, 1_003);
        assert_eq!(track.lines[0].annotations.len(), 2);
        assert_eq!(
            track.lines[0].annotations[0].kind,
            AnnotationKind::Romanization
        );
        assert_eq!(track.lines[1].start_ms, 3_000);
    }

    #[test]
    fn inserts_instrumental_break_for_large_gap() {
        let track = normalize_lines(
            vec![parsed_line(1_000, "Before"), parsed_line(20_000, "After")],
            Some(30_000),
        );

        assert_eq!(track.lines.len(), 3);
        assert_eq!(track.lines[1].role, LineRole::InstrumentalBreak);
        assert_eq!(track.lines[1].start_ms, track.lines[0].end_ms + 1);
        assert_eq!(track.lines[1].end_ms, 19_999);
    }

    #[test]
    fn uses_next_start_minus_one_for_small_gap() {
        let track = normalize_lines(
            vec![parsed_line(1_000, "One"), parsed_line(2_000, "Two")],
            Some(10_000),
        );

        assert_eq!(track.lines[0].end_ms, 1_999);
        assert_eq!(track.lines[1].end_ms, 10_000);
    }

    #[test]
    fn classifies_translation_and_secondary_annotations() {
        let translated = normalize_lines(
            vec![
                parsed_line(1_000, "Hello"),
                parsed_line(1_001, "こんにちは"),
            ],
            Some(10_000),
        );
        assert_eq!(
            translated.lines[0].annotations[0].kind,
            AnnotationKind::Translation
        );

        let secondary = normalize_lines(
            vec![parsed_line(1_000, "Hello"), parsed_line(1_001, "Hola")],
            Some(10_000),
        );
        assert_eq!(
            secondary.lines[0].annotations[0].kind,
            AnnotationKind::Secondary
        );
    }

    #[test]
    fn resolves_word_ends_from_explicit_line_and_estimate() {
        let line = ParsedLine {
            start_ms: 1_000,
            end_ms: None,
            text: "ab cd".to_string(),
            words: vec![
                ParsedWord {
                    byte_range: (0, 3),
                    start_ms: 1_000,
                    end_ms: None,
                },
                ParsedWord {
                    byte_range: (3, 5),
                    start_ms: 1_500,
                    end_ms: None,
                },
            ],
            explicit_last_word_end_ms: None,
        };
        let track = normalize_lines(vec![line], Some(5_000));

        assert_eq!(track.granularity, SyncGranularity::Word);
        assert_eq!(track.lines[0].words[0].end_ms, 1_500);
        assert_eq!(track.lines[0].words[1].end_ms, 1_800);
    }

    #[test]
    fn enforces_word_monotonicity_and_preserves_elrc_end_before_break() {
        let line = ParsedLine {
            start_ms: 1_000,
            end_ms: None,
            text: "hi there".to_string(),
            words: vec![
                ParsedWord {
                    byte_range: (0, 3),
                    start_ms: 1_200,
                    end_ms: Some(2_500),
                },
                ParsedWord {
                    byte_range: (3, 8),
                    start_ms: 1_100,
                    end_ms: None,
                },
            ],
            explicit_last_word_end_ms: Some(5_500),
        };
        let track = normalize_lines(vec![line, parsed_line(20_000, "After break")], Some(30_000));

        assert_eq!(track.lines[0].end_ms, 5_500);
        assert_eq!(track.lines[0].words[0].start_ms, 1_200);
        assert_eq!(track.lines[0].words[0].end_ms, 1_200);
        assert_eq!(track.lines[0].words[1].start_ms, 1_200);
        assert_eq!(track.lines[0].words[1].end_ms, 5_500);
        assert_eq!(track.lines[1].role, LineRole::InstrumentalBreak);
        assert_eq!(track.lines[1].start_ms, 5_501);
    }
}

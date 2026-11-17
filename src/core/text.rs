use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::core::model::WordToken;

const CJK_CHAR_WEIGHT: f64 = 1.0;
const ALPHABETIC_WEIGHT_PER_GRAPHEME: f64 = 0.35;
const MIN_WORD_WEIGHT: f64 = 1.0;
const COMMA_PAUSE_WEIGHT: f64 = 0.5;
const PERIOD_PAUSE_WEIGHT: f64 = 0.9;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WrappedText {
    pub text: String,
    pub display_width: u16,
    pub source_range: (usize, usize),
}

pub fn display_width(text: &str) -> u16 {
    display_width_with_ambiguous(text, 1)
}

pub fn display_width_with_ambiguous(text: &str, ambiguous_width: u8) -> u16 {
    let width = if ambiguous_width == 2 {
        UnicodeWidthStr::width_cjk(text)
    } else {
        UnicodeWidthStr::width(text)
    };
    u16::try_from(width).unwrap_or(u16::MAX)
}

pub fn grapheme_count(text: &str) -> usize {
    text.graphemes(true).count()
}

pub fn is_cjk(ch: char) -> bool {
    matches!(
        ch as u32,
        0x2E80..=0x2EFF
            | 0x3000..=0x303F
            | 0x3040..=0x30FF
            | 0x31C0..=0x31FF
            | 0x3400..=0x4DBF
            | 0x4E00..=0x9FFF
            | 0xAC00..=0xD7AF
            | 0xF900..=0xFAFF
            | 0xFE30..=0xFE4F
            | 0xFF00..=0xFFEF
    )
}

pub fn cannot_start_line(ch: char) -> bool {
    matches!(
        ch,
        '\u{3041}'
            | '\u{3043}'
            | '\u{3045}'
            | '\u{3047}'
            | '\u{3049}'
            | '\u{3083}'
            | '\u{3085}'
            | '\u{3087}'
            | '\u{308E}'
            | '\u{3095}'
            | '\u{3096}'
            | '\u{30A1}'
            | '\u{30A3}'
            | '\u{30A5}'
            | '\u{30A7}'
            | '\u{30A9}'
            | '\u{30C3}'
            | '\u{30E3}'
            | '\u{30E5}'
            | '\u{30E7}'
            | '\u{30EE}'
            | '\u{30F5}'
            | '\u{30F6}'
            | '\u{31F0}'..='\u{31FF}'
            | '\u{FF67}'..='\u{FF70}'
            | ')'
            | ']'
            | '}'
            | '>'
            | ','
            | '.'
            | ';'
            | ':'
            | '!'
            | '?'
            | '、'
            | '。'
            | '，'
            | '．'
            | '！'
            | '？'
            | '：'
            | '；'
            | '」'
            | '』'
            | '】'
            | '〉'
            | '》'
            | '〕'
            | '〗'
            | '〙'
            | '〛'
            | '）'
            | '］'
            | '｝'
            | '＞'
            | 'ー'
            | '々'
            | '〻'
            | 'ゝ'
            | 'ゞ'
            | 'ヽ'
            | 'ヾ'
    )
}

pub fn cannot_end_line(ch: char) -> bool {
    matches!(
        ch,
        '(' | '['
            | '{'
            | '<'
            | '（'
            | '［'
            | '｛'
            | '〈'
            | '《'
            | '「'
            | '『'
            | '【'
            | '〘'
            | '〚'
            | '〖'
            | '〝'
    )
}

pub fn clamp_estimate(graphemes: usize, per_grapheme: u64, min: u64, max: u64) -> u64 {
    let estimate = (graphemes as u64).saturating_mul(per_grapheme);
    estimate.clamp(min, max)
}

pub fn trim_range(text: &str, start: usize, end: usize) -> (usize, usize) {
    let start = start.min(text.len());
    let end = end.min(text.len()).max(start);
    let slice = &text[start..end];
    let trimmed = slice.trim();
    let leading = slice.len() - slice.trim_start().len();
    let trimmed_start = start + leading;
    let trimmed_end = trimmed_start + trimmed.len();
    (trimmed_start, trimmed_end)
}

pub fn truncate_to_width(text: &str, max_width: u16) -> &str {
    truncate_to_width_with_ambiguous(text, max_width, 1)
}

pub fn truncate_to_width_with_ambiguous(text: &str, max_width: u16, ambiguous_width: u8) -> &str {
    if max_width == 0 {
        return "";
    }
    let mut width = 0usize;
    for (index, grapheme) in text.grapheme_indices(true) {
        let grapheme_width = usize::from(display_width_with_ambiguous(grapheme, ambiguous_width));
        if width + grapheme_width > usize::from(max_width) {
            return &text[..index];
        }
        width += grapheme_width;
    }
    text
}

pub fn wrap_text(text: &str, max_width: u16) -> Vec<WrappedText> {
    wrap_text_with_ambiguous(text, max_width, 1)
}

pub fn wrap_text_with_ambiguous(
    text: &str,
    max_width: u16,
    ambiguous_width: u8,
) -> Vec<WrappedText> {
    let max_width = usize::from(max_width.max(1));
    if text.is_empty() {
        return vec![WrappedText {
            text: String::new(),
            display_width: 0,
            source_range: (0, 0),
        }];
    }

    let mut wrapped = Vec::new();
    let mut start = 0usize;

    while start < text.len() {
        while start < text.len() {
            let grapheme = text[start..].graphemes(true).next().unwrap_or("");
            if grapheme.is_empty() || !grapheme.chars().all(char::is_whitespace) {
                break;
            }
            start += grapheme.len();
        }
        if start >= text.len() {
            break;
        }

        let mut line_end = start;
        let mut width = 0usize;
        let mut last_space_end = None;

        for (offset, grapheme) in text[start..].grapheme_indices(true) {
            let grapheme_start = start + offset;
            let grapheme_width =
                usize::from(display_width_with_ambiguous(grapheme, ambiguous_width));
            if width > 0 && width + grapheme_width > max_width {
                break;
            }
            width += grapheme_width;
            line_end = grapheme_start + grapheme.len();
            if grapheme.chars().all(char::is_whitespace) {
                last_space_end = Some(line_end);
            }
        }

        let next_is_whitespace = text[line_end..]
            .graphemes(true)
            .next()
            .is_some_and(|grapheme| grapheme.chars().all(char::is_whitespace));

        if line_end == start {
            let grapheme = text[start..].graphemes(true).next().unwrap_or("");
            line_end = start + grapheme.len();
        } else if line_end < text.len()
            && !next_is_whitespace
            && let Some(space_end) = last_space_end
            && space_end > start
            && space_end < line_end
        {
            line_end = space_end;
        }

        let (trimmed_start, trimmed_end) = trim_range(text, start, line_end);
        if trimmed_start < trimmed_end {
            wrapped.push(WrappedText {
                text: text[trimmed_start..trimmed_end].to_string(),
                display_width: display_width_with_ambiguous(
                    &text[trimmed_start..trimmed_end],
                    ambiguous_width,
                ),
                source_range: (trimmed_start, trimmed_end),
            });
        }

        start = line_end;
    }

    if wrapped.is_empty() {
        wrapped.push(WrappedText {
            text: String::new(),
            display_width: 0,
            source_range: (0, 0),
        });
    }

    wrapped
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SyntheticTokenKind {
    Cjk,
    General,
}

#[derive(Debug, Clone, Copy)]
struct SyntheticRawToken {
    start: usize,
    end: usize,
    kind: SyntheticTokenKind,
    graphemes: usize,
    pause_weight: f64,
    ended_with_pause: bool,
}

impl SyntheticRawToken {
    fn speak_weight(self) -> f64 {
        match self.kind {
            SyntheticTokenKind::Cjk => CJK_CHAR_WEIGHT,
            SyntheticTokenKind::General => {
                (self.graphemes as f64 * ALPHABETIC_WEIGHT_PER_GRAPHEME).max(MIN_WORD_WEIGHT)
            }
        }
    }
}

fn is_cjk_script_char(ch: char) -> bool {
    is_cjk(ch)
        && !matches!(
            ch as u32,
            0x3000..=0x303F
                | 0xFF01..=0xFF0F
                | 0xFF1A..=0xFF20
                | 0xFF3B..=0xFF40
                | 0xFF5B..=0xFF65
        )
}

fn is_cjk_grapheme(grapheme: &str) -> bool {
    grapheme.chars().any(is_cjk_script_char)
}

fn pause_weight_for(grapheme: &str) -> f64 {
    if matches!(grapheme, "." | "。" | "．" | "!" | "！" | "?" | "？" | "…") {
        PERIOD_PAUSE_WEIGHT
    } else if matches!(grapheme, "," | "、" | "，" | ";" | "；" | ":" | "：") {
        COMMA_PAUSE_WEIGHT
    } else {
        0.0
    }
}

fn is_trailing_punct(grapheme: &str) -> bool {
    pause_weight_for(grapheme) > 0.0
        || matches!(
            grapheme,
            ")" | "]"
                | "}"
                | ">"
                | "\""
                | "”"
                | "’"
                | "）"
                | "］"
                | "｝"
                | "＞"
                | "」"
                | "』"
                | "】"
                | "〉"
                | "》"
                | "〕"
                | "〗"
                | "〙"
                | "〛"
        )
}

fn is_opening_punct(grapheme: &str) -> bool {
    grapheme.chars().all(cannot_end_line) || matches!(grapheme, "“" | "‘")
}

/// Deterministically estimate word timings over `[start_ms, end_ms]` for a line without word tags.
pub fn estimate_word_timings(line_text: &str, start_ms: u64, end_ms: u64) -> Vec<WordToken> {
    if line_text.is_empty() || end_ms < start_ms {
        return Vec::new();
    }

    let mut raw_tokens: Vec<SyntheticRawToken> = Vec::new();
    let mut open: Option<SyntheticRawToken> = None;
    let mut pending_prefix: Option<(usize, usize)> = None;

    for (byte_start, grapheme) in line_text.grapheme_indices(true) {
        let byte_end = byte_start + grapheme.len();

        if grapheme.chars().all(char::is_whitespace) {
            if let Some(token) = open.take() {
                raw_tokens.push(token);
            }
            if let Some((ps, pe)) = pending_prefix.take() {
                raw_tokens.push(SyntheticRawToken {
                    start: ps,
                    end: pe,
                    kind: SyntheticTokenKind::General,
                    graphemes: 1,
                    pause_weight: 0.0,
                    ended_with_pause: false,
                });
            }
            continue;
        }

        if let Some(current) = open.as_mut()
            && is_trailing_punct(grapheme)
        {
            current.end = byte_end;
            let pause = pause_weight_for(grapheme);
            if pause > 0.0 {
                current.pause_weight = current.pause_weight.max(pause);
                current.ended_with_pause = true;
            }
            continue;
        }

        if is_opening_punct(grapheme) || (open.is_none() && matches!(grapheme, "\"" | "'")) {
            if let Some(token) = open.take() {
                raw_tokens.push(token);
            }
            match &mut pending_prefix {
                Some((_, pe)) => *pe = byte_end,
                None => pending_prefix = Some((byte_start, byte_end)),
            }
            continue;
        }

        let grapheme_kind = if is_cjk_grapheme(grapheme) {
            SyntheticTokenKind::Cjk
        } else {
            SyntheticTokenKind::General
        };

        let should_flush = open.is_some_and(|current| {
            current.ended_with_pause
                || current.kind == SyntheticTokenKind::Cjk
                || grapheme_kind == SyntheticTokenKind::Cjk
        });
        if should_flush && let Some(token) = open.take() {
            raw_tokens.push(token);
        }

        if let Some(current) = open.as_mut() {
            current.end = byte_end;
            current.graphemes = current.graphemes.saturating_add(1);
        } else {
            let start = pending_prefix.take().map_or(byte_start, |(ps, _)| ps);
            open = Some(SyntheticRawToken {
                start,
                end: byte_end,
                kind: grapheme_kind,
                graphemes: 1,
                pause_weight: 0.0,
                ended_with_pause: false,
            });
        }
    }

    if let Some(token) = open.take() {
        raw_tokens.push(token);
    }
    if let Some((ps, pe)) = pending_prefix.take() {
        raw_tokens.push(SyntheticRawToken {
            start: ps,
            end: pe,
            kind: SyntheticTokenKind::General,
            graphemes: 1,
            pause_weight: 0.0,
            ended_with_pause: false,
        });
    }

    if raw_tokens.is_empty() {
        return Vec::new();
    }

    // Trailing pause at the end of a line keeps the final token active up to `end_ms`.
    if let Some(last) = raw_tokens.last_mut() {
        last.pause_weight = 0.0;
    }

    let total_weight: f64 = raw_tokens
        .iter()
        .map(|tok| tok.speak_weight() + tok.pause_weight)
        .sum();
    let total_ms = end_ms.saturating_sub(start_ms) as f64;

    let mut result = Vec::with_capacity(raw_tokens.len());
    let mut cum_weight = 0.0f64;
    let mut prev_end = start_ms;

    for (idx, token) in raw_tokens.iter().enumerate() {
        let start_fraction = if total_weight > 0.0 {
            (cum_weight / total_weight).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let mut tok_start = start_ms
            .saturating_add((start_fraction * total_ms).round() as u64)
            .clamp(prev_end, end_ms);
        if idx == 0 {
            tok_start = start_ms;
        }

        let speak_end_weight = cum_weight + token.speak_weight();
        let end_fraction = if total_weight > 0.0 {
            (speak_end_weight / total_weight).clamp(0.0, 1.0)
        } else {
            1.0
        };
        let mut tok_end = if idx + 1 == raw_tokens.len() && token.pause_weight == 0.0 {
            end_ms
        } else {
            start_ms
                .saturating_add((end_fraction * total_ms).round() as u64)
                .clamp(tok_start, end_ms)
        };
        if tok_end < tok_start {
            tok_end = tok_start;
        }

        result.push(WordToken {
            byte_range: (token.start, token.end),
            start_ms: tok_start,
            end_ms: tok_end,
            display_width: display_width(&line_text[token.start..token.end]),
        });

        cum_weight = speak_end_weight + token.pause_weight;
        prev_end = tok_end;
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn measures_wide_graphemes() {
        assert_eq!(display_width("abc"), 3);
        assert_eq!(display_width("世界"), 4);
        assert_eq!(grapheme_count("👩🏽‍💻"), 1);
    }

    #[test]
    fn measures_ambiguous_graphemes_by_configuration() {
        assert_eq!(display_width_with_ambiguous("·", 1), 1);
        assert_eq!(display_width_with_ambiguous("·", 2), 2);
        assert_eq!(display_width_with_ambiguous("世界", 2), 4);
    }

    #[test]
    fn wraps_ambiguous_text_by_configuration() {
        let narrow = wrap_text_with_ambiguous("··", 2, 1);
        assert_eq!(narrow.len(), 1);
        assert_eq!(narrow[0].display_width, 2);

        let wide = wrap_text_with_ambiguous("··", 2, 2);
        assert_eq!(wide.len(), 2);
        assert_eq!(wide[0].display_width, 2);
    }

    #[test]
    fn truncates_ambiguous_text_by_configuration() {
        assert_eq!(truncate_to_width_with_ambiguous("····", 2, 1), "··");
        assert_eq!(truncate_to_width_with_ambiguous("····", 2, 2), "·");
    }

    #[test]
    fn wraps_without_splitting_graphemes() {
        let wrapped = wrap_text("hello world", 5);
        assert_eq!(
            wrapped
                .iter()
                .map(|segment| segment.text.as_str())
                .collect::<Vec<_>>(),
            vec!["hello", "world"]
        );

        let exact_fit = wrap_text("a full world", 6);
        assert_eq!(
            exact_fit
                .iter()
                .map(|segment| segment.text.as_str())
                .collect::<Vec<_>>(),
            vec!["a full", "world"]
        );
    }

    #[test]
    fn classifies_cjk_and_kinsoku_boundaries() {
        assert!(is_cjk('あ'));
        assert!(is_cjk('世'));
        assert!(is_cjk('한'));
        assert!(!is_cjk('A'));

        assert!(cannot_start_line('ゃ'));
        assert!(cannot_start_line('。'));
        assert!(!cannot_start_line('あ'));

        assert!(cannot_end_line('「'));
        assert!(cannot_end_line('('));
        assert!(!cannot_end_line('」'));
    }

    #[test]
    fn wraps_wide_text_by_width() {
        let wrapped = wrap_text("世界世界", 4);
        assert_eq!(wrapped.len(), 2);
        assert_eq!(wrapped[0].text, "世界");
        assert_eq!(wrapped[1].text, "世界");
    }

    #[test]
    fn keeps_source_ranges_valid() {
        let text = "  hello world  ";
        for segment in wrap_text(text, 5) {
            let (start, end) = segment.source_range;
            assert!(text.is_char_boundary(start));
            assert!(text.is_char_boundary(end));
            assert_eq!(&text[start..end], segment.text);
        }
    }

    #[test]
    fn estimate_word_timings_handles_empty_whitespace_cjk_and_pauses() {
        assert!(estimate_word_timings("", 0, 1_000).is_empty());
        assert!(estimate_word_timings("   \t\u{3000} ", 0, 1_000).is_empty());
        assert!(estimate_word_timings("hello", 2_000, 1_000).is_empty());

        let cjk = estimate_word_timings("夜に駆ける", 1_000, 6_000);
        assert_eq!(cjk.len(), 5);
        assert_eq!(cjk[0].start_ms, 1_000);
        assert_eq!(cjk[4].end_ms, 6_000);
        for pair in cjk.windows(2) {
            assert!(pair[0].end_ms <= pair[1].start_ms);
        }

        let paused = estimate_word_timings("Hello, world", 0, 3_000);
        assert_eq!(paused.len(), 2);
        assert_eq!(
            &"Hello, world"[paused[0].byte_range.0..paused[0].byte_range.1],
            "Hello,"
        );
        assert_eq!(
            &"Hello, world"[paused[1].byte_range.0..paused[1].byte_range.1],
            "world"
        );
        assert!(
            paused[0].end_ms < paused[1].start_ms,
            "comma should create pause before next word: {paused:?}"
        );
    }
}

pub mod krc;
pub mod lrc;
pub mod lyricsfile;
pub mod qrc;
pub mod ttml;
pub mod yrc;

use crate::core::model::{Annotation, LineRole};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParsedMetadata {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub language: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedWord {
    pub byte_range: (usize, usize),
    pub start_ms: u64,
    pub end_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedLine {
    pub start_ms: u64,
    pub end_ms: Option<u64>,
    pub role: LineRole,
    pub text: String,
    pub words: Vec<ParsedWord>,
    pub annotations: Vec<Annotation>,
    pub explicit_last_word_end_ms: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ParsedTrack {
    pub lines: Vec<ParsedLine>,
    pub intrinsic_offset_ms: i64,
    pub track_duration_ms: Option<u64>,
    pub is_instrumental: bool,
    pub metadata: ParsedMetadata,
}

#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("no lyric lines found")]
    Empty,
    #[error("invalid lyricsfile: {0}")]
    InvalidLyricsFile(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WordTimeMode {
    Absolute,
    Relative,
}

pub(crate) fn parse_line_header(line: &str) -> Option<(u64, u64, &str)> {
    let rest = line.strip_prefix('[')?;
    let end = rest.find(']')?;
    let (start, duration) = rest[..end].split_once(',')?;
    let start = start.trim().parse::<u64>().ok()?;
    let duration = duration.trim().parse::<u64>().ok()?;
    Some((start, duration, &rest[end + 1..]))
}

pub(crate) fn parse_tagged_words(
    text: &str,
    line_start: u64,
    open: char,
    close: char,
    mode: WordTimeMode,
) -> (String, Vec<ParsedWord>, Option<u64>) {
    parse_tagged_words_with(text, line_start, open, close, mode, |segment| {
        segment.to_string()
    })
}

pub(crate) fn parse_tagged_words_decoded<F>(
    text: &str,
    line_start: u64,
    open: char,
    close: char,
    mode: WordTimeMode,
    decode: F,
) -> (String, Vec<ParsedWord>, Option<u64>)
where
    F: Fn(&str) -> String,
{
    parse_tagged_words_with(text, line_start, open, close, mode, decode)
}

fn parse_tagged_words_with<F>(
    text: &str,
    line_start: u64,
    open: char,
    close: char,
    mode: WordTimeMode,
    decode: F,
) -> (String, Vec<ParsedWord>, Option<u64>)
where
    F: Fn(&str) -> String,
{
    let mut output = String::new();
    let mut words = Vec::new();
    let mut cursor = 0;

    while let Some(relative_open) = text[cursor..].find(open) {
        let open_index = cursor + relative_open;
        output.push_str(&decode(&text[cursor..open_index]));
        let Some(relative_close) = text[open_index + open.len_utf8()..].find(close) else {
            cursor = open_index;
            break;
        };
        let close_index = open_index + open.len_utf8() + relative_close;
        let tag = &text[open_index + open.len_utf8()..close_index];
        let next_open = text[close_index + close.len_utf8()..]
            .find(open)
            .map_or(text.len(), |offset| close_index + close.len_utf8() + offset);
        let word_text = &text[close_index + close.len_utf8()..next_open];

        if let Some((timestamp, duration)) = tag.split_once(',').and_then(|(start, rest)| {
            let duration = rest.split(',').next()?;
            Some((
                start.trim().parse::<u64>().ok()?,
                duration.trim().parse::<u64>().ok()?,
            ))
        }) {
            let start_ms = match mode {
                WordTimeMode::Absolute if timestamp >= line_start => timestamp,
                WordTimeMode::Absolute => line_start.saturating_add(timestamp),
                WordTimeMode::Relative => line_start.saturating_add(timestamp),
            };
            let end_ms = start_ms.saturating_add(duration);
            let start_byte = output.len();
            output.push_str(&decode(word_text));
            let end_byte = output.len();
            if start_byte < end_byte {
                words.push(ParsedWord {
                    byte_range: (start_byte, end_byte),
                    start_ms,
                    end_ms: Some(end_ms),
                });
            }
        } else {
            output.push_str(&decode(&text[open_index..next_open]));
        }

        cursor = next_open;
    }

    output.push_str(&decode(&text[cursor..]));
    let explicit_last_word_end_ms = words.last().and_then(|word| word.end_ms);
    (output, words, explicit_last_word_end_ms)
}

pub(crate) fn trim_line_with_words(
    text: String,
    mut words: Vec<ParsedWord>,
) -> (String, Vec<ParsedWord>) {
    let start_trim = text.len() - text.trim_start().len();
    let end_trim = text.trim_end().len();

    let trimmed = if start_trim == 0 && end_trim == text.len() {
        text
    } else {
        text[start_trim..end_trim].to_string()
    };

    for word in &mut words {
        let start = word
            .byte_range
            .0
            .saturating_sub(start_trim)
            .min(trimmed.len());
        let end = word
            .byte_range
            .1
            .saturating_sub(start_trim)
            .min(trimmed.len());
        word.byte_range = crate::core::text::trim_range(&trimmed, start, end);
    }
    words.retain(|word| word.byte_range.0 < word.byte_range.1);

    (trimmed, words)
}

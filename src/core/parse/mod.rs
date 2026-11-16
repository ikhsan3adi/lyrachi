pub mod lrc;
pub mod lyricsfile;
pub mod ttml;

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

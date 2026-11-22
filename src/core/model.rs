use serde::{Deserialize, Serialize};

pub const PARSER_VERSION: u16 = 2;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CanonicalTrackKey {
    pub norm_artist: String,
    pub norm_title: String,
    pub norm_album: Option<String>,
    pub duration_bucket_sec: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum SyncGranularity {
    Unsynced,
    Line,
    Word,
    Syllable,
}

impl SyncGranularity {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unsynced => "unsynced",
            Self::Line => "line",
            Self::Word => "word",
            Self::Syllable => "syllable",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LineRole {
    Primary,
    DuetSecondary,
    BackgroundVocal,
    InstrumentalBreak,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WordToken {
    pub byte_range: (usize, usize),
    pub start_ms: u64,
    pub end_ms: u64,
    pub display_width: u16,
}

pub fn is_intra_word_join(line_text: &str, prev: &WordToken, next: &WordToken) -> bool {
    let (prev_start, boundary) = prev.byte_range;
    let (next_start, next_end) = next.byte_range;

    if prev_start >= boundary
        || boundary != next_start
        || next_start >= next_end
        || next_end > line_text.len()
        || !line_text.is_char_boundary(prev_start)
        || !line_text.is_char_boundary(next_end)
    {
        return false;
    }

    let Some(before) = line_text.get(..boundary) else {
        return false;
    };
    let Some(after) = line_text.get(boundary..) else {
        return false;
    };
    let Some(before) = before.chars().next_back() else {
        return false;
    };
    let Some(after) = after.chars().next() else {
        return false;
    };

    !before.is_whitespace() && !after.is_whitespace()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AnnotationKind {
    Translation,
    Romanization,
    Secondary,
    Background,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Annotation {
    pub kind: AnnotationKind,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LyricLine {
    pub start_ms: u64,
    pub end_ms: u64,
    pub role: LineRole,
    pub text: String,
    pub words: Vec<WordToken>,
    pub annotations: Vec<Annotation>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SyncedTrack {
    pub source_provider: String,
    pub source_track_id: Option<String>,
    pub parser_version: u16,
    pub is_instrumental: bool,
    pub granularity: SyncGranularity,
    pub intrinsic_offset_ms: i64,
    pub user_offset_ms: i64,
    pub match_confidence: f32,
    pub lines: Vec<LyricLine>,
}

impl SyncedTrack {
    pub fn status_label(&self) -> &'static str {
        if self.is_instrumental {
            "instrumental"
        } else {
            self.granularity.as_str()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn word(byte_range: (usize, usize)) -> WordToken {
        WordToken {
            byte_range,
            start_ms: 0,
            end_ms: 0,
            display_width: 0,
        }
    }

    #[test]
    fn parser_version_tracks_the_syllable_ir() {
        assert_eq!(PARSER_VERSION, 2);
    }

    #[test]
    fn granularity_orders_syllable_above_word_line_and_unsynced() {
        assert!(SyncGranularity::Syllable > SyncGranularity::Word);
        assert!(SyncGranularity::Word > SyncGranularity::Line);
        assert!(SyncGranularity::Line > SyncGranularity::Unsynced);
    }

    #[test]
    fn detects_only_adjacent_non_whitespace_word_ranges() {
        assert!(is_intra_word_join("hello", &word((0, 3)), &word((3, 5))));
        assert!(!is_intra_word_join("hel lo", &word((0, 3)), &word((4, 6))));
        assert!(!is_intra_word_join("hel lo", &word((0, 4)), &word((4, 6))));
        assert!(!is_intra_word_join("hello", &word((0, 3)), &word((4, 5))));
    }

    #[test]
    fn rejects_empty_or_out_of_bounds_word_ranges() {
        assert!(!is_intra_word_join("hello", &word((0, 0)), &word((0, 5))));
        assert!(!is_intra_word_join("hello", &word((0, 3)), &word((3, 9))));
        assert!(!is_intra_word_join("hello", &word((0, 3)), &word((3, 3))));
    }
}

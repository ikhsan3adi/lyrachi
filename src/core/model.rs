use serde::{Deserialize, Serialize};

pub const PARSER_VERSION: u16 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CanonicalTrackKey {
    pub norm_artist: String,
    pub norm_title: String,
    pub norm_album: Option<String>,
    pub duration_bucket_sec: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SyncGranularity {
    Word,
    Line,
    Unsynced,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AnnotationKind {
    Translation,
    Romanization,
    Secondary,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use crate::compiler::phrase::{PhraseChunk, compile_line_phrases};
use crate::core::clock::PlaybackStatus;
use crate::core::model::{LineRole, SyncGranularity, SyncedTrack, WordToken};
use crate::core::text::{estimate_word_timings, wrap_text_with_ambiguous};

pub const HORIZONTAL_PADDING: u16 = 2;
const MIN_WRAP_WIDTH: u16 = 1;
const DEFAULT_VIEWPORT_WIDTH: u16 = 80;
const DEFAULT_PHRASE_MAX_CHARS: u16 = 18;
const DEFAULT_MIN_GAP_MS: u64 = 4_000;
pub const MIN_LINE_HIGHLIGHT_MS: u64 = 1_500;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TimelineContext {
    pub viewport_width: u16,
    pub ambiguous_width: u8,
    pub padding_horizontal: u16,
    pub max_width: Option<u16>,
    pub synthetic_word_timing: bool,
    pub phrase_max_chars: u16,
    pub instrumental_marker: char,
    pub min_gap_ms: u64,
    pub track_duration_ms: Option<u64>,
}

impl Default for TimelineContext {
    fn default() -> Self {
        Self {
            viewport_width: DEFAULT_VIEWPORT_WIDTH,
            ambiguous_width: 1,
            padding_horizontal: HORIZONTAL_PADDING,
            max_width: None,
            synthetic_word_timing: false,
            phrase_max_chars: DEFAULT_PHRASE_MAX_CHARS,
            instrumental_marker: crate::style::DEFAULT_INSTRUMENTAL_MARKER,
            min_gap_ms: DEFAULT_MIN_GAP_MS,
            track_duration_ms: None,
        }
    }
}

impl TimelineContext {
    pub fn new(viewport_width: u16) -> Self {
        Self {
            viewport_width,
            ..Self::default()
        }
    }

    pub fn options_hash(self) -> u64 {
        let mut hasher = DefaultHasher::new();
        self.hash(&mut hasher);
        hasher.finish()
    }

    pub fn wrap_width(self) -> u16 {
        let available = self
            .viewport_width
            .saturating_sub(self.padding_horizontal.saturating_mul(2));
        self.max_width
            .map_or(available, |max_width| available.min(max_width))
            .max(MIN_WRAP_WIDTH)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WrappedSubLine {
    pub text: String,
    pub display_width: u16,
    pub source_byte_range: (usize, usize),
}

#[derive(Debug, Clone, PartialEq)]
pub struct CompiledLine {
    pub source_line_idx: usize,
    pub start_ms: u64,
    pub end_ms: u64,
    pub role: LineRole,
    pub wrapped_main: Vec<WrappedSubLine>,
    pub wrapped_annotations: Vec<String>,
    pub words: Vec<WordToken>,
    pub phrases: Vec<PhraseChunk>,
}

impl CompiledLine {
    pub fn active_phrase_index(&self, effective_ms: u64) -> Option<usize> {
        if self.phrases.is_empty() {
            return None;
        }
        let next_idx = self
            .phrases
            .partition_point(|phrase| phrase.start_ms <= effective_ms);
        let idx = next_idx.checked_sub(1)?;
        (effective_ms <= self.phrases[idx].end_ms).then_some(idx)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitKind {
    Intro,
    Instrumental,
    Outro,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WaitingGap {
    pub start_ms: u64,
    pub display_start_ms: u64,
    pub end_ms: u64,
    pub kind: WaitKind,
}

impl WaitingGap {
    pub fn duration_ms(self) -> u64 {
        self.end_ms.saturating_sub(self.start_ms)
    }

    pub fn remaining_ms(self, effective_ms: u64) -> u64 {
        self.end_ms.saturating_sub(effective_ms)
    }

    pub fn progress(self, effective_ms: u64) -> f32 {
        interval_progress(self.display_start_ms, self.end_ms, effective_ms)
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ActiveStageCursor {
    pub active_line_idx: Option<usize>,
    pub active_word_idx: Option<usize>,
    pub progress: f32,
    pub waiting: Option<WaitingGap>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CompiledTimeline {
    pub viewport_width: u16,
    pub context: TimelineContext,
    pub granularity: SyncGranularity,
    pub has_synthetic_words: bool,
    pub lines: Vec<CompiledLine>,
}

impl CompiledTimeline {
    pub fn compile(track: &SyncedTrack, context: TimelineContext) -> Self {
        let wrap_width = context.wrap_width();
        let mut has_synthetic_words = false;

        let lines = track
            .lines
            .iter()
            .enumerate()
            .map(|(source_line_idx, line)| {
                let display_text = if line.role == LineRole::InstrumentalBreak {
                    context.instrumental_marker.to_string()
                } else {
                    line.text.clone()
                };
                let wrapped_main =
                    wrap_text_with_ambiguous(&display_text, wrap_width, context.ambiguous_width)
                        .into_iter()
                        .map(|wrapped| WrappedSubLine {
                            text: wrapped.text,
                            display_width: wrapped.display_width,
                            source_byte_range: wrapped.source_range,
                        })
                        .collect();
                let wrapped_annotations = line
                    .annotations
                    .iter()
                    .flat_map(|annotation| {
                        wrap_text_with_ambiguous(
                            &annotation.text,
                            wrap_width,
                            context.ambiguous_width,
                        )
                    })
                    .map(|wrapped| wrapped.text)
                    .collect();

                let synthetic_eligible = context.synthetic_word_timing
                    && track.granularity == SyncGranularity::Line
                    && !track.is_instrumental
                    && line.role != LineRole::InstrumentalBreak
                    && line.words.is_empty();
                let words = if synthetic_eligible {
                    let estimated = estimate_word_timings(&line.text, line.start_ms, line.end_ms);
                    if !estimated.is_empty() {
                        has_synthetic_words = true;
                    }
                    estimated
                } else {
                    line.words.clone()
                };
                let phrases = if track.is_instrumental || line.role == LineRole::InstrumentalBreak {
                    Vec::new()
                } else {
                    compile_line_phrases(
                        &line.text,
                        line.start_ms,
                        line.end_ms,
                        &words,
                        context.phrase_max_chars,
                    )
                };

                CompiledLine {
                    source_line_idx,
                    start_ms: line.start_ms,
                    end_ms: line.end_ms,
                    role: line.role,
                    wrapped_main,
                    wrapped_annotations,
                    words,
                    phrases,
                }
            })
            .collect();

        Self {
            viewport_width: context.viewport_width,
            context,
            granularity: track.granularity,
            has_synthetic_words,
            lines,
        }
    }

    pub fn evaluate(&self, effective_ms: u64) -> ActiveStageCursor {
        self.evaluate_for_status(effective_ms, PlaybackStatus::Playing)
    }

    pub fn evaluate_for_status(
        &self,
        effective_ms: u64,
        status: PlaybackStatus,
    ) -> ActiveStageCursor {
        if self.granularity == SyncGranularity::Unsynced || self.lines.is_empty() {
            return ActiveStageCursor {
                active_line_idx: None,
                active_word_idx: None,
                progress: 0.0,
                waiting: None,
            };
        }

        let active_line_idx = self.line_index(effective_ms);
        let waiting = if status == PlaybackStatus::Stopped {
            None
        } else {
            self.waiting_gap(effective_ms, active_line_idx)
        };

        let Some(active_line_idx) = active_line_idx else {
            return ActiveStageCursor {
                active_line_idx: None,
                active_word_idx: None,
                progress: waiting.map_or(0.0, |gap| gap.progress(effective_ms)),
                waiting,
            };
        };
        let line = &self.lines[active_line_idx];
        let active_word_idx = self.word_index(line, effective_ms);
        let progress = match (active_word_idx, waiting) {
            (Some(word_idx), _) => {
                let word = &line.words[word_idx];
                interval_progress(word.start_ms, word.end_ms, effective_ms)
            }
            (None, Some(gap)) => gap.progress(effective_ms),
            (None, None) => interval_progress(line.start_ms, line.end_ms, effective_ms),
        };

        ActiveStageCursor {
            active_line_idx: Some(active_line_idx),
            active_word_idx,
            progress,
            waiting,
        }
    }

    fn waiting_gap(&self, effective_ms: u64, active_line_idx: Option<usize>) -> Option<WaitingGap> {
        if let Some(idx) = active_line_idx {
            if self.lines[idx].role != LineRole::InstrumentalBreak {
                return None;
            }
            let prev_lyric = self.lines[..idx]
                .iter()
                .rfind(|line| line.role != LineRole::InstrumentalBreak);
            let next_lyric = self.lines[idx + 1..]
                .iter()
                .find(|line| line.role != LineRole::InstrumentalBreak);
            return self.waiting_gap_from_bounds(
                effective_ms,
                prev_lyric,
                next_lyric,
                Some(self.lines[idx].end_ms),
            );
        }

        let next_idx = self
            .lines
            .partition_point(|line| line.start_ms <= effective_ms);
        let prev_lyric = self.lines[..next_idx]
            .iter()
            .rfind(|line| line.role != LineRole::InstrumentalBreak);
        let next_lyric = self.lines[next_idx..]
            .iter()
            .find(|line| line.role != LineRole::InstrumentalBreak);
        self.waiting_gap_from_bounds(effective_ms, prev_lyric, next_lyric, None)
    }

    fn waiting_gap_from_bounds(
        &self,
        effective_ms: u64,
        prev_lyric: Option<&CompiledLine>,
        next_lyric: Option<&CompiledLine>,
        outro_fallback_end_ms: Option<u64>,
    ) -> Option<WaitingGap> {
        let min_gap_ms = self.context.min_gap_ms;
        match (prev_lyric, next_lyric) {
            (None, Some(next)) => (effective_ms < next.start_ms && next.start_ms >= min_gap_ms)
                .then_some(WaitingGap {
                    start_ms: 0,
                    display_start_ms: 0,
                    end_ms: next.start_ms,
                    kind: WaitKind::Intro,
                }),
            (Some(prev), Some(next)) => (next.start_ms.saturating_sub(prev.end_ms) >= min_gap_ms)
                .then_some(WaitingGap {
                    start_ms: prev.end_ms,
                    display_start_ms: line_hold_end_ms(prev).min(next.start_ms),
                    end_ms: next.start_ms,
                    kind: WaitKind::Instrumental,
                }),
            (Some(prev), None) => {
                let end_ms = match (self.context.track_duration_ms, outro_fallback_end_ms) {
                    (Some(duration), Some(fallback)) => duration.max(fallback),
                    (Some(duration), None) => duration,
                    (None, Some(fallback)) => fallback,
                    (None, None) => return None,
                };
                (effective_ms < end_ms && end_ms.saturating_sub(prev.end_ms) >= min_gap_ms)
                    .then_some(WaitingGap {
                        start_ms: prev.end_ms,
                        display_start_ms: line_hold_end_ms(prev).min(end_ms),
                        end_ms,
                        kind: WaitKind::Outro,
                    })
            }
            (None, None) => None,
        }
    }

    pub fn next_boundary_ms(&self, effective_ms: u64) -> Option<u64> {
        let next_line_idx = self
            .lines
            .partition_point(|line| line.start_ms <= effective_ms);
        let mut earliest = self.lines.get(next_line_idx).map(|line| line.start_ms);

        if let Some(line_idx) = self.line_index(effective_ms) {
            let line = &self.lines[line_idx];
            if line.end_ms < u64::MAX {
                earliest = min_boundary(earliest, Some(line.end_ms + 1), effective_ms);
            }
            if line.role != LineRole::InstrumentalBreak {
                let held_end_ms = line_hold_end_ms(line);
                if held_end_ms > line.end_ms {
                    earliest = min_boundary(earliest, Some(held_end_ms), effective_ms);
                }
            }

            let next_word_idx = line
                .words
                .partition_point(|word| word.start_ms <= effective_ms);
            let next_word_start = line.words.get(next_word_idx).map(|word| word.start_ms);
            earliest = min_boundary(earliest, next_word_start, effective_ms);

            if let Some(word_idx) = self.word_index(line, effective_ms) {
                let word = &line.words[word_idx];
                if word.end_ms < u64::MAX {
                    earliest = min_boundary(earliest, Some(word.end_ms + 1), effective_ms);
                }
            }

            let next_phrase_idx = line
                .phrases
                .partition_point(|phrase| phrase.start_ms <= effective_ms);
            let next_phrase_start = line
                .phrases
                .get(next_phrase_idx)
                .map(|phrase| phrase.start_ms);
            earliest = min_boundary(earliest, next_phrase_start, effective_ms);
        }

        if let Some(gap) = self.waiting_gap(effective_ms, self.line_index(effective_ms)) {
            earliest = min_boundary(earliest, Some(gap.end_ms), effective_ms);
        }

        earliest
    }

    pub fn anchor_line_index(&self, effective_ms: u64) -> Option<usize> {
        if self.lines.is_empty() {
            return None;
        }
        if let Some(active) = self.line_index(effective_ms) {
            return Some(active);
        }
        let next_idx = self
            .lines
            .partition_point(|line| line.start_ms <= effective_ms);
        Some(next_idx.saturating_sub(1))
    }

    fn line_index(&self, effective_ms: u64) -> Option<usize> {
        let next_idx = self
            .lines
            .partition_point(|line| line.start_ms <= effective_ms);
        let idx = next_idx.checked_sub(1)?;
        let line = &self.lines[idx];

        if line.role == LineRole::InstrumentalBreak {
            if let Some(prev_idx) = self.previous_lyric_index(idx)
                && effective_ms < line_hold_end_ms(&self.lines[prev_idx])
            {
                return Some(prev_idx);
            }
            return (effective_ms <= line.end_ms).then_some(idx);
        }

        if effective_ms <= line.end_ms {
            return Some(idx);
        }

        (effective_ms < line_hold_end_ms(line)).then_some(idx)
    }

    fn previous_lyric_index(&self, idx: usize) -> Option<usize> {
        self.lines[..idx]
            .iter()
            .rposition(|line| line.role != LineRole::InstrumentalBreak)
    }

    fn word_index(&self, line: &CompiledLine, effective_ms: u64) -> Option<usize> {
        let next_idx = line
            .words
            .partition_point(|word| word.start_ms <= effective_ms);
        let idx = next_idx.checked_sub(1)?;
        (effective_ms <= line.words[idx].end_ms).then_some(idx)
    }
}

fn line_hold_end_ms(line: &CompiledLine) -> u64 {
    line.end_ms
        .max(line.start_ms.saturating_add(MIN_LINE_HIGHLIGHT_MS))
}

fn min_boundary(current: Option<u64>, candidate: Option<u64>, after: u64) -> Option<u64> {
    let candidate = candidate.filter(|candidate| *candidate > after);
    match (current, candidate) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

fn interval_progress(start_ms: u64, end_ms: u64, effective_ms: u64) -> f32 {
    if end_ms <= start_ms || effective_ms <= start_ms {
        return 0.0;
    }

    let elapsed = effective_ms.saturating_sub(start_ms) as f32;
    let total = end_ms.saturating_sub(start_ms) as f32;
    (elapsed / total).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::model::LyricLine;
    use crate::core::text::display_width;

    fn word(text: &str, start_ms: u64, end_ms: u64) -> WordToken {
        WordToken {
            byte_range: (0, text.len()),
            start_ms,
            end_ms,
            display_width: display_width(text),
        }
    }

    fn track() -> SyncedTrack {
        SyncedTrack {
            source_provider: "test".to_string(),
            source_track_id: None,
            parser_version: 1,
            is_instrumental: false,
            granularity: SyncGranularity::Word,
            intrinsic_offset_ms: 0,
            user_offset_ms: 0,
            match_confidence: 1.0,
            lines: vec![
                LyricLine {
                    start_ms: 1_000,
                    end_ms: 3_000,
                    role: LineRole::Primary,
                    text: "hello wide world".to_string(),
                    words: vec![
                        word("hello ", 1_000, 1_400),
                        word("wide ", 1_400, 1_900),
                        word("world", 1_900, 3_000),
                    ],
                    annotations: vec![crate::core::model::Annotation {
                        kind: crate::core::model::AnnotationKind::Translation,
                        text: "translated line".to_string(),
                    }],
                },
                LyricLine {
                    start_ms: 4_000,
                    end_ms: 5_000,
                    role: LineRole::Primary,
                    text: "next".to_string(),
                    words: vec![word("next", 4_000, 5_000)],
                    annotations: Vec::new(),
                },
            ],
        }
    }

    #[test]
    fn instrumental_break_uses_context_marker_for_display_text() {
        let mut track = track();
        track.lines.push(LyricLine {
            start_ms: 6_000,
            end_ms: 8_000,
            role: LineRole::InstrumentalBreak,
            text: "[Instrumental]".to_string(),
            words: Vec::new(),
            annotations: Vec::new(),
        });
        let mut context = TimelineContext::new(40);
        context.instrumental_marker = '♪';

        let timeline = CompiledTimeline::compile(&track, context);

        assert_eq!(timeline.lines[2].wrapped_main.len(), 1);
        assert_eq!(timeline.lines[2].wrapped_main[0].text, "♪");
        assert_eq!(track.lines[2].text, "[Instrumental]");
    }

    #[test]
    fn wraps_lines_and_annotations_within_viewport() {
        let timeline = CompiledTimeline::compile(&track(), TimelineContext::new(16));

        assert_eq!(timeline.viewport_width, 16);
        assert_eq!(timeline.lines[0].wrapped_main.len(), 2);
        assert_eq!(
            timeline.lines[0].wrapped_annotations,
            ["translated", "line"]
        );
        for line in &timeline.lines {
            for wrapped in &line.wrapped_main {
                assert!(wrapped.display_width <= 12);
            }
        }
    }

    #[test]
    fn evaluates_line_word_and_progress() {
        let timeline = CompiledTimeline::compile(&track(), TimelineContext::new(40));

        let before = timeline.evaluate(999);
        assert_eq!(before.active_line_idx, None);

        let first = timeline.evaluate(1_500);
        assert_eq!(first.active_line_idx, Some(0));
        assert_eq!(first.active_word_idx, Some(1));
        assert!((first.progress - 0.2).abs() < 0.0001);

        let second = timeline.evaluate(4_500);
        assert_eq!(second.active_line_idx, Some(1));
        assert_eq!(second.active_word_idx, Some(0));
        assert!((second.progress - 0.5).abs() < 0.0001);

        let held = timeline.evaluate(5_001);
        assert_eq!(held.active_line_idx, Some(1));
        assert_eq!(held.active_word_idx, None);

        let after = timeline.evaluate(5_500);
        assert_eq!(after.active_line_idx, None);
    }

    #[test]
    fn reports_next_line_or_word_boundary() {
        let timeline = CompiledTimeline::compile(&track(), TimelineContext::new(40));

        assert_eq!(timeline.next_boundary_ms(500), Some(1_000));
        assert_eq!(timeline.next_boundary_ms(1_500), Some(1_900));
        assert_eq!(timeline.next_boundary_ms(2_500), Some(3_001));
        assert_eq!(timeline.next_boundary_ms(3_500), Some(4_000));
        assert_eq!(timeline.next_boundary_ms(4_500), Some(5_001));
        assert_eq!(timeline.next_boundary_ms(5_001), Some(5_500));
        assert_eq!(timeline.next_boundary_ms(5_500), None);
    }

    #[test]
    fn reports_waiting_gap_boundary_for_intro_and_outro() {
        let mut context = TimelineContext::new(40);
        context.min_gap_ms = 4_000;
        context.track_duration_ms = Some(10_000);

        let mut intro_outro_track = track();
        // First line starts at 5_000 (>= min_gap_ms 4_000 -> intro gap 0..5000)
        intro_outro_track.lines[0].start_ms = 5_000;
        intro_outro_track.lines[0].end_ms = 6_000;
        intro_outro_track.lines[0].words = vec![word("intro-test", 5_000, 6_000)];
        intro_outro_track.lines.truncate(1);

        let timeline = CompiledTimeline::compile(&intro_outro_track, context);
        // During intro waiting gap (effective_ms 2_000), next boundary is 5_000 (gap.end_ms / line start)
        assert_eq!(timeline.next_boundary_ms(2_000), Some(5_000));
        // During outro waiting gap (effective_ms 7_000), next boundary is 10_000 (gap.end_ms)
        assert_eq!(timeline.next_boundary_ms(7_000), Some(10_000));
        // Past track duration, no next boundary
        assert_eq!(timeline.next_boundary_ms(10_500), None);
    }

    #[test]
    fn reports_word_end_before_midline_pause() {
        let mut paused_track = track();
        paused_track.lines[0].words = vec![
            word("hello ", 1_000, 1_300),
            word("wide ", 1_800, 2_200),
            word("world", 2_200, 3_000),
        ];
        let timeline = CompiledTimeline::compile(&paused_track, TimelineContext::new(40));

        assert_eq!(timeline.next_boundary_ms(1_200), Some(1_301));
        assert_eq!(timeline.next_boundary_ms(1_500), Some(1_800));
    }

    #[test]
    fn anchors_viewport_across_active_lines_and_gaps() {
        let timeline = CompiledTimeline::compile(&track(), TimelineContext::new(40));

        assert_eq!(timeline.anchor_line_index(500), Some(0));
        assert_eq!(timeline.anchor_line_index(1_500), Some(0));
        assert_eq!(timeline.anchor_line_index(3_500), Some(0));
        assert_eq!(timeline.anchor_line_index(4_500), Some(1));
        assert_eq!(timeline.anchor_line_index(6_000), Some(1));

        let mut empty = track();
        empty.lines.clear();
        assert_eq!(
            CompiledTimeline::compile(&empty, TimelineContext::new(40)).anchor_line_index(0),
            None
        );
    }

    #[test]
    fn compiles_and_evaluates_syllable_tokens_without_splitting_words() {
        let syllable_track = SyncedTrack {
            source_provider: "test".to_string(),
            source_track_id: None,
            parser_version: 2,
            is_instrumental: false,
            granularity: SyncGranularity::Syllable,
            intrinsic_offset_ms: 0,
            user_offset_ms: 0,
            match_confidence: 1.0,
            lines: vec![LyricLine {
                start_ms: 1_000,
                end_ms: 2_000,
                role: LineRole::Primary,
                text: "hello wonderful".to_string(),
                words: vec![
                    WordToken {
                        byte_range: (0, 3),
                        start_ms: 1_000,
                        end_ms: 1_200,
                        display_width: 3,
                    },
                    WordToken {
                        byte_range: (3, 5),
                        start_ms: 1_200,
                        end_ms: 1_400,
                        display_width: 2,
                    },
                    WordToken {
                        byte_range: (6, 12),
                        start_ms: 1_400,
                        end_ms: 1_700,
                        display_width: 6,
                    },
                    WordToken {
                        byte_range: (12, 15),
                        start_ms: 1_700,
                        end_ms: 2_000,
                        display_width: 3,
                    },
                ],
                annotations: Vec::new(),
            }],
        };

        let timeline = CompiledTimeline::compile(&syllable_track, TimelineContext::new(14));
        assert_eq!(timeline.granularity, SyncGranularity::Syllable);
        assert_eq!(
            timeline.lines[0]
                .wrapped_main
                .iter()
                .map(|sub| sub.text.as_str())
                .collect::<Vec<_>>(),
            vec!["hello", "wonderful"]
        );

        let second_syllable = timeline.evaluate(1_300);
        assert_eq!(second_syllable.active_line_idx, Some(0));
        assert_eq!(second_syllable.active_word_idx, Some(1));
        assert!((second_syllable.progress - 0.5).abs() < 0.0001);
        assert_eq!(timeline.next_boundary_ms(1_300), Some(1_400));
    }

    #[test]
    fn context_controls_wrap_width_and_is_stored_on_timeline() {
        let base = TimelineContext::new(40);
        assert_eq!(base.wrap_width(), 36);

        let timeline = CompiledTimeline::compile(&track(), base);
        assert_eq!(timeline.context, base);
        assert_eq!(timeline.viewport_width, 40);

        let mut capped = base;
        capped.max_width = Some(10);
        assert_eq!(capped.wrap_width(), 10);
        assert_ne!(
            CompiledTimeline::compile(&track(), capped).lines[0]
                .wrapped_main
                .len(),
            timeline.lines[0].wrapped_main.len()
        );

        let mut padded = base;
        padded.padding_horizontal = 18;
        assert_eq!(padded.wrap_width(), 4);
        assert_ne!(
            CompiledTimeline::compile(&track(), padded).lines[0]
                .wrapped_main
                .len(),
            timeline.lines[0].wrapped_main.len()
        );
    }

    #[test]
    fn ambiguous_width_changes_wrapping() {
        let mut ambiguous_track = track();
        ambiguous_track.lines[0].text = "······".to_string();
        ambiguous_track.lines[0].words.clear();

        let narrow = CompiledTimeline::compile(&ambiguous_track, TimelineContext::new(6));
        let mut wide_context = TimelineContext::new(6);
        wide_context.ambiguous_width = 2;
        let wide = CompiledTimeline::compile(&ambiguous_track, wide_context);

        assert_eq!(narrow.lines[0].wrapped_main.len(), 3);
        assert_eq!(wide.lines[0].wrapped_main.len(), 6);
    }

    #[test]
    fn options_hash_covers_every_timeline_field() {
        let base = TimelineContext::new(40);
        let variants = [
            TimelineContext {
                viewport_width: 41,
                ..base
            },
            TimelineContext {
                ambiguous_width: 2,
                ..base
            },
            TimelineContext {
                padding_horizontal: 3,
                ..base
            },
            TimelineContext {
                max_width: Some(20),
                ..base
            },
            TimelineContext {
                synthetic_word_timing: true,
                ..base
            },
            TimelineContext {
                phrase_max_chars: 20,
                ..base
            },
            TimelineContext {
                instrumental_marker: '♪',
                ..base
            },
            TimelineContext {
                min_gap_ms: 5_000,
                ..base
            },
            TimelineContext {
                track_duration_ms: Some(180_000),
                ..base
            },
        ];

        for variant in variants {
            assert_ne!(variant.options_hash(), base.options_hash());
        }
        assert_eq!(TimelineContext::new(40).options_hash(), base.options_hash());
    }

    #[test]
    fn evaluates_intro_instrumental_and_outro_waiting_gaps() {
        let gap_track = SyncedTrack {
            source_provider: "test".to_string(),
            source_track_id: None,
            parser_version: 2,
            is_instrumental: false,
            granularity: SyncGranularity::Line,
            intrinsic_offset_ms: 0,
            user_offset_ms: 0,
            match_confidence: 1.0,
            lines: vec![
                LyricLine {
                    start_ms: 8_000,
                    end_ms: 10_000,
                    role: LineRole::Primary,
                    text: "first line".to_string(),
                    words: Vec::new(),
                    annotations: Vec::new(),
                },
                LyricLine {
                    start_ms: 10_001,
                    end_ms: 19_999,
                    role: LineRole::InstrumentalBreak,
                    text: "[Instrumental]".to_string(),
                    words: Vec::new(),
                    annotations: Vec::new(),
                },
                LyricLine {
                    start_ms: 20_000,
                    end_ms: 22_000,
                    role: LineRole::Primary,
                    text: "second line".to_string(),
                    words: Vec::new(),
                    annotations: Vec::new(),
                },
                LyricLine {
                    start_ms: 27_000,
                    end_ms: 29_000,
                    role: LineRole::Primary,
                    text: "third line".to_string(),
                    words: Vec::new(),
                    annotations: Vec::new(),
                },
            ],
        };

        let mut context = TimelineContext::new(40);
        context.min_gap_ms = 4_000;
        context.track_duration_ms = Some(35_000);
        let timeline = CompiledTimeline::compile(&gap_track, context);

        // Intro gap: 0..8_000
        let intro = timeline.evaluate_for_status(2_000, PlaybackStatus::Playing);
        assert_eq!(
            intro.waiting,
            Some(WaitingGap {
                start_ms: 0,
                display_start_ms: 0,
                end_ms: 8_000,
                kind: WaitKind::Intro,
            })
        );
        assert!((intro.progress - 0.25).abs() < 0.0001);

        // Active lyric line clears waiting gap
        assert_eq!(
            timeline
                .evaluate_for_status(9_000, PlaybackStatus::Playing)
                .waiting,
            None
        );

        // Explicit InstrumentalBreak line: 10_000..20_000
        let inst_break = timeline.evaluate_for_status(15_000, PlaybackStatus::Playing);
        assert_eq!(
            inst_break.waiting,
            Some(WaitingGap {
                start_ms: 10_000,
                display_start_ms: 10_000,
                end_ms: 20_000,
                kind: WaitKind::Instrumental,
            })
        );
        assert!((inst_break.progress - 0.5).abs() < 0.0001);

        // Silent inter-line gap without InstrumentalBreak: 22_000..27_000
        let inter_gap = timeline.evaluate_for_status(24_500, PlaybackStatus::Playing);
        assert_eq!(
            inter_gap.waiting,
            Some(WaitingGap {
                start_ms: 22_000,
                display_start_ms: 22_000,
                end_ms: 27_000,
                kind: WaitKind::Instrumental,
            })
        );
        assert!((inter_gap.progress - 0.5).abs() < 0.0001);

        // Outro gap: 29_000..35_000
        let outro = timeline.evaluate_for_status(32_000, PlaybackStatus::Playing);
        assert_eq!(
            outro.waiting,
            Some(WaitingGap {
                start_ms: 29_000,
                display_start_ms: 29_000,
                end_ms: 35_000,
                kind: WaitKind::Outro,
            })
        );
        assert!((outro.progress - 0.5).abs() < 0.0001);

        // Stopped and Unsynced produce no waiting gap; Paused preserves frozen progress
        assert_eq!(
            timeline
                .evaluate_for_status(2_000, PlaybackStatus::Stopped)
                .waiting,
            None
        );
        let paused = timeline.evaluate_for_status(2_000, PlaybackStatus::Paused);
        assert_eq!(paused.waiting, intro.waiting);
        assert!((paused.progress - 0.25).abs() < 0.0001);

        let mut unsynced = gap_track;
        unsynced.granularity = SyncGranularity::Unsynced;
        let unsynced_timeline = CompiledTimeline::compile(&unsynced, context);
        assert_eq!(
            unsynced_timeline
                .evaluate_for_status(2_000, PlaybackStatus::Playing)
                .waiting,
            None
        );
    }

    #[test]
    fn holds_short_line_until_next_line_without_waiting_gap() {
        let mut held_track = track();
        held_track.lines = vec![
            LyricLine {
                start_ms: 1_000,
                end_ms: 1_200,
                role: LineRole::Primary,
                text: "short".to_string(),
                words: Vec::new(),
                annotations: Vec::new(),
            },
            LyricLine {
                start_ms: 1_800,
                end_ms: 2_500,
                role: LineRole::Primary,
                text: "next".to_string(),
                words: Vec::new(),
                annotations: Vec::new(),
            },
        ];
        let timeline = CompiledTimeline::compile(&held_track, TimelineContext::new(40));

        for effective_ms in [1_201, 1_300, 1_799] {
            let cursor = timeline.evaluate(effective_ms);
            assert_eq!(cursor.active_line_idx, Some(0), "at {effective_ms}");
            assert_eq!(cursor.waiting, None, "at {effective_ms}");
        }
        assert_eq!(timeline.next_boundary_ms(1_300), Some(1_800));
        assert_eq!(timeline.evaluate(1_800).active_line_idx, Some(1));
    }

    #[test]
    fn holds_short_line_before_instrumental_gap_then_starts_countdown() {
        let mut held_track = track();
        held_track.lines = vec![
            LyricLine {
                start_ms: 1_000,
                end_ms: 1_200,
                role: LineRole::Primary,
                text: "short".to_string(),
                words: Vec::new(),
                annotations: Vec::new(),
            },
            LyricLine {
                start_ms: 6_000,
                end_ms: 7_000,
                role: LineRole::Primary,
                text: "next".to_string(),
                words: Vec::new(),
                annotations: Vec::new(),
            },
        ];
        let mut context = TimelineContext::new(40);
        context.min_gap_ms = 4_000;
        let timeline = CompiledTimeline::compile(&held_track, context);

        let held = timeline.evaluate(2_499);
        assert_eq!(held.active_line_idx, Some(0));
        assert_eq!(held.waiting, None);
        assert_eq!(timeline.next_boundary_ms(1_300), Some(2_500));

        let waiting = timeline.evaluate(2_500);
        assert_eq!(waiting.active_line_idx, None);
        assert_eq!(
            waiting.waiting,
            Some(WaitingGap {
                start_ms: 1_200,
                display_start_ms: 2_500,
                end_ms: 6_000,
                kind: WaitKind::Instrumental,
            })
        );
        assert_eq!(waiting.progress, 0.0);
        assert!((timeline.evaluate(4_250).progress - 0.5).abs() < 0.0001);
    }

    #[test]
    fn holds_lyric_across_instrumental_break_before_waiting() {
        let mut held_track = track();
        held_track.lines = vec![
            LyricLine {
                start_ms: 1_000,
                end_ms: 1_200,
                role: LineRole::Primary,
                text: "short".to_string(),
                words: Vec::new(),
                annotations: Vec::new(),
            },
            LyricLine {
                start_ms: 1_200,
                end_ms: 6_000,
                role: LineRole::InstrumentalBreak,
                text: "[Instrumental]".to_string(),
                words: Vec::new(),
                annotations: Vec::new(),
            },
            LyricLine {
                start_ms: 6_000,
                end_ms: 7_000,
                role: LineRole::Primary,
                text: "next".to_string(),
                words: Vec::new(),
                annotations: Vec::new(),
            },
        ];
        let mut context = TimelineContext::new(40);
        context.min_gap_ms = 4_000;
        let timeline = CompiledTimeline::compile(&held_track, context);

        let held = timeline.evaluate(2_499);
        assert_eq!(held.active_line_idx, Some(0));
        assert_eq!(held.waiting, None);
        assert_eq!(timeline.next_boundary_ms(2_000), Some(2_500));

        let waiting = timeline.evaluate(2_500);
        assert_eq!(waiting.active_line_idx, Some(1));
        assert_eq!(
            waiting.waiting,
            Some(WaitingGap {
                start_ms: 1_200,
                display_start_ms: 2_500,
                end_ms: 6_000,
                kind: WaitKind::Instrumental,
            })
        );
        assert_eq!(waiting.progress, 0.0);
    }

    #[test]
    fn skips_waiting_gaps_below_min_gap_or_without_duration() {
        let mut context = TimelineContext::new(40);
        context.min_gap_ms = 5_000;
        context.track_duration_ms = None;

        // track() starts at 1_000, gap between line 0 and 1 is 1_000 ms, no duration -> no WaitingGap anywhere
        let timeline = CompiledTimeline::compile(&track(), context);
        assert_eq!(timeline.evaluate(500).waiting, None);
        assert_eq!(timeline.evaluate(3_500).waiting, None);
        assert_eq!(timeline.evaluate(6_000).waiting, None);
    }

    #[test]
    fn synthetic_word_timing_applies_only_to_line_sync_without_words() {
        let mut line_track = track();
        line_track.granularity = SyncGranularity::Line;
        line_track.lines[0].words.clear();
        line_track.lines[0].text = "Hello, world".to_string();
        line_track.lines[1].words.clear();
        line_track.lines[1].text = "   ".to_string();
        let original = line_track.clone();

        let off = CompiledTimeline::compile(&line_track, TimelineContext::new(80));
        assert!(!off.has_synthetic_words);
        assert!(off.lines[0].words.is_empty());

        let mut on_ctx = TimelineContext::new(80);
        on_ctx.synthetic_word_timing = true;
        let on = CompiledTimeline::compile(&line_track, on_ctx);
        assert!(on.has_synthetic_words);
        assert_eq!(on.lines[0].words.len(), 2);
        assert!(on.lines[1].words.is_empty());
        assert_eq!(
            line_track, original,
            "original SyncedTrack must stay intact"
        );

        // Word/Syllable/Instrumental tracks never synthesize or overwrite existing words.
        let word_timeline = CompiledTimeline::compile(&track(), on_ctx);
        assert!(!word_timeline.has_synthetic_words);
        assert_eq!(word_timeline.lines[0].words, track().lines[0].words);

        let mut instrumental = line_track.clone();
        instrumental.is_instrumental = true;
        let inst_timeline = CompiledTimeline::compile(&instrumental, on_ctx);
        assert!(!inst_timeline.has_synthetic_words);
        assert!(inst_timeline.lines[0].words.is_empty());
    }

    proptest::proptest! {
        #[test]
        fn synthetic_word_timing_invariants_hold_for_arbitrary_text(
            text in "([a-zA-Z0-9 ,.!?\"'()\\[\\]\\u{3000}-\\u{303F}\\u{3042}-\\u{3093}\\u{4e00}-\\u{4e20}\\u{ac00}-\\u{ac10}\\u{0301}\\u{2728}]){0,64}",
            start_ms in 0u64..100_000,
            duration_ms in 0u64..60_000,
        ) {
            let end_ms = start_ms + duration_ms;
            let tokens = estimate_word_timings(&text, start_ms, end_ms);
            if text.trim().is_empty() {
                proptest::prop_assert!(tokens.is_empty());
            }
            let mut prev_byte_end = 0usize;
            let mut prev_ms_end = start_ms;
            for token in &tokens {
                let (bs, be) = token.byte_range;
                proptest::prop_assert!(bs < be && be <= text.len());
                proptest::prop_assert!(text.is_char_boundary(bs));
                proptest::prop_assert!(text.is_char_boundary(be));
                proptest::prop_assert!(bs >= prev_byte_end);
                proptest::prop_assert!(token.start_ms >= start_ms);
                proptest::prop_assert!(token.start_ms >= prev_ms_end);
                proptest::prop_assert!(token.start_ms <= token.end_ms);
                proptest::prop_assert!(token.end_ms <= end_ms);
                prev_byte_end = be;
                prev_ms_end = token.end_ms;
            }
            if !tokens.is_empty() {
                proptest::prop_assert_eq!(tokens.first().unwrap().start_ms, start_ms);
                proptest::prop_assert_eq!(tokens.last().unwrap().end_ms, end_ms);
            }
        }
    }
}

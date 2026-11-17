use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use crate::core::model::{LineRole, SyncGranularity, SyncedTrack, WordToken};
use crate::core::text::wrap_text_with_ambiguous;

pub const HORIZONTAL_PADDING: u16 = 2;
const MIN_WRAP_WIDTH: u16 = 1;
const DEFAULT_VIEWPORT_WIDTH: u16 = 80;
const DEFAULT_PHRASE_MAX_CHARS: u16 = 18;
const DEFAULT_MIN_GAP_MS: u64 = 4_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TimelineContext {
    pub viewport_width: u16,
    pub ambiguous_width: u8,
    pub padding_horizontal: u16,
    pub max_width: Option<u16>,
    pub synthetic_word_timing: bool,
    pub phrase_max_chars: u16,
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
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ActiveStageCursor {
    pub active_line_idx: Option<usize>,
    pub active_word_idx: Option<usize>,
    pub progress: f32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CompiledTimeline {
    pub viewport_width: u16,
    pub context: TimelineContext,
    pub granularity: SyncGranularity,
    pub lines: Vec<CompiledLine>,
}

impl CompiledTimeline {
    pub fn compile(track: &SyncedTrack, context: TimelineContext) -> Self {
        let wrap_width = context.wrap_width();

        let lines = track
            .lines
            .iter()
            .enumerate()
            .map(|(source_line_idx, line)| {
                let wrapped_main =
                    wrap_text_with_ambiguous(&line.text, wrap_width, context.ambiguous_width)
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

                CompiledLine {
                    source_line_idx,
                    start_ms: line.start_ms,
                    end_ms: line.end_ms,
                    role: line.role,
                    wrapped_main,
                    wrapped_annotations,
                    words: line.words.clone(),
                }
            })
            .collect();

        Self {
            viewport_width: context.viewport_width,
            context,
            granularity: track.granularity,
            lines,
        }
    }

    pub fn evaluate(&self, effective_ms: u64) -> ActiveStageCursor {
        let Some(active_line_idx) = self.line_index(effective_ms) else {
            return ActiveStageCursor {
                active_line_idx: None,
                active_word_idx: None,
                progress: 0.0,
            };
        };
        let line = &self.lines[active_line_idx];
        let active_word_idx = self.word_index(line, effective_ms);
        let progress = match active_word_idx {
            Some(word_idx) => {
                let word = &line.words[word_idx];
                interval_progress(word.start_ms, word.end_ms, effective_ms)
            }
            None => interval_progress(line.start_ms, line.end_ms, effective_ms),
        };

        ActiveStageCursor {
            active_line_idx: Some(active_line_idx),
            active_word_idx,
            progress,
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
                earliest = min_boundary(earliest, Some(line.end_ms + 1));
            }

            let next_word_idx = line
                .words
                .partition_point(|word| word.start_ms <= effective_ms);
            let next_word_start = line.words.get(next_word_idx).map(|word| word.start_ms);
            earliest = min_boundary(earliest, next_word_start);

            if let Some(word_idx) = self.word_index(line, effective_ms) {
                let word = &line.words[word_idx];
                if word.end_ms < u64::MAX {
                    earliest = min_boundary(earliest, Some(word.end_ms + 1));
                }
            }
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
        (effective_ms <= self.lines[idx].end_ms).then_some(idx)
    }

    fn word_index(&self, line: &CompiledLine, effective_ms: u64) -> Option<usize> {
        let next_idx = line
            .words
            .partition_point(|word| word.start_ms <= effective_ms);
        let idx = next_idx.checked_sub(1)?;
        (effective_ms <= line.words[idx].end_ms).then_some(idx)
    }
}

fn min_boundary(current: Option<u64>, candidate: Option<u64>) -> Option<u64> {
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

        let after = timeline.evaluate(5_001);
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
        assert_eq!(timeline.next_boundary_ms(5_500), None);
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
}

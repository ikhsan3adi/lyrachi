use ratatui::{
    Frame,
    layout::Rect,
    text::{Line, Span},
};

use crate::compiler::timeline::CompiledTimeline;
use crate::core::model::{LineRole, SyncedTrack, WordToken};
use crate::core::text::display_width;
use crate::style::{BigTextMaxSize, BigTextOverflow, HighlightMode};
use crate::ui::UiState;
use crate::ui::render::{
    CountdownSize, ModeRenderer, RenderContext, centered_message, line_source_text, render_lines,
    waiting_countdown_lines,
};

use super::block::{
    BlockFontSize, BlockStageCache,
    render::{BlockRenderContext, render_phrase_lines},
    single_line_chunks, stage,
};

pub(crate) struct KineticRenderer;

impl ModeRenderer for KineticRenderer {
    fn render(&self, frame: &mut Frame, area: Rect, ctx: &mut RenderContext<'_>) {
        let state = ctx.state;
        let timeline = ctx.timeline;
        let track = ctx.track;
        let effective_ms = ctx.effective_ms;
        let cache = &mut ctx.cache.block;
        render_kinetic_synced(frame, area, state, timeline, track, cache, effective_ms);
    }
}

fn render_kinetic_synced(
    frame: &mut Frame,
    area: Rect,
    state: &UiState,
    timeline: &CompiledTimeline,
    track: &SyncedTrack,
    cache: &mut BlockStageCache,
    effective_ms: u64,
) {
    if timeline.lines.is_empty() {
        centered_message(
            frame,
            area,
            &state.style,
            "No lyrics found",
            "",
            false,
            None,
        );
        return;
    }

    let style = &state.style;
    let cursor = timeline.evaluate_for_status(effective_ms, state.playback);
    if let Some(waiting) = cursor.waiting {
        if let Some(countdown) = waiting_countdown_lines(
            waiting,
            effective_ms,
            state.terminal_unicode,
            style,
            CountdownSize::Block,
        ) {
            let end = countdown.len();
            render_lines(frame, area, countdown, Some(0), Some(end));
        }
        return;
    }

    let anchor = cursor
        .active_line_idx
        .or_else(|| timeline.anchor_line_index(effective_ms))
        .unwrap_or(0);
    let compiled = &timeline.lines[anchor];
    let active = cursor.active_line_idx == Some(anchor);
    let holding = active && effective_ms > compiled.end_ms;
    let word_highlight = state.effective_word_highlight();
    let viewport_width = timeline.context.viewport_width;
    let alignment = style.alignment(compiled.role, viewport_width);

    let mut below_lines = Vec::new();
    if style.annotations_show {
        for annotation in &compiled.wrapped_annotations {
            below_lines.push(
                Line::from(Span::styled(annotation.clone(), style.annotation_style()))
                    .alignment(alignment),
            );
        }
    }

    if let Some(bg) = timeline.lines.get(anchor + 1).filter(|next| {
        next.role == LineRole::BackgroundVocal
            && next.start_ms <= compiled.end_ms
            && next.end_ms >= compiled.start_ms
    }) {
        let bg_active = effective_ms >= bg.start_ms && effective_ms <= bg.end_ms;
        let bg_style = style.line_style(LineRole::BackgroundVocal, 0, bg_active);
        for sub in &bg.wrapped_main {
            below_lines
                .push(Line::from(Span::styled(sub.text.clone(), bg_style)).alignment(alignment));
        }
    }

    let min_stage_rows = usize::from(area.height.min(5));
    let max_below = usize::from(area.height).saturating_sub(min_stage_rows);
    below_lines.truncate(max_below);

    let source_text = line_source_text(track, compiled, timeline.context.instrumental_marker);
    let source_text = source_text.as_ref();
    let mut chunk_text = source_text;
    let mut chunk_range = (0, source_text.len());
    let mut chunk_span: Option<(u64, u64)> = None;
    if !compiled.phrases.is_empty() {
        let phrase_idx = if holding || (!active && effective_ms >= compiled.start_ms) {
            compiled.phrases.len() - 1
        } else if active {
            compiled.active_phrase_index(effective_ms).unwrap_or(0)
        } else {
            0
        };
        let phrase = &compiled.phrases[phrase_idx];
        let (trimmed, range) = phrase.trimmed_slice(source_text);
        if !trimmed.is_empty() {
            chunk_text = trimmed;
            chunk_range = range;
            chunk_span = Some((phrase.start_ms, phrase.end_ms));
        }
    }

    let wrap_width = timeline.context.wrap_width().min(area.width).max(1);
    let below_rows = u16::try_from(below_lines.len()).unwrap_or(u16::MAX);
    let max_height = area.height.saturating_sub(below_rows).max(1);

    cache.prepare(timeline.context.options_hash(), Some(anchor));
    // Under single_line, a phrase that cannot fit one row is split into
    // smaller chunks shown one after another instead of dropping to plain
    // text. Decide with a probe layout first so the cache is only consulted
    // once per frame for the text that actually renders.
    let fits_one_row = {
        let probe = cache.layout_for_with_options(
            anchor,
            chunk_text,
            wrap_width,
            max_height,
            timeline.context.ambiguous_width,
            style.big_text_max_size,
            style.big_text_overflow,
        );
        probe.font_size != BlockFontSize::Plain
    };
    if style.big_text_overflow == BigTextOverflow::SingleLine
        && style.big_text_max_size != BigTextMaxSize::Plain
        && !fits_one_row
        && let Some((phrase_start, phrase_end)) = chunk_span
    {
        let sub_ranges =
            single_line_chunks(chunk_text, wrap_width, timeline.context.ambiguous_width);
        if sub_ranges.len() > 1 {
            let base = chunk_range.0;
            let absolute: Vec<(usize, usize)> = sub_ranges
                .iter()
                .map(|(start, end)| (base + start, base + end))
                .collect();
            let index = active_sub_chunk(
                chunk_text,
                &absolute,
                (phrase_start, phrase_end),
                &compiled.words,
                effective_ms,
            );
            let (start, end) = absolute[index];
            chunk_text = &source_text[start..end];
            chunk_range = (start, end);
        }
    }

    let layout = cache.layout_for_with_options(
        anchor,
        chunk_text,
        wrap_width,
        max_height,
        timeline.context.ambiguous_width,
        style.big_text_max_size,
        style.big_text_overflow,
    );

    let mut lines = render_phrase_lines(
        layout,
        chunk_range,
        BlockRenderContext {
            words: &compiled.words,
            cursor,
            effective_ms,
            active,
            holding,
            word_highlight: word_highlight && compiled.role != LineRole::InstrumentalBreak,
            wipe: word_highlight
                && style.highlight_mode == HighlightMode::Wipe
                && !style.reduced_motion
                && compiled.role != LineRole::InstrumentalBreak,
            role: compiled.role,
            style,
            styles: stage::block_styles(compiled, word_highlight, 0, style),
            alignment,
        },
    );
    let active_end = Some(lines.len());
    lines.extend(below_lines);

    render_lines(frame, area, lines, Some(0), active_end);
}

/// Picks the sub-chunk of a phrase that should be on stage at `now`.
/// Timings come from the words overlapping each chunk; without word timing
/// the phrase span is divided proportionally to display width. A moment in
/// the gap between two chunks keeps the earlier one on stage.
fn active_sub_chunk(
    chunk_text: &str,
    ranges: &[(usize, usize)],
    span: (u64, u64),
    words: &[WordToken],
    now: u64,
) -> usize {
    if ranges.is_empty() {
        return 0;
    }
    let (span_start, span_end) = span;
    let duration = span_end.saturating_sub(span_start).max(1);

    let timings: Vec<(u64, u64)> = if words.is_empty() {
        let total: u64 = ranges
            .iter()
            .map(|(start, end)| u64::from(display_width(&chunk_text[*start..*end]).max(1)))
            .sum();
        let mut elapsed = 0u64;
        ranges
            .iter()
            .map(|(start, end)| {
                let share = u64::from(display_width(&chunk_text[*start..*end]).max(1)) * duration
                    / total.max(1);
                let begin = span_start + elapsed;
                elapsed += share;
                (begin, span_start + elapsed)
            })
            .collect()
    } else {
        ranges
            .iter()
            .map(|(start, end)| {
                let overlapping = words
                    .iter()
                    .filter(|word| word.byte_range.0 < *end && word.byte_range.1 > *start);
                let begin = overlapping
                    .clone()
                    .map(|word| word.start_ms)
                    .min()
                    .unwrap_or(span_start);
                let finish = overlapping
                    .map(|word| word.end_ms)
                    .max()
                    .unwrap_or(span_end);
                let begin = begin.max(span_start);
                (begin, finish.min(span_end).max(begin))
            })
            .collect()
    };

    timings
        .partition_point(|(start, _)| *start <= now)
        .saturating_sub(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn word_backed_sub_chunks_follow_the_active_word() {
        let text = "Rivers carry every light";
        // Chunks "Rivers carry" (0..12) and "every light" (13..24).
        let ranges = vec![(0, 12), (13, 24)];
        let words = vec![
            WordToken {
                byte_range: (0, 6),
                start_ms: 1_000,
                end_ms: 1_800,
                display_width: 6,
            },
            WordToken {
                byte_range: (7, 12),
                start_ms: 1_800,
                end_ms: 2_600,
                display_width: 5,
            },
            WordToken {
                byte_range: (13, 18),
                start_ms: 2_600,
                end_ms: 3_200,
                display_width: 5,
            },
            WordToken {
                byte_range: (19, 24),
                start_ms: 3_200,
                end_ms: 4_000,
                display_width: 5,
            },
        ];

        assert_eq!(
            active_sub_chunk(text, &ranges, (1_000, 4_000), &words, 1_400),
            0
        );
        assert_eq!(
            active_sub_chunk(text, &ranges, (1_000, 4_000), &words, 2_500),
            0
        );
        assert_eq!(
            active_sub_chunk(text, &ranges, (1_000, 4_000), &words, 2_600),
            1
        );
        assert_eq!(
            active_sub_chunk(text, &ranges, (1_000, 4_000), &words, 9_999),
            1
        );
        assert_eq!(
            active_sub_chunk(text, &ranges, (1_000, 4_000), &words, 0),
            0
        );
    }

    #[test]
    fn without_words_the_span_is_divided_proportionally() {
        let text = "aaaaaaaaaa bb";
        let ranges = vec![(0, 10), (11, 13)];

        // The first chunk is five times wider, so it owns five sixths of the
        // span: 0..5_000. A moment at 4_000 stays on the first chunk.
        assert_eq!(active_sub_chunk(text, &ranges, (0, 6_000), &[], 4_000), 0);
        assert_eq!(active_sub_chunk(text, &ranges, (0, 6_000), &[], 5_500), 1);
    }
}

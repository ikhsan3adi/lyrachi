use ratatui::{
    Frame,
    layout::Rect,
    text::{Line, Span},
};

use crate::compiler::timeline::CompiledTimeline;
use crate::core::model::{LineRole, SyncedTrack};
use crate::style::HighlightMode;
use crate::ui::UiState;
use crate::ui::render::{
    CountdownSize, ModeRenderer, RenderContext, centered_message, line_source_text, render_lines,
    waiting_countdown_lines,
};

use super::block::{
    BlockStageCache,
    render::{BlockRenderContext, render_phrase_lines},
    stage,
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
        centered_message(frame, area, "No lyrics found", "");
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
    let (chunk_text, chunk_range) = if compiled.phrases.is_empty() {
        (source_text, (0, source_text.len()))
    } else {
        let phrase_idx = if holding || (!active && effective_ms >= compiled.start_ms) {
            compiled.phrases.len() - 1
        } else if active {
            compiled.active_phrase_index(effective_ms).unwrap_or(0)
        } else {
            0
        };
        let (trimmed, range) = compiled.phrases[phrase_idx].trimmed_slice(source_text);
        if trimmed.is_empty() {
            (source_text, (0, source_text.len()))
        } else {
            (trimmed, range)
        }
    };

    let wrap_width = timeline.context.wrap_width().min(area.width).max(1);
    let below_rows = u16::try_from(below_lines.len()).unwrap_or(u16::MAX);
    let max_height = area.height.saturating_sub(below_rows).max(1);

    cache.prepare(timeline.context.options_hash(), Some(anchor));
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

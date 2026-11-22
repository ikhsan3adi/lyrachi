use ratatui::{
    Frame,
    layout::Rect,
    text::{Line, Span},
};

use crate::compiler::timeline::{CompiledLine, CompiledTimeline, WaitKind};
use crate::core::model::{LineRole, SyncedTrack};
use crate::style::{BigTextMaxSize, HighlightMode, ResolvedStyle};
use crate::ui::UiState;
use crate::ui::render::{
    BLOCK_COUNTDOWN_ROWS, CountdownSize, ModeRenderer, RenderContext, WordState, centered_message,
    compiled_is_waiting_break, has_waiting_break_slot, line_source_text, render_lines,
    waiting_countdown_lines, word_state_style,
};

use super::{
    BLOCK_WINDOW_AFTER, BLOCK_WINDOW_BEFORE, BlockFontSize, BlockStageCache,
    render::{BlockRenderContext, BlockStyles, render_lines as render_block_lines},
};

const ACTIVE_GAP_ROWS: usize = 2;
const BLOCK_CONTEXT_MAX_ROWS: u16 = 4;
const BLOCK_VOICE_PLAIN_MIN_WIDTH: u16 = 40;
const BLOCK_CONTEXT_BLOCK_MIN_HEIGHT: u16 = 32;
const BLOCK_CONTEXT_BLOCK_MIN_WIDTH: u16 = 100;

#[derive(Debug)]
struct BlockLineMeta {
    index: usize,
    active: bool,
    focus: bool,
    countdown_slot: bool,
    max_height: u16,
    layout_max_size: BigTextMaxSize,
    rows: usize,
}

pub(crate) struct BlockRenderer;

impl ModeRenderer for BlockRenderer {
    fn render(&self, frame: &mut Frame, area: Rect, ctx: &mut RenderContext<'_>) {
        let state = ctx.state;
        let timeline = ctx.timeline;
        let track = ctx.track;
        let effective_ms = ctx.effective_ms;
        let cache = &mut ctx.cache.block;
        render_block_synced(frame, area, state, timeline, track, cache, effective_ms);
    }
}

fn render_block_synced(
    frame: &mut Frame,
    area: Rect,
    state: &UiState,
    timeline: &CompiledTimeline,
    track: &SyncedTrack,
    cache: &mut BlockStageCache,
    effective_ms: u64,
) {
    if timeline.lines.is_empty() {
        centered_message(frame, area, &state.style, "No lyrics found", "", false);
        return;
    }

    let word_highlight = state.effective_word_highlight();
    let cursor = timeline.evaluate_for_status(effective_ms, state.playback);
    let anchor = timeline.anchor_line_index(effective_ms).unwrap_or(0);
    let wrap_width = timeline.context.wrap_width().min(area.width).max(1);
    cache.prepare(timeline.context.options_hash(), Some(anchor));

    let start = anchor.saturating_sub(BLOCK_WINDOW_BEFORE);
    let end = anchor
        .saturating_add(BLOCK_WINDOW_AFTER)
        .saturating_add(1)
        .min(timeline.lines.len());
    let has_break_slot = has_waiting_break_slot(&timeline.lines[start..end], cursor.waiting);
    let mut metas = Vec::new();
    let context_plain = block_context_plain(area);
    let mut context_needs_plain = context_plain;

    for (index, compiled) in timeline.lines.iter().enumerate().take(end).skip(start) {
        let countdown_slot = cursor
            .waiting
            .is_some_and(|waiting| compiled_is_waiting_break(compiled, waiting));
        let active = cursor.active_line_idx == Some(index);
        let focus = index == anchor && cursor.waiting.is_none();
        let annotation_rows = if state.style.annotations_show {
            compiled.wrapped_annotations.len()
        } else {
            0
        };
        let max_height = block_line_height(area, compiled.role, focus, annotation_rows);
        let is_context = !countdown_slot && !active && !focus;
        if is_context && !context_plain {
            let layout = cache.layout_for_with_options(
                index,
                line_source_text(track, compiled, timeline.context.instrumental_marker).as_ref(),
                wrap_width,
                max_height,
                timeline.context.ambiguous_width,
                state.style.big_text_max_size,
                state.style.big_text_overflow,
            );
            context_needs_plain |= layout.font_size == BlockFontSize::Plain;
        }
        metas.push(BlockLineMeta {
            index,
            active,
            focus,
            countdown_slot,
            max_height,
            layout_max_size: state.style.big_text_max_size,
            rows: 0,
        });
    }

    let mut total_rows = if let Some(waiting) = cursor.waiting
        && waiting.kind == WaitKind::Intro
        && !has_break_slot
    {
        waiting_countdown_lines(
            waiting,
            effective_ms,
            state.terminal_unicode,
            &state.style,
            CountdownSize::Block,
        )
        .map_or(0, |lines| lines.len())
    } else {
        0
    };

    for meta in &mut metas {
        if context_needs_plain && !meta.active && !meta.focus && !meta.countdown_slot {
            meta.layout_max_size = BigTextMaxSize::Plain;
        }
        meta.rows = if meta.countdown_slot {
            usize::from(state.instrumental_countdown_enabled()) * BLOCK_COUNTDOWN_ROWS
        } else {
            let compiled = &timeline.lines[meta.index];
            let annotation_rows = if state.style.annotations_show {
                compiled.wrapped_annotations.len()
            } else {
                0
            };
            let layout = cache.layout_for_with_options(
                meta.index,
                line_source_text(track, compiled, timeline.context.instrumental_marker).as_ref(),
                wrap_width,
                meta.max_height,
                timeline.context.ambiguous_width,
                meta.layout_max_size,
                state.style.big_text_overflow,
            );
            usize::from(layout.height).saturating_add(annotation_rows)
        };
        total_rows = total_rows.saturating_add(meta.rows);
        if meta.focus || (meta.countdown_slot && state.instrumental_countdown_enabled()) {
            total_rows = total_rows.saturating_add(ACTIVE_GAP_ROWS.saturating_mul(2));
        }
    }

    while total_rows > usize::from(area.height) && metas.len() > 1 {
        let Some(farthest) = metas
            .iter()
            .enumerate()
            .filter(|(_, meta)| !meta.active && !meta.focus && !meta.countdown_slot)
            .max_by_key(|(_, meta)| meta.index.abs_diff(anchor))
            .map(|(position, _)| position)
        else {
            break;
        };
        total_rows = total_rows.saturating_sub(metas.remove(farthest).rows);
    }

    let style = &state.style;
    let viewport_width = timeline.context.viewport_width;
    let mut lines = Vec::new();
    let mut active_start = None;
    let mut active_end = None;
    let mut countdown_rendered = false;

    if let Some(waiting) = cursor.waiting
        && waiting.kind == WaitKind::Intro
        && !has_break_slot
        && let Some(countdown) = waiting_countdown_lines(
            waiting,
            effective_ms,
            state.terminal_unicode,
            style,
            CountdownSize::Block,
        )
    {
        active_start = Some(lines.len());
        lines.extend(countdown);
        active_end = Some(lines.len());
        lines.extend(std::iter::repeat_with(Line::default).take(ACTIVE_GAP_ROWS));
        countdown_rendered = true;
    }

    for meta in metas {
        let compiled = &timeline.lines[meta.index];
        if meta.countdown_slot
            && let Some(waiting) = cursor.waiting
            && !countdown_rendered
        {
            if let Some(countdown) = waiting_countdown_lines(
                waiting,
                effective_ms,
                state.terminal_unicode,
                style,
                CountdownSize::Block,
            ) {
                if !lines.is_empty() {
                    lines.extend(std::iter::repeat_with(Line::default).take(ACTIVE_GAP_ROWS));
                }
                active_start = Some(lines.len());
                lines.extend(countdown);
                active_end = Some(lines.len());
                lines.extend(std::iter::repeat_with(Line::default).take(ACTIVE_GAP_ROWS));
            }
            countdown_rendered = true;
            continue;
        }

        let text = line_source_text(track, compiled, timeline.context.instrumental_marker);
        let layout = cache.layout_for_with_options(
            meta.index,
            text.as_ref(),
            wrap_width,
            meta.max_height,
            timeline.context.ambiguous_width,
            meta.layout_max_size,
            style.big_text_overflow,
        );
        let distance = meta.index.abs_diff(anchor);
        let alignment = style.alignment(compiled.role, viewport_width);
        let is_focus = meta.focus && cursor.waiting.is_none();
        if is_focus && !lines.is_empty() {
            lines.extend(std::iter::repeat_with(Line::default).take(ACTIVE_GAP_ROWS));
        }
        if is_focus {
            active_start = Some(lines.len());
        }
        lines.extend(render_block_lines(
            layout,
            BlockRenderContext {
                words: &compiled.words,
                cursor,
                effective_ms,
                active: meta.active,
                holding: meta.active && effective_ms > compiled.end_ms,
                word_highlight: word_highlight && compiled.role != LineRole::InstrumentalBreak,
                wipe: word_highlight
                    && style.highlight_mode == HighlightMode::Wipe
                    && !style.reduced_motion
                    && compiled.role != LineRole::InstrumentalBreak,
                role: compiled.role,
                style,
                styles: block_styles(compiled, word_highlight, distance, style),
                alignment,
            },
        ));
        if style.annotations_show {
            for annotation in &compiled.wrapped_annotations {
                lines.push(
                    Line::from(Span::styled(annotation.clone(), style.annotation_style()))
                        .alignment(alignment),
                );
            }
        }
        if is_focus {
            active_end = Some(lines.len());
            lines.extend(std::iter::repeat_with(Line::default).take(ACTIVE_GAP_ROWS));
        }

        if let Some(waiting) = cursor.waiting
            && !countdown_rendered
            && !has_break_slot
            && meta.index == anchor
            && matches!(waiting.kind, WaitKind::Instrumental | WaitKind::Outro)
            && let Some(countdown) = waiting_countdown_lines(
                waiting,
                effective_ms,
                state.terminal_unicode,
                style,
                CountdownSize::Block,
            )
        {
            if !lines.is_empty() {
                lines.extend(std::iter::repeat_with(Line::default).take(ACTIVE_GAP_ROWS));
            }
            active_start = Some(lines.len());
            lines.extend(countdown);
            active_end = Some(lines.len());
            lines.extend(std::iter::repeat_with(Line::default).take(ACTIVE_GAP_ROWS));
            countdown_rendered = true;
        }
    }

    render_lines(frame, area, lines, active_start, active_end);
}

fn block_context_plain(area: Rect) -> bool {
    area.height < BLOCK_CONTEXT_BLOCK_MIN_HEIGHT || area.width < BLOCK_CONTEXT_BLOCK_MIN_WIDTH
}

fn block_line_height(area: Rect, role: LineRole, focus: bool, annotation_rows: usize) -> u16 {
    let max_rows = if focus {
        let annotation_rows = u16::try_from(annotation_rows).unwrap_or(u16::MAX);
        area.height.saturating_sub(annotation_rows).max(1)
    } else {
        BLOCK_CONTEXT_MAX_ROWS.min(area.height.max(1))
    };

    if matches!(role, LineRole::DuetSecondary | LineRole::BackgroundVocal)
        && area.width < BLOCK_VOICE_PLAIN_MIN_WIDTH
    {
        1
    } else {
        max_rows
    }
}

pub(crate) fn block_styles(
    compiled: &CompiledLine,
    word_highlight: bool,
    distance: usize,
    style: &ResolvedStyle,
) -> BlockStyles {
    let active = if compiled.role == LineRole::InstrumentalBreak
        || (word_highlight
            && compiled.words.is_empty()
            && compiled.role == LineRole::BackgroundVocal)
    {
        style.line_style(compiled.role, 0, true)
    } else {
        word_state_style(style, compiled.role, WordState::Active)
    };

    BlockStyles {
        active,
        held: style.line_style(compiled.role, 0, true),
        sung: word_state_style(style, compiled.role, WordState::Sung),
        unsung: word_state_style(style, compiled.role, WordState::Unsung),
        inactive: style.line_style(compiled.role, distance, false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_context_font_follows_viewport_geometry() {
        assert!(block_context_plain(Rect::new(0, 0, 80, 24)));
        assert!(block_context_plain(Rect::new(0, 0, 120, 24)));
        assert!(block_context_plain(Rect::new(0, 0, 80, 40)));
        assert!(!block_context_plain(Rect::new(0, 0, 120, 40)));
    }
}

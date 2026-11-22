use ratatui::{
    Frame,
    layout::{Alignment, Rect},
    style::Style,
    text::{Line, Span},
    widgets::Paragraph,
};
use unicode_segmentation::UnicodeSegmentation;

use crate::compiler::timeline::{
    ActiveStageCursor, CompiledLine, CompiledTimeline, WaitKind, WrappedSubLine,
};
use crate::core::model::{LineRole, WordToken};
use crate::style::{AnnotationPosition, HighlightMode, ResolvedStyle};

use super::{
    UiState,
    render::{
        CountdownSize, ModeRenderer, RenderContext, WordState, centered_message,
        compiled_is_waiting_break, has_waiting_break_slot, render_lines, waiting_countdown_lines,
        word_state_style,
    },
};

#[derive(Debug, Clone, Copy)]
pub(crate) struct StageRender<'a> {
    cursor: ActiveStageCursor,
    effective_ms: u64,
    plain: bool,
    word_highlight: bool,
    viewport_width: u16,
    style: &'a ResolvedStyle,
}

pub(crate) struct NormalRenderer;

impl ModeRenderer for NormalRenderer {
    fn render(&self, frame: &mut Frame, area: Rect, ctx: &mut RenderContext<'_>) {
        let state = ctx.state;
        let timeline = ctx.timeline;
        let effective_ms = ctx.effective_ms;
        render_synced(frame, area, state, timeline, effective_ms);
    }
}

fn render_synced(
    frame: &mut Frame,
    area: Rect,
    state: &UiState,
    timeline: &CompiledTimeline,
    effective_ms: u64,
) {
    if timeline.lines.is_empty() {
        centered_message(frame, area, &state.style, "No lyrics found", "", false);
        return;
    }

    let cursor = timeline.evaluate_for_status(effective_ms, state.playback);
    let total = timeline.lines.len();
    let anchor = timeline.anchor_line_index(effective_ms).unwrap_or(0);
    let visible_prev = state.style.visible_prev;
    let visible_next = state.style.visible_next;
    let window_len = visible_prev.saturating_add(visible_next).max(1);
    let start = anchor.saturating_sub(visible_prev);
    let end = (anchor + visible_next + 1)
        .max(start + window_len)
        .min(total);
    let stage = StageRender {
        cursor,
        effective_ms,
        plain: false,
        word_highlight: state.effective_word_highlight(),
        viewport_width: timeline.context.viewport_width,
        style: &state.style,
    };
    let mut lines = Vec::new();
    let mut active_start = None;
    let mut active_end = None;
    let mut countdown_rendered = false;
    let spacing = usize::from(state.style.line_spacing);
    let mut last_had_spacing = false;

    let has_break_slot = has_waiting_break_slot(&timeline.lines[start..end], cursor.waiting);

    if let Some(waiting) = cursor.waiting
        && waiting.kind == WaitKind::Intro
        && !has_break_slot
        && let Some(countdown) = waiting_countdown_lines(
            waiting,
            effective_ms,
            state.terminal_unicode,
            &state.style,
            CountdownSize::Line,
        )
    {
        active_start = Some(lines.len());
        lines.extend(countdown);
        active_end = Some(lines.len());
        if end > start {
            lines.extend(std::iter::repeat_with(Line::default).take(spacing));
            last_had_spacing = true;
        }
        countdown_rendered = true;
    }

    for (index, compiled) in timeline.lines.iter().enumerate().take(end).skip(start) {
        if let Some(waiting) = cursor.waiting
            && !countdown_rendered
            && compiled_is_waiting_break(compiled, waiting)
        {
            if let Some(countdown) = waiting_countdown_lines(
                waiting,
                effective_ms,
                state.terminal_unicode,
                &state.style,
                CountdownSize::Line,
            ) {
                if !last_had_spacing && !lines.is_empty() {
                    lines.extend(std::iter::repeat_with(Line::default).take(spacing));
                }
                active_start = Some(lines.len());
                lines.extend(countdown);
                active_end = Some(lines.len());
                if index + 1 < end {
                    lines.extend(std::iter::repeat_with(Line::default).take(spacing));
                    last_had_spacing = true;
                } else {
                    last_had_spacing = false;
                }
            }
            countdown_rendered = true;
            continue;
        }

        let active = cursor.active_line_idx == Some(index);
        let distance = index.abs_diff(anchor);
        let is_focus = index == anchor && cursor.waiting.is_none();
        if is_focus {
            active_start = Some(lines.len());
        }
        let add_spacing = index + 1 < end;
        push_compiled_line(&mut lines, compiled, stage, active, distance, add_spacing);
        last_had_spacing = add_spacing;
        if is_focus {
            active_end = Some(lines.len());
        }

        if let Some(waiting) = cursor.waiting
            && !countdown_rendered
            && !has_break_slot
            && index == anchor
            && matches!(waiting.kind, WaitKind::Instrumental | WaitKind::Outro)
            && let Some(countdown) = waiting_countdown_lines(
                waiting,
                effective_ms,
                state.terminal_unicode,
                &state.style,
                CountdownSize::Line,
            )
        {
            if !last_had_spacing && !lines.is_empty() {
                lines.extend(std::iter::repeat_with(Line::default).take(spacing));
            }
            active_start = Some(lines.len());
            lines.extend(countdown);
            active_end = Some(lines.len());
            if index + 1 < end {
                lines.extend(std::iter::repeat_with(Line::default).take(spacing));
                last_had_spacing = true;
            } else {
                last_had_spacing = false;
            }
            countdown_rendered = true;
        }
    }

    render_lines(frame, area, lines, active_start, active_end);
}

pub(crate) fn render_unsynced(frame: &mut Frame, area: Rect, ctx: &mut RenderContext<'_>) {
    let state = ctx.state;
    let timeline = ctx.timeline;
    let mut lines = Vec::new();
    let start = state.scroll.min(timeline.lines.len());
    let stage = StageRender {
        cursor: ActiveStageCursor {
            active_line_idx: None,
            active_word_idx: None,
            progress: 0.0,
            waiting: None,
        },
        effective_ms: 0,
        plain: true,
        word_highlight: state.effective_word_highlight(),
        viewport_width: timeline.context.viewport_width,
        style: &state.style,
    };

    for (offset, compiled) in timeline.lines[start..].iter().enumerate() {
        let add_spacing = start + offset + 1 < timeline.lines.len();
        push_compiled_line(&mut lines, compiled, stage, false, 1, add_spacing);
        if lines.len() >= usize::from(area.height) {
            break;
        }
    }

    lines.truncate(usize::from(area.height));
    frame.render_widget(Paragraph::new(lines).alignment(Alignment::Center), area);
}

fn active_line_style(style: &ResolvedStyle, role: LineRole) -> Style {
    match role {
        LineRole::Primary | LineRole::DuetSecondary => {
            word_state_style(style, role, WordState::Active)
        }
        LineRole::BackgroundVocal | LineRole::InstrumentalBreak => style.line_style(role, 0, true),
    }
}

fn push_compiled_line(
    lines: &mut Vec<Line<'static>>,
    compiled: &CompiledLine,
    stage: StageRender<'_>,
    active: bool,
    distance: usize,
    add_spacing: bool,
) {
    let alignment = stage.style.alignment(compiled.role, stage.viewport_width);
    let subline_count = compiled.wrapped_main.len();
    for (sub_idx, subline) in compiled.wrapped_main.iter().enumerate() {
        let mut spans = spans_for_subline(compiled, subline, active, distance, stage);
        if sub_idx == 0 {
            let prefix = if active {
                stage.style.active_prefix.as_str()
            } else {
                stage.style.inactive_prefix.as_str()
            };
            if !prefix.is_empty() {
                spans.insert(
                    0,
                    Span::styled(
                        prefix.to_string(),
                        stage.style.line_style(compiled.role, distance, active),
                    ),
                );
            }
        }
        if stage.style.annotations_show
            && stage.style.annotations_position == AnnotationPosition::Inline
            && sub_idx + 1 == subline_count
            && !compiled.wrapped_annotations.is_empty()
        {
            spans.push(Span::styled(
                format!("  {}", compiled.wrapped_annotations.join(" ")),
                stage.style.annotation_style(),
            ));
        }
        lines.push(Line::from(spans).alignment(alignment));
    }

    if stage.style.annotations_show && stage.style.annotations_position == AnnotationPosition::Below
    {
        for annotation in &compiled.wrapped_annotations {
            lines.push(
                Line::from(Span::styled(
                    annotation.clone(),
                    stage.style.annotation_style(),
                ))
                .alignment(alignment),
            );
        }
    }

    if add_spacing {
        for _ in 0..stage.style.line_spacing {
            lines.push(Line::default());
        }
    }
}

fn held_line_spans(
    compiled: &CompiledLine,
    subline: &WrappedSubLine,
    stage: StageRender<'_>,
) -> Vec<Span<'static>> {
    let word_held = stage.word_highlight && !compiled.words.is_empty();
    let held_style = if word_held {
        word_state_style(stage.style, compiled.role, WordState::Sung)
    } else {
        stage.style.line_style(compiled.role, 0, true)
    };
    if !word_held {
        return vec![Span::styled(subline.text.clone(), held_style)];
    }

    let last_word_idx = compiled.words.len() - 1;
    let active_style = word_state_style(stage.style, compiled.role, WordState::Active);
    let (sub_start, sub_end) = subline.source_byte_range;
    let mut spans = Vec::new();
    let mut cursor = sub_start;

    for (index, word) in compiled.words.iter().enumerate() {
        let word_start = word.byte_range.0.max(sub_start).max(cursor);
        let word_end = word.byte_range.1.min(sub_end);
        if word_end <= word_start {
            continue;
        }
        if cursor < word_start {
            push_span(&mut spans, subline, cursor, word_start, held_style);
        }
        let style = if index == last_word_idx {
            active_style
        } else {
            held_style
        };
        push_span(&mut spans, subline, word_start, word_end, style);
        cursor = word_end;
    }

    if cursor < sub_end {
        push_span(&mut spans, subline, cursor, sub_end, held_style);
    }
    if spans.is_empty() {
        spans.push(Span::styled(subline.text.clone(), held_style));
    }
    spans
}

fn spans_for_subline(
    compiled: &CompiledLine,
    subline: &WrappedSubLine,
    active_line: bool,
    distance: usize,
    stage: StageRender<'_>,
) -> Vec<Span<'static>> {
    if stage.plain {
        return vec![Span::raw(subline.text.clone())];
    }
    if !active_line {
        return vec![Span::styled(
            subline.text.clone(),
            stage.style.line_style(compiled.role, distance, false),
        )];
    }
    if compiled.role == LineRole::InstrumentalBreak {
        return vec![Span::styled(
            subline.text.clone(),
            stage.style.line_style(compiled.role, 0, true),
        )];
    }
    if stage.effective_ms > compiled.end_ms {
        return held_line_spans(compiled, subline, stage);
    }
    if !stage.word_highlight {
        return vec![Span::styled(
            subline.text.clone(),
            active_line_style(stage.style, compiled.role),
        )];
    }
    if compiled.words.is_empty() {
        return vec![Span::styled(
            subline.text.clone(),
            active_line_style(stage.style, compiled.role),
        )];
    }

    let active_word_idx = stage.cursor.active_word_idx;
    let effective_ms = stage.effective_ms;

    let (sub_start, sub_end) = subline.source_byte_range;
    let mut spans = Vec::new();
    let mut cursor = sub_start;

    for (index, word) in compiled.words.iter().enumerate() {
        let word_start = word.byte_range.0.max(sub_start).max(cursor);
        let word_end = word.byte_range.1.min(sub_end);
        if word_end <= word_start {
            continue;
        }

        if cursor < word_start {
            let gap_state = if effective_ms >= word.start_ms {
                WordState::Sung
            } else {
                WordState::Unsung
            };
            push_span(
                &mut spans,
                subline,
                cursor,
                word_start,
                word_state_style(stage.style, compiled.role, gap_state),
            );
        }

        let word_state = if Some(index) == active_word_idx {
            WordState::Active
        } else if active_word_idx.is_some_and(|active| index < active) || effective_ms > word.end_ms
        {
            WordState::Sung
        } else {
            WordState::Unsung
        };
        if Some(index) == active_word_idx
            && stage.style.highlight_mode == HighlightMode::Wipe
            && !stage.style.reduced_motion
        {
            push_wipe_word_spans(
                &mut spans,
                subline,
                word_start,
                word_end,
                stage.cursor.progress,
                stage.style,
                compiled.role,
            );
        } else {
            push_span(
                &mut spans,
                subline,
                word_start,
                word_end,
                word_state_style(stage.style, compiled.role, word_state),
            );
        }
        cursor = word_end;
    }

    if cursor < sub_end {
        let trailing_style = trailing_gap_style(
            compiled.role,
            stage.style,
            &compiled.words,
            cursor,
            effective_ms,
        );
        push_span(&mut spans, subline, cursor, sub_end, trailing_style);
    }
    if spans.is_empty() {
        spans.push(Span::styled(
            subline.text.clone(),
            trailing_gap_style(
                compiled.role,
                stage.style,
                &compiled.words,
                sub_start,
                effective_ms,
            ),
        ));
    }

    spans
}

#[allow(clippy::too_many_arguments)]
fn push_wipe_word_spans(
    spans: &mut Vec<Span<'static>>,
    subline: &WrappedSubLine,
    word_start: usize,
    word_end: usize,
    progress: f32,
    style: &ResolvedStyle,
    role: LineRole,
) {
    let (sub_start, _) = subline.source_byte_range;
    let Some(text) = subline
        .text
        .get(word_start.saturating_sub(sub_start)..word_end.saturating_sub(sub_start))
    else {
        return;
    };
    let graphemes = text
        .grapheme_indices(true)
        .map(|(start, grapheme)| (start, start + grapheme.len()))
        .collect::<Vec<_>>();
    if graphemes.is_empty() {
        return;
    }

    let head = progress.clamp(0.0, 1.0) * graphemes.len() as f32;
    let head_index = head.floor() as usize;
    let trail = f32::from(style.highlight_trail);
    for (ordinal, (start, end)) in graphemes.into_iter().enumerate() {
        let distance = head - (ordinal as f32 + 0.5);
        let cell_style = if ordinal == head_index {
            word_state_style(style, role, WordState::Active)
        } else if distance < 0.0 {
            word_state_style(style, role, WordState::Unsung)
        } else if trail > 0.0 && distance <= trail {
            style.wipe_style(role, distance / trail)
        } else {
            word_state_style(style, role, WordState::Sung)
        };
        push_span(
            spans,
            subline,
            word_start + start,
            word_start + end,
            cell_style,
        );
    }
}

fn trailing_gap_style(
    role: LineRole,
    style: &ResolvedStyle,
    words: &[WordToken],
    byte_pos: usize,
    effective_ms: u64,
) -> Style {
    let state = if let Some(next_word) = words.iter().find(|word| word.byte_range.0 >= byte_pos) {
        if effective_ms >= next_word.start_ms {
            WordState::Sung
        } else {
            WordState::Unsung
        }
    } else if words.last().is_some_and(|last| effective_ms > last.end_ms) {
        WordState::Sung
    } else {
        WordState::Unsung
    };
    word_state_style(style, role, state)
}

fn push_span(
    spans: &mut Vec<Span<'static>>,
    subline: &WrappedSubLine,
    start: usize,
    end: usize,
    style: Style,
) {
    let (sub_start, sub_end) = subline.source_byte_range;
    let start = start.max(sub_start);
    let end = end.min(sub_end);
    if start >= end {
        return;
    }
    let local_start = start - sub_start;
    let local_end = end - sub_start;
    let Some(text) = subline.text.get(local_start..local_end) else {
        return;
    };
    spans.push(Span::styled(text.to_string(), style));
}

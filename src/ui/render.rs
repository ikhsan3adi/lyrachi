use std::borrow::Cow;
use std::time::Instant;

use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Paragraph, Wrap},
};
use unicode_segmentation::UnicodeSegmentation;

use crate::compiler::timeline::{
    ActiveStageCursor, CompiledLine, CompiledTimeline, WaitKind, WaitingGap, WrappedSubLine,
};
use crate::core::model::{LineRole, SyncedTrack, WordToken};
use crate::media::MediaAvailability;
use crate::style::{
    AnnotationPosition, BigTextMaxSize, HighlightMode, InstrumentalStyle, ResolvedStyle,
};

use super::{
    LyricsView, StatusBarPlacement, UiModal, UiState,
    block::{
        BLOCK_WINDOW_AFTER, BLOCK_WINDOW_BEFORE, BlockFontSize, BlockStageCache,
        render::{
            BlockRenderContext, BlockStyles, render_lines as render_block_lines,
            render_phrase_lines,
        },
    },
    effects::UiEffects,
    help, menu,
    status_bar::{banner_line, render as render_status_bar},
};

const BLOCK_CONTEXT_MAX_ROWS: u16 = 4;
const BLOCK_VOICE_PLAIN_MIN_WIDTH: u16 = 40;
const BLOCK_COUNTDOWN_ROWS: usize = 3;
const ACTIVE_GAP_ROWS: usize = 2;
const BLOCK_CONTEXT_BLOCK_MIN_HEIGHT: u16 = 32;
const BLOCK_CONTEXT_BLOCK_MIN_WIDTH: u16 = 100;

#[derive(Clone, Copy)]
enum CountdownSize {
    Line,
    Block,
}

pub fn render(frame: &mut Frame, state: &UiState, effective_ms: u64) {
    let mut cache = BlockStageCache::default();
    let _ = render_with_cache_area(frame, state, &mut cache, effective_ms);
}

pub fn render_with_cache(
    frame: &mut Frame,
    state: &UiState,
    block_cache: &mut BlockStageCache,
    effective_ms: u64,
) {
    let _ = render_with_cache_area(frame, state, block_cache, effective_ms);
}

pub fn render_with_cache_and_effects(
    frame: &mut Frame,
    state: &UiState,
    block_cache: &mut BlockStageCache,
    effects: &mut UiEffects,
    effective_ms: u64,
) -> Rect {
    effects.observe_state(state, effective_ms);
    let stage_area = render_internal(frame, state, block_cache, effective_ms);
    effects.process_now(Instant::now(), state, frame.buffer_mut(), stage_area);
    stage_area
}

pub fn render_with_cache_area(
    frame: &mut Frame,
    state: &UiState,
    block_cache: &mut BlockStageCache,
    effective_ms: u64,
) -> Rect {
    render_internal(frame, state, block_cache, effective_ms)
}

fn render_internal(
    frame: &mut Frame,
    state: &UiState,
    block_cache: &mut BlockStageCache,
    effective_ms: u64,
) -> Rect {
    let area = frame.area();
    if area.height == 0 || area.width == 0 {
        return area;
    }

    if state.style.bg.is_some() {
        frame.render_widget(Block::default().style(state.style.canvas_style()), area);
    }

    let placement = state.status_bar_placement_for_height(area.height);
    let header_rows = u16::from(state.effective_show_header());
    let top_status_rows = u16::from(placement == Some(StatusBarPlacement::Top));
    let bottom_status_rows = u16::from(placement == Some(StatusBarPlacement::Bottom));
    let banner_rows = u16::from(state.banner_visible());
    let [header, top_status, body, banner, bottom_status] = Layout::vertical([
        Constraint::Length(header_rows),
        Constraint::Length(top_status_rows),
        Constraint::Min(1),
        Constraint::Length(banner_rows),
        Constraint::Length(bottom_status_rows),
    ])
    .areas(area);

    let padding = state.timeline_context().padding_horizontal;
    let stage_area = vertical_padded_area(content_area(body, padding), state.style.padding[0]);
    if header_rows > 0 {
        render_header(frame, content_area(header, padding), state);
    }
    if top_status_rows > 0 {
        render_status_bar(frame, top_status, state);
    }
    render_body(frame, stage_area, state, block_cache, effective_ms);
    if banner_rows > 0
        && let Some(line) = banner_line(state)
    {
        frame.render_widget(Paragraph::new(line), content_area(banner, padding));
    }
    if bottom_status_rows > 0 {
        render_status_bar(frame, bottom_status, state);
    }
    match &state.modal {
        Some(UiModal::Help) => help::render(frame, area, &state.style),
        Some(UiModal::ContextMenu(menu_state)) => {
            menu::render(frame, area, menu_state, &state.style)
        }
        None => {}
    }
    stage_area
}

fn content_area(area: Rect, padding: u16) -> Rect {
    Rect {
        x: area.x.saturating_add(padding),
        y: area.y,
        width: area.width.saturating_sub(padding.saturating_mul(2)),
        height: area.height,
    }
}

fn vertical_padded_area(area: Rect, padding_v: u16) -> Rect {
    if padding_v == 0 || area.height <= 2 {
        return area;
    }
    let inset = padding_v.min((area.height - 1) / 2);
    Rect {
        x: area.x,
        y: area.y.saturating_add(inset),
        width: area.width,
        height: area.height.saturating_sub(inset.saturating_mul(2)),
    }
}

#[derive(Debug, Clone, Copy)]
enum WordState {
    Active,
    Sung,
    Unsung,
}

#[derive(Debug, Clone, Copy)]
struct StageRender<'a> {
    cursor: ActiveStageCursor,
    effective_ms: u64,
    plain: bool,
    word_highlight: bool,
    viewport_width: u16,
    style: &'a ResolvedStyle,
}

fn word_state_style(style: &ResolvedStyle, role: LineRole, word_state: WordState) -> Style {
    match word_state {
        WordState::Active => style.word_style(role, true, false),
        WordState::Sung => style.word_style(role, false, true),
        WordState::Unsung => style.word_style(role, false, false),
    }
}

fn active_line_style(style: &ResolvedStyle, role: LineRole) -> Style {
    match role {
        LineRole::Primary | LineRole::DuetSecondary => {
            word_state_style(style, role, WordState::Active)
        }
        LineRole::BackgroundVocal | LineRole::InstrumentalBreak => style.line_style(role, 0, true),
    }
}

fn render_header(frame: &mut Frame, area: Rect, state: &UiState) {
    let line = if let Some(track) = &state.track {
        if let Some(format) = &state.style.header_format {
            let text = format
                .replace("{title}", &track.title)
                .replace("{artist}", &track.artist);
            let mut style = Style::default().add_modifier(Modifier::BOLD);
            if let Some(fg) = state.style.active_fg {
                style = style.fg(fg);
            }
            Line::from(Span::styled(text, style))
        } else {
            let mut spans = vec![Span::styled(
                track.title.clone(),
                Style::default().add_modifier(Modifier::BOLD),
            )];
            if !track.artist.is_empty() {
                spans.push(Span::raw("  "));
                spans.push(Span::styled(
                    track.artist.clone(),
                    Style::default().add_modifier(Modifier::DIM),
                ));
            }
            Line::from(spans)
        }
    } else {
        Line::from(Span::styled(
            "Lyrachi",
            Style::default().add_modifier(Modifier::BOLD),
        ))
    };

    frame.render_widget(Paragraph::new(line).alignment(Alignment::Center), area);
}

fn render_body(
    frame: &mut Frame,
    area: Rect,
    state: &UiState,
    block_cache: &mut BlockStageCache,
    effective_ms: u64,
) {
    if area.height == 0 || area.width == 0 {
        return;
    }

    match &state.lyrics {
        LyricsView::Idle => match state.media_availability {
            MediaAvailability::Unsupported => centered_message(
                frame,
                area,
                "Media source unavailable",
                "This build has no media backend for this platform yet.",
            ),
            MediaAvailability::Unavailable => centered_message(
                frame,
                area,
                "Media source unavailable",
                "Could not connect to the media session.",
            ),
            MediaAvailability::Available if state.player.is_none() => centered_message(
                frame,
                area,
                "No active player",
                "Start playback in a supported media player.",
            ),
            MediaAvailability::Available => centered_message(
                frame,
                area,
                "No track playing",
                "Play something to load lyrics.",
            ),
        },
        LyricsView::Loading => {
            centered_message(
                frame,
                area,
                "Loading lyrics",
                "Searching local files and online providers.",
            );
        }
        LyricsView::NotFound => {
            centered_message(
                frame,
                area,
                "No lyrics found",
                "Add a local .lrc file or try another track.",
            );
        }
        LyricsView::Error(message) => {
            centered_message(frame, area, "Could not load lyrics", message);
        }
        LyricsView::Loaded { track, .. } if track.is_instrumental => {
            let marker = state.style.instrumental_marker.to_string();
            centered_message(frame, area, &marker, "No lyrics for this track.");
        }
        LyricsView::Loaded { track, timeline }
            if track.granularity == crate::core::model::SyncGranularity::Unsynced =>
        {
            render_unsynced(frame, area, state, timeline);
        }
        LyricsView::Loaded { track, timeline }
            if state.effective_display_mode() == super::DisplayMode::Block =>
        {
            render_block_synced(
                frame,
                area,
                state,
                timeline,
                track,
                block_cache,
                effective_ms,
            );
        }
        LyricsView::Loaded { track, timeline }
            if state.effective_display_mode() == super::DisplayMode::Kinetic =>
        {
            render_kinetic_synced(
                frame,
                area,
                state,
                timeline,
                track,
                block_cache,
                effective_ms,
            );
        }
        LyricsView::Loaded { timeline, .. } => {
            render_synced(frame, area, state, timeline, effective_ms);
        }
    }
}

fn centered_message(frame: &mut Frame, area: Rect, title: &str, detail: &str) {
    let mut lines = vec![Line::from(Span::styled(
        title.to_string(),
        Style::default().add_modifier(Modifier::BOLD),
    ))];
    if !detail.is_empty() {
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            detail.to_string(),
            Style::default().add_modifier(Modifier::DIM),
        )));
    }

    let rows = u16::try_from(lines.len()).unwrap_or(u16::MAX);
    let vertical = Layout::vertical([
        Constraint::Fill(1),
        Constraint::Length(rows.min(area.height)),
        Constraint::Fill(1),
    ])
    .split(area)[1];

    frame.render_widget(
        Paragraph::new(lines)
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: true }),
        vertical,
    );
}

fn render_synced(
    frame: &mut Frame,
    area: Rect,
    state: &UiState,
    timeline: &CompiledTimeline,
    effective_ms: u64,
) {
    if timeline.lines.is_empty() {
        centered_message(frame, area, "No lyrics found", "");
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

fn has_waiting_break_slot(lines: &[CompiledLine], waiting: Option<WaitingGap>) -> bool {
    waiting.is_some_and(|waiting| {
        lines
            .iter()
            .any(|line| compiled_is_waiting_break(line, waiting))
    })
}

fn compiled_is_waiting_break(line: &CompiledLine, waiting: WaitingGap) -> bool {
    line.role == LineRole::InstrumentalBreak
        && line.start_ms >= waiting.start_ms
        && line.end_ms <= waiting.end_ms
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
        centered_message(frame, area, "No lyrics found", "");
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
                block_source_text(track, compiled, timeline.context.instrumental_marker).as_ref(),
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
                block_source_text(track, compiled, timeline.context.instrumental_marker).as_ref(),
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

        let text = block_source_text(track, compiled, timeline.context.instrumental_marker);
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

    let source_text = block_source_text(track, compiled, timeline.context.instrumental_marker);
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
            styles: block_styles(compiled, word_highlight, 0, style),
            alignment,
        },
    );
    let active_end = Some(lines.len());
    lines.extend(below_lines);

    render_lines(frame, area, lines, Some(0), active_end);
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

fn block_styles(
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

fn block_source_text<'a>(
    track: &'a SyncedTrack,
    compiled: &CompiledLine,
    marker: char,
) -> Cow<'a, str> {
    if compiled.role == LineRole::InstrumentalBreak {
        return Cow::Owned(marker.to_string());
    }
    Cow::Borrowed(
        track
            .lines
            .get(compiled.source_line_idx)
            .map_or("", |line| line.text.as_str()),
    )
}

fn waiting_countdown_lines(
    waiting: WaitingGap,
    effective_ms: u64,
    unicode: bool,
    style: &ResolvedStyle,
    size: CountdownSize,
) -> Option<Vec<Line<'static>>> {
    if style.instrumental_style == InstrumentalStyle::Hidden {
        return None;
    }

    let progress = if style.reduced_motion {
        0.0
    } else {
        waiting.progress(effective_ms)
    };
    let filled_count = ((progress * 4.0).floor() as usize).clamp(0, 3);
    let mut accent_style = Style::default().add_modifier(Modifier::BOLD);
    if let Some(accent) = style.accent {
        accent_style = accent_style.fg(accent);
    }
    let mut empty_style = Style::default().add_modifier(Modifier::DIM);
    if let Some(fg) = style.inactive_fg {
        empty_style = empty_style.fg(fg);
    }

    match size {
        CountdownSize::Line => {
            let (filled_glyph, empty_glyph) = if unicode { ("●", "○") } else { ("*", ".") };
            let mut spans = Vec::with_capacity(5);
            for idx in 0..3 {
                if idx > 0 {
                    spans.push(Span::raw(" "));
                }
                if idx < filled_count {
                    spans.push(Span::styled(filled_glyph, accent_style));
                } else {
                    spans.push(Span::styled(empty_glyph, empty_style));
                }
            }
            Some(vec![Line::from(spans).alignment(Alignment::Center)])
        }
        CountdownSize::Block => {
            let (filled, empty) = if unicode {
                ([" ███ ", "█████", " ███ "], [" ░░░ ", "░   ░", " ░░░ "])
            } else {
                ([" ### ", "#####", " ### "], [" ... ", ".   .", " ... "])
            };
            let mut lines = Vec::with_capacity(BLOCK_COUNTDOWN_ROWS);
            for row in 0..BLOCK_COUNTDOWN_ROWS {
                let mut spans = Vec::with_capacity(5);
                for idx in 0..3 {
                    if idx > 0 {
                        spans.push(Span::raw("  "));
                    }
                    if idx < filled_count {
                        spans.push(Span::styled(filled[row], accent_style));
                    } else {
                        spans.push(Span::styled(empty[row], empty_style));
                    }
                }
                lines.push(Line::from(spans).alignment(Alignment::Center));
            }
            Some(lines)
        }
    }
}

fn render_unsynced(frame: &mut Frame, area: Rect, state: &UiState, timeline: &CompiledTimeline) {
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

fn render_lines(
    frame: &mut Frame,
    area: Rect,
    lines: Vec<Line<'static>>,
    active_start: Option<usize>,
    active_end: Option<usize>,
) {
    let height = usize::from(area.height);
    if lines.len() <= height {
        let top_padding = (height - lines.len()) / 2;
        let mut padded = vec![Line::default(); top_padding];
        padded.extend(lines);
        frame.render_widget(Paragraph::new(padded).alignment(Alignment::Center), area);
        return;
    }

    let focus = active_start.unwrap_or(0);
    let focus_end = active_end.unwrap_or(focus + 1);
    let focus_height = focus_end.saturating_sub(focus).max(1);
    let centered = focus.saturating_sub(height.saturating_sub(focus_height) / 2);
    let start = centered.min(lines.len().saturating_sub(height));
    let visible = lines[start..start + height].to_vec();
    frame.render_widget(Paragraph::new(visible).alignment(Alignment::Center), area);
}

#[cfg(test)]
mod tests {
    use ratatui::{Terminal, backend::TestBackend, buffer::Buffer, style::Color};

    use super::*;
    use crate::core::clock::PlaybackStatus;
    use crate::core::model::{Annotation, LyricLine, SyncGranularity, SyncedTrack};
    use crate::style::{
        AnnotationPosition, BUILTIN_PRESETS, HighlightMode, InstrumentalStyle, StyleProfile,
        color::ColorCapability,
    };

    fn sample_track() -> SyncedTrack {
        SyncedTrack {
            source_provider: "local".to_string(),
            source_track_id: None,
            parser_version: crate::core::model::PARSER_VERSION,
            granularity: SyncGranularity::Word,
            is_instrumental: false,
            intrinsic_offset_ms: 0,
            user_offset_ms: 0,
            match_confidence: 1.0,
            lines: vec![
                LyricLine {
                    start_ms: 0,
                    end_ms: 1_000,
                    text: "Far before".to_string(),
                    words: Vec::new(),
                    annotations: Vec::new(),
                    role: LineRole::Primary,
                },
                LyricLine {
                    start_ms: 1_000,
                    end_ms: 2_000,
                    text: "Just before".to_string(),
                    words: Vec::new(),
                    annotations: Vec::new(),
                    role: LineRole::Primary,
                },
                LyricLine {
                    start_ms: 2_000,
                    end_ms: 4_000,
                    text: "Active line".to_string(),
                    words: vec![WordToken {
                        display_width: 6,
                        start_ms: 2_000,
                        end_ms: 3_000,
                        byte_range: (0, 6),
                    }],
                    annotations: vec![Annotation {
                        kind: crate::core::model::AnnotationKind::Translation,
                        text: "Baris aktif".to_string(),
                    }],
                    role: LineRole::Primary,
                },
                LyricLine {
                    start_ms: 4_000,
                    end_ms: 5_000,
                    text: "Just after".to_string(),
                    words: Vec::new(),
                    annotations: Vec::new(),
                    role: LineRole::Primary,
                },
                LyricLine {
                    start_ms: 5_000,
                    end_ms: 6_000,
                    text: "Far after".to_string(),
                    words: Vec::new(),
                    annotations: Vec::new(),
                    role: LineRole::Primary,
                },
            ],
        }
    }

    fn render_buffer(state: &UiState, effective_ms: u64, width: u16, height: u16) -> Buffer {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render(frame, state, effective_ms))
            .unwrap();
        terminal.backend().buffer().clone()
    }

    fn render_buffer_text(state: &UiState, effective_ms: u64, width: u16, height: u16) -> String {
        let buf = render_buffer(state, effective_ms, width, height);
        (0..height)
            .map(|y| (0..width).map(|x| buf[(x, y)].symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn position_of(buf: &Buffer, needle: &str) -> (u16, u16) {
        for y in 0..buf.area.height {
            let row = (0..buf.area.width)
                .map(|x| buf[(x, y)].symbol())
                .collect::<String>();
            if let Some(position) = row.find(needle) {
                return (u16::try_from(position).unwrap(), y);
            }
        }
        panic!("{needle} not rendered");
    }

    #[test]
    fn every_builtin_style_renders_lyrics_states_and_modals() {
        for name in BUILTIN_PRESETS {
            for (width, height) in [(80, 24), (120, 40)] {
                let profile = StyleProfile::builtin(name, width).unwrap();
                let resolved = profile.resolve(*name, ColorCapability::TrueColor, None);
                let mut state = UiState::new();
                state.playback = PlaybackStatus::Playing;
                state.track = Some(crate::media::TrackMeta {
                    title: "Song Title".to_string(),
                    artist: "Artist".to_string(),
                    album: None,
                    duration_ms: Some(10_000),
                    track_id: None,
                    url: None,
                });
                state.set_resolved_style(resolved);
                state.set_lyrics(sample_track());
                render_buffer_text(&state, 2_200, width, height);

                state.show_banner("Offline mode", Instant::now());
                render_buffer_text(&state, 2_200, width, height);

                state.open_context_menu();
                render_buffer_text(&state, 2_200, width, height);

                state.open_candidate_loading("Artist - Song Title".to_string());
                render_buffer_text(&state, 2_200, width, height);

                state.open_search_input("Artist".to_string(), "Song Title".to_string());
                render_buffer_text(&state, 2_200, width, height);

                state.close_modal();
                state.set_not_found();
                render_buffer_text(&state, 0, width, height);
            }
        }
    }

    #[test]
    fn header_override_precedence_and_inline_annotations() {
        let mut minimal = StyleProfile::builtin("builtin:minimal", 80).unwrap();
        minimal.annotations.position = AnnotationPosition::Inline;
        let mut state = UiState::new();
        state.playback = PlaybackStatus::Playing;
        state.track = Some(crate::media::TrackMeta {
            title: "Song Title".to_string(),
            artist: "Artist".to_string(),
            album: None,
            duration_ms: Some(10_000),
            track_id: None,
            url: None,
        });
        state.set_resolved_style(minimal.resolve("minimal", ColorCapability::TrueColor, None));
        state.set_lyrics(sample_track());

        // builtin:minimal sets header.show = false, so header stays hidden by default
        let hidden = render_buffer_text(&state, 2_200, 60, 12);
        assert!(!hidden.contains("Song Title"), "{hidden}");
        assert!(hidden.contains("Active line  Baris aktif"), "{hidden}");

        // Explicit [ui].show_header / user toggle wins over style's header.show = false
        state.set_header_override(Some(true));
        let shown = render_buffer_text(&state, 2_200, 60, 12);
        assert!(shown.contains("Song Title"), "{shown}");
    }

    #[test]
    fn word_highlight_switch_overrides_style_word_mode() {
        let aurora = StyleProfile::builtin("builtin:aurora", 100).unwrap();
        let resolved = aurora.resolve("aurora", ColorCapability::TrueColor, None);
        assert_eq!(resolved.highlight_mode, HighlightMode::Word);

        let mut state = UiState::new();
        state.set_resolved_style(resolved);
        assert!(state.effective_word_highlight());

        // Global [ui].word_highlight = false disables word highlighting across all styles
        state.word_highlight = false;
        assert!(!state.effective_word_highlight());

        let mut minimal = StyleProfile::builtin("builtin:minimal", 100).unwrap();
        minimal.highlight.mode = HighlightMode::Line;
        state.word_highlight = true;
        state.set_resolved_style(minimal.resolve("minimal", ColorCapability::Ansi256, None));
        assert!(!state.effective_word_highlight());
    }

    #[test]
    fn fade_styles_distant_lines_toward_bg_and_supports_instrumental_hidden() {
        let mut dark = StyleProfile::builtin("builtin:dark", 100).unwrap();
        dark.instrumental.style = InstrumentalStyle::Hidden;
        let resolved = dark.resolve("dark", ColorCapability::TrueColor, None);
        let near = resolved.line_style(LineRole::Primary, 1, false).fg;
        let far = resolved.line_style(LineRole::Primary, 3, false).fg;
        assert_ne!(near, far);
        assert!(matches!(far, Some(Color::Rgb(_, _, _))));

        assert_eq!(
            waiting_countdown_lines(
                WaitingGap {
                    kind: WaitKind::Intro,
                    start_ms: 0,
                    display_start_ms: 0,
                    end_ms: 5_000,
                },
                1_000,
                true,
                &resolved,
                CountdownSize::Line,
            ),
            None
        );
    }

    #[test]
    fn active_prefix_prepends_prefix_only_to_active_line() {
        let mut profile = StyleProfile::builtin("builtin:default", 80).unwrap();
        profile.lines.active.prefix = "> ".to_string();
        profile.lines.inactive.prefix = String::new();
        let mut state = UiState::new();
        state.playback = PlaybackStatus::Playing;
        state.set_resolved_style(profile.resolve("default", ColorCapability::TrueColor, None));
        state.set_lyrics(sample_track());

        let output = render_buffer_text(&state, 2_200, 80, 14);
        assert!(output.contains("> Active line"), "{output}");
        assert!(!output.contains("> Just before"), "{output}");
        assert!(!output.contains("> Just after"), "{output}");
    }

    #[test]
    fn line_spacing_inserts_blank_lines_between_rendered_lines() {
        let mut profile = StyleProfile::builtin("builtin:default", 80).unwrap();
        profile.layout.line_spacing = 1;
        let mut state = UiState::new();
        state.playback = PlaybackStatus::Playing;
        state.set_resolved_style(profile.resolve("default", ColorCapability::TrueColor, None));
        state.set_lyrics(sample_track());

        let output = render_buffer_text(&state, 2_200, 80, 14);
        let lines: Vec<&str> = output.lines().map(str::trim).collect();
        let active_pos = lines
            .iter()
            .position(|l| l.contains("Active line"))
            .expect("active line rendered");
        let before_pos = lines
            .iter()
            .position(|l| l.contains("Just before"))
            .expect("before line rendered");
        assert_eq!(active_pos.saturating_sub(before_pos), 2, "{output}");
        assert!(lines[before_pos + 1].is_empty(), "{output}");
    }

    #[test]
    fn countdown_uses_same_line_spacing_as_surrounding_lines() {
        let mut profile = StyleProfile::builtin("builtin:default", 80).unwrap();
        profile.layout.line_spacing = 1;
        let mut state = UiState::new();
        state.playback = PlaybackStatus::Playing;
        state.set_resolved_style(profile.resolve("default", ColorCapability::TrueColor, None));
        state.set_lyrics(SyncedTrack {
            source_provider: "local".to_string(),
            source_track_id: None,
            parser_version: crate::core::model::PARSER_VERSION,
            granularity: SyncGranularity::Line,
            is_instrumental: false,
            intrinsic_offset_ms: 0,
            user_offset_ms: 0,
            match_confidence: 1.0,
            lines: vec![
                LyricLine {
                    start_ms: 0,
                    end_ms: 1_000,
                    text: "Before gap".to_string(),
                    words: Vec::new(),
                    annotations: Vec::new(),
                    role: LineRole::Primary,
                },
                LyricLine {
                    start_ms: 1_000,
                    end_ms: 2_000,
                    text: "Last lyric".to_string(),
                    words: Vec::new(),
                    annotations: Vec::new(),
                    role: LineRole::Primary,
                },
                LyricLine {
                    start_ms: 10_000,
                    end_ms: 11_000,
                    text: "After gap".to_string(),
                    words: Vec::new(),
                    annotations: Vec::new(),
                    role: LineRole::Primary,
                },
            ],
        });

        let output = render_buffer_text(&state, 6_000, 80, 20);
        let lines: Vec<&str> = output.lines().map(str::trim).collect();
        let dot_pos = lines
            .iter()
            .position(|line| line.contains('●'))
            .expect("countdown rendered");
        assert!(lines[dot_pos - 1].is_empty(), "{output}");
        assert!(lines[dot_pos + 1].is_empty(), "{output}");
    }

    #[test]
    fn block_context_font_follows_viewport_geometry() {
        assert!(block_context_plain(Rect::new(0, 0, 80, 24)));
        assert!(block_context_plain(Rect::new(0, 0, 120, 24)));
        assert!(block_context_plain(Rect::new(0, 0, 80, 40)));
        assert!(!block_context_plain(Rect::new(0, 0, 120, 40)));
    }

    #[test]
    fn held_line_uses_sung_color_after_line_end() {
        let mut profile = StyleProfile::builtin("builtin:dark", 80).unwrap();
        profile.highlight.mode = HighlightMode::Word;
        let resolved = profile.resolve("dark", ColorCapability::TrueColor, None);
        let word_active_fg = resolved
            .word_style(LineRole::Primary, true, false)
            .fg
            .unwrap();
        let held_fg = resolved
            .word_style(LineRole::Primary, false, true)
            .fg
            .unwrap();
        assert_ne!(word_active_fg, held_fg);

        let mut state = UiState::new();
        state.playback = PlaybackStatus::Playing;
        state.set_resolved_style(resolved);
        state.set_lyrics(SyncedTrack {
            source_provider: "local".to_string(),
            source_track_id: None,
            parser_version: crate::core::model::PARSER_VERSION,
            granularity: SyncGranularity::Word,
            is_instrumental: false,
            intrinsic_offset_ms: 0,
            user_offset_ms: 0,
            match_confidence: 1.0,
            lines: vec![
                LyricLine {
                    start_ms: 1_000,
                    end_ms: 2_000,
                    text: "Held last".to_string(),
                    words: vec![
                        WordToken {
                            display_width: 5,
                            start_ms: 1_000,
                            end_ms: 1_500,
                            byte_range: (0, 5),
                        },
                        WordToken {
                            display_width: 4,
                            start_ms: 1_500,
                            end_ms: 2_000,
                            byte_range: (5, 9),
                        },
                    ],
                    annotations: Vec::new(),
                    role: LineRole::Primary,
                },
                LyricLine {
                    start_ms: 4_000,
                    end_ms: 5_000,
                    text: "Next line".to_string(),
                    words: Vec::new(),
                    annotations: Vec::new(),
                    role: LineRole::Primary,
                },
            ],
        });

        let buf = render_buffer(&state, 1_200, 80, 14);
        let (x, y) = position_of(&buf, "Held last");
        assert_eq!(buf[(x, y)].fg, word_active_fg);

        let buf = render_buffer(&state, 2_400, 80, 14);
        let (x, y) = position_of(&buf, "Held last");
        assert_eq!(buf[(x, y)].fg, held_fg);
        let (x, y) = position_of(&buf, "last");
        assert_eq!(buf[(x, y)].fg, word_active_fg);

        let buf = render_buffer(&state, 3_900, 80, 14);
        let (x, y) = position_of(&buf, "Held last");
        assert_eq!(buf[(x, y)].fg, held_fg);
        let (x, y) = position_of(&buf, "last");
        assert_eq!(buf[(x, y)].fg, word_active_fg);
    }

    #[test]
    fn kinetic_hold_keeps_last_phrase() {
        let mut profile = StyleProfile::builtin("builtin:dark", 80).unwrap();
        profile.big_text.max_chars_per_phrase = 8;
        profile.big_text.max_size = BigTextMaxSize::Plain;

        let mut state = UiState::new();
        state.playback = PlaybackStatus::Playing;
        state.display_mode = crate::ui::DisplayMode::Kinetic;
        state.set_resolved_style(profile.resolve("dark", ColorCapability::TrueColor, None));
        state.set_lyrics(SyncedTrack {
            source_provider: "local".to_string(),
            source_track_id: None,
            parser_version: crate::core::model::PARSER_VERSION,
            granularity: SyncGranularity::Line,
            is_instrumental: false,
            intrinsic_offset_ms: 0,
            user_offset_ms: 0,
            match_confidence: 1.0,
            lines: vec![
                LyricLine {
                    start_ms: 1_000,
                    end_ms: 3_000,
                    text: "alpha beta gamma delta".to_string(),
                    words: Vec::new(),
                    annotations: Vec::new(),
                    role: LineRole::Primary,
                },
                LyricLine {
                    start_ms: 7_000,
                    end_ms: 8_000,
                    text: "next line".to_string(),
                    words: Vec::new(),
                    annotations: Vec::new(),
                    role: LineRole::Primary,
                },
            ],
        });

        let output = render_buffer_text(&state, 3_500, 80, 14);
        assert!(output.contains("delta"), "{output}");
        assert!(!output.contains("alpha"), "{output}");
    }

    #[test]
    fn wipe_highlight_renders_gradient_across_active_word() {
        let mut profile = StyleProfile::builtin("builtin:dark", 80).unwrap();
        profile.highlight.mode = HighlightMode::Wipe;
        profile.highlight.trail = 3;
        let active_rgb = Color::Rgb(0, 200, 255);
        let sung_rgb = Color::Rgb(180, 180, 180);
        let unsung_rgb = Color::Rgb(60, 60, 60);
        profile.highlight.active_fg = Some(crate::style::color::StyleColor::new(active_rgb));
        profile.highlight.sung_fg = Some(crate::style::color::StyleColor::new(sung_rgb));
        profile.highlight.unsung_fg = Some(crate::style::color::StyleColor::new(unsung_rgb));

        let mut state = UiState::new();
        state.playback = PlaybackStatus::Playing;
        state.set_resolved_style(profile.resolve("dark", ColorCapability::TrueColor, None));
        state.set_lyrics(sample_track());

        let backend = TestBackend::new(80, 14);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, &state, 2_500)).unwrap();
        let buf = terminal.backend().buffer();
        let mut word_colors = Vec::new();
        let mut found = false;
        for y in 0..14 {
            let row = (0..80).map(|x| buf[(x, y)].symbol()).collect::<String>();
            if let Some(col) = row.find("Active") {
                found = true;
                for offset in 0..6 {
                    word_colors.push(buf[(u16::try_from(col + offset).unwrap(), y)].fg);
                }
            }
        }
        assert!(found, "active line was not rendered");
        let has_blended_trail = word_colors.iter().any(|fg| {
            matches!(fg, Color::Rgb(..))
                && *fg != active_rgb
                && *fg != sung_rgb
                && *fg != unsung_rgb
        });
        assert!(
            has_blended_trail,
            "expected an interpolated trail color in \"Active\", got {word_colors:?}"
        );
    }

    #[test]
    fn effects_preserve_header_status_and_modal_on_test_backend() {
        let mut profile = StyleProfile::builtin("builtin:default", 80).unwrap();
        profile.palette.bg = crate::style::color::StyleColor::new(Color::Reset);
        profile.animation.on_line_change = vec![crate::style::LineChangeEffect {
            effect: crate::style::LineChangeEffectKind::FadeOut,
            duration_ms: 400,
        }];
        let resolved = profile.resolve("default", ColorCapability::TrueColor, None);

        let mut state = UiState::new();
        state.set_player(Some("mpv".to_string()));
        state.set_track(crate::media::TrackMeta {
            title: "Song Title".to_string(),
            artist: "Artist".to_string(),
            album: None,
            duration_ms: Some(10_000),
            track_id: None,
            url: None,
        });
        state.playback = PlaybackStatus::Playing;
        state.set_resolved_style(resolved.clone());
        state.set_lyrics(sample_track());

        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut block_cache = BlockStageCache::default();

        // Render clean reference without effects to compare header and status rows.
        terminal
            .draw(|frame| render_with_cache(frame, &state, &mut block_cache, 2_500))
            .unwrap();
        let ref_buf = terminal.backend().buffer().clone();

        // Render with active line-change effects using Color::Reset.
        let mut effects = UiEffects::new();
        effects.observe_state(&state, 2_500);
        assert!(effects.is_running());

        terminal
            .draw(|frame| {
                let stage_area = render_with_cache_area(frame, &state, &mut block_cache, 2_500);
                effects.process_elapsed(
                    std::time::Duration::from_millis(100),
                    frame.buffer_mut(),
                    stage_area,
                    &resolved,
                    false,
                );
            })
            .unwrap();
        let fx_buf = terminal.backend().buffer();
        for x in 0..80 {
            assert_eq!(
                fx_buf[(x, 0)],
                ref_buf[(x, 0)],
                "header row mutated at x={x}"
            );
            assert_eq!(
                fx_buf[(x, 23)],
                ref_buf[(x, 23)],
                "status row mutated at x={x}"
            );
        }

        // Open Help modal: effects stay frozen while modal is visible.
        state.toggle_help();
        assert!(state.is_modal_open());
        terminal
            .draw(|frame| {
                render_with_cache_and_effects(frame, &state, &mut block_cache, &mut effects, 2_600);
            })
            .unwrap();
        assert!(effects.is_running(), "modal must freeze active effects");

        // Close modal and complete effect duration.
        state.toggle_help();
        terminal
            .draw(|frame| {
                let stage_area = render_with_cache_area(frame, &state, &mut block_cache, 2_600);
                effects.process_elapsed(
                    std::time::Duration::from_millis(1_000),
                    frame.buffer_mut(),
                    stage_area,
                    &resolved,
                    false,
                );
            })
            .unwrap();
        assert!(!effects.is_running());
    }
}

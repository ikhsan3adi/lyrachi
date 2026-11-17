use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Wrap},
};

use crate::compiler::timeline::{
    ActiveStageCursor, CompiledLine, CompiledTimeline, WrappedSubLine,
};
use crate::core::model::{LineRole, WordToken};

use super::{LyricsView, UiState, status::footer_line};

const DUET_RIGHT_ALIGN_MIN_WIDTH: u16 = 60;

pub fn render(frame: &mut Frame, state: &UiState, effective_ms: u64) {
    let area = frame.area();
    let header_rows = u16::from(state.show_header);
    let footer_rows = u16::from(state.footer_visible());
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(header_rows),
        Constraint::Min(1),
        Constraint::Length(footer_rows),
    ])
    .areas(area);

    let padding = state.timeline_context().padding_horizontal;
    if header_rows > 0 {
        render_header(frame, content_area(header, padding), state);
    }
    render_body(frame, content_area(body, padding), state, effective_ms);
    if let Some(line) = footer_line(state) {
        frame.render_widget(Paragraph::new(line), footer);
    }
}

fn content_area(area: Rect, padding: u16) -> Rect {
    Rect {
        x: area.x.saturating_add(padding),
        y: area.y,
        width: area.width.saturating_sub(padding.saturating_mul(2)),
        height: area.height,
    }
}

#[derive(Debug, Clone, Copy)]
struct StageStyle {
    accent: Color,
    duet_accent: Color,
    background_vocal: Color,
    background_vocal_active: Color,
}

impl Default for StageStyle {
    fn default() -> Self {
        Self {
            accent: Color::Cyan,
            duet_accent: Color::LightMagenta,
            background_vocal: Color::DarkGray,
            background_vocal_active: Color::Gray,
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum WordState {
    Active,
    Sung,
    Unsung,
}

#[derive(Debug, Clone, Copy)]
struct StageRender {
    cursor: ActiveStageCursor,
    effective_ms: u64,
    plain: bool,
    viewport_width: u16,
    styles: StageStyle,
}

impl StageStyle {
    fn line_style(self, role: LineRole, active: bool) -> Style {
        match role {
            LineRole::Primary => {
                if active {
                    Style::default()
                } else {
                    Style::default().add_modifier(Modifier::DIM)
                }
            }
            LineRole::DuetSecondary => {
                let style = Style::default().fg(self.duet_accent);
                if active {
                    style
                } else {
                    style.add_modifier(Modifier::DIM)
                }
            }
            LineRole::BackgroundVocal => {
                let style = Style::default()
                    .fg(self.background_vocal)
                    .add_modifier(Modifier::ITALIC);
                if active {
                    style
                } else {
                    style.add_modifier(Modifier::DIM)
                }
            }
            LineRole::InstrumentalBreak => {
                Style::default().add_modifier(Modifier::DIM | Modifier::ITALIC)
            }
        }
    }

    fn word_style(self, role: LineRole, state: WordState) -> Style {
        match role {
            LineRole::Primary => match state {
                WordState::Active => Style::default()
                    .fg(self.accent)
                    .add_modifier(Modifier::BOLD),
                WordState::Sung => Style::default(),
                WordState::Unsung => Style::default().add_modifier(Modifier::DIM),
            },
            LineRole::DuetSecondary => {
                let style = Style::default().fg(self.duet_accent);
                match state {
                    WordState::Active => style.add_modifier(Modifier::BOLD),
                    WordState::Sung => style,
                    WordState::Unsung => style.add_modifier(Modifier::DIM),
                }
            }
            LineRole::BackgroundVocal => {
                let style = Style::default()
                    .fg(self.background_vocal)
                    .add_modifier(Modifier::ITALIC);
                match state {
                    WordState::Active => Style::default()
                        .fg(self.background_vocal_active)
                        .add_modifier(Modifier::BOLD | Modifier::ITALIC),
                    WordState::Sung => style,
                    WordState::Unsung => style.add_modifier(Modifier::DIM),
                }
            }
            LineRole::InstrumentalBreak => {
                Style::default().add_modifier(Modifier::DIM | Modifier::ITALIC)
            }
        }
    }

    fn annotation_style(self) -> Style {
        Style::default().add_modifier(Modifier::DIM | Modifier::ITALIC)
    }

    fn alignment(self, role: LineRole, viewport_width: u16) -> Alignment {
        match role {
            LineRole::DuetSecondary if viewport_width >= DUET_RIGHT_ALIGN_MIN_WIDTH => {
                Alignment::Right
            }
            LineRole::DuetSecondary => Alignment::Left,
            LineRole::Primary | LineRole::BackgroundVocal | LineRole::InstrumentalBreak => {
                Alignment::Center
            }
        }
    }
}

fn render_header(frame: &mut Frame, area: Rect, state: &UiState) {
    let line = if let Some(track) = &state.track {
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
    } else {
        Line::from(Span::styled(
            "Lyrachi",
            Style::default().add_modifier(Modifier::BOLD),
        ))
    };

    frame.render_widget(Paragraph::new(line).alignment(Alignment::Center), area);
}

fn render_body(frame: &mut Frame, area: Rect, state: &UiState, effective_ms: u64) {
    if area.height == 0 || area.width == 0 {
        return;
    }

    match &state.lyrics {
        LyricsView::Idle => {
            if state.player.is_none() {
                centered_message(
                    frame,
                    area,
                    "No MPRIS player found",
                    "Start a player that exposes MPRIS.",
                );
            } else {
                centered_message(
                    frame,
                    area,
                    "No track playing",
                    "Play something to load lyrics.",
                );
            }
        }
        LyricsView::Loading => {
            centered_message(
                frame,
                area,
                "Loading lyrics",
                "Searching local files and LRCLIB.",
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
            centered_message(frame, area, "[Instrumental]", "No lyrics for this track.");
        }
        LyricsView::Loaded { track, timeline }
            if track.granularity == crate::core::model::SyncGranularity::Unsynced =>
        {
            render_unsynced(frame, area, state, timeline);
        }
        LyricsView::Loaded { timeline, .. } => {
            render_synced(frame, area, timeline, effective_ms);
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

fn render_synced(frame: &mut Frame, area: Rect, timeline: &CompiledTimeline, effective_ms: u64) {
    if timeline.lines.is_empty() {
        centered_message(frame, area, "No lyrics found", "");
        return;
    }

    let cursor = timeline.evaluate(effective_ms);
    let total = timeline.lines.len();
    let anchor = timeline.anchor_line_index(effective_ms).unwrap_or(0);
    let start = anchor.saturating_sub(2);
    let end = (anchor + 4).max(start + 5).min(total);
    let stage = StageRender {
        cursor,
        effective_ms,
        plain: false,
        viewport_width: timeline.context.viewport_width,
        styles: StageStyle::default(),
    };
    let mut lines = Vec::new();
    let mut active_start = None;
    let mut active_end = None;

    for index in start..end {
        let compiled = &timeline.lines[index];
        let active = cursor.active_line_idx == Some(index);
        let is_focus = index == anchor;
        if is_focus {
            active_start = Some(lines.len());
        }
        push_compiled_line(&mut lines, compiled, stage, active);
        if is_focus {
            active_end = Some(lines.len());
        }
    }

    render_lines(frame, area, lines, active_start, active_end);
}

fn render_unsynced(frame: &mut Frame, area: Rect, state: &UiState, timeline: &CompiledTimeline) {
    let mut lines = Vec::new();
    let start = state.scroll.min(timeline.lines.len());
    let stage = StageRender {
        cursor: ActiveStageCursor {
            active_line_idx: None,
            active_word_idx: None,
            progress: 0.0,
        },
        effective_ms: 0,
        plain: true,
        viewport_width: timeline.context.viewport_width,
        styles: StageStyle::default(),
    };

    for compiled in &timeline.lines[start..] {
        push_compiled_line(&mut lines, compiled, stage, false);
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
    stage: StageRender,
    active: bool,
) {
    let alignment = stage.styles.alignment(compiled.role, stage.viewport_width);
    for subline in &compiled.wrapped_main {
        lines.push(
            Line::from(spans_for_subline(compiled, subline, active, stage)).alignment(alignment),
        );
    }

    for annotation in &compiled.wrapped_annotations {
        lines.push(
            Line::from(Span::styled(
                annotation.clone(),
                stage.styles.annotation_style(),
            ))
            .alignment(alignment),
        );
    }
}

fn spans_for_subline(
    compiled: &CompiledLine,
    subline: &WrappedSubLine,
    active_line: bool,
    stage: StageRender,
) -> Vec<Span<'static>> {
    if stage.plain {
        return vec![Span::raw(subline.text.clone())];
    }
    if !active_line {
        return vec![Span::styled(
            subline.text.clone(),
            stage.styles.line_style(compiled.role, false),
        )];
    }
    if compiled.role == LineRole::InstrumentalBreak {
        return vec![Span::styled(
            subline.text.clone(),
            stage.styles.line_style(compiled.role, true),
        )];
    }
    if compiled.words.is_empty() {
        let style = match compiled.role {
            LineRole::Primary | LineRole::DuetSecondary => {
                stage.styles.word_style(compiled.role, WordState::Active)
            }
            LineRole::BackgroundVocal | LineRole::InstrumentalBreak => {
                stage.styles.line_style(compiled.role, true)
            }
        };
        return vec![Span::styled(subline.text.clone(), style)];
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
            let gap_style = if effective_ms >= word.start_ms {
                stage.styles.word_style(compiled.role, WordState::Sung)
            } else {
                stage.styles.word_style(compiled.role, WordState::Unsung)
            };
            push_span(&mut spans, subline, cursor, word_start, gap_style);
        }

        let style = if Some(index) == active_word_idx {
            stage.styles.word_style(compiled.role, WordState::Active)
        } else if active_word_idx.is_some_and(|active| index < active) || effective_ms > word.end_ms
        {
            stage.styles.word_style(compiled.role, WordState::Sung)
        } else {
            stage.styles.word_style(compiled.role, WordState::Unsung)
        };
        push_span(&mut spans, subline, word_start, word_end, style);
        cursor = word_end;
    }

    if cursor < sub_end {
        let trailing_style = trailing_gap_style(
            compiled.role,
            stage.styles,
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
                stage.styles,
                &compiled.words,
                sub_start,
                effective_ms,
            ),
        ));
    }

    spans
}

fn trailing_gap_style(
    role: LineRole,
    styles: StageStyle,
    words: &[WordToken],
    byte_pos: usize,
    effective_ms: u64,
) -> Style {
    if let Some(next_word) = words.iter().find(|word| word.byte_range.0 >= byte_pos) {
        if effective_ms >= next_word.start_ms {
            styles.word_style(role, WordState::Sung)
        } else {
            styles.word_style(role, WordState::Unsung)
        }
    } else if words.last().is_some_and(|last| effective_ms > last.end_ms) {
        styles.word_style(role, WordState::Sung)
    } else {
        styles.word_style(role, WordState::Unsung)
    }
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

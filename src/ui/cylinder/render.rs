use ratatui::{
    Frame,
    layout::{Alignment, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};
use unicode_segmentation::UnicodeSegmentation;

use crate::compiler::timeline::{ActiveStageCursor, CompiledLine};
use crate::core::model::{LineRole, SyncedTrack};
use crate::core::text::{display_width_with_ambiguous, truncate_to_width_with_ambiguous};
use crate::style::{HighlightMode, ResolvedStyle};
use crate::ui::CylinderAnnotationMode;
use crate::ui::render::{
    ModeRenderer, RenderContext, WordState, centered_message, word_state_style,
};

use super::{
    AnnotationRow, CYLINDER_MAX_ANNOTATIONS, CYLINDER_MAX_VISIBLE_LINES, CYLINDER_MIN_MAIN_HEIGHT,
    CylinderGlyph, CylinderStageCache, GridCell, layout::CylinderGeometry, text_hash,
};

pub(crate) struct CylinderRenderer;

impl ModeRenderer for CylinderRenderer {
    fn render(&self, frame: &mut Frame, area: Rect, ctx: &mut RenderContext<'_>) {
        let state = ctx.state;
        let timeline = ctx.timeline;
        let track = ctx.track;
        let effective_ms = ctx.effective_ms;

        if timeline.lines.is_empty() {
            centered_message(frame, area, "No lyrics found", "");
            return;
        }

        let ambiguous_width = timeline.context.ambiguous_width;
        let cursor = timeline.evaluate_for_status(effective_ms, state.playback);
        let annotations = collect_annotations(
            state,
            &cursor,
            timeline,
            track,
            area,
            effective_ms,
            ambiguous_width,
        );
        let gap_rows = u16::from(!annotations.is_empty());
        let annotation_rows = u16::try_from(annotations.len()).unwrap_or(u16::MAX);
        let main_height = area
            .height
            .saturating_sub(annotation_rows.saturating_add(gap_rows))
            .max(1);
        let main_area = Rect {
            x: area.x,
            y: area.y,
            width: area.width,
            height: main_height,
        };
        let annotation_area = Rect {
            x: area.x,
            y: area.y.saturating_add(main_height).saturating_add(gap_rows),
            width: area.width,
            height: annotation_rows,
        };

        let geometry = CylinderGeometry::new(main_area.width, main_area.height);
        let cache = &mut ctx.cache.cylinder;
        cache.prepare(timeline.context.options_hash());

        let start_index = timeline
            .lines
            .partition_point(|line| line.start_ms <= effective_ms);
        let mut visible = Vec::new();

        for (scanned, index) in (0..start_index).rev().enumerate() {
            if scanned >= CYLINDER_MAX_VISIBLE_LINES {
                break;
            }

            let compiled = &timeline.lines[index];
            if matches!(
                compiled.role,
                LineRole::BackgroundVocal | LineRole::InstrumentalBreak
            ) {
                continue;
            }

            let text = line_text(track, compiled);
            let text_width = cache.ensure_line(index, text, ambiguous_width, || {
                glyphs_for_text(text, ambiguous_width)
            });
            let placement = geometry.place(compiled.start_ms, effective_ms, text_width);
            if placement.visible {
                visible.push((index, text, placement.first_col));
            }
        }

        visible.reverse();
        let visible_indices = visible
            .iter()
            .map(|(index, _, _)| *index)
            .collect::<Vec<_>>();
        cache.retain_visible(&visible_indices);
        cache.prepare_grid(main_area.width, main_area.height);

        let mut any_cell = false;
        for (index, text, first_col) in visible {
            let Some(entry) = cache.line(index, text_hash(text), ambiguous_width).cloned() else {
                continue;
            };
            let compiled = &timeline.lines[index];
            let active = cursor.active_line_idx == Some(index);
            let holding = active && effective_ms > compiled.end_ms;
            let mut x = first_col;
            let mut last_cell = None;

            for glyph in &entry.glyphs {
                let screen_x = x.round() as i32;
                let style = glyph_style(
                    compiled,
                    text,
                    glyph,
                    &cursor,
                    active,
                    holding,
                    effective_ms,
                    geometry.depth(screen_x),
                    state.effective_word_highlight(),
                    &state.style,
                );
                any_cell |= place_grapheme(
                    cache,
                    screen_x,
                    geometry.screen_y(screen_x),
                    glyph,
                    style,
                    &mut last_cell,
                );
                x += f64::from(glyph.display_width);
            }
        }

        if any_cell {
            let (lines, _) = grid_to_lines(cache);
            frame.render_widget(Paragraph::new(lines), main_area);
        }

        if !annotations.is_empty() {
            let lines = annotations
                .into_iter()
                .map(|row| {
                    Line::from(Span::styled(row.text, row.style)).alignment(Alignment::Center)
                })
                .collect::<Vec<_>>();
            frame.render_widget(Paragraph::new(lines), annotation_area);
        }
    }
}

fn collect_annotations(
    state: &crate::ui::UiState,
    cursor: &ActiveStageCursor,
    timeline: &crate::compiler::timeline::CompiledTimeline,
    track: &SyncedTrack,
    area: Rect,
    effective_ms: u64,
    ambiguous_width: u8,
) -> Vec<AnnotationRow> {
    if state.cylinder_annotations == CylinderAnnotationMode::Hide {
        return Vec::new();
    }
    if state.cylinder_annotations == CylinderAnnotationMode::Auto && !state.style.annotations_show {
        return Vec::new();
    }
    let Some(active_idx) = cursor.active_line_idx else {
        return Vec::new();
    };
    let active = &timeline.lines[active_idx];
    let mut rows = Vec::new();

    if let Some(source) = track.lines.get(active.source_line_idx) {
        for annotation in &source.annotations {
            if rows.len() >= CYLINDER_MAX_ANNOTATIONS {
                break;
            }
            rows.push(AnnotationRow {
                text: truncate_annotation(&annotation.text, area.width, ambiguous_width),
                style: state.style.annotation_style(),
            });
        }
    }

    for line in &timeline.lines {
        if rows.len() >= CYLINDER_MAX_ANNOTATIONS {
            break;
        }
        if line.role == LineRole::BackgroundVocal
            && line.start_ms <= effective_ms
            && line.end_ms >= effective_ms
        {
            rows.push(AnnotationRow {
                text: truncate_annotation(line_text(track, line), area.width, ambiguous_width),
                style: state.style.line_style(LineRole::BackgroundVocal, 0, true),
            });
        }
    }

    let gap_rows = u16::from(!rows.is_empty());
    if state.cylinder_annotations == CylinderAnnotationMode::Auto
        && area
            .height
            .saturating_sub(u16::try_from(rows.len()).unwrap_or(u16::MAX))
            .saturating_sub(gap_rows)
            < CYLINDER_MIN_MAIN_HEIGHT
    {
        return Vec::new();
    }
    rows
}

fn truncate_annotation(text: &str, width: u16, ambiguous_width: u8) -> String {
    if display_width_with_ambiguous(text, ambiguous_width) <= width {
        return text.to_string();
    }
    if width <= 3 {
        return truncate_to_width_with_ambiguous(text, width, ambiguous_width).to_string();
    }
    let prefix = truncate_to_width_with_ambiguous(text, width - 3, ambiguous_width);
    format!("{prefix}...")
}

fn glyphs_for_text(text: &str, ambiguous_width: u8) -> (Vec<CylinderGlyph>, u16) {
    let mut glyphs: Vec<CylinderGlyph> = Vec::new();
    for (start, grapheme) in text.grapheme_indices(true) {
        let width = display_width_with_ambiguous(grapheme, ambiguous_width);
        if width == 0
            && let Some(previous) = glyphs.last_mut()
        {
            previous.text.push_str(grapheme);
            previous.byte_range.1 = start + grapheme.len();
            continue;
        }
        glyphs.push(CylinderGlyph {
            text: grapheme.to_string(),
            byte_range: (start, start + grapheme.len()),
            display_width: width,
        });
    }
    let text_width = glyphs.iter().fold(0u16, |width, glyph| {
        width.saturating_add(glyph.display_width)
    });
    (glyphs, text_width)
}

fn line_text<'a>(track: &'a SyncedTrack, compiled: &CompiledLine) -> &'a str {
    track
        .lines
        .get(compiled.source_line_idx)
        .map_or("", |line| line.text.as_str())
}

#[allow(clippy::too_many_arguments)]
fn glyph_style(
    compiled: &CompiledLine,
    line_text: &str,
    glyph: &CylinderGlyph,
    cursor: &ActiveStageCursor,
    active: bool,
    holding: bool,
    effective_ms: u64,
    depth: f64,
    word_highlight: bool,
    style: &ResolvedStyle,
) -> Style {
    let style = if !active {
        style.line_style(compiled.role, 1, false)
    } else if holding {
        held_style(compiled, glyph, word_highlight, style)
    } else if !word_highlight || compiled.words.is_empty() {
        active_line_style(style, compiled.role)
    } else {
        active_word_style(compiled, line_text, glyph, cursor, effective_ms, style)
    };

    if depth < 0.45 {
        style.add_modifier(Modifier::DIM)
    } else {
        style
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

fn held_style(
    compiled: &CompiledLine,
    glyph: &CylinderGlyph,
    word_highlight: bool,
    style: &ResolvedStyle,
) -> Style {
    if !word_highlight || compiled.words.is_empty() {
        return style.line_style(compiled.role, 0, true);
    }
    let last_word = compiled.words.len() - 1;
    match word_index_for_byte(compiled, glyph.byte_range.0) {
        Some(index) if index == last_word => {
            word_state_style(style, compiled.role, WordState::Active)
        }
        _ => word_state_style(style, compiled.role, WordState::Sung),
    }
}

fn active_word_style(
    compiled: &CompiledLine,
    line_text: &str,
    glyph: &CylinderGlyph,
    cursor: &ActiveStageCursor,
    effective_ms: u64,
    style: &ResolvedStyle,
) -> Style {
    let Some(word_idx) = word_index_for_byte(compiled, glyph.byte_range.0) else {
        return gap_style(compiled, glyph, effective_ms, style);
    };
    let word = &compiled.words[word_idx];
    let state = if cursor.active_word_idx == Some(word_idx) {
        if style.highlight_mode == HighlightMode::Wipe && !style.reduced_motion {
            return wipe_word_style(compiled, line_text, glyph, cursor, word_idx, style);
        }
        WordState::Active
    } else if cursor
        .active_word_idx
        .is_some_and(|active| word_idx < active)
        || effective_ms > word.end_ms
    {
        WordState::Sung
    } else {
        WordState::Unsung
    };
    word_state_style(style, compiled.role, state)
}

fn gap_style(
    compiled: &CompiledLine,
    glyph: &CylinderGlyph,
    effective_ms: u64,
    style: &ResolvedStyle,
) -> Style {
    let next_word = compiled
        .words
        .iter()
        .find(|word| word.byte_range.0 >= glyph.byte_range.0);
    let sung = match next_word {
        Some(word) => effective_ms >= word.start_ms,
        None => compiled
            .words
            .last()
            .is_some_and(|word| effective_ms > word.end_ms),
    };
    word_state_style(
        style,
        compiled.role,
        if sung {
            WordState::Sung
        } else {
            WordState::Unsung
        },
    )
}

fn wipe_word_style(
    compiled: &CompiledLine,
    line_text: &str,
    glyph: &CylinderGlyph,
    cursor: &ActiveStageCursor,
    word_idx: usize,
    style: &ResolvedStyle,
) -> Style {
    let word = &compiled.words[word_idx];
    let word_text = line_text
        .get(word.byte_range.0..word.byte_range.1)
        .unwrap_or("");
    let glyph_offset = glyph
        .byte_range
        .0
        .saturating_sub(word.byte_range.0)
        .min(word_text.len());
    let before = word_text.get(..glyph_offset).unwrap_or("");
    let ordinal = before.graphemes(true).count();
    let total = word_text.graphemes(true).count();
    if total == 0 {
        return word_state_style(style, compiled.role, WordState::Active);
    }

    let head = cursor.progress.clamp(0.0, 1.0) * total as f32;
    let head_index = head.floor() as usize;
    let trail = f32::from(style.highlight_trail);
    let distance = head - (ordinal as f32 + 0.5);
    let state = if ordinal == head_index {
        WordState::Active
    } else if distance < 0.0 {
        WordState::Unsung
    } else if trail > 0.0 && distance <= trail {
        return style.wipe_style(compiled.role, distance / trail);
    } else {
        WordState::Sung
    };
    word_state_style(style, compiled.role, state)
}

fn word_index_for_byte(compiled: &CompiledLine, byte: usize) -> Option<usize> {
    compiled
        .words
        .iter()
        .position(|word| word.byte_range.0 <= byte && byte < word.byte_range.1)
}

fn place_grapheme(
    cache: &mut CylinderStageCache,
    screen_x: i32,
    screen_y: u16,
    glyph: &CylinderGlyph,
    style: Style,
    last_cell: &mut Option<(u16, u16)>,
) -> bool {
    let Ok(x) = u16::try_from(screen_x) else {
        return false;
    };
    if x >= cache.grid_width() || screen_y >= cache.grid_height() {
        return false;
    }
    if glyph.display_width == 0 {
        if let Some((last_x, last_y)) = *last_cell
            && let Some(cell) = cache.cell_mut(last_x, last_y)
        {
            cell.text.push_str(&glyph.text);
        }
        return false;
    }

    let cells = glyph.display_width.max(1);
    let end = usize::from(x).saturating_add(usize::from(cells));
    if end > usize::from(cache.grid_width()) {
        return false;
    }
    for column in x..x.saturating_add(cells) {
        clear_cell_links(cache, column, screen_y);
    }
    for offset in 0..cells {
        let column = x.saturating_add(offset);
        let Some(slot) = cache.cell_slot_mut(column, screen_y) else {
            continue;
        };
        *slot = Some(GridCell {
            text: if offset == 0 {
                glyph.text.clone()
            } else {
                String::new()
            },
            style,
            continuation: offset > 0,
        });
    }
    *last_cell = Some((x, screen_y));
    true
}

fn clear_cell_links(cache: &mut CylinderStageCache, x: u16, y: u16) {
    if cache.cell(x, y).is_some_and(|cell| cell.continuation)
        && let Some(previous) = x.checked_sub(1)
        && let Some(slot) = cache.cell_slot_mut(previous, y)
    {
        *slot = None;
    }
    let next = x.saturating_add(1);
    if cache.cell(next, y).is_some_and(|cell| cell.continuation)
        && let Some(slot) = cache.cell_slot_mut(next, y)
    {
        *slot = None;
    }
}

fn grid_to_lines(cache: &CylinderStageCache) -> (Vec<Line<'static>>, bool) {
    let width = cache.grid_width();
    let height = cache.grid_height();
    let mut any = false;
    let mut lines = Vec::with_capacity(usize::from(height));

    for y in 0..height {
        let mut spans = Vec::new();
        let mut text = String::new();
        let mut style = Style::default();
        let mut started = false;

        for x in 0..width {
            let Some(cell) = cache.cell(x, y) else {
                push_grid_span(&mut spans, &mut text, style, &mut started);
                style = Style::default();
                text.push(' ');
                started = true;
                continue;
            };
            if cell.continuation {
                continue;
            }
            any = true;
            if !started {
                style = cell.style;
                started = true;
            } else if cell.style != style {
                push_grid_span(&mut spans, &mut text, style, &mut started);
                style = cell.style;
                started = true;
            }
            text.push_str(&cell.text);
        }
        push_grid_span(&mut spans, &mut text, style, &mut started);
        lines.push(Line::from(spans));
    }

    (lines, any)
}

fn push_grid_span(
    spans: &mut Vec<Span<'static>>,
    text: &mut String,
    style: Style,
    started: &mut bool,
) {
    if !*started || text.is_empty() {
        text.clear();
        return;
    }
    spans.push(Span::styled(std::mem::take(text), style));
}

#[cfg(test)]
mod tests {
    use ratatui::{Terminal, backend::TestBackend, buffer::Buffer};

    use super::*;
    use crate::compiler::timeline::{CompiledTimeline, TimelineContext};
    use crate::core::clock::PlaybackStatus;
    use crate::core::model::{Annotation, LyricLine, SyncGranularity};
    use crate::style::{StyleProfile, color::ColorCapability};
    use crate::ui::{DisplayMode, UiState};

    fn lyric_line(start: u64, end: u64, text: &str) -> LyricLine {
        lyric_line_role(start, end, text, LineRole::Primary)
    }

    fn lyric_line_role(start: u64, end: u64, text: &str, role: LineRole) -> LyricLine {
        LyricLine {
            start_ms: start,
            end_ms: end,
            text: text.to_string(),
            words: Vec::new(),
            annotations: Vec::new(),
            role,
        }
    }

    fn track(lines: Vec<LyricLine>) -> SyncedTrack {
        SyncedTrack {
            source_provider: "local".to_string(),
            source_track_id: None,
            parser_version: crate::core::model::PARSER_VERSION,
            granularity: SyncGranularity::Line,
            is_instrumental: false,
            intrinsic_offset_ms: 0,
            user_offset_ms: 0,
            match_confidence: 1.0,
            lines,
        }
    }

    fn annotated_state(annotations: CylinderAnnotationMode, show: bool) -> UiState {
        let mut state = UiState::new();
        let mut profile = StyleProfile::builtin("builtin:dark", 80).unwrap();
        profile.annotations.show = show;
        let resolved = profile.resolve("dark", ColorCapability::TrueColor, None);
        state.set_resolved_style(resolved);
        state.set_cylinder_annotations(annotations);
        state
    }

    fn annotation_cursor(active: Option<usize>) -> ActiveStageCursor {
        ActiveStageCursor {
            active_line_idx: active,
            active_word_idx: None,
            progress: 0.0,
            waiting: None,
        }
    }

    fn buffer_text(buffer: &Buffer) -> Vec<String> {
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect()
    }

    fn render_cylinder(track: SyncedTrack, effective_ms: u64, width: u16, height: u16) -> Buffer {
        let mut state = UiState::new();
        state.playback = PlaybackStatus::Playing;
        let profile = StyleProfile::builtin("builtin:dark", width).unwrap();
        let resolved = profile.resolve("dark", ColorCapability::TrueColor, None);
        state.set_resolved_style(resolved);
        state.display_mode = DisplayMode::Cylinder;
        state.set_lyrics(track);

        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| crate::ui::render::render(frame, &state, effective_ms))
            .unwrap();
        terminal.backend().buffer().clone()
    }

    fn glyphs(text: &str) -> Vec<CylinderGlyph> {
        glyphs_for_text(text, 1).0
    }

    fn grid_with(width: u16, height: u16) -> CylinderStageCache {
        let mut cache = CylinderStageCache::default();
        cache.prepare_grid(width, height);
        cache
    }

    #[test]
    fn glyphs_measure_width_and_merge_zero_width_graphemes() {
        let glyphs = glyphs("e\u{301}ab漢");
        assert_eq!(glyphs.len(), 4);
        assert_eq!(glyphs[0].text, "e\u{301}");
        assert_eq!(glyphs[0].display_width, 1);
        assert_eq!(glyphs[0].byte_range, (0, 3));
        assert_eq!(glyphs[1].text, "a");
        assert_eq!(glyphs[3].text, "漢");
        assert_eq!(glyphs[3].display_width, 2);
    }

    #[test]
    fn wide_grapheme_reserves_two_cells() {
        let mut cache = grid_with(10, 3);
        let glyph = &glyphs("漢")[0];
        assert!(place_grapheme(
            &mut cache,
            2,
            1,
            glyph,
            Style::default(),
            &mut None
        ));
        assert_eq!(cache.cell(2, 1).unwrap().text, "漢");
        assert!(cache.cell(3, 1).unwrap().continuation);
        let (lines, _) = grid_to_lines(&cache);
        assert!(
            lines[1]
                .spans
                .iter()
                .any(|span| span.content.contains('漢'))
        );
    }

    #[test]
    fn wide_grapheme_skipped_when_second_cell_leaves_the_area() {
        let mut cache = grid_with(10, 3);
        let glyph = &glyphs("漢")[0];
        assert!(!place_grapheme(
            &mut cache,
            9,
            1,
            glyph,
            Style::default(),
            &mut None
        ));
        assert!(cache.cell(9, 1).is_none());
    }

    #[test]
    fn clipping_at_edges_does_not_panic() {
        let mut cache = grid_with(10, 3);
        let glyph = &glyphs("A")[0];
        assert!(!place_grapheme(
            &mut cache,
            -1,
            1,
            glyph,
            Style::default(),
            &mut None
        ));
        assert!(!place_grapheme(
            &mut cache,
            10,
            1,
            glyph,
            Style::default(),
            &mut None
        ));
        assert!(!place_grapheme(
            &mut cache,
            0,
            99,
            glyph,
            Style::default(),
            &mut None
        ));
        assert!(cache.cell(0, 1).is_none());
    }

    #[test]
    fn newer_line_overwrites_older_line_at_the_same_cell() {
        let mut cache = grid_with(10, 3);
        let older = &glyphs("A")[0];
        let newer = &glyphs("B")[0];
        assert!(place_grapheme(
            &mut cache,
            2,
            1,
            older,
            Style::default(),
            &mut None
        ));
        assert!(place_grapheme(
            &mut cache,
            2,
            1,
            newer,
            Style::default(),
            &mut None
        ));
        assert_eq!(cache.cell(2, 1).unwrap().text, "B");
    }

    #[test]
    fn two_contiguous_lines_render_in_order() {
        let buffer = render_cylinder(
            track(vec![
                lyric_line(0, 3_000, "first line"),
                lyric_line(600, 3_600, "second line"),
            ]),
            2_950,
            120,
            40,
        );
        let rows = buffer_text(&buffer);
        let text = rows.join("\n");
        assert!(text.contains("first line"), "{text}");
        assert!(text.contains("second line"), "{text}");
        let row = rows
            .iter()
            .find(|row| row.contains("first line") && row.contains("second line"))
            .expect("contiguous lines share the flat center band");
        assert!(row.find("first line").unwrap() < row.find("second line").unwrap());
    }

    #[test]
    fn long_line_survives_more_than_eight_newer_starts() {
        let alphabet = "abcdefghijklmnopqrstuvwxyz";
        let long_text: String = alphabet.repeat(12).chars().take(300).collect();
        let tail: String = long_text.chars().skip(294).collect();
        let mut lines = vec![lyric_line(0, 1_000, &long_text)];
        for index in 1..=10u64 {
            lines.push(lyric_line(
                index * 1_000,
                index * 1_000 + 500,
                &format!("w{index}"),
            ));
        }
        let buffer = render_cylinder(track(lines), 10_000, 120, 40);
        let text = buffer_text(&buffer).join("\n");
        assert!(text.contains(&tail), "{text}");
    }

    #[test]
    fn instrumental_break_does_not_render_on_the_belt() {
        let buffer = render_cylinder(
            track(vec![
                lyric_line(0, 1_000, "before break"),
                lyric_line_role(1_000, 5_000, "BREAKLINE", LineRole::InstrumentalBreak),
                lyric_line(5_000, 6_000, "after break"),
            ]),
            3_000,
            120,
            40,
        );
        let text = buffer_text(&buffer).join("\n");
        assert!(!text.contains("BREAKLINE"), "{text}");
        assert!(!text.contains('●'), "{text}");
    }

    #[test]
    fn hide_never_renders_annotations() {
        let state = annotated_state(CylinderAnnotationMode::Hide, true);
        let source = track(vec![lyric_line(0, 4_000, "main")]);
        let timeline = CompiledTimeline::compile(&source, TimelineContext::default());
        let rows = collect_annotations(
            &state,
            &annotation_cursor(Some(0)),
            &timeline,
            &source,
            Rect::new(0, 0, 80, 24),
            1_000,
            1,
        );
        assert!(rows.is_empty());
    }

    #[test]
    fn auto_requires_style_annotations_show() {
        let state = annotated_state(CylinderAnnotationMode::Auto, false);
        let source = track(vec![lyric_line(0, 4_000, "main")]);
        let timeline = CompiledTimeline::compile(&source, TimelineContext::default());
        let rows = collect_annotations(
            &state,
            &annotation_cursor(Some(0)),
            &timeline,
            &source,
            Rect::new(0, 0, 80, 24),
            1_000,
            1,
        );
        assert!(rows.is_empty());
    }

    #[test]
    fn auto_hides_annotations_when_main_area_would_be_too_short() {
        let state = annotated_state(CylinderAnnotationMode::Auto, true);
        let mut line = lyric_line(0, 4_000, "main");
        line.annotations = vec![Annotation {
            kind: crate::core::model::AnnotationKind::Translation,
            text: "note".to_string(),
        }];
        let source = track(vec![line]);
        let timeline = CompiledTimeline::compile(&source, TimelineContext::default());
        let rows = collect_annotations(
            &state,
            &annotation_cursor(Some(0)),
            &timeline,
            &source,
            Rect::new(0, 0, 80, 6),
            1_000,
            1,
        );
        assert!(rows.is_empty());

        let mut shown = state;
        shown.set_cylinder_annotations(CylinderAnnotationMode::Show);
        let rows = collect_annotations(
            &shown,
            &annotation_cursor(Some(0)),
            &timeline,
            &source,
            Rect::new(0, 0, 80, 6),
            1_000,
            1,
        );
        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn show_renders_up_to_the_annotation_cap() {
        let state = annotated_state(CylinderAnnotationMode::Show, true);
        let mut line = lyric_line(0, 4_000, "main");
        line.annotations = vec![
            Annotation {
                kind: crate::core::model::AnnotationKind::Translation,
                text: "one".to_string(),
            },
            Annotation {
                kind: crate::core::model::AnnotationKind::Translation,
                text: "two".to_string(),
            },
            Annotation {
                kind: crate::core::model::AnnotationKind::Translation,
                text: "three".to_string(),
            },
        ];
        let source = track(vec![line]);
        let timeline = CompiledTimeline::compile(&source, TimelineContext::default());
        let rows = collect_annotations(
            &state,
            &annotation_cursor(Some(0)),
            &timeline,
            &source,
            Rect::new(0, 0, 80, 24),
            1_000,
            1,
        );
        assert_eq!(rows.len(), CYLINDER_MAX_ANNOTATIONS);
        assert_eq!(rows[0].text, "one");
        assert_eq!(rows[1].text, "two");
    }

    #[test]
    fn background_vocal_becomes_an_annotation() {
        let state = annotated_state(CylinderAnnotationMode::Show, true);
        let source = track(vec![
            lyric_line(0, 4_000, "main"),
            lyric_line_role(1_000, 3_000, "backing vocals", LineRole::BackgroundVocal),
        ]);
        let timeline = CompiledTimeline::compile(&source, TimelineContext::default());
        let rows = collect_annotations(
            &state,
            &annotation_cursor(Some(0)),
            &timeline,
            &source,
            Rect::new(0, 0, 80, 24),
            2_000,
            1,
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].text, "backing vocals");
        assert_eq!(
            rows[0].style,
            state.style.line_style(LineRole::BackgroundVocal, 0, true)
        );
    }

    #[test]
    fn annotations_are_truncated_to_area_width() {
        let state = annotated_state(CylinderAnnotationMode::Show, true);
        let long_text = "a".repeat(40);
        let mut line = lyric_line(0, 4_000, "main");
        line.annotations = vec![Annotation {
            kind: crate::core::model::AnnotationKind::Translation,
            text: long_text,
        }];
        let source = track(vec![line]);
        let timeline = CompiledTimeline::compile(&source, TimelineContext::default());
        let rows = collect_annotations(
            &state,
            &annotation_cursor(Some(0)),
            &timeline,
            &source,
            Rect::new(0, 0, 20, 24),
            1_000,
            1,
        );
        assert_eq!(rows.len(), 1);
        assert!(rows[0].text.ends_with("..."));
        assert_eq!(display_width_with_ambiguous(&rows[0].text, 1), 20);
    }

    #[test]
    fn annotations_render_centered_below_the_belt() {
        let mut state = UiState::new();
        state.playback = PlaybackStatus::Playing;
        let mut profile = StyleProfile::builtin("builtin:dark", 80).unwrap();
        profile.annotations.show = true;
        let resolved = profile.resolve("dark", ColorCapability::TrueColor, None);
        state.set_resolved_style(resolved);
        state.display_mode = DisplayMode::Cylinder;
        let mut line = lyric_line(0, 4_000, "main line");
        line.annotations = vec![Annotation {
            kind: crate::core::model::AnnotationKind::Translation,
            text: "translation".to_string(),
        }];
        state.set_lyrics(track(vec![line]));

        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| crate::ui::render::render(frame, &state, 1_800))
            .unwrap();
        let rows = buffer_text(terminal.backend().buffer());
        let row_index = rows
            .iter()
            .position(|row| row.contains("translation"))
            .expect("annotation rendered");
        let column = rows[row_index].find("translation").unwrap();
        let expected = (80 - "translation".len()) / 2;
        assert!(
            (i32::from(column as u16) - i32::from(expected as u16)).abs() <= 1,
            "column {column}, expected ~{expected}"
        );
        let main_row = rows
            .iter()
            .position(|row| row.contains("main line"))
            .expect("main line rendered");
        assert!(main_row < row_index, "annotations must sit below the belt");
    }
}

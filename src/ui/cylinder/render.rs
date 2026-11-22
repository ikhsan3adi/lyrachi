use ratatui::{
    Frame,
    layout::{Alignment, Rect},
    style::Style,
    text::{Line, Span},
    widgets::Paragraph,
};
use unicode_segmentation::UnicodeSegmentation;

use crate::compiler::timeline::{ActiveStageCursor, CompiledLine};
use crate::core::model::{AnnotationKind, LineRole, SyncedTrack};
use crate::core::text::{display_width_with_ambiguous, truncate_to_width_with_ambiguous};
use crate::ui::CylinderAnnotationMode;
use crate::ui::block::BlockFontSize;
use crate::ui::block::font::{glyph_bitmap_for_cluster, placeholder_bitmap, space_bitmap};
use crate::ui::render::{ModeRenderer, RenderContext, centered_message};
use crate::ui::word_style;

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
            centered_message(frame, area, &state.style, "No lyrics found", "", false);
            return;
        }

        let ambiguous_width = timeline.context.ambiguous_width;
        let cursor = timeline.evaluate_for_status(effective_ms, state.playback);
        let geometry = CylinderGeometry::new(
            area.width,
            area.height,
            state.cylinder_speed_mode,
            f64::from(state.cylinder_speed_pct) / 100.0,
            state.cylinder_entry_lead_ms,
        );
        let has_bg_content = timeline.lines.iter().any(|line| {
            line.role == LineRole::BackgroundVocal
                || track.lines.get(line.source_line_idx).is_some_and(|source| {
                    source
                        .annotations
                        .iter()
                        .any(|annotation| annotation.kind == AnnotationKind::Background)
                })
        });
        // Main belt ink reaches at most belt_y + 3 (edge-stretched glyphs);
        // the strip sits one blank row below the lyric text block itself.
        let strip_y = area.y.saturating_add(geometry.belt_y()).saturating_add(5);
        let strip_visible = has_bg_content
            && background_strip_allowed(state)
            && strip_y < area.y.saturating_add(area.height);
        let annotations =
            collect_annotations(state, &cursor, timeline, track, area, ambiguous_width);
        let annotation_rows = u16::try_from(annotations.len()).unwrap_or(u16::MAX);

        // The belt always gets the full area so its geometry never depends
        // on background vocals or annotations; the strip renders right below
        // the lyric text and the annotation rows overlay beneath it.
        let main_area = area;
        let bg_area = Rect {
            x: area.x,
            y: strip_y,
            width: area.width,
            height: u16::from(strip_visible),
        };
        let annotation_y = if strip_visible {
            strip_y.saturating_add(1)
        } else {
            strip_y
        };
        let annotation_height = annotation_y
            .saturating_add(annotation_rows)
            .min(area.y.saturating_add(area.height))
            .saturating_sub(annotation_y);
        let annotation_area = Rect {
            x: area.x,
            y: annotation_y,
            width: area.width,
            height: annotation_height,
        };
        let cache = &mut ctx.cache.cylinder;
        cache.prepare(timeline.context.options_hash());

        let lead = state.cylinder_entry_lead_ms;
        let scan_until = effective_ms.saturating_add(lead);
        let start_index = timeline
            .lines
            .partition_point(|line| line.start_ms <= scan_until);
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
            let mut leftmost: Option<i32> = None;

            for glyph in &entry.glyphs {
                let screen_x = x.round() as i32;
                let style = word_style::style_for_byte(
                    compiled,
                    text,
                    glyph.byte_range,
                    &cursor,
                    active,
                    holding,
                    effective_ms,
                    geometry.depth(screen_x),
                    state.effective_word_highlight(),
                    &state.style,
                );
                let out_rows = geometry.stretch_rows(screen_x, BLOCK_ROWS);
                let top = geometry.glyph_top(out_rows);
                let base_cols = glyph.display_width.max(1);
                let out_cols = geometry.stretch_cols(screen_x, base_cols);
                let left_col = (f64::from(screen_x) + f64::from(base_cols) / 2.0
                    - f64::from(out_cols) / 2.0)
                    .round() as i32;
                if place_bitmap(cache, left_col, top, out_rows, out_cols, glyph, style) {
                    let visible_left = left_col.max(0);
                    leftmost = Some(leftmost.map_or(visible_left, |prev| prev.min(visible_left)));
                }
                x += f64::from(glyph.display_width);
            }

            if let Some(leftmost) = leftmost {
                clear_column(cache, leftmost - 1);
                any_cell = true;
            }
        }

        if any_cell {
            let (lines, _) = grid_to_lines(cache);
            frame.render_widget(Paragraph::new(lines), main_area);
        }

        if strip_visible {
            render_background_strip(
                frame,
                bg_area,
                state,
                timeline,
                track,
                effective_ms,
                ambiguous_width,
            );
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
            if annotation.kind == AnnotationKind::Background {
                continue;
            }
            rows.push(AnnotationRow {
                text: truncate_annotation(&annotation.text, area.width, ambiguous_width),
                style: state.style.annotation_style(),
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

/// Whether the background-vocal strip may render. The strip overlays the
/// last row of the area instead of reserving it, so this never affects the
/// belt geometry.
fn background_strip_allowed(state: &crate::ui::UiState) -> bool {
    if state.cylinder_annotations == CylinderAnnotationMode::Hide {
        return false;
    }
    !(state.cylinder_annotations == CylinderAnnotationMode::Auto && !state.style.annotations_show)
}

/// Renders background vocals as a single row of normal characters beneath
/// the belt. The text rides the same belt timing as the main lines but is
/// never stretched at the edges, and long text simply clips at the screen
/// borders while it scrolls through.
fn render_background_strip(
    frame: &mut Frame,
    area: Rect,
    state: &crate::ui::UiState,
    timeline: &crate::compiler::timeline::CompiledTimeline,
    track: &SyncedTrack,
    effective_ms: u64,
    ambiguous_width: u8,
) {
    let geometry = CylinderGeometry::new(
        area.width,
        area.height,
        state.cylinder_speed_mode,
        f64::from(state.cylinder_speed_pct) / 100.0,
        state.cylinder_entry_lead_ms,
    );
    let scan_until = effective_ms.saturating_add(state.cylinder_entry_lead_ms);
    let start_index = timeline
        .lines
        .partition_point(|line| line.start_ms <= scan_until);
    let mut visible: Vec<(&str, f64)> = Vec::new();
    for (scanned, index) in (0..start_index).rev().enumerate() {
        if scanned >= CYLINDER_MAX_VISIBLE_LINES {
            break;
        }
        let compiled = &timeline.lines[index];
        if compiled.role == LineRole::BackgroundVocal {
            let text = line_text(track, compiled);
            let text_width = display_width_with_ambiguous(text, ambiguous_width);
            let placement = geometry.place(compiled.start_ms, effective_ms, text_width);
            if placement.visible {
                visible.push((text, placement.first_col));
            }
        }
        let Some(source) = track.lines.get(compiled.source_line_idx) else {
            continue;
        };
        for annotation in &source.annotations {
            if annotation.kind != AnnotationKind::Background {
                continue;
            }
            let text = annotation.text.trim();
            if text.is_empty() {
                continue;
            }
            let text_width = display_width_with_ambiguous(text, ambiguous_width);
            let placement = geometry.place(compiled.start_ms, effective_ms, text_width);
            if placement.visible {
                visible.push((text, placement.first_col));
            }
        }
    }
    visible.reverse();

    let style = state.style.line_style(LineRole::BackgroundVocal, 0, true);
    let mut cells: Vec<Option<String>> = vec![None; usize::from(area.width)];
    for (text, first_col) in visible {
        let mut col = first_col;
        let mut last_placed: Option<usize> = None;
        for grapheme in text.graphemes(true) {
            let width = display_width_with_ambiguous(grapheme, ambiguous_width);
            if width == 0 {
                if let Some(previous) = last_placed
                    && let Some(slot) = &mut cells[previous]
                {
                    slot.push_str(grapheme);
                }
                continue;
            }
            let start = col.round();
            if start >= 0.0 && start < f64::from(area.width) {
                let index = start as usize;
                cells[index] = Some(grapheme.to_string());
                last_placed = Some(index);
            } else {
                last_placed = None;
            }
            col += f64::from(width);
        }
    }

    let buffer = frame.buffer_mut();
    for (offset, symbol) in cells.iter().enumerate() {
        let Some(symbol) = symbol else {
            continue;
        };
        let x = area.x.saturating_add(offset as u16);
        if let Some(cell) = buffer.cell_mut((x, area.y)) {
            cell.set_symbol(symbol.as_str());
            cell.set_style(style);
        }
    }
}

const BLOCK_ROWS: u16 = 4;

fn glyphs_for_text(text: &str, ambiguous_width: u8) -> (Vec<CylinderGlyph>, u16) {
    let mut glyphs: Vec<CylinderGlyph> = Vec::new();
    for (start, grapheme) in text.grapheme_indices(true) {
        let width = display_width_with_ambiguous(grapheme, ambiguous_width);
        if width == 0
            && let Some(previous) = glyphs.last_mut()
        {
            previous.text.push_str(grapheme);
            previous.byte_range.1 = start + grapheme.len();
            previous.bitmap = bitmap_for_cluster(&previous.text);
            previous.display_width = previous.bitmap.width;
            continue;
        }
        let bitmap = bitmap_for_cluster(grapheme);
        glyphs.push(CylinderGlyph {
            text: grapheme.to_string(),
            byte_range: (start, start + grapheme.len()),
            display_width: bitmap.width,
            bitmap,
        });
    }
    let text_width = glyphs.iter().fold(0u16, |width, glyph| {
        width.saturating_add(glyph.display_width)
    });
    (glyphs, text_width)
}

fn bitmap_for_cluster(cluster: &str) -> crate::ui::block::GlyphBitmap {
    if cluster.chars().all(char::is_whitespace) {
        return space_bitmap(BlockFontSize::Block);
    }
    glyph_bitmap_for_cluster(cluster, BlockFontSize::Block)
        .unwrap_or_else(|| placeholder_bitmap(BlockFontSize::Block))
}

fn line_text<'a>(track: &'a SyncedTrack, compiled: &CompiledLine) -> &'a str {
    track
        .lines
        .get(compiled.source_line_idx)
        .map_or("", |line| line.text.as_str())
}

fn place_bitmap(
    cache: &mut CylinderStageCache,
    left_col: i32,
    top: u16,
    out_rows: u16,
    out_cols: u16,
    glyph: &CylinderGlyph,
    style: Style,
) -> bool {
    let Ok(x) = u16::try_from(left_col.max(0)) else {
        return false;
    };
    let grid_width = cache.grid_width();
    let grid_height = cache.grid_height();
    if x >= grid_width || top >= grid_height {
        return false;
    }
    let base_rows = u16::try_from(glyph.bitmap.rows.len())
        .unwrap_or(BLOCK_ROWS)
        .max(1);
    let base_cols = usize::from(glyph.bitmap.width.max(1));
    let out_rows = out_rows.max(1);
    let total_out_cols = usize::from(out_cols.max(1));
    let skipped_cols = usize::try_from(
        left_col
            .checked_neg()
            .unwrap_or(i32::MAX)
            .max(0)
            .unsigned_abs(),
    )
    .unwrap_or(usize::MAX);
    if skipped_cols >= total_out_cols {
        return false;
    }
    let visible_out_cols = total_out_cols - skipped_cols;
    let mut placed = false;

    for j in 0..out_rows {
        let src_row = (usize::from(j) * usize::from(base_rows) / usize::from(out_rows))
            .min(usize::from(base_rows) - 1);
        let Some(row) = glyph.bitmap.rows.get(src_row) else {
            continue;
        };
        let y = top.saturating_add(j);
        if y >= grid_height {
            break;
        }
        let chars: Vec<char> = row.chars().collect();
        for c in 0..visible_out_cols {
            let source_col = c + skipped_cols;
            let src_col = (source_col * base_cols / total_out_cols).min(base_cols - 1);
            let Some(symbol) = chars.get(src_col) else {
                continue;
            };
            let column = x.saturating_add(c as u16);
            if column >= grid_width {
                break;
            }
            let Some(slot) = cache.cell_slot_mut(column, y) else {
                continue;
            };
            *slot = Some(GridCell {
                text: symbol.to_string(),
                style,
            });
            placed = true;
        }
    }
    placed
}

fn clear_column(cache: &mut CylinderStageCache, column: i32) {
    let Ok(x) = u16::try_from(column) else {
        return;
    };
    if x >= cache.grid_width() {
        return;
    }
    for y in 0..cache.grid_height() {
        if let Some(slot) = cache.cell_slot_mut(x, y) {
            *slot = None;
        }
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

    use super::super::{
        CYLINDER_DEFAULT_ENTRY_LEAD_MS, CYLINDER_MAX_SPEED, CYLINDER_MAX_STRETCH,
        CYLINDER_MIN_SPEED, CYLINDER_TRAVERSE_SECONDS,
    };
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

    fn find_text_pos(buffer: &Buffer, needle: &str) -> Option<(usize, usize)> {
        buffer_text(buffer)
            .into_iter()
            .enumerate()
            .find_map(|(y, row)| row.find(needle).map(|x| (y, x)))
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

    fn render_cylinder_with_area(
        track: SyncedTrack,
        effective_ms: u64,
        width: u16,
        height: u16,
    ) -> (Buffer, Rect) {
        let mut state = UiState::new();
        state.playback = PlaybackStatus::Playing;
        let profile = StyleProfile::builtin("builtin:dark", width).unwrap();
        let resolved = profile.resolve("dark", ColorCapability::TrueColor, None);
        state.set_resolved_style(resolved);
        state.display_mode = DisplayMode::Cylinder;
        state.set_lyrics(track);
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut cache = crate::ui::render::RenderCache::default();
        let mut area = Rect::default();
        terminal
            .draw(|frame| {
                area = crate::ui::render::render_with_cache_area(
                    frame,
                    &state,
                    &mut cache,
                    effective_ms,
                );
            })
            .unwrap();
        (terminal.backend().buffer().clone(), area)
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
    fn glyphs_merge_zero_width_graphemes_and_use_block_bitmaps() {
        let glyphs = glyphs("e\u{301}ab漢");
        assert_eq!(glyphs.len(), 4);
        assert_eq!(glyphs[0].text, "e\u{301}");
        assert_eq!(glyphs[0].byte_range, (0, 3));
        assert_eq!(glyphs[0].display_width, glyphs[0].bitmap.width);
        assert_eq!(glyphs[0].bitmap.rows.len(), 4);
        assert_eq!(glyphs[1].text, "a");
        assert_eq!(glyphs[3].text, "漢");
        assert_eq!(glyphs[3].bitmap, placeholder_bitmap(BlockFontSize::Block));
    }

    #[test]
    fn bitmap_fills_every_row_and_column() {
        let mut cache = grid_with(10, 8);
        let glyph = &glyphs("A")[0];
        let width = glyph.bitmap.width;
        assert!(place_bitmap(
            &mut cache,
            2,
            1,
            4,
            glyph.bitmap.width,
            glyph,
            Style::default()
        ));
        for row in 0..4u16 {
            for column in 0..width {
                assert!(
                    cache.cell(2 + column, 1 + row).is_some(),
                    "missing cell at {column},{row}"
                );
            }
        }
        let (lines, _) = grid_to_lines(&cache);
        assert!(lines[1].spans.iter().any(|span| span.content.contains('▀')
            || span.content.contains('█')
            || span.content.contains('▄')));
    }

    #[test]
    fn bitmap_clips_at_the_right_edge() {
        let mut cache = grid_with(10, 8);
        let glyph = &glyphs("A")[0];
        assert!(place_bitmap(
            &mut cache,
            8,
            1,
            4,
            glyph.bitmap.width,
            glyph,
            Style::default()
        ));
        assert!(cache.cell(9, 1).is_some());
        assert!(cache.cell(10, 1).is_none());
    }

    #[test]
    fn bitmap_clips_at_the_left_edge() {
        let mut cache = grid_with(10, 8);
        let glyph = &glyphs("A")[0];
        assert!(place_bitmap(
            &mut cache,
            -1,
            1,
            4,
            glyph.bitmap.width,
            glyph,
            Style::default()
        ));
        assert!(cache.cell(0, 1).is_some());
        assert!(cache.cell(0, 5).is_none());
    }

    #[test]
    fn clipping_at_edges_does_not_panic() {
        let mut cache = grid_with(10, 8);
        let glyph = &glyphs("A")[0];
        assert!(!place_bitmap(
            &mut cache,
            -(i32::from(glyph.bitmap.width)),
            1,
            4,
            glyph.bitmap.width,
            glyph,
            Style::default()
        ));
        assert!(!place_bitmap(
            &mut cache,
            10,
            1,
            4,
            glyph.bitmap.width,
            glyph,
            Style::default()
        ));
        assert!(!place_bitmap(
            &mut cache,
            0,
            99,
            4,
            glyph.bitmap.width,
            glyph,
            Style::default()
        ));
        assert!(cache.cell(0, 1).is_none());
    }

    #[test]
    fn stretched_bitmap_duplicates_source_rows() {
        let mut cache = grid_with(10, 10);
        let glyph = &glyphs("A")[0];
        assert!(place_bitmap(
            &mut cache,
            0,
            0,
            7,
            glyph.bitmap.width,
            glyph,
            Style::default()
        ));
        let rows = (0..7u16)
            .map(|row| {
                (0..glyph.bitmap.width)
                    .map(|column| {
                        cache
                            .cell(column, row)
                            .map(|cell| cell.text.clone())
                            .unwrap_or_default()
                    })
                    .collect::<String>()
            })
            .collect::<Vec<_>>();
        assert_eq!(rows[0], rows[1]);
        assert_eq!(rows[6], glyph.bitmap.rows[3]);
        assert!(rows.len() == 7);
    }

    fn synthetic_glyph(rows: &[&str]) -> CylinderGlyph {
        let width = rows[0].chars().count() as u16;
        CylinderGlyph {
            text: "x".to_string(),
            byte_range: (0, 1),
            display_width: width,
            bitmap: crate::ui::block::GlyphBitmap::new(
                width,
                rows.iter().map(|row| row.to_string()).collect(),
            ),
        }
    }

    #[test]
    fn stretched_bitmap_duplicates_columns_nearest_neighbor() {
        let mut cache = grid_with(10, 10);
        let glyph = synthetic_glyph(&["ab", "cd", "ef", "gh"]);
        assert!(place_bitmap(
            &mut cache,
            0,
            0,
            4,
            4,
            &glyph,
            Style::default()
        ));
        let row_text = |y: u16| {
            (0..4u16)
                .map(|x| {
                    cache
                        .cell(x, y)
                        .map(|cell| cell.text.clone())
                        .unwrap_or_default()
                })
                .collect::<String>()
        };
        assert_eq!(row_text(0), "aabb");
        assert_eq!(row_text(1), "ccdd");
        assert_eq!(row_text(2), "eeff");
        assert_eq!(row_text(3), "gghh");
    }

    #[test]
    fn clear_column_erases_every_cell_in_the_column() {
        let mut cache = grid_with(10, 8);
        let glyph = &glyphs("A")[0];
        assert!(place_bitmap(
            &mut cache,
            4,
            1,
            4,
            glyph.bitmap.width,
            glyph,
            Style::default()
        ));
        clear_column(&mut cache, 4);
        assert!(cache.cell(4, 1).is_none());
        assert!(cache.cell(4, 4).is_none());
        assert!(cache.cell(5, 1).is_some());
    }

    #[test]
    fn newer_line_overwrites_older_line_at_the_same_cell() {
        let mut cache = grid_with(10, 8);
        let older = &glyphs("A")[0];
        let newer = &glyphs("B")[0];
        assert!(place_bitmap(
            &mut cache,
            2,
            1,
            4,
            older.bitmap.width,
            older,
            Style::default()
        ));
        assert!(place_bitmap(
            &mut cache,
            2,
            1,
            4,
            newer.bitmap.width,
            newer,
            Style::default()
        ));
        assert_eq!(
            cache.cell(2, 1).unwrap().text,
            newer.bitmap.rows[0].chars().next().unwrap().to_string()
        );
    }

    fn any_ink(buffer: &Buffer, x: u16, rows: std::ops::Range<u16>) -> bool {
        rows.into_iter().any(|y| {
            matches!(
                buffer[(x, y)].symbol(),
                "▀" | "▄" | "█" | "▐" | "▌" | "▖" | "▗" | "▘" | "▝"
            )
        })
    }

    fn ink_count(
        buffer: &Buffer,
        columns: std::ops::Range<u16>,
        rows: std::ops::Range<u16>,
    ) -> usize {
        let mut count = 0;
        for y in rows {
            for x in columns.clone() {
                if any_ink(buffer, x, y..y + 1) {
                    count += 1;
                }
            }
        }
        count
    }

    fn lane_speed(area: Rect) -> f64 {
        (f64::from(area.width) / CYLINDER_TRAVERSE_SECONDS)
            .clamp(CYLINDER_MIN_SPEED, CYLINDER_MAX_SPEED)
    }

    fn first_col_at(area: Rect, start_ms: u64, effective_ms: u64) -> f64 {
        let lead_sec = CYLINDER_DEFAULT_ENTRY_LEAD_MS as f64 / 1000.0;
        f64::from(area.width - 1)
            - (((effective_ms - start_ms) as f64 / 1000.0) + lead_sec) * lane_speed(area)
    }

    fn stretched_left(area: Rect, head: f64, base_cols: u16) -> i32 {
        let t =
            ((head - f64::from(area.width) / 2.0) / (f64::from(area.width) / 2.0)).clamp(-1.0, 1.0);
        let out = (f64::from(base_cols) * (1.0 + CYLINDER_MAX_STRETCH * t * t))
            .round()
            .max(1.0);
        (head + f64::from(base_cols) / 2.0 - out / 2.0).round() as i32
    }

    fn column_is_blank(buffer: &Buffer, area: Rect, x: i32) -> bool {
        let Ok(x) = u16::try_from(x) else {
            return false;
        };
        (area.y..area.y + area.height).all(|y| {
            buffer[(x, y)]
                .symbol()
                .chars()
                .all(|symbol| symbol == ' ' || symbol.is_whitespace())
        })
    }

    #[test]
    fn two_contiguous_lines_render_across_the_lane() {
        let (_, area) =
            render_cylinder_with_area(track(vec![lyric_line(0, 1_000, "m")]), 0, 160, 40);
        let fc0 = first_col_at(area, 0, 3_000);
        let fc1 = first_col_at(area, 800, 3_000);
        let buffer = render_cylinder(
            track(vec![
                lyric_line(0, 3_000, "abc"),
                lyric_line(800, 3_800, "xyz"),
            ]),
            3_000,
            160,
            40,
        );
        let belt = area.y + area.height / 2;
        let band = belt.saturating_sub(4)..belt + 5;
        for head in [fc0, fc1] {
            let column = area.x as i32 + head.round() as i32;
            assert!(
                (column - 2..=column + 2)
                    .any(|x| x >= 0 && any_ink(&buffer, x as u16, band.clone())),
                "line head near column {column} must paint ink"
            );
        }
        let edge_ink = ink_count(
            &buffer,
            area.x..area.x + area.width,
            belt.saturating_sub(3)..belt + 5,
        );
        assert!(edge_ink > 20, "both lines painted, got {edge_ink} cells");
    }

    #[test]
    fn overlapping_lines_keep_one_blank_column() {
        let (_, area) =
            render_cylinder_with_area(track(vec![lyric_line(0, 1_000, "m")]), 0, 160, 40);
        let w_m = glyphs("m")[0].display_width;
        let fc0 = first_col_at(area, 0, 3_000);
        let nh = fc0 + f64::from(4 * w_m) - 10.0;
        let s1 = (3000.0
            - ((f64::from(area.width - 1) - nh) / lane_speed(area)
                - CYLINDER_DEFAULT_ENTRY_LEAD_MS as f64 / 1000.0)
                * 1000.0)
            .round() as u64;
        let (buffer, area) = render_cylinder_with_area(
            track(vec![
                lyric_line(0, 3_000, &"m".repeat(4)),
                lyric_line(s1, s1 + 800, "mmm"),
            ]),
            3_000,
            160,
            40,
        );
        let left = stretched_left(area, nh, w_m);
        let belt = area.y + area.height / 2;
        let band = belt.saturating_sub(4)..belt + 5;
        assert!(
            any_ink(&buffer, (area.x as i32 + left - 2) as u16, band.clone()),
            "older ink at {}",
            left - 2
        );
        assert!(
            any_ink(&buffer, (area.x as i32 + left) as u16, band.clone()),
            "newer ink at {left}"
        );
        assert!(
            column_is_blank(&buffer, area, area.x as i32 + left - 1),
            "column {} must stay blank between touching lines",
            left - 1
        );
    }

    #[test]
    fn many_overlapping_lines_keep_gaps_everywhere() {
        let (_, area) =
            render_cylinder_with_area(track(vec![lyric_line(0, 1_000, "m")]), 0, 160, 40);
        let w_m = glyphs("m")[0].display_width;
        let spacing = 2 * w_m - 6;
        let fc0 = first_col_at(area, 0, 3_000);
        let mut lines = Vec::new();
        let mut heads = Vec::new();
        for index in 0..12usize {
            let head = fc0 + (index as f64) * f64::from(spacing);
            if head >= f64::from(area.width) {
                break;
            }
            let start = (3000.0
                - ((f64::from(area.width - 1) - head) / lane_speed(area)
                    - CYLINDER_DEFAULT_ENTRY_LEAD_MS as f64 / 1000.0)
                    * 1000.0)
                .round()
                .max(0.0) as u64;
            lines.push(lyric_line(start, start + 500, &"m".repeat(2)));
            heads.push(head);
        }
        assert!(heads.len() >= 4, "expected several visible lines");
        let (buffer, area) = render_cylinder_with_area(track(lines), 3_000, 160, 40);
        for head in heads {
            let left = area.x as i32 + stretched_left(area, head, w_m);
            assert!(
                column_is_blank(&buffer, area, left - 1),
                "gap column {} must stay blank",
                left - 1
            );
        }
    }

    #[test]
    fn long_line_survives_more_than_eight_newer_starts() {
        let alphabet = "abcdefghijklmnopqrstuvwxyz";
        let long_text: String = alphabet.repeat(12).chars().take(300).collect();
        let mut lines = vec![lyric_line(0, 1_000, &long_text)];
        for index in 1..=10u64 {
            lines.push(lyric_line(
                index * 1_000,
                index * 1_000 + 500,
                &format!("w{index}"),
            ));
        }
        let buffer = render_cylinder(track(lines), 10_000, 120, 40);
        let center = buffer.area.height / 2;
        let long_line_ink = ink_count(&buffer, 0..40, center - 6..center + 8);
        assert!(
            long_line_ink > 0,
            "long line must remain painted near the left edge"
        );
        let newer_ink = ink_count(&buffer, 80..120, center - 4..center + 6);
        assert!(newer_ink > 0, "newer lines paint near the right edge");
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
            1,
        );
        assert_eq!(rows.len(), CYLINDER_MAX_ANNOTATIONS);
        assert_eq!(rows[0].text, "one");
        assert_eq!(rows[1].text, "two");
    }

    #[test]
    fn annotations_do_not_shift_the_belt() {
        let mut annotated = lyric_line(0, 4_000, "main line");
        annotated.annotations = vec![Annotation {
            kind: crate::core::model::AnnotationKind::Translation,
            text: "terjemahan".to_string(),
        }];
        let with = track(vec![annotated, lyric_line(5_000, 9_000, "second line")]);
        let without = track(vec![
            lyric_line(0, 4_000, "main line"),
            lyric_line(5_000, 9_000, "second line"),
        ]);
        let (a, area) = render_cylinder_with_area(with, 2_000, 80, 24);
        let (b, _) = render_cylinder_with_area(without, 2_000, 80, 24);

        let rows_a = buffer_text(&a);
        let rows_b = buffer_text(&b);
        let annotation_row = usize::from(area.y) + usize::from(area.height) / 2 + 5;
        for (y, row) in rows_a.iter().enumerate().take(annotation_row) {
            assert_eq!(
                row, &rows_b[y],
                "belt row {y} must not move for annotations"
            );
        }
        assert!(
            rows_a[annotation_row].contains("terjemahan"),
            "annotation renders on the overlay row"
        );
        assert!(
            rows_b[annotation_row].chars().all(|c| c == ' '),
            "no annotation, no overlay ink"
        );
    }

    #[test]
    fn background_vocals_render_as_a_moving_strip() {
        let source = track(vec![
            lyric_line(0, 60_000, "main line"),
            lyric_line_role(1_000, 3_000, "backing vocals", LineRole::BackgroundVocal),
        ]);
        let (first, area) = render_cylinder_with_area(source.clone(), 2_000, 80, 24);
        let (later, _) = render_cylinder_with_area(source, 3_000, 80, 24);

        let strip_row = usize::from(area.y) + usize::from(area.height) / 2 + 5;
        let (row, col) = find_text_pos(&first, "backing vocals").expect("strip text rendered");
        assert_eq!(
            row, strip_row,
            "strip sits one blank row below the lyric text block"
        );
        assert!(
            col > usize::from(area.width) / 2,
            "strip enters from the right: col {col}"
        );
        let (_, later_col) = find_text_pos(&later, "backing vocals").expect("still rendered");
        assert!(
            later_col < col,
            "strip moves left with the belt: {later_col} vs {col}"
        );

        let gap_row = &buffer_text(&first)[strip_row - 1];
        assert!(
            gap_row.chars().all(|c| c == ' '),
            "gap row between belt and strip must be blank: {gap_row:?}"
        );
    }

    #[test]
    fn background_vocals_do_not_shift_the_main_layout() {
        let source = track(vec![
            lyric_line(0, 60_000, "main line"),
            lyric_line_role(10_000, 12_000, "backing vocals", LineRole::BackgroundVocal),
        ]);
        let (before, area) = render_cylinder_with_area(source.clone(), 2_000, 80, 24);
        let (during, _) = render_cylinder_with_area(source, 11_000, 80, 24);

        let strip_row = usize::from(area.y) + usize::from(area.height) / 2 + 5;
        let before_rows = buffer_text(&before);
        let during_rows = buffer_text(&during);
        assert!(
            before_rows[strip_row].chars().all(|c| c == ' '),
            "strip row is reserved before the vocal starts"
        );
        assert!(
            during_rows[strip_row].contains("backing vocals"),
            "strip renders once the vocal is active"
        );
        let gap_row = strip_row - 1;
        assert!(
            before_rows[gap_row].chars().all(|c| c == ' '),
            "gap row stays blank before the vocal starts"
        );
        assert!(
            during_rows[gap_row].chars().all(|c| c == ' '),
            "gap row stays blank while the vocal renders, so the belt never shifts"
        );
    }

    #[test]
    fn inline_background_annotations_ride_the_strip() {
        let mut line = lyric_line(0, 60_000, "main line");
        line.annotations = vec![Annotation {
            kind: crate::core::model::AnnotationKind::Background,
            text: "(echo)".to_string(),
        }];
        let source = track(vec![line, lyric_line(5_000, 9_000, "second line")]);
        let (first, area) = render_cylinder_with_area(source.clone(), 2_000, 80, 24);
        let (later, _) = render_cylinder_with_area(source, 3_000, 80, 24);

        let strip_row = usize::from(area.y) + usize::from(area.height) / 2 + 5;
        let (row, col) = find_text_pos(&first, "(echo)").expect("strip text rendered");
        assert_eq!(
            row, strip_row,
            "inline background rides the strip below the lyrics"
        );
        assert!(
            col + 6 > usize::from(area.width) / 2,
            "strip enters from the right: col {col}"
        );
        let (_, later_col) = find_text_pos(&later, "(echo)").expect("still rendered");
        assert!(
            later_col < col,
            "strip moves left with the belt: {later_col} vs {col}"
        );

        let rows = buffer_text(&first);
        assert!(
            !rows[strip_row + 1].contains("(echo)"),
            "inline background must not render as a static annotation"
        );
    }

    #[test]
    fn background_strip_respects_hide_mode() {
        let source = track(vec![
            lyric_line(0, 60_000, "main line"),
            lyric_line_role(1_000, 3_000, "backing vocals", LineRole::BackgroundVocal),
        ]);
        let mut state = UiState::new();
        state.playback = PlaybackStatus::Playing;
        let profile = StyleProfile::builtin("builtin:dark", 80).unwrap();
        let resolved = profile.resolve("dark", ColorCapability::TrueColor, None);
        state.set_resolved_style(resolved);
        state.display_mode = DisplayMode::Cylinder;
        state.set_cylinder_annotations(CylinderAnnotationMode::Hide);
        state.set_lyrics(source);
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| crate::ui::render::render(frame, &state, 2_000))
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        assert!(
            find_text_pos(&buffer, "backing vocals").is_none(),
            "hide mode must suppress the strip"
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
        let buffer = terminal.backend().buffer().clone();
        let rows = buffer_text(&buffer);
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
        let belt_ink = ink_count(&buffer, 0..80, 0..(row_index as u16));
        assert!(belt_ink > 0, "main block line rendered");
        assert!(row_index as u16 > 0, "annotations must sit below the belt");
    }
}

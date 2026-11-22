use std::f32::consts::FRAC_PI_2;

use ratatui::{Frame, layout::Rect, widgets::Paragraph};
use unicode_segmentation::UnicodeSegmentation;

use crate::compiler::timeline::CompiledLine;
use crate::core::model::{LineRole, SyncedTrack};
use crate::core::text::display_width_with_ambiguous;
use crate::ui::block::geometry::{
    ink_width, push_source_points, source_bitmap, source_bitmap_for_cluster,
};
use crate::ui::render::{ModeRenderer, RenderContext, centered_message};
use crate::ui::word_style;

use super::{
    CANVAS3D_CROSS_SECONDS, CANVAS3D_FLIP_MS, CANVAS3D_GLYPH_GAP, CANVAS3D_GLYPH_SCALE,
    CANVAS3D_MAX_VISIBLE_LINES, CANVAS3D_SPACE_ADVANCE, CANVAS3D_Y_STRETCH, CANVAS3D_ZPOP_SCALE,
    CanvasGlyph, camera::CrawlCamera, dim_depth_for, spawn_v_for, text_hash,
};

pub(crate) struct Canvas3DRenderer;

impl ModeRenderer for Canvas3DRenderer {
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
        let cache = &mut ctx.cache.canvas;
        cache.prepare(timeline.context.options_hash());
        cache.prepare_canvas(area.width, area.height);
        let tilt_deg = f32::from(state.canvas3d_tilt_deg);
        let camera = CrawlCamera::new(area.width, area.height, tilt_deg);
        let v_enter = spawn_v_for(tilt_deg, state.canvas3d_spawn_y_pct);
        let dim_depth = dim_depth_for(v_enter);
        let glyph_scale = CANVAS3D_GLYPH_SCALE * f32::from(state.canvas3d_zoom_pct) / 100.0;

        let scan_lead = u64::from(state.canvas3d_flip) * CANVAS3D_FLIP_MS;
        let scan_until = effective_ms.saturating_add(scan_lead);
        let start_index = timeline
            .lines
            .partition_point(|line| line.start_ms <= scan_until);
        let mut visible = Vec::new();
        let speed_v =
            v_enter / CANVAS3D_CROSS_SECONDS * (f32::from(state.canvas3d_speed_pct) / 100.0);

        for (scanned, index) in (0..start_index).rev().enumerate() {
            if scanned >= CANVAS3D_MAX_VISIBLE_LINES {
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
            cache.ensure_line(index, text, ambiguous_width, || {
                glyphs_for_text(text, ambiguous_width)
            });
            let elapsed = effective_ms.saturating_sub(compiled.start_ms) as f32;
            let v_line = v_enter - elapsed / 1000.0 * speed_v;
            if v_line > 1.0 {
                visible.push((index, text, v_line));
            }
        }

        visible.reverse();
        let visible_indices = visible
            .iter()
            .map(|(index, _, _)| *index)
            .collect::<Vec<_>>();
        cache.retain_visible(&visible_indices);

        for (index, text, v_line) in visible {
            let Some(entry) = cache.line(index, text_hash(text), ambiguous_width).cloned() else {
                continue;
            };
            let compiled = &timeline.lines[index];
            let active = cursor.active_line_idx == Some(index);
            let holding = active && effective_ms > compiled.end_ms;
            let flip_angle = if state.canvas3d_flip {
                flip_angle_for(compiled, effective_ms)
            } else {
                0.0
            };
            if flip_angle >= FRAC_PI_2 {
                continue;
            }
            let total_px: u16 = entry
                .glyphs
                .iter()
                .map(|glyph| glyph.advance)
                .fold(0u16, |sum, advance| sum.saturating_add(advance));
            let half_width = i64::from(total_px / 2);
            let mut glyph_x = -half_width;

            for glyph in &entry.glyphs {
                let style = word_style::style_for_byte(
                    compiled,
                    text,
                    glyph.byte_range,
                    &cursor,
                    active,
                    holding,
                    effective_ms,
                    if v_line > dim_depth { 0.0 } else { 1.0 },
                    state.effective_word_highlight(),
                    &state.style,
                );
                let pop = state.effective_word_highlight()
                    && active
                    && !holding
                    && cursor.active_word_idx.is_some()
                    && word_style::word_index_for_byte(compiled, glyph.byte_range.0)
                        == cursor.active_word_idx;
                let scale = if pop { CANVAS3D_ZPOP_SCALE } else { 1.0 };
                let center_x = f32::from(glyph.advance.saturating_sub(CANVAS3D_GLYPH_GAP)) / 2.0;
                for (px, py) in &glyph.points {
                    let sx = center_x + (f32::from(*px) - center_x) * scale;
                    let sy = 3.5 + (f32::from(*py) - 3.5) * scale;
                    let u_rel = glyph_x as f32 + sx;
                    let u = u_rel * flip_angle.cos() * glyph_scale;
                    let w = u_rel * flip_angle.sin() * glyph_scale;
                    let v = v_line + (7.0 - sy) * CANVAS3D_Y_STRETCH * glyph_scale;
                    if let Some((x, y, _)) = camera.project(u, v, w) {
                        cache.plot(x, y, style);
                    }
                }
                glyph_x += i64::from(glyph.advance);
            }
        }

        if cache.has_dots() {
            let lines = cache.canvas_lines();
            frame.render_widget(Paragraph::new(lines), area);
        }
    }
}

fn flip_angle_for(compiled: &CompiledLine, effective_ms: u64) -> f32 {
    if effective_ms >= compiled.start_ms {
        return 0.0;
    }
    let flip_start = compiled.start_ms.saturating_sub(CANVAS3D_FLIP_MS);
    if effective_ms <= flip_start {
        return FRAC_PI_2;
    }
    let progress = (effective_ms - flip_start) as f32 / CANVAS3D_FLIP_MS as f32;
    (1.0 - progress) * FRAC_PI_2
}

fn line_text<'a>(track: &'a SyncedTrack, compiled: &CompiledLine) -> &'a str {
    track
        .lines
        .get(compiled.source_line_idx)
        .map(|line| line.text.as_str())
        .unwrap_or("")
}

fn glyphs_for_text(text: &str, ambiguous_width: u8) -> (Vec<CanvasGlyph>, u16) {
    let mut glyphs: Vec<CanvasGlyph> = Vec::new();
    for (start, grapheme) in text.grapheme_indices(true) {
        let width = display_width_with_ambiguous(grapheme, ambiguous_width);
        if width == 0
            && let Some(previous) = glyphs.last_mut()
        {
            previous.text.push_str(grapheme);
            previous.byte_range.1 = start + grapheme.len();
            let (points, advance) = glyph_points(&previous.text);
            previous.points = points;
            previous.advance = advance;
            continue;
        }
        let (points, advance) = glyph_points(grapheme);
        glyphs.push(CanvasGlyph {
            text: grapheme.to_string(),
            byte_range: (start, start + grapheme.len()),
            points,
            advance,
        });
    }
    let text_width = glyphs
        .iter()
        .fold(0u16, |width, glyph| width.saturating_add(glyph.advance));
    (glyphs, text_width)
}

fn glyph_points(cluster: &str) -> (Vec<(u8, u8)>, u16) {
    if cluster.chars().all(char::is_whitespace) {
        return (Vec::new(), CANVAS3D_SPACE_ADVANCE);
    }
    let Some(bitmap) = source_bitmap_for_cluster(cluster).or_else(|| source_bitmap('?')) else {
        return (Vec::new(), CANVAS3D_SPACE_ADVANCE);
    };
    let mut points = Vec::new();
    push_source_points(bitmap, &mut points);
    let advance = ink_width(bitmap).max(1) + CANVAS3D_GLYPH_GAP;
    (points, advance)
}

#[cfg(test)]
mod tests {
    use ratatui::{Terminal, backend::TestBackend, buffer::Buffer};

    use crate::ui::render::{WordState, word_state_style};

    use super::super::{
        CANVAS3D_DEFAULT_SPAWN_PCT, CANVAS3D_DEFAULT_TILT_DEG, CANVAS3D_DEFAULT_ZOOM_PCT,
        CANVAS3D_MIN_HEIGHT, CANVAS3D_MIN_WIDTH,
    };
    use super::*;
    use crate::core::clock::PlaybackStatus;
    use crate::core::model::{LyricLine, SyncGranularity, WordToken};
    use crate::style::{StyleProfile, color::ColorCapability};
    use crate::ui::{DisplayMode, UiState};

    fn lyric_line(start: u64, end: u64, text: &str) -> LyricLine {
        LyricLine {
            start_ms: start,
            end_ms: end,
            text: text.to_string(),
            words: Vec::new(),
            annotations: Vec::new(),
            role: LineRole::Primary,
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

    fn render_canvas3d(track: SyncedTrack, effective_ms: u64, width: u16, height: u16) -> Buffer {
        render_canvas3d_opts(track, effective_ms, width, height, Canvas3dTestOpts::new())
    }

    #[derive(Clone, Default)]
    struct Canvas3dTestOpts {
        flip: bool,
        speed_pct: u16,
        tilt_deg: u16,
        spawn_pct: u16,
        zoom_pct: u16,
    }

    impl Canvas3dTestOpts {
        fn new() -> Self {
            Self {
                speed_pct: 100,
                tilt_deg: CANVAS3D_DEFAULT_TILT_DEG,
                spawn_pct: CANVAS3D_DEFAULT_SPAWN_PCT,
                zoom_pct: CANVAS3D_DEFAULT_ZOOM_PCT,
                ..Self::default()
            }
        }
    }

    fn render_canvas3d_opts(
        track: SyncedTrack,
        effective_ms: u64,
        width: u16,
        height: u16,
        opts: Canvas3dTestOpts,
    ) -> Buffer {
        let mut state = UiState::new();
        state.set_canvas3d_flip(opts.flip);
        state.set_canvas3d_speed_pct(opts.speed_pct);
        state.set_canvas3d_tilt_deg(opts.tilt_deg);
        state.set_canvas3d_spawn_y_pct(opts.spawn_pct);
        state.set_canvas3d_zoom_pct(opts.zoom_pct);
        state.resize(width, height);
        state.playback = PlaybackStatus::Playing;
        let profile = StyleProfile::builtin("builtin:dark", width).unwrap();
        let resolved = profile.resolve("dark", ColorCapability::TrueColor, None);
        state.set_resolved_style(resolved);
        state.display_mode = DisplayMode::Canvas3D;
        state.set_lyrics(track);
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| crate::ui::render::render(frame, &state, effective_ms))
            .unwrap();
        terminal.backend().buffer().clone()
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

    fn count_braille(buffer: &Buffer) -> usize {
        buffer_text(buffer)
            .iter()
            .flat_map(|row| row.chars())
            .filter(|c| ('\u{2800}'..='\u{28FF}').contains(c))
            .count()
    }

    #[test]
    fn glyphs_assign_points_and_advances() {
        let (glyphs, width) = glyphs_for_text("ab", 1);
        assert_eq!(glyphs.len(), 2);
        assert!(!glyphs[0].points.is_empty());
        assert!(!glyphs[1].points.is_empty());
        assert_eq!(width, glyphs[0].advance + glyphs[1].advance);
        assert!(glyphs[0].advance >= 2);
    }

    #[test]
    fn spaces_advance_without_points() {
        let (glyphs, width) = glyphs_for_text("a b", 1);
        assert_eq!(glyphs.len(), 3);
        assert!(glyphs[1].points.is_empty());
        assert_eq!(glyphs[1].advance, CANVAS3D_SPACE_ADVANCE);
        assert_eq!(
            width,
            glyphs[0].advance + CANVAS3D_SPACE_ADVANCE + glyphs[2].advance
        );
    }

    #[test]
    fn unrenderable_cluster_uses_placeholder_points() {
        let (glyphs, _) = glyphs_for_text("世", 1);
        assert_eq!(glyphs.len(), 1);
        let placeholder: Vec<(u8, u8)> = {
            let mut points = Vec::new();
            push_source_points(source_bitmap('?').unwrap(), &mut points);
            points
        };
        assert_eq!(glyphs[0].points, placeholder);
    }

    #[test]
    fn active_line_renders_braille_dots() {
        let source = track(vec![lyric_line(0, 60_000, "hold the line")]);
        let buffer = render_canvas3d(source, 2_000, 120, 40);
        assert!(count_braille(&buffer) > 50, "expected braille dots");
    }

    #[test]
    fn line_before_entry_renders_no_dots() {
        let source = track(vec![lyric_line(100_000, 160_000, "hold the line")]);
        let buffer = render_canvas3d(source, 0, 120, 40);
        assert_eq!(count_braille(&buffer), 0);
    }

    #[test]
    fn line_past_exit_renders_no_dots() {
        let source = track(vec![lyric_line(0, 60_000, "hold the line")]);
        let buffer = render_canvas3d(source, 400_000, 120, 40);
        assert_eq!(count_braille(&buffer), 0);
    }

    #[test]
    fn narrow_viewport_falls_back_to_normal_rendering() {
        let source = track(vec![lyric_line(0, 60_000, "hold the line")]);
        let buffer = render_canvas3d(source, 1_500, CANVAS3D_MIN_WIDTH - 1, 40);
        let text = buffer_text(&buffer).join("\n");
        assert!(
            text.contains("hold the line"),
            "must fall back to Normal below 60 cols"
        );
    }

    #[test]
    fn short_viewport_falls_back_to_normal_rendering() {
        let source = track(vec![lyric_line(0, 60_000, "hold the line")]);
        let buffer = render_canvas3d(source, 1_500, 120, CANVAS3D_MIN_HEIGHT - 1);
        let text = buffer_text(&buffer).join("\n");
        assert!(
            text.contains("hold the line"),
            "must fall back to Normal below 16 rows"
        );
    }

    #[test]
    fn flip_angle_completes_exactly_at_line_start() {
        let compiled = CompiledLine {
            source_line_idx: 0,
            start_ms: 8_000,
            end_ms: 16_000,
            role: LineRole::Primary,
            wrapped_main: Vec::new(),
            wrapped_annotations: Vec::new(),
            words: Vec::new(),
            phrases: Vec::new(),
        };
        assert_eq!(flip_angle_for(&compiled, 9_000), 0.0);
        assert_eq!(flip_angle_for(&compiled, 8_000), 0.0);
        assert_eq!(
            flip_angle_for(&compiled, 7_800),
            std::f32::consts::FRAC_PI_4
        );
        assert_eq!(
            flip_angle_for(&compiled, 7_600),
            std::f32::consts::FRAC_PI_2
        );
        assert_eq!(
            flip_angle_for(&compiled, 7_000),
            std::f32::consts::FRAC_PI_2
        );
    }

    #[test]
    fn line_is_hidden_until_its_flip_opens() {
        let source = track(vec![lyric_line(8_000, 16_000, "hold the line")]);
        let flip_opts = Canvas3dTestOpts {
            flip: true,
            ..Canvas3dTestOpts::new()
        };
        let before = render_canvas3d_opts(source.clone(), 7_000, 120, 40, flip_opts.clone());
        assert_eq!(count_braille(&before), 0, "edge-on flip must hide the line");
        let mid = render_canvas3d_opts(source.clone(), 7_800, 120, 40, flip_opts.clone());
        assert!(
            count_braille(&mid) > 0,
            "mid-flip line must be partly visible"
        );
        let open = render_canvas3d_opts(source, 10_000, 120, 40, flip_opts);
        assert!(
            count_braille(&open) > count_braille(&mid),
            "open line shows more dots"
        );
    }

    #[test]
    fn flip_disabled_hides_the_line_before_its_start() {
        let source = track(vec![lyric_line(8_000, 16_000, "hold the line")]);
        let buffer = render_canvas3d_opts(source, 7_000, 120, 40, Canvas3dTestOpts::new());
        assert_eq!(
            count_braille(&buffer),
            0,
            "without flip and without lead the line must wait for its start"
        );
    }

    #[test]
    fn speed_pct_shifts_the_line_along_the_plane() {
        let source = track(vec![lyric_line(0, 60_000, "hold the line")]);
        let slow = render_canvas3d_opts(
            source.clone(),
            2_000,
            120,
            40,
            Canvas3dTestOpts {
                speed_pct: 50,
                ..Canvas3dTestOpts::new()
            },
        );
        let fast = render_canvas3d_opts(
            source,
            2_000,
            120,
            40,
            Canvas3dTestOpts {
                speed_pct: 300,
                ..Canvas3dTestOpts::new()
            },
        );
        let v_enter = spawn_v_for(
            f32::from(CANVAS3D_DEFAULT_TILT_DEG),
            CANVAS3D_DEFAULT_SPAWN_PCT,
        );
        let base_speed = v_enter / CANVAS3D_CROSS_SECONDS;
        let slow_v = v_enter - 2.0 * base_speed * 0.5;
        let fast_v = v_enter - 2.0 * base_speed * 3.0;
        assert!(
            fast_v < 1.0,
            "300% must push the line past the exit: {fast_v}"
        );
        assert_eq!(count_braille(&fast), 0, "fast line must have exited");
        assert!(slow_v > 1.0, "slow line still on the plane: {slow_v}");
        assert!(count_braille(&slow) > 0);
    }

    #[test]
    fn shallower_tilt_renders_larger_entry_text() {
        let source = track(vec![lyric_line(0, 60_000, "hold the line")]);
        let steep = render_canvas3d_opts(source.clone(), 1_500, 120, 40, Canvas3dTestOpts::new());
        let shallow = render_canvas3d_opts(
            source,
            1_500,
            120,
            40,
            Canvas3dTestOpts {
                tilt_deg: 45,
                ..Canvas3dTestOpts::new()
            },
        );
        assert!(
            count_braille(&shallow) > count_braille(&steep),
            "shallower tilt must render more dots: {} vs {}",
            count_braille(&shallow),
            count_braille(&steep)
        );
    }

    #[test]
    fn lower_spawn_height_renders_larger_text() {
        let source = track(vec![lyric_line(0, 60_000, "hold the line")]);
        let deep = render_canvas3d_opts(source.clone(), 1_500, 120, 40, Canvas3dTestOpts::new());
        let low = render_canvas3d_opts(
            source,
            1_500,
            120,
            40,
            Canvas3dTestOpts {
                spawn_pct: 55,
                ..Canvas3dTestOpts::new()
            },
        );
        assert!(
            count_braille(&low) > count_braille(&deep),
            "lower spawn must render more dots: {} vs {}",
            count_braille(&low),
            count_braille(&deep)
        );
    }

    #[test]
    fn zoom_out_fits_the_whole_line() {
        let long_line = "la ".repeat(20);
        let source = track(vec![lyric_line(0, 60_000, long_line.trim_end())]);
        let braille_span = |buffer: &Buffer| {
            let mut min_x = u16::MAX;
            let mut max_x = 0u16;
            for y in 0..buffer.area.height {
                for x in 0..buffer.area.width {
                    let sym = buffer[(x, y)].symbol().chars().next().unwrap_or(' ');
                    if ('\u{2800}'..='\u{28FF}').contains(&sym) {
                        min_x = min_x.min(x);
                        max_x = max_x.max(x);
                    }
                }
            }
            assert!(min_x != u16::MAX, "expected braille dots");
            u32::from(max_x - min_x)
        };
        let zoomed_in =
            render_canvas3d_opts(source.clone(), 1_500, 120, 40, Canvas3dTestOpts::new());
        let zoomed_out = render_canvas3d_opts(
            source,
            1_500,
            120,
            40,
            Canvas3dTestOpts {
                zoom_pct: 50,
                ..Canvas3dTestOpts::new()
            },
        );
        assert!(
            braille_span(&zoomed_out) < braille_span(&zoomed_in),
            "zoomed-out line must fit within a narrower span: {} vs {}",
            braille_span(&zoomed_out),
            braille_span(&zoomed_in)
        );
    }

    #[test]
    fn flat_lines_are_centered_horizontally() {
        let source = track(vec![lyric_line(0, 60_000, "hold the line")]);
        let buffer = render_canvas3d(source, 1_500, 120, 40);
        let mut min_x = u16::MAX;
        let mut max_x = 0u16;
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                let sym = buffer[(x, y)].symbol().chars().next().unwrap_or(' ');
                if ('\u{2800}'..='\u{28FF}').contains(&sym) {
                    min_x = min_x.min(x);
                    max_x = max_x.max(x);
                }
            }
        }
        assert!(min_x != u16::MAX, "expected braille dots");
        let center = (u32::from(min_x) + u32::from(max_x)) as f32 / 2.0;
        let expected = f32::from(buffer.area.width) / 2.0;
        assert!(
            (center - expected).abs() < 4.0,
            "dots span {min_x}..{max_x}, center {center} vs {expected}"
        );
    }

    #[test]
    fn active_word_dots_carry_the_active_style() {
        let mut line = lyric_line(0, 60_000, "hold the line");
        line.words = vec![
            WordToken {
                byte_range: (0, 4),
                start_ms: 0,
                end_ms: 2_000,
                display_width: 4,
            },
            WordToken {
                byte_range: (5, 8),
                start_ms: 4_000,
                end_ms: 8_000,
                display_width: 3,
            },
            WordToken {
                byte_range: (9, 13),
                start_ms: 9_000,
                end_ms: 12_000,
                display_width: 4,
            },
        ];
        let source = track(vec![line]);
        let buffer = render_canvas3d(source, 2_000, 120, 40);

        let profile = StyleProfile::builtin("builtin:dark", 120).unwrap();
        let resolved = profile.resolve("dark", ColorCapability::TrueColor, None);
        let active_style = word_state_style(&resolved, LineRole::Primary, WordState::Active);
        let unsung_style = word_state_style(&resolved, LineRole::Primary, WordState::Unsung);
        assert_ne!(
            active_style.fg, unsung_style.fg,
            "test setup needs distinct colours"
        );

        let mut saw_active = false;
        let mut saw_unsung = false;
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                let cell = &buffer[(x, y)];
                let sym = cell.symbol().chars().next().unwrap_or(' ');
                if ('\u{2800}'..='\u{28FF}').contains(&sym) {
                    saw_active |= Some(cell.fg) == active_style.fg;
                    saw_unsung |= Some(cell.fg) == unsung_style.fg;
                }
            }
        }
        assert!(saw_active, "active word dots must carry the active style");
        assert!(saw_unsung, "unsung word dots must carry the unsung style");
    }
}

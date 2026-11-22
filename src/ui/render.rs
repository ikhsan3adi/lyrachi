use std::borrow::Cow;
use std::time::Instant;

use super::{
    DisplayMode, LyricsView, StatusBarPlacement, UiModal, UiState,
    block::{BlockStageCache, stage::BlockRenderer},
    canvas3d::{CanvasStageCache, render::Canvas3DRenderer},
    cylinder::{CylinderStageCache, render::CylinderRenderer},
    effects::UiEffects,
    help,
    kinetic::KineticRenderer,
    menu,
    normal::{NormalRenderer, render_unsynced},
    status_bar::{banner_line, render as render_status_bar},
};
use crate::compiler::timeline::{CompiledLine, CompiledTimeline, WaitingGap};
use crate::core::model::{LineRole, SyncedTrack};
use crate::media::MediaAvailability;
use crate::style::{InstrumentalStyle, ResolvedStyle};
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Paragraph, Wrap},
};

pub(crate) const BLOCK_COUNTDOWN_ROWS: usize = 3;

#[derive(Clone, Copy)]
pub(crate) enum CountdownSize {
    Line,
    Block,
}

#[derive(Debug, Default)]
pub struct RenderCache {
    pub block: BlockStageCache,
    pub cylinder: CylinderStageCache,
    pub canvas: CanvasStageCache,
}

impl RenderCache {
    pub fn clear(&mut self) {
        self.block.clear();
        self.cylinder.clear();
        self.canvas.clear();
    }
}

pub(crate) struct RenderContext<'a> {
    pub state: &'a UiState,
    pub timeline: &'a CompiledTimeline,
    pub track: &'a SyncedTrack,
    pub effective_ms: u64,
    pub cache: &'a mut RenderCache,
}

pub(crate) trait ModeRenderer {
    fn render(&self, frame: &mut Frame, area: Rect, ctx: &mut RenderContext<'_>);
}

pub fn render(frame: &mut Frame, state: &UiState, effective_ms: u64) {
    let mut cache = RenderCache::default();
    let _ = render_with_cache_area(frame, state, &mut cache, effective_ms);
}

pub fn render_with_cache(
    frame: &mut Frame,
    state: &UiState,
    render_cache: &mut RenderCache,
    effective_ms: u64,
) {
    let _ = render_with_cache_area(frame, state, render_cache, effective_ms);
}

pub fn render_with_cache_and_effects(
    frame: &mut Frame,
    state: &UiState,
    render_cache: &mut RenderCache,
    effects: &mut UiEffects,
    effective_ms: u64,
) -> Rect {
    effects.observe_state(state, effective_ms);
    let stage_area = render_internal(frame, state, render_cache, effective_ms);
    effects.process_now(Instant::now(), state, frame.buffer_mut(), stage_area);
    stage_area
}

pub fn render_with_cache_area(
    frame: &mut Frame,
    state: &UiState,
    render_cache: &mut RenderCache,
    effective_ms: u64,
) -> Rect {
    render_internal(frame, state, render_cache, effective_ms)
}

fn render_internal(
    frame: &mut Frame,
    state: &UiState,
    render_cache: &mut RenderCache,
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
    render_body(frame, stage_area, state, render_cache, effective_ms);
    if banner_rows > 0
        && let Some(line) = banner_line(state)
    {
        frame.render_widget(Paragraph::new(line), content_area(banner, padding));
    }
    if bottom_status_rows > 0 {
        render_status_bar(frame, bottom_status, state);
    }
    match &state.modal {
        Some(UiModal::Help) => help::render(frame, area, state),
        Some(UiModal::ContextMenu(menu_state)) => menu::render(
            frame,
            area,
            menu_state,
            &state.style,
            state.terminal_unicode,
        ),
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
pub(crate) enum WordState {
    Active,
    Sung,
    Unsung,
}

pub(crate) fn word_state_style(
    style: &ResolvedStyle,
    role: LineRole,
    word_state: WordState,
) -> Style {
    match word_state {
        WordState::Active => style.word_style(role, true, false),
        WordState::Sung => style.word_style(role, false, true),
        WordState::Unsung => style.word_style(role, false, false),
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
    render_cache: &mut RenderCache,
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
                &state.style,
                "Media source unavailable",
                "This build has no media backend for this platform yet.",
                false,
            ),
            MediaAvailability::Unavailable => centered_message(
                frame,
                area,
                &state.style,
                "Media source unavailable",
                "Could not connect to the media session.",
                false,
            ),
            MediaAvailability::Available if state.player.is_none() => centered_message(
                frame,
                area,
                &state.style,
                "No active player",
                "Start playback in a supported media player.",
                false,
            ),
            MediaAvailability::Available => centered_message(
                frame,
                area,
                &state.style,
                "No track playing",
                "Play something to load lyrics.",
                false,
            ),
        },
        LyricsView::Loading => {
            centered_message(
                frame,
                area,
                &state.style,
                "Loading lyrics",
                "Searching local files and online providers.",
                false,
            );
        }
        LyricsView::NotFound => {
            centered_message(
                frame,
                area,
                &state.style,
                "No lyrics found",
                "Add a local .lrc file or try another track.",
                false,
            );
        }
        LyricsView::Error(message) => {
            centered_message(
                frame,
                area,
                &state.style,
                "Could not load lyrics",
                message,
                true,
            );
        }
        LyricsView::Loaded { track, .. } if track.is_instrumental => {
            let marker = state.style.instrumental_marker.to_string();
            centered_message(
                frame,
                area,
                &state.style,
                &marker,
                "No lyrics for this track.",
                false,
            );
        }
        LyricsView::Loaded { track, timeline }
            if track.granularity == crate::core::model::SyncGranularity::Unsynced =>
        {
            let mut ctx = RenderContext {
                state,
                timeline,
                track,
                effective_ms,
                cache: render_cache,
            };
            render_unsynced(frame, area, &mut ctx);
        }
        LyricsView::Loaded { track, timeline } => {
            let mut ctx = RenderContext {
                state,
                timeline,
                track,
                effective_ms,
                cache: render_cache,
            };
            match state.effective_display_mode() {
                DisplayMode::Normal => NormalRenderer.render(frame, area, &mut ctx),
                DisplayMode::Block => BlockRenderer.render(frame, area, &mut ctx),
                DisplayMode::Kinetic => KineticRenderer.render(frame, area, &mut ctx),
                DisplayMode::Cylinder => CylinderRenderer.render(frame, area, &mut ctx),
                DisplayMode::Canvas3D => Canvas3DRenderer.render(frame, area, &mut ctx),
            }
        }
    }
}

pub(crate) fn centered_message(
    frame: &mut Frame,
    area: Rect,
    style: &crate::style::ResolvedStyle,
    title: &str,
    detail: &str,
    error: bool,
) {
    let title_fg = if error { style.accent } else { style.active_fg };
    let mut title_style = Style::default().add_modifier(Modifier::BOLD);
    if let Some(color) = title_fg {
        title_style = title_style.fg(color);
    }
    let mut detail_style = Style::default();
    if let Some(color) = style.inactive_fg {
        detail_style = detail_style.fg(color);
    } else {
        detail_style = detail_style.add_modifier(Modifier::DIM);
    }
    let mut lines = vec![Line::from(Span::styled(title.to_string(), title_style))];
    if !detail.is_empty() {
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(detail.to_string(), detail_style)));
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

pub(crate) fn has_waiting_break_slot(lines: &[CompiledLine], waiting: Option<WaitingGap>) -> bool {
    waiting.is_some_and(|waiting| {
        lines
            .iter()
            .any(|line| compiled_is_waiting_break(line, waiting))
    })
}

pub(crate) fn compiled_is_waiting_break(line: &CompiledLine, waiting: WaitingGap) -> bool {
    line.role == LineRole::InstrumentalBreak
        && line.start_ms >= waiting.start_ms
        && line.end_ms <= waiting.end_ms
}

pub(crate) fn line_source_text<'a>(
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

pub(crate) fn waiting_countdown_lines(
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

pub(crate) fn render_lines(
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
    use crate::compiler::timeline::WaitKind;
    use crate::core::clock::PlaybackStatus;
    use crate::core::model::{Annotation, LyricLine, SyncGranularity, SyncedTrack, WordToken};
    use crate::style::{
        AnnotationPosition, BUILTIN_PRESETS, BigTextMaxSize, HighlightMode, InstrumentalStyle,
        StyleProfile, color::ColorCapability,
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
        let mut render_cache = RenderCache::default();

        // Render clean reference without effects to compare header and status rows.
        terminal
            .draw(|frame| render_with_cache(frame, &state, &mut render_cache, 2_500))
            .unwrap();
        let ref_buf = terminal.backend().buffer().clone();

        // Render with active line-change effects using Color::Reset.
        let mut effects = UiEffects::new();
        effects.observe_state(&state, 2_500);
        assert!(effects.is_running());

        terminal
            .draw(|frame| {
                let stage_area = render_with_cache_area(frame, &state, &mut render_cache, 2_500);
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
                render_with_cache_and_effects(
                    frame,
                    &state,
                    &mut render_cache,
                    &mut effects,
                    2_600,
                );
            })
            .unwrap();
        assert!(effects.is_running(), "modal must freeze active effects");

        // Close modal and complete effect duration.
        state.toggle_help();
        terminal
            .draw(|frame| {
                let stage_area = render_with_cache_area(frame, &state, &mut render_cache, 2_600);
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

    #[test]
    fn render_cache_clear_resets_stage_caches() {
        let mut cache = RenderCache::default();
        cache.block.layout_for(0, "abc", 20, 4, 1);
        assert!(cache.block.cached_line_count() > 0);
        cache.clear();
        assert_eq!(cache.block.cached_line_count(), 0);
    }
}

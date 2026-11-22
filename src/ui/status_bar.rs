use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};

use crate::core::model::SyncGranularity;
use crate::core::text::{display_width, truncate_to_width};

use super::{BannerSeverity, LyricsView, UiState};

pub fn render(frame: &mut Frame, area: Rect, state: &UiState, effective_ms: u64) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    frame.render_widget(
        Paragraph::new(status_bar_line(state, area.width, effective_ms)),
        area,
    );
}

pub fn banner_line(state: &UiState) -> Option<Line<'static>> {
    let (message, severity) = state.banner()?;
    let style = &state.style;
    let accent = style.accent;
    let line = match severity {
        BannerSeverity::Info => base_style(style),
        BannerSeverity::Warning => Style::default()
            .fg(accent.unwrap_or(Color::Yellow))
            .add_modifier(Modifier::BOLD),
        BannerSeverity::Error => {
            let prefix = if state.terminal_unicode { "✗ " } else { "! " };
            return Some(Line::from(vec![
                Span::styled(
                    prefix.to_string(),
                    Style::default()
                        .fg(accent.unwrap_or(Color::Red))
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    message.to_string(),
                    Style::default()
                        .fg(accent.unwrap_or(Color::Red))
                        .add_modifier(Modifier::BOLD),
                ),
            ]));
        }
    };
    Some(Line::from(Span::styled(message.to_string(), line)))
}

fn base_style(style: &crate::style::ResolvedStyle) -> Style {
    match style.inactive_fg {
        Some(color) => Style::default().fg(color),
        None => Style::default().add_modifier(Modifier::DIM),
    }
}

fn accent_style(style: &crate::style::ResolvedStyle, base: Style) -> Style {
    style
        .accent
        .map(|color| Style::default().fg(color))
        .unwrap_or(base)
}

pub fn status_bar_line(state: &UiState, width: u16, effective_ms: u64) -> Line<'static> {
    let separator = if state.terminal_unicode {
        " · "
    } else {
        " | "
    };
    let with_player = width >= 96;
    let with_style = width >= 96;
    let with_detail = width >= 96;
    let with_system = width >= 96;

    let base = base_style(&state.style);
    let accent = accent_style(&state.style, base);

    let mut head: Vec<(String, Style)> = Vec::new();
    head.push((playback_label(state, with_player), accent));
    if width >= 80
        && state.lyrics_track().is_some()
        && let Some(time) = time_label(state, effective_ms)
    {
        head.push((time, base));
    }
    if width >= 80 {
        head.push((mode_label(state, with_style), base));
    }
    head.push((source_label(state, with_detail), base));
    if with_system {
        for marker in lyric_markers(state) {
            head.push((marker, base));
        }
        head.push((system_label(state), base));
        if let Some(stage) = stage_label(state) {
            head.push((stage, base));
        }
    }
    if width >= 80 {
        head.push(("? help".to_string(), base));
    }

    let offset = format!("{:+} ms", state.effective_offset_ms());
    let spans = compose_spans(head, &offset, base, width, separator);
    Line::from(spans)
}

fn time_label(state: &UiState, effective_ms: u64) -> Option<String> {
    let elapsed = format_ms(effective_ms);
    Some(
        match state.track.as_ref().and_then(|track| track.duration_ms) {
            Some(duration_ms) => format!("{elapsed}/{}", format_ms(duration_ms)),
            None => elapsed,
        },
    )
}

fn format_ms(ms: u64) -> String {
    let total_seconds = ms / 1_000;
    format!("{:02}:{:02}", total_seconds / 60, total_seconds % 60)
}

fn lyric_markers(state: &UiState) -> Vec<String> {
    let (has_background, has_translation) = state.lyric_markers;
    let mut markers = Vec::new();
    if has_background {
        markers.push("bg".to_string());
    }
    if has_translation {
        markers.push("tr".to_string());
    }
    markers
}

fn stage_label(state: &UiState) -> Option<String> {
    match state.effective_display_mode() {
        crate::ui::DisplayMode::Cylinder => Some(format!(
            "cyl {}% {:.1}s",
            state.cylinder_speed_pct,
            state.cylinder_entry_lead_ms as f64 / 1_000.0
        )),
        crate::ui::DisplayMode::Canvas3D => {
            let degree = if state.terminal_unicode { "°" } else { "deg" };
            Some(format!(
                "c3d {}% {}{} {}% {}%{}",
                state.canvas3d_speed_pct,
                state.canvas3d_tilt_deg,
                degree,
                state.canvas3d_spawn_y_pct,
                state.canvas3d_zoom_pct,
                if state.canvas3d_flip { " f" } else { "" },
            ))
        }
        _ => None,
    }
}

fn compose_spans(
    mut head: Vec<(String, Style)>,
    offset: &str,
    base: Style,
    width: u16,
    separator: &str,
) -> Vec<Span<'static>> {
    let separator_width = display_width(separator);
    let budget = width.saturating_sub(display_width(offset).saturating_add(separator_width));
    while !head.is_empty() && joined_width(&head, separator_width) > usize::from(budget) {
        head.pop();
    }

    let mut spans = Vec::new();
    for (index, (text, style)) in head.iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled(separator.to_string(), base));
        }
        spans.push(Span::styled(text.clone(), base.patch(*style)));
    }
    if !head.is_empty() {
        spans.push(Span::styled(separator.to_string(), base));
    }
    spans.push(Span::styled(offset.to_string(), base));

    let joined = spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect::<String>();
    if display_width(&joined) > width {
        let truncated = truncate_to_width(&joined, width).to_string();
        return vec![Span::styled(truncated, base)];
    }
    spans
}

fn joined_width(head: &[(String, Style)], separator_width: u16) -> usize {
    let text_width = head
        .iter()
        .map(|(text, _)| usize::from(display_width(text)))
        .sum::<usize>();
    let separators = head.len().saturating_sub(1);
    text_width + separators * usize::from(separator_width)
}

fn playback_label(state: &UiState, with_player: bool) -> String {
    let playback = state.playback.as_str();
    if with_player {
        match state.player.as_deref() {
            Some(player) => {
                if state.backend.is_supported() && state.backend != crate::media::BackendKind::Mpris
                {
                    format!("{playback} {player} [{}]", state.backend.as_str())
                } else {
                    format!("{playback} {player}")
                }
            }
            None => format!("{playback} no player"),
        }
    } else {
        playback.to_string()
    }
}

fn mode_label(state: &UiState, with_style: bool) -> String {
    let mode = state.effective_display_mode().as_str();
    if with_style {
        let style = state
            .style
            .name
            .strip_prefix("builtin:")
            .unwrap_or(&state.style.name);
        format!("{mode}/{style}")
    } else {
        mode.to_string()
    }
}

fn source_label(state: &UiState, detailed: bool) -> String {
    if state.refetching {
        return "refetching".to_string();
    }
    match &state.lyrics {
        LyricsView::Idle => "-".to_string(),
        LyricsView::Loading => "loading".to_string(),
        LyricsView::NotFound => "not found".to_string(),
        LyricsView::Error(_) => "error".to_string(),
        LyricsView::Loaded { track, .. } => {
            if detailed {
                let label = state
                    .track_status_label()
                    .unwrap_or_else(|| track.status_label());
                let confidence = state
                    .confidence()
                    .map(|confidence| format!("{:.0}%", confidence * 100.0))
                    .unwrap_or_else(|| "-".to_string());
                format!("{}/{label} {confidence}", track.source_provider)
            } else {
                track.source_provider.clone()
            }
        }
    }
}

fn system_label(state: &UiState) -> String {
    let mut parts = vec![format!(
        "word-hl {}",
        if word_highlight_active(state) {
            "on"
        } else {
            "off"
        }
    )];
    if state.offline {
        parts.push("offline".to_string());
    }
    if let Some(warning) = &state.config_warning {
        parts.push(warning.clone());
    }
    parts.join(" ")
}

fn word_highlight_active(state: &UiState) -> bool {
    state.word_highlight
        && state.lyrics_track().is_some_and(|track| {
            !track.is_instrumental
                && (matches!(
                    track.granularity,
                    SyncGranularity::Word | SyncGranularity::Syllable
                ) || (track.granularity == SyncGranularity::Line
                    && state
                        .timeline()
                        .is_some_and(|timeline| timeline.has_synthetic_words)))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::clock::PlaybackStatus;
    use crate::core::model::{LineRole, LyricLine, SyncedTrack};
    use crate::media::TrackMeta;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn sample_track(granularity: SyncGranularity, is_instrumental: bool) -> SyncedTrack {
        SyncedTrack {
            source_provider: "lrclib".to_string(),
            source_track_id: None,
            parser_version: 1,
            is_instrumental,
            granularity,
            intrinsic_offset_ms: 0,
            user_offset_ms: 0,
            match_confidence: 0.9,
            lines: vec![LyricLine {
                start_ms: 0,
                end_ms: 1_000,
                role: LineRole::Primary,
                text: "line".to_string(),
                words: Vec::new(),
                annotations: Vec::new(),
            }],
        }
    }

    #[test]
    fn status_bar_adapts_to_width() {
        let state = UiState::new();
        assert_eq!(
            status_bar_line(&state, 120, 0).to_string(),
            "stopped no player · normal/default · - · word-hl off · ? help · -300 ms"
        );
        assert_eq!(
            status_bar_line(&state, 80, 0).to_string(),
            "stopped · normal · - · ? help · -300 ms"
        );
        assert_eq!(
            status_bar_line(&state, 40, 0).to_string(),
            "stopped · - · -300 ms"
        );
    }

    #[test]
    fn status_bar_merges_playback_mode_source_and_system() {
        let mut state = UiState::new();
        state.set_player(Some("mpv".to_string()));
        state.playback = PlaybackStatus::Playing;
        state.set_lyrics(sample_track(SyncGranularity::Word, false));

        assert_eq!(
            status_bar_line(&state, 120, 0).to_string(),
            "playing mpv · 00:00 · normal/default · lrclib/word 90% · word-hl on · ? help · -300 ms"
        );
        assert_eq!(
            status_bar_line(&state, 80, 0).to_string(),
            "playing · 00:00 · normal · lrclib · ? help · -300 ms"
        );
        assert_eq!(
            status_bar_line(&state, 40, 0).to_string(),
            "playing · lrclib · -300 ms"
        );
    }

    #[test]
    fn status_bar_drops_less_important_parts_before_offset() {
        let mut state = UiState::new();
        state.set_player(Some("a very long player name".to_string()));
        state.set_lyrics(sample_track(SyncGranularity::Line, false));

        for width in [24, 32, 48, 79, 95] {
            let line = status_bar_line(&state, width, 0).to_string();
            assert!(display_width(&line) <= width, "{width}: {line}");
            assert!(line.ends_with("-300 ms"), "{width}: {line}");
        }
    }

    #[test]
    fn status_bar_uses_ascii_separator_without_unicode() {
        let mut state = UiState::new();
        state.terminal_unicode = false;
        assert_eq!(
            status_bar_line(&state, 80, 0).to_string(),
            "stopped | normal | - | ? help | -300 ms"
        );
    }

    #[test]
    fn status_bar_surfaces_offline_and_warnings_at_wide_width() {
        let mut state = UiState::new();
        state.player = Some("mpv".to_string());
        state.offline = true;
        state.config_warning = Some("invalid config: unknown key".to_string());
        state.set_not_found();

        let wide = status_bar_line(&state, 120, 0).to_string();
        assert!(wide.contains("not found"), "{wide}");
        assert!(wide.contains("offline"), "{wide}");
        assert!(wide.contains("invalid config: unknown key"), "{wide}");
        assert!(wide.ends_with("-300 ms"), "{wide}");
    }

    #[test]
    fn banner_is_independent_from_status_bar() {
        let mut state = UiState::new();
        state.clear_banner();
        assert!(banner_line(&state).is_none());

        state.show_banner("Offline mode", std::time::Instant::now());
        assert_eq!(banner_line(&state).unwrap().to_string(), "Offline mode");
        assert!(state.status_bar_visible());
    }

    #[test]
    fn render_handles_zero_area_without_panic() {
        let state = UiState::new();
        let backend = TestBackend::new(80, 1);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                render(frame, Rect::new(0, 0, 0, 0), &state, 0);
            })
            .unwrap();
    }

    #[test]
    fn playback_label_includes_backend_when_non_mpris() {
        let mut state = UiState::new();
        state.set_player(Some("mpd".to_string()));
        state.playback = PlaybackStatus::Playing;
        state.backend = crate::media::BackendKind::Mpd;
        assert_eq!(playback_label(&state, true), "playing mpd [mpd]");

        state.backend = crate::media::BackendKind::Mpris;
        assert_eq!(playback_label(&state, true), "playing mpd");
    }

    #[test]
    fn status_bar_shows_elapsed_and_duration_when_known() {
        let mut state = UiState::new();
        state.set_lyrics(sample_track(SyncGranularity::Word, false));
        state.track = Some(TrackMeta {
            title: "Yellow".to_string(),
            artist: "Coldplay".to_string(),
            album: None,
            duration_ms: Some(200_000),
            track_id: None,
            url: None,
        });

        let line = status_bar_line(&state, 120, 83_000).to_string();
        assert!(line.contains("01:23/03:20"), "{line}");
    }

    #[test]
    fn status_bar_shows_lyric_markers() {
        let mut track = sample_track(SyncGranularity::Word, false);
        track.lines[0].role = LineRole::BackgroundVocal;
        let mut state = UiState::new();
        state.set_lyrics(track);
        state.track = Some(TrackMeta {
            title: "Yellow".to_string(),
            artist: "Coldplay".to_string(),
            album: None,
            duration_ms: None,
            track_id: None,
            url: None,
        });

        let line = status_bar_line(&state, 120, 0).to_string();
        assert!(line.contains("· bg ·"), "{line}");
        assert!(!line.contains("tr "), "{line}");
    }

    #[test]
    fn status_bar_shows_stage_parameters_for_stage_modes() {
        let mut state = UiState::new();
        state.display_mode = crate::ui::DisplayMode::Cylinder;
        let cylinder = status_bar_line(&state, 120, 0).to_string();
        assert!(cylinder.contains("cyl 100% 1.0s"), "{cylinder}");

        state.display_mode = crate::ui::DisplayMode::Canvas3D;
        let canvas3d = status_bar_line(&state, 120, 0).to_string();
        assert!(canvas3d.contains("c3d 100% 60° 82% 100%"), "{canvas3d}");

        state.display_mode = crate::ui::DisplayMode::Normal;
        let normal = status_bar_line(&state, 120, 0).to_string();
        assert!(!normal.contains("cyl "), "{normal}");
        assert!(!normal.contains("c3d "), "{normal}");
    }
}

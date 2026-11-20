use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};

use crate::core::model::SyncGranularity;
use crate::core::text::{display_width, truncate_to_width};

use super::{LyricsView, UiState};

pub fn render(frame: &mut Frame, area: Rect, state: &UiState) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    frame.render_widget(Paragraph::new(status_bar_line(state, area.width)), area);
}

pub fn banner_line(state: &UiState) -> Option<Line<'static>> {
    state.banner_message().map(|message| {
        Line::from(Span::styled(
            message.to_string(),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ))
    })
}

pub fn status_bar_line(state: &UiState, width: u16) -> Line<'static> {
    let separator = if state.terminal_unicode {
        " · "
    } else {
        " | "
    };
    let with_player = width >= 96;
    let with_style = width >= 96;
    let with_detail = width >= 96;
    let with_system = width >= 96;

    let mut head = Vec::new();
    head.push(playback_label(state, with_player));
    if width >= 80 {
        head.push(mode_label(state, with_style));
    }
    head.push(source_label(state, with_detail));
    if with_system {
        head.push(system_label(state));
    }

    let offset = format!("{:+} ms", state.effective_offset_ms());
    let text = compose_line(head, &offset, width, separator);
    Line::from(text).style(Style::default().add_modifier(Modifier::DIM))
}

fn compose_line(mut head: Vec<String>, offset: &str, width: u16, separator: &str) -> String {
    let separator_width = display_width(separator);
    let budget = width.saturating_sub(display_width(offset).saturating_add(separator_width));
    while !head.is_empty() && display_width(&head.join(separator)) > budget {
        head.pop();
    }

    let mut text = head.join(separator);
    if !head.is_empty() {
        text.push_str(separator);
    }
    text.push_str(offset);
    truncate_to_width(&text, width).to_string()
}

fn playback_label(state: &UiState, with_player: bool) -> String {
    let playback = state.playback.as_str();
    if with_player {
        match state.player.as_deref() {
            Some(player) => format!("{playback} {player}"),
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
            status_bar_line(&state, 120).to_string(),
            "stopped no player · normal/default · - · word-hl off · -300 ms"
        );
        assert_eq!(
            status_bar_line(&state, 80).to_string(),
            "stopped · normal · - · -300 ms"
        );
        assert_eq!(
            status_bar_line(&state, 40).to_string(),
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
            status_bar_line(&state, 120).to_string(),
            "playing mpv · normal/default · lrclib/word 90% · word-hl on · -300 ms"
        );
        assert_eq!(
            status_bar_line(&state, 80).to_string(),
            "playing · normal · lrclib · -300 ms"
        );
        assert_eq!(
            status_bar_line(&state, 40).to_string(),
            "playing · lrclib · -300 ms"
        );
    }

    #[test]
    fn status_bar_drops_less_important_parts_before_offset() {
        let mut state = UiState::new();
        state.set_player(Some("a very long player name".to_string()));
        state.set_lyrics(sample_track(SyncGranularity::Line, false));

        for width in [24, 32, 48, 79, 95] {
            let line = status_bar_line(&state, width).to_string();
            assert!(display_width(&line) <= width, "{width}: {line}");
            assert!(line.ends_with("-300 ms"), "{width}: {line}");
        }
    }

    #[test]
    fn status_bar_uses_ascii_separator_without_unicode() {
        let mut state = UiState::new();
        state.terminal_unicode = false;
        assert_eq!(
            status_bar_line(&state, 80).to_string(),
            "stopped | normal | - | -300 ms"
        );
    }

    #[test]
    fn status_bar_surfaces_offline_and_warnings_at_wide_width() {
        let mut state = UiState::new();
        state.player = Some("mpv".to_string());
        state.offline = true;
        state.config_warning = Some("invalid config: unknown key".to_string());
        state.set_not_found();

        let wide = status_bar_line(&state, 120).to_string();
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
                render(frame, Rect::new(0, 0, 0, 0), &state);
            })
            .unwrap();
    }
}

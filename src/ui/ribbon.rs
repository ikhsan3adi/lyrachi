use ratatui::{
    Frame,
    layout::{Alignment, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};

use crate::core::model::SyncGranularity;

use super::{LyricsView, UiState};

pub fn render(frame: &mut Frame, area: Rect, state: &UiState) {
    if area.height == 0 || area.width == 0 {
        return;
    }

    frame.render_widget(
        Paragraph::new(ribbon_line(state)).alignment(Alignment::Center),
        area,
    );
}

pub fn ribbon_line(state: &UiState) -> Line<'static> {
    let display_mode = state.effective_display_mode().as_str();
    let style_name = state
        .style
        .name
        .strip_prefix("builtin:")
        .unwrap_or(&state.style.name)
        .to_string();
    let source = source_label(state);
    let word_hl = if word_highlight_active(state) {
        "word-hl: on"
    } else {
        "word-hl: off"
    };
    let offset = format!("{:+} ms", state.effective_offset_ms());

    let sep = || Span::styled(" | ", Style::default().add_modifier(Modifier::DIM));
    let mut mode_style = Style::default().add_modifier(Modifier::BOLD);
    if let Some(accent) = state.style.accent {
        mode_style = mode_style.fg(accent);
    }

    Line::from(vec![
        Span::styled(display_mode, mode_style),
        sep(),
        Span::styled(style_name, Style::default().add_modifier(Modifier::DIM)),
        sep(),
        Span::raw(source),
        sep(),
        Span::styled(word_hl, Style::default().add_modifier(Modifier::DIM)),
        sep(),
        Span::styled(offset, Style::default().add_modifier(Modifier::BOLD)),
    ])
}

fn source_label(state: &UiState) -> String {
    if state.refetching {
        return "refetching...".to_string();
    }
    match &state.lyrics {
        LyricsView::Idle => "-".to_string(),
        LyricsView::Loading => "loading".to_string(),
        LyricsView::NotFound => "not found".to_string(),
        LyricsView::Error(_) => "error".to_string(),
        LyricsView::Loaded { track, .. } => {
            format!("{} ({})", track.source_provider, track.status_label())
        }
    }
}

fn word_highlight_active(state: &UiState) -> bool {
    state.word_highlight
        && state.lyrics_track().is_some_and(|track| {
            !track.is_instrumental
                && matches!(
                    track.granularity,
                    SyncGranularity::Word | SyncGranularity::Syllable
                )
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::model::{LineRole, LyricLine, SyncedTrack};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn sample_track(granularity: SyncGranularity) -> SyncedTrack {
        SyncedTrack {
            source_provider: "lrclib".to_string(),
            source_track_id: None,
            parser_version: 1,
            is_instrumental: false,
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
    fn formats_compact_ribbon_line() {
        let mut state = UiState::new();
        assert_eq!(
            ribbon_line(&state).to_string(),
            "normal | default | - | word-hl: off | +0 ms"
        );

        state.global_offset_ms = -100;
        state.user_offset_ms = 250;
        state.set_lyrics(sample_track(SyncGranularity::Syllable));
        assert_eq!(
            ribbon_line(&state).to_string(),
            "normal | default | lrclib (syllable) | word-hl: on | +150 ms"
        );

        let mut instrumental = sample_track(SyncGranularity::Line);
        instrumental.is_instrumental = true;
        state.set_lyrics(instrumental);
        assert_eq!(
            ribbon_line(&state).to_string(),
            "normal | default | lrclib (instrumental) | word-hl: off | +150 ms"
        );

        state.refetching = true;
        assert_eq!(
            ribbon_line(&state).to_string(),
            "normal | default | refetching... | word-hl: off | +150 ms"
        );
        state.refetching = false;

        state.set_not_found();
        assert_eq!(
            ribbon_line(&state).to_string(),
            "normal | default | not found | word-hl: off | +150 ms"
        );

        state.set_error("API timeout".to_string());
        assert_eq!(
            ribbon_line(&state).to_string(),
            "normal | default | error | word-hl: off | +150 ms"
        );

        state.set_lyrics(sample_track(SyncGranularity::Word));
        state.toggle_display_mode();
        state.toggle_word_highlight();
        assert_eq!(
            ribbon_line(&state).to_string(),
            "block | default | lrclib (word) | word-hl: off | +150 ms"
        );
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

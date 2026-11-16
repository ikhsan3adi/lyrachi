use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

use crate::core::{clock::PlaybackStatus, model::SyncGranularity};

use super::{LyricsView, UiState};

pub fn footer_line(state: &UiState) -> Option<Line<'static>> {
    if state.show_status {
        return Some(status_line(state));
    }

    state.banner_message().map(|message| {
        Line::from(Span::styled(
            message.to_string(),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ))
    })
}

pub fn status_line(state: &UiState) -> Line<'static> {
    let player = state.player.as_deref().unwrap_or("no player");
    let playback = match state.playback {
        PlaybackStatus::Stopped => "stopped",
        PlaybackStatus::Paused => "paused",
        PlaybackStatus::Playing => "playing",
    };
    let source = if state.refetching {
        "refetching..."
    } else {
        match &state.lyrics {
            LyricsView::Idle => "-",
            LyricsView::Loading => "loading",
            LyricsView::NotFound => "not found",
            LyricsView::Error(_) => "error",
            LyricsView::Loaded { track, .. } => track.source_provider.as_str(),
        }
    };
    let confidence = state
        .confidence()
        .map(|confidence| format!("{:.0}%", confidence * 100.0))
        .unwrap_or_else(|| "-".to_string());
    let offset = state.effective_offset_ms();

    let mut text = format!("{player} | {playback} | {source} | {offset:+} ms | {confidence}");
    if let Some(extra) = extra_status(state) {
        text.push_str(" | ");
        text.push_str(&extra);
    }
    if state.offline {
        text.push_str(" | offline");
    }
    if let Some(warning) = &state.config_warning {
        text.push_str(" | ");
        text.push_str(warning);
    }

    Line::from(text).style(Style::default().add_modifier(Modifier::DIM))
}

fn extra_status(state: &UiState) -> Option<String> {
    let track = state.lyrics_track()?;
    if track.is_instrumental {
        return Some("instrumental".to_string());
    }
    match track.granularity {
        SyncGranularity::Syllable => Some("syllable".to_string()),
        SyncGranularity::Word => Some("word".to_string()),
        SyncGranularity::Line => Some("line".to_string()),
        SyncGranularity::Unsynced => {
            if track.lines.is_empty() {
                Some("unsynced".to_string())
            } else {
                Some(format!(
                    "unsynced {}/{}",
                    state.scroll + 1,
                    track.lines.len()
                ))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::model::{LineRole, LyricLine, SyncedTrack};

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
    fn formats_status_granularity_and_empty_states() {
        let mut state = UiState::new();
        assert_eq!(
            status_line(&state).to_string(),
            "no player | stopped | - | +0 ms | -"
        );

        state.set_player(Some("mpv".to_string()));
        state.playback = PlaybackStatus::Playing;
        state.set_lyrics(sample_track(SyncGranularity::Syllable, false));
        assert_eq!(
            status_line(&state).to_string(),
            "mpv | playing | lrclib | +0 ms | 90% | syllable"
        );

        state.set_lyrics(sample_track(SyncGranularity::Word, false));
        assert_eq!(
            status_line(&state).to_string(),
            "mpv | playing | lrclib | +0 ms | 90% | word"
        );

        state.set_lyrics(sample_track(SyncGranularity::Line, false));
        assert_eq!(
            status_line(&state).to_string(),
            "mpv | playing | lrclib | +0 ms | 90% | line"
        );

        state.set_lyrics(sample_track(SyncGranularity::Line, true));
        assert_eq!(
            status_line(&state).to_string(),
            "mpv | playing | lrclib | +0 ms | 90% | instrumental"
        );

        state.refetching = true;
        assert_eq!(
            status_line(&state).to_string(),
            "mpv | playing | refetching... | +0 ms | 90% | instrumental"
        );
    }

    #[test]
    fn footer_uses_banner_when_status_is_hidden() {
        let mut state = UiState::new();
        state.toggle_status();
        assert!(footer_line(&state).is_none());

        state.show_banner("Offline mode", std::time::Instant::now());
        assert_eq!(footer_line(&state).unwrap().to_string(), "Offline mode");
    }

    #[test]
    fn shows_offline_mode_and_config_warnings() {
        let mut state = UiState::new();
        state.player = Some("mpv".to_string());
        state.offline = true;
        state.config_warning = Some("invalid config: unknown key".to_string());
        state.set_not_found();

        assert_eq!(
            status_line(&state).to_string(),
            "mpv | stopped | not found | +0 ms | - | offline | invalid config: unknown key"
        );
    }
}

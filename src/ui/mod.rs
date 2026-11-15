pub mod render;
pub mod status;

use crate::compiler::timeline::CompiledTimeline;
use crate::core::clock::PlaybackStatus;
use crate::core::model::{SyncGranularity, SyncedTrack};
use crate::media::TrackMeta;
use crate::offsets::{MAX_OFFSET_MS, MIN_OFFSET_MS};

const HEADER_ROWS: u16 = 1;
const STATUS_ROWS: u16 = 1;

#[derive(Debug, Clone, PartialEq)]
pub enum LyricsView {
    Idle,
    Loading,
    NotFound,
    Error(String),
    Loaded {
        track: SyncedTrack,
        timeline: CompiledTimeline,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct UiState {
    pub player: Option<String>,
    pub playback: PlaybackStatus,
    pub track: Option<TrackMeta>,
    pub lyrics: LyricsView,
    pub global_offset_ms: i64,
    pub user_offset_ms: i64,
    pub lookahead_ms: u64,
    pub offline: bool,
    pub config_warning: Option<String>,
    pub scroll: usize,
    pub viewport_width: u16,
    pub viewport_height: u16,
}

impl Default for UiState {
    fn default() -> Self {
        Self::new()
    }
}

impl UiState {
    pub fn new() -> Self {
        Self {
            player: None,
            playback: PlaybackStatus::Stopped,
            track: None,
            lyrics: LyricsView::Idle,
            global_offset_ms: 0,
            user_offset_ms: 0,
            lookahead_ms: 0,
            offline: false,
            config_warning: None,
            scroll: 0,
            viewport_width: 80,
            viewport_height: 24,
        }
    }

    pub fn resize(&mut self, width: u16, height: u16) {
        if width != self.viewport_width {
            self.viewport_width = width;
            self.recompile();
        }
        self.viewport_height = height;
        self.clamp_scroll();
    }

    pub fn set_no_player(&mut self) {
        self.player = None;
        self.playback = PlaybackStatus::Stopped;
        self.track = None;
        self.lyrics = LyricsView::Idle;
        self.user_offset_ms = 0;
        self.scroll = 0;
    }

    pub fn set_player(&mut self, player: Option<String>) {
        self.player = player;
    }

    pub fn set_track(&mut self, track: TrackMeta) {
        self.track = Some(track);
        self.lyrics = LyricsView::Loading;
        self.user_offset_ms = 0;
        self.scroll = 0;
    }

    pub fn set_no_track(&mut self) {
        self.track = None;
        self.lyrics = LyricsView::Idle;
        self.user_offset_ms = 0;
        self.scroll = 0;
    }

    pub fn set_lyrics(&mut self, track: SyncedTrack) {
        if self.user_offset_ms == 0 && track.user_offset_ms != 0 {
            self.user_offset_ms = track.user_offset_ms;
        }
        let timeline = CompiledTimeline::compile(&track, self.viewport_width);
        self.lyrics = LyricsView::Loaded { track, timeline };
        self.scroll = 0;
    }

    pub fn set_not_found(&mut self) {
        self.lyrics = LyricsView::NotFound;
    }

    pub fn set_error(&mut self, message: String) {
        self.lyrics = LyricsView::Error(message);
    }

    pub fn adjust_offset(&mut self, delta_ms: i64) {
        self.user_offset_ms = self
            .user_offset_ms
            .saturating_add(delta_ms)
            .clamp(MIN_OFFSET_MS, MAX_OFFSET_MS);
    }

    pub fn reset_offset(&mut self) {
        self.user_offset_ms = 0;
    }

    pub fn effective_offset_ms(&self) -> i64 {
        self.global_offset_ms.saturating_add(self.user_offset_ms)
    }

    pub fn is_unsynced(&self) -> bool {
        self.lyrics_track()
            .is_some_and(|track| track.granularity == SyncGranularity::Unsynced)
    }

    pub fn is_instrumental(&self) -> bool {
        self.lyrics_track()
            .is_some_and(|track| track.is_instrumental)
    }

    pub fn lyrics_track(&self) -> Option<&SyncedTrack> {
        match &self.lyrics {
            LyricsView::Loaded { track, .. } => Some(track),
            _ => None,
        }
    }

    pub fn timeline(&self) -> Option<&CompiledTimeline> {
        match &self.lyrics {
            LyricsView::Loaded { timeline, .. } => Some(timeline),
            _ => None,
        }
    }

    pub fn source_provider(&self) -> Option<&str> {
        self.lyrics_track()
            .map(|track| track.source_provider.as_str())
    }

    pub fn confidence(&self) -> Option<f32> {
        self.lyrics_track().map(|track| track.match_confidence)
    }

    pub fn scroll_up(&mut self) {
        self.scroll = self.scroll.saturating_sub(1);
    }

    pub fn scroll_down(&mut self) {
        if !self.is_unsynced() {
            return;
        }
        let max_scroll = self.max_scroll();
        self.scroll = (self.scroll + 1).min(max_scroll);
    }

    fn body_rows(&self) -> usize {
        usize::from(
            self.viewport_height
                .saturating_sub(HEADER_ROWS + STATUS_ROWS)
                .max(1),
        )
    }

    fn clamp_scroll(&mut self) {
        if !self.is_unsynced() {
            self.scroll = 0;
            return;
        }
        self.scroll = self.scroll.min(self.max_scroll());
    }

    fn max_scroll(&self) -> usize {
        let Some(timeline) = self.timeline() else {
            return 0;
        };
        let body_rows = self.body_rows();
        let mut rows = 0usize;
        let mut start = timeline.lines.len();

        for (index, line) in timeline.lines.iter().enumerate().rev() {
            let line_rows = line.wrapped_main.len().max(1) + line.wrapped_annotations.len();
            if rows > 0 && rows + line_rows > body_rows {
                break;
            }
            rows = rows.saturating_add(line_rows);
            start = index;
            if rows >= body_rows {
                break;
            }
        }

        start
    }

    fn recompile(&mut self) {
        let LyricsView::Loaded { track, timeline } = &mut self.lyrics else {
            return;
        };
        *timeline = CompiledTimeline::compile(track, self.viewport_width);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::model::{LineRole, LyricLine};

    fn track(granularity: SyncGranularity) -> SyncedTrack {
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
                text: "a line".to_string(),
                words: Vec::new(),
                annotations: Vec::new(),
            }],
        }
    }

    #[test]
    fn recompiles_only_when_width_changes() {
        let mut state = UiState::new();
        state.set_lyrics(track(SyncGranularity::Line));
        let original = state.timeline().unwrap().clone();

        state.resize(80, 30);
        assert_eq!(state.timeline().unwrap(), &original);

        state.resize(120, 30);
        assert_eq!(state.timeline().unwrap().viewport_width, 120);
    }

    #[test]
    fn offset_keys_clamp_and_reset_user_offset() {
        let mut state = UiState::new();
        state.global_offset_ms = 50;

        state.adjust_offset(100);
        assert_eq!(state.effective_offset_ms(), 150);
        state.adjust_offset(-300);
        assert_eq!(state.effective_offset_ms(), -150);
        state.reset_offset();
        assert_eq!(state.effective_offset_ms(), 50);
    }

    #[test]
    fn unsynced_scroll_is_bounded() {
        let mut state = UiState::new();
        state.viewport_height = 5;
        let mut track = track(SyncGranularity::Unsynced);
        track.lines = (0..10)
            .map(|index| LyricLine {
                start_ms: index * 1_000,
                end_ms: (index + 1) * 1_000,
                role: LineRole::Primary,
                text: format!("line {index}"),
                words: Vec::new(),
                annotations: Vec::new(),
            })
            .collect();
        state.set_lyrics(track);

        for _ in 0..20 {
            state.scroll_down();
        }

        assert_eq!(state.scroll, 7);
    }

    #[test]
    fn user_offset_resets_across_tracks_and_player_loss() {
        let mut state = UiState::new();
        state.global_offset_ms = 50;
        state.adjust_offset(200);
        assert_eq!(state.user_offset_ms, 200);

        state.set_track(TrackMeta {
            title: "Song B".to_string(),
            artist: "Artist".to_string(),
            album: None,
            duration_ms: None,
            track_id: None,
            url: None,
        });
        assert_eq!(state.user_offset_ms, 0);
        assert_eq!(state.effective_offset_ms(), 50);

        let mut loaded = track(SyncGranularity::Line);
        loaded.user_offset_ms = -150;
        state.set_lyrics(loaded);
        assert_eq!(state.user_offset_ms, -150);

        state.set_no_track();
        assert_eq!(state.user_offset_ms, 0);

        state.adjust_offset(100);
        state.set_no_player();
        assert_eq!(state.user_offset_ms, 0);
    }
}

pub mod help;
pub mod render;
pub mod ribbon;
pub mod status;

use std::time::{Duration, Instant};

use crate::compiler::timeline::{CompiledTimeline, TimelineContext};
use crate::config::RibbonMode;
use crate::core::clock::PlaybackStatus;
use crate::core::model::{SyncGranularity, SyncedTrack};
use crate::media::TrackMeta;
use crate::offsets::{MAX_OFFSET_MS, MIN_OFFSET_MS};

const BANNER_DURATION: Duration = Duration::from_secs(3);
pub const RIBBON_AUTO_HIDE_DURATION: Duration = Duration::from_secs(3);
pub const MIN_RIBBON_TERMINAL_HEIGHT: u16 = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RibbonPlacement {
    Top,
    Bottom,
}

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiModal {
    Help,
}

#[derive(Debug, Clone, PartialEq)]
pub struct UiState {
    pub player: Option<String>,
    pub playback: PlaybackStatus,
    pub track: Option<TrackMeta>,
    pub lyrics: LyricsView,
    pub refetching: bool,
    pub global_offset_ms: i64,
    pub user_offset_ms: i64,
    pub lookahead_ms: u64,
    pub offline: bool,
    pub config_warning: Option<String>,
    pub show_header: bool,
    pub show_status: bool,
    pub ribbon_mode: RibbonMode,
    pub ribbon_visible_until: Option<Instant>,
    pub ribbon_temp_hidden: bool,
    pub terminal_unicode: bool,
    pub modal: Option<UiModal>,
    pub transient_banner: Option<(String, Instant)>,
    fullscreen_restore: Option<(bool, bool)>,
    pub scroll: usize,
    pub viewport_width: u16,
    pub viewport_height: u16,
    pub timeline_options_hash: u64,
    timeline_context: TimelineContext,
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
            refetching: false,
            global_offset_ms: 0,
            user_offset_ms: 0,
            lookahead_ms: 0,
            offline: false,
            config_warning: None,
            show_header: true,
            show_status: true,
            ribbon_mode: RibbonMode::default(),
            ribbon_visible_until: None,
            ribbon_temp_hidden: false,
            terminal_unicode: true,
            modal: None,
            transient_banner: None,
            fullscreen_restore: None,
            scroll: 0,
            viewport_width: 80,
            viewport_height: 24,
            timeline_options_hash: TimelineContext::default().options_hash(),
            timeline_context: TimelineContext::default(),
        }
    }

    pub fn toggle_help(&mut self) {
        self.modal = match self.modal {
            Some(UiModal::Help) => None,
            None => Some(UiModal::Help),
        };
    }

    pub fn open_help(&mut self) {
        self.modal = Some(UiModal::Help);
    }

    pub fn close_modal(&mut self) {
        self.modal = None;
    }

    pub fn is_modal_open(&self) -> bool {
        self.modal.is_some()
    }

    pub fn resize(&mut self, width: u16, height: u16) {
        if width != self.viewport_width {
            self.viewport_width = width;
            self.timeline_context.viewport_width = width;
            self.recompile();
        }
        self.viewport_height = height;
        self.clamp_scroll();
    }

    pub fn set_ambiguous_width(&mut self, ambiguous_width: u8) {
        let ambiguous_width = if ambiguous_width == 2 { 2 } else { 1 };
        if self.timeline_context.ambiguous_width == ambiguous_width {
            return;
        }
        self.timeline_context.ambiguous_width = ambiguous_width;
        self.recompile_if_context_changed();
    }

    pub fn set_padding_horizontal(&mut self, padding: u16) {
        if self.timeline_context.padding_horizontal == padding {
            return;
        }
        self.timeline_context.padding_horizontal = padding;
        self.recompile_if_context_changed();
    }

    pub fn set_max_width(&mut self, max_width: Option<u16>) {
        if self.timeline_context.max_width == max_width {
            return;
        }
        self.timeline_context.max_width = max_width;
        self.recompile_if_context_changed();
    }

    pub fn set_terminal_unicode(&mut self, terminal_unicode: bool) {
        self.terminal_unicode = terminal_unicode;
    }

    pub fn timeline_context(&self) -> &TimelineContext {
        &self.timeline_context
    }

    pub fn configure_layout(&mut self, fullscreen: bool, show_header: bool, show_status: bool) {
        if fullscreen {
            self.fullscreen_restore =
                (show_header || show_status).then_some((show_header, show_status));
            self.show_header = false;
            self.show_status = false;
        } else {
            self.show_header = show_header;
            self.show_status = show_status;
            self.fullscreen_restore = None;
        }
        self.clamp_scroll();
    }

    pub fn toggle_fullscreen(&mut self) {
        if self.show_header || self.show_status {
            self.fullscreen_restore = Some((self.show_header, self.show_status));
            self.show_header = false;
            self.show_status = false;
        } else {
            (self.show_header, self.show_status) =
                self.fullscreen_restore.take().unwrap_or((true, true));
        }
        self.clamp_scroll();
    }

    pub fn toggle_header(&mut self) {
        self.show_header = !self.show_header;
        self.fullscreen_restore = None;
        self.clamp_scroll();
    }

    pub fn toggle_status(&mut self) {
        self.show_status = !self.show_status;
        self.fullscreen_restore = None;
        self.clamp_scroll();
    }

    pub fn configure_ribbon(&mut self, mode: RibbonMode) {
        self.ribbon_mode = mode;
        self.ribbon_visible_until = None;
        self.ribbon_temp_hidden = false;
        self.clamp_scroll();
    }

    pub fn toggle_ribbon(&mut self, now: Instant) {
        match self.ribbon_mode {
            RibbonMode::Auto | RibbonMode::Off => {
                if self
                    .ribbon_visible_until
                    .is_some_and(|visible_until| now < visible_until)
                {
                    self.ribbon_visible_until = None;
                } else {
                    self.ribbon_visible_until = Some(now + RIBBON_AUTO_HIDE_DURATION);
                }
            }
            RibbonMode::Top | RibbonMode::Bottom => {
                self.ribbon_temp_hidden = !self.ribbon_temp_hidden;
            }
        }
        self.clamp_scroll();
    }

    pub fn expire_ribbon(&mut self, now: Instant) {
        if let Some(visible_until) = self.ribbon_visible_until
            && now >= visible_until
        {
            self.ribbon_visible_until = None;
            self.clamp_scroll();
        }
    }

    pub fn time_until_ribbon_expiry(&self, now: Instant) -> Option<Duration> {
        if !matches!(self.ribbon_mode, RibbonMode::Auto | RibbonMode::Off) {
            return None;
        }
        let visible_until = self.ribbon_visible_until?;
        Some(visible_until.saturating_duration_since(now))
    }

    pub fn ribbon_placement(&self) -> Option<RibbonPlacement> {
        self.ribbon_placement_for_height(self.viewport_height)
    }

    pub fn ribbon_placement_for_height(&self, height: u16) -> Option<RibbonPlacement> {
        if height < MIN_RIBBON_TERMINAL_HEIGHT {
            return None;
        }
        match self.ribbon_mode {
            RibbonMode::Auto | RibbonMode::Off => {
                self.ribbon_visible_until.map(|_| RibbonPlacement::Top)
            }
            RibbonMode::Top => (!self.ribbon_temp_hidden).then_some(RibbonPlacement::Top),
            RibbonMode::Bottom => (!self.ribbon_temp_hidden).then_some(RibbonPlacement::Bottom),
        }
    }

    pub fn is_fullscreen(&self) -> bool {
        !self.show_header && !self.show_status
    }

    pub fn persisted_layout(&self) -> (bool, bool, bool) {
        let fullscreen = self.is_fullscreen();
        let (show_header, show_status) = if fullscreen {
            self.fullscreen_restore.unwrap_or((true, true))
        } else {
            (self.show_header, self.show_status)
        };
        (fullscreen, show_header, show_status)
    }

    pub fn show_banner(&mut self, message: impl Into<String>, now: Instant) {
        self.transient_banner = Some((message.into(), now));
    }

    pub fn clear_banner(&mut self) {
        self.transient_banner = None;
    }

    pub fn banner_message(&self) -> Option<&str> {
        self.transient_banner
            .as_ref()
            .map(|(message, _)| message.as_str())
    }

    pub fn expire_banner(&mut self, now: Instant) {
        if let Some((_, shown_at)) = &self.transient_banner
            && now.duration_since(*shown_at) >= BANNER_DURATION
        {
            self.transient_banner = None;
        }
    }

    pub fn time_until_banner_expiry(&self, now: Instant) -> Option<Duration> {
        let (_, shown_at) = self.transient_banner.as_ref()?;
        Some(BANNER_DURATION.saturating_sub(now.duration_since(*shown_at)))
    }

    pub fn set_no_player(&mut self) {
        self.player = None;
        self.playback = PlaybackStatus::Stopped;
        self.track = None;
        self.lyrics = LyricsView::Idle;
        self.refetching = false;
        self.user_offset_ms = 0;
        self.scroll = 0;
    }

    pub fn set_player(&mut self, player: Option<String>) {
        self.player = player;
    }

    pub fn set_track(&mut self, track: TrackMeta) {
        self.track = Some(track);
        self.lyrics = LyricsView::Loading;
        self.refetching = false;
        self.user_offset_ms = 0;
        self.scroll = 0;
    }

    pub fn set_no_track(&mut self) {
        self.track = None;
        self.lyrics = LyricsView::Idle;
        self.refetching = false;
        self.user_offset_ms = 0;
        self.scroll = 0;
    }

    pub fn set_lyrics(&mut self, track: SyncedTrack) {
        if self.user_offset_ms == 0 && track.user_offset_ms != 0 {
            self.user_offset_ms = track.user_offset_ms;
        }
        self.sync_timeline_context();
        self.timeline_options_hash = self.timeline_context.options_hash();
        let timeline = CompiledTimeline::compile(&track, self.timeline_context);
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

    pub fn footer_visible(&self) -> bool {
        self.show_status || self.transient_banner.is_some()
    }

    fn body_rows(&self) -> usize {
        let header_rows = u16::from(self.show_header);
        let ribbon_rows = u16::from(self.ribbon_placement().is_some());
        let footer_rows = u16::from(self.footer_visible());
        usize::from(
            self.viewport_height
                .saturating_sub(header_rows + ribbon_rows + footer_rows)
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

    fn sync_timeline_context(&mut self) {
        self.timeline_context.viewport_width = self.viewport_width;
        self.timeline_context.track_duration_ms =
            self.track.as_ref().and_then(|track| track.duration_ms);
    }

    fn recompile(&mut self) {
        self.sync_timeline_context();
        self.timeline_options_hash = self.timeline_context.options_hash();
        let LyricsView::Loaded { track, timeline } = &mut self.lyrics else {
            return;
        };
        *timeline = CompiledTimeline::compile(track, self.timeline_context);
    }

    fn recompile_if_context_changed(&mut self) {
        self.sync_timeline_context();
        let options_hash = self.timeline_context.options_hash();
        if options_hash == self.timeline_options_hash {
            return;
        }
        self.timeline_options_hash = options_hash;
        let LyricsView::Loaded { track, timeline } = &mut self.lyrics else {
            return;
        };
        *timeline = CompiledTimeline::compile(track, self.timeline_context);
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
    fn ambiguous_width_recompiles_without_visual_state_changes() {
        let mut state = UiState::new();
        state.resize(6, 24);
        let mut track = track(SyncGranularity::Line);
        track.lines[0].text = "······".to_string();
        state.set_lyrics(track);

        let narrow_rows = state.timeline().unwrap().lines[0].wrapped_main.len();
        let timeline = state.timeline().unwrap().clone();
        let options_hash = state.timeline_options_hash;

        state.toggle_header();
        state.show_banner("visual only", Instant::now());
        state.set_terminal_unicode(false);
        assert_eq!(state.timeline(), Some(&timeline));
        assert_eq!(state.timeline_options_hash, options_hash);
        assert_eq!(narrow_rows, 3);

        state.set_ambiguous_width(2);
        assert_eq!(state.timeline().unwrap().lines[0].wrapped_main.len(), 6);
        assert_ne!(state.timeline_options_hash, options_hash);
        assert_eq!(state.timeline_context().ambiguous_width, 2);
    }

    #[test]
    fn padding_and_max_width_recompile_timeline() {
        let mut state = UiState::new();
        state.resize(40, 24);
        state.set_lyrics(track(SyncGranularity::Line));

        let initial_rows = state.timeline().unwrap().lines[0].wrapped_main.len();
        let initial_hash = state.timeline_options_hash;

        state.set_padding_horizontal(18);
        assert_ne!(state.timeline_options_hash, initial_hash);
        assert_ne!(
            state.timeline().unwrap().lines[0].wrapped_main.len(),
            initial_rows
        );
        assert_eq!(state.timeline_context().padding_horizontal, 18);

        let padded_hash = state.timeline_options_hash;
        state.set_max_width(Some(2));
        assert_ne!(state.timeline_options_hash, padded_hash);
        assert_eq!(state.timeline_context().max_width, Some(2));
    }

    #[test]
    fn layout_toggles_restore_fullscreen_state() {
        let mut state = UiState::new();
        assert_eq!(state.persisted_layout(), (false, true, true));

        state.toggle_fullscreen();
        assert!(state.is_fullscreen());
        assert_eq!(state.persisted_layout(), (true, true, true));

        state.toggle_fullscreen();
        assert_eq!((state.show_header, state.show_status), (true, true));

        state.toggle_header();
        assert_eq!((state.show_header, state.show_status), (false, true));
        state.configure_layout(true, state.show_header, state.show_status);
        assert!(state.is_fullscreen());
        assert_eq!(state.persisted_layout(), (true, false, true));

        state.toggle_fullscreen();
        assert_eq!((state.show_header, state.show_status), (false, true));
    }

    #[test]
    fn body_rows_follow_layout_and_banner() {
        let mut state = UiState::new();
        state.viewport_height = 24;
        assert_eq!(state.body_rows(), 22);

        state.toggle_header();
        assert_eq!(state.body_rows(), 23);

        state.toggle_status();
        assert_eq!(state.body_rows(), 24);

        state.show_banner("Offline mode", Instant::now());
        assert_eq!(state.body_rows(), 23);

        state.expire_banner(Instant::now() + BANNER_DURATION);
        assert_eq!(state.body_rows(), 24);
    }

    #[test]
    fn banner_expires_after_three_seconds() {
        let now = Instant::now();
        let mut state = UiState::new();
        state.show_banner("Could not save preferences", now);

        assert_eq!(
            state.time_until_banner_expiry(now + Duration::from_secs(1)),
            Some(Duration::from_secs(2))
        );
        state.expire_banner(now + Duration::from_secs(2));
        assert_eq!(state.banner_message(), Some("Could not save preferences"));
        state.expire_banner(now + BANNER_DURATION);
        assert_eq!(state.banner_message(), None);
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

    #[test]
    fn help_modal_toggles_without_recompiling_timeline() {
        let mut state = UiState::new();
        state.set_lyrics(track(SyncGranularity::Line));
        let timeline = state.timeline().unwrap().clone();
        let hash = state.timeline_options_hash;

        assert!(!state.is_modal_open());
        state.toggle_help();
        assert_eq!(state.modal, Some(UiModal::Help));
        assert!(state.is_modal_open());
        assert_eq!(state.timeline(), Some(&timeline));
        assert_eq!(state.timeline_options_hash, hash);

        state.toggle_help();
        assert_eq!(state.modal, None);
        assert!(!state.is_modal_open());

        state.open_help();
        assert_eq!(state.modal, Some(UiModal::Help));
        state.close_modal();
        assert_eq!(state.modal, None);
    }

    #[test]
    fn ribbon_state_machine_covers_all_modes_and_breakpoint() {
        let mut state = UiState::new();
        let t0 = Instant::now();

        // Auto: hidden by default, Tab toggles 3s visibility, expires at >= 3s
        state.configure_ribbon(RibbonMode::Auto);
        assert_eq!(state.ribbon_placement(), None);
        state.toggle_ribbon(t0);
        assert_eq!(state.ribbon_placement(), Some(RibbonPlacement::Top));
        assert_eq!(
            state.time_until_ribbon_expiry(t0 + Duration::from_secs(1)),
            Some(Duration::from_secs(2))
        );
        state.expire_ribbon(t0 + Duration::from_secs(2));
        assert_eq!(state.ribbon_placement(), Some(RibbonPlacement::Top));
        state.expire_ribbon(t0 + RIBBON_AUTO_HIDE_DURATION);
        assert_eq!(state.ribbon_placement(), None);

        // Auto: second Tab before expiry hides immediately
        state.toggle_ribbon(t0);
        assert_eq!(state.ribbon_placement(), Some(RibbonPlacement::Top));
        state.toggle_ribbon(t0 + Duration::from_secs(1));
        assert_eq!(state.ribbon_placement(), None);

        // Off: same 3s peek behavior on Tab
        state.configure_ribbon(RibbonMode::Off);
        assert_eq!(state.ribbon_placement(), None);
        state.toggle_ribbon(t0);
        assert_eq!(state.ribbon_placement(), Some(RibbonPlacement::Top));
        state.expire_ribbon(t0 + RIBBON_AUTO_HIDE_DURATION);
        assert_eq!(state.ribbon_placement(), None);

        // Top: permanently visible, Tab toggles temporary hide/show
        state.configure_ribbon(RibbonMode::Top);
        assert_eq!(state.ribbon_placement(), Some(RibbonPlacement::Top));
        assert_eq!(state.time_until_ribbon_expiry(t0), None);
        state.toggle_ribbon(t0);
        assert_eq!(state.ribbon_placement(), None);
        state.toggle_ribbon(t0 + Duration::from_secs(1));
        assert_eq!(state.ribbon_placement(), Some(RibbonPlacement::Top));

        // Bottom: permanently visible at bottom, Tab toggles temporary hide/show
        state.configure_ribbon(RibbonMode::Bottom);
        assert_eq!(state.ribbon_placement(), Some(RibbonPlacement::Bottom));
        state.toggle_ribbon(t0);
        assert_eq!(state.ribbon_placement(), None);
        state.toggle_ribbon(t0 + Duration::from_secs(1));
        assert_eq!(state.ribbon_placement(), Some(RibbonPlacement::Bottom));

        // Terminal height < 8 rows automatically hides ribbon in all modes
        state.resize(80, 7);
        assert_eq!(state.ribbon_placement(), None);
        state.resize(80, 8);
        assert_eq!(state.ribbon_placement(), Some(RibbonPlacement::Bottom));
    }
}

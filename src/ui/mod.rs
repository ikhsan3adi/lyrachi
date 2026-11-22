pub mod block;
pub mod canvas3d;
pub mod cylinder;
pub mod effects;
pub mod help;
pub(crate) mod kinetic;
pub mod menu;
pub(crate) mod normal;
pub mod render;
pub mod status_bar;
pub mod word_style;

pub use menu::{
    CandidateMenuItem, ContextMenuAction, ContextMenuState, ContextMenuView, SearchField,
    SearchInputState,
};

use std::time::{Duration, Instant};

use crate::compiler::timeline::{CompiledTimeline, TimelineContext};
use crate::config::{DEFAULT_GLOBAL_OFFSET_MS, StatusBarMode};
use crate::core::clock::PlaybackStatus;
use crate::core::model::{SyncGranularity, SyncedTrack};
use crate::media::{BackendKind, MediaAvailability, TrackMeta};
use crate::offsets::{MAX_OFFSET_MS, MIN_OFFSET_MS};
use crate::style::{HighlightMode, InstrumentalStyle, ResolvedStyle};

const BANNER_DURATION: Duration = Duration::from_secs(3);
pub const STATUS_BAR_PEEK_DURATION: Duration = Duration::from_secs(3);
pub const MIN_STATUS_BAR_AUTO_HEIGHT: u16 = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusBarPlacement {
    Top,
    Bottom,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum DisplayMode {
    #[default]
    Normal,
    Block,
    Kinetic,
    Cylinder,
    Canvas3D,
}

impl DisplayMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Block => "block",
            Self::Kinetic => "kinetic",
            Self::Cylinder => "cylinder",
            Self::Canvas3D => "canvas3d",
        }
    }

    pub fn from_config_name(name: &str) -> Option<Self> {
        match name {
            "normal" => Some(Self::Normal),
            "block" => Some(Self::Block),
            "kinetic" => Some(Self::Kinetic),
            "cylinder" => Some(Self::Cylinder),
            "canvas3d" => Some(Self::Canvas3D),
            _ => None,
        }
    }

    pub const fn toggled(self) -> Self {
        match self {
            Self::Normal => Self::Block,
            Self::Block => Self::Kinetic,
            Self::Kinetic => Self::Cylinder,
            Self::Cylinder => Self::Canvas3D,
            Self::Canvas3D => Self::Normal,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CylinderAnnotationMode {
    #[default]
    Auto,
    Show,
    Hide,
}

impl CylinderAnnotationMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Show => "show",
            Self::Hide => "hide",
        }
    }

    pub fn from_config_name(name: &str) -> Option<Self> {
        match name {
            "auto" => Some(Self::Auto),
            "show" => Some(Self::Show),
            "hide" => Some(Self::Hide),
            _ => None,
        }
    }
}

pub const CYLINDER_MIN_SPEED_PCT: i32 = 50;
pub const CYLINDER_MAX_SPEED_PCT: i32 = 300;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CylinderSpeedMode {
    #[default]
    Constant,
    LineLength,
}

impl CylinderSpeedMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Constant => "constant",
            Self::LineLength => "line_length",
        }
    }

    pub fn from_config_name(name: &str) -> Option<Self> {
        match name {
            "constant" => Some(Self::Constant),
            "line_length" => Some(Self::LineLength),
            _ => None,
        }
    }
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiModal {
    Help,
    ContextMenu(Box<menu::ContextMenuState>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct UiState {
    pub media_availability: MediaAvailability,
    pub backend: BackendKind,
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
    pub header_override: Option<bool>,
    pub fullscreen: bool,
    pub status_bar: StatusBarMode,
    pub status_bar_visible_until: Option<Instant>,
    pub status_bar_paused_at: Option<Instant>,
    status_bar_last_visible: StatusBarMode,
    pub terminal_unicode: bool,
    pub display_mode: DisplayMode,
    pub cylinder_annotations: CylinderAnnotationMode,
    pub cylinder_speed_mode: CylinderSpeedMode,
    pub cylinder_speed_pct: u16,
    pub cylinder_entry_lead_ms: u64,
    pub canvas3d_flip: bool,
    pub canvas3d_speed_pct: u16,
    pub canvas3d_tilt_deg: u16,
    pub canvas3d_spawn_y_pct: u16,
    pub canvas3d_zoom_pct: u16,
    pub word_highlight: bool,
    pub fps_active: u16,
    pub style: ResolvedStyle,
    pub modal: Option<UiModal>,
    pub pinned_source_label: Option<String>,
    pub pinned_style: Option<String>,
    pub transient_banner: Option<(String, Instant)>,
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
            media_availability: MediaAvailability::Available,
            backend: BackendKind::current(),
            player: None,
            playback: PlaybackStatus::Stopped,
            track: None,
            lyrics: LyricsView::Idle,
            refetching: false,
            global_offset_ms: DEFAULT_GLOBAL_OFFSET_MS,
            user_offset_ms: 0,
            lookahead_ms: 0,
            offline: false,
            config_warning: None,
            show_header: true,
            header_override: None,
            fullscreen: false,
            status_bar: StatusBarMode::default(),
            status_bar_visible_until: None,
            status_bar_paused_at: None,
            status_bar_last_visible: StatusBarMode::default(),
            terminal_unicode: true,
            display_mode: DisplayMode::Normal,
            cylinder_annotations: CylinderAnnotationMode::Auto,
            cylinder_speed_mode: CylinderSpeedMode::Constant,
            cylinder_speed_pct: 100,
            cylinder_entry_lead_ms: cylinder::CYLINDER_DEFAULT_ENTRY_LEAD_MS,
            canvas3d_flip: false,
            canvas3d_speed_pct: 100,
            canvas3d_tilt_deg: canvas3d::CANVAS3D_DEFAULT_TILT_DEG,
            canvas3d_spawn_y_pct: canvas3d::CANVAS3D_DEFAULT_SPAWN_PCT,
            canvas3d_zoom_pct: canvas3d::CANVAS3D_DEFAULT_ZOOM_PCT,
            word_highlight: true,
            fps_active: 60,
            style: ResolvedStyle::default(),
            modal: None,
            pinned_source_label: None,
            pinned_style: None,
            transient_banner: None,
            scroll: 0,
            viewport_width: 80,
            viewport_height: 24,
            timeline_options_hash: TimelineContext::default().options_hash(),
            timeline_context: TimelineContext::default(),
        }
    }

    pub fn toggle_help(&mut self) {
        self.toggle_help_at(Instant::now());
    }

    pub fn toggle_help_at(&mut self, now: Instant) {
        match self.modal {
            Some(UiModal::Help) => {
                self.modal = None;
                self.resume_status_bar_timer(now);
            }
            Some(UiModal::ContextMenu(_)) | None => {
                self.pause_status_bar_timer(now);
                self.modal = Some(UiModal::Help);
            }
        }
    }

    pub fn open_help(&mut self) {
        self.pause_status_bar_timer(Instant::now());
        self.modal = Some(UiModal::Help);
    }

    pub fn open_context_menu(&mut self) -> bool {
        self.open_context_menu_at(Instant::now())
    }

    pub fn open_context_menu_at(&mut self, now: Instant) -> bool {
        let Some(track) = &self.track else {
            return false;
        };
        let label = if track.artist.is_empty() {
            track.title.clone()
        } else {
            format!("{} - {}", track.artist, track.title)
        };
        self.pause_status_bar_timer(now);
        self.modal = Some(UiModal::ContextMenu(Box::new(ContextMenuState::actions(
            label,
            self.pinned_source_label.clone(),
            self.pinned_style.clone(),
        ))));
        true
    }

    pub fn context_menu(&self) -> Option<&ContextMenuState> {
        match &self.modal {
            Some(UiModal::ContextMenu(menu)) => Some(menu),
            _ => None,
        }
    }

    pub fn context_menu_mut(&mut self) -> Option<&mut ContextMenuState> {
        match &mut self.modal {
            Some(UiModal::ContextMenu(menu)) => Some(menu),
            _ => None,
        }
    }

    pub fn open_search_input(&mut self, artist: String, title: String) {
        let search = ContextMenuView::SearchInput(SearchInputState {
            artist,
            title,
            active_field: SearchField::Artist,
        });
        if let Some(menu) = self.context_menu_mut() {
            menu.view = search;
        } else if self.open_context_menu()
            && let Some(menu) = self.context_menu_mut()
        {
            menu.view = search;
        }
    }

    pub fn open_candidate_loading(&mut self, query_label: String) {
        let candidates = ContextMenuView::Candidates {
            query_label,
            items: Vec::new(),
            selected: 0,
            loading: true,
            hydrating: false,
            status_message: None,
        };
        if let Some(menu) = self.context_menu_mut() {
            menu.view = candidates;
        } else if self.open_context_menu()
            && let Some(menu) = self.context_menu_mut()
        {
            menu.view = candidates;
        }
    }

    pub fn set_candidate_results(
        &mut self,
        items: Vec<CandidateMenuItem>,
        status_message: Option<String>,
    ) {
        if let Some(menu) = self.context_menu_mut()
            && let ContextMenuView::Candidates {
                items: slot,
                selected,
                loading,
                hydrating,
                status_message: status_slot,
                ..
            } = &mut menu.view
        {
            *slot = items;
            *selected = 0;
            *loading = false;
            *hydrating = false;
            *status_slot = status_message;
        }
    }

    pub fn set_candidate_hydrating(
        &mut self,
        hydrating_flag: bool,
        status_message: Option<String>,
    ) {
        if let Some(menu) = self.context_menu_mut()
            && let ContextMenuView::Candidates {
                hydrating,
                status_message: status_slot,
                ..
            } = &mut menu.view
        {
            *hydrating = hydrating_flag;
            *status_slot = status_message;
        }
    }

    pub fn sync_menu_preferences(&mut self) {
        let source = self.pinned_source_label.clone();
        let style = self.pinned_style.clone();
        if let Some(menu) = self.context_menu_mut() {
            menu.pinned_source_label = source;
            menu.pinned_style = style;
        }
    }

    pub fn close_context_menu(&mut self) {
        self.close_context_menu_at(Instant::now());
    }

    pub fn close_context_menu_at(&mut self, now: Instant) {
        if matches!(self.modal, Some(UiModal::ContextMenu(_))) {
            self.modal = None;
            self.resume_status_bar_timer(now);
        }
    }

    pub fn close_modal(&mut self) {
        self.close_modal_at(Instant::now());
    }

    pub fn close_modal_at(&mut self, now: Instant) {
        self.modal = None;
        self.resume_status_bar_timer(now);
    }

    pub fn is_modal_open(&self) -> bool {
        self.modal.is_some()
    }

    pub fn modal_fits_viewport(&self) -> bool {
        match &self.modal {
            Some(UiModal::Help) => self.help_fits_viewport(),
            Some(UiModal::ContextMenu(_)) => self.context_menu_fits_viewport(),
            None => true,
        }
    }

    pub fn help_fits_viewport(&self) -> bool {
        self.viewport_width >= help::MIN_AREA_WIDTH && self.viewport_height >= help::MIN_AREA_HEIGHT
    }

    pub fn context_menu_fits_viewport(&self) -> bool {
        self.viewport_width >= menu::MIN_AREA_WIDTH && self.viewport_height >= menu::MIN_AREA_HEIGHT
    }

    fn pause_status_bar_timer(&mut self, now: Instant) {
        if self.status_bar_visible_until.is_some() && self.status_bar_paused_at.is_none() {
            self.status_bar_paused_at = Some(now);
        }
    }

    fn resume_status_bar_timer(&mut self, now: Instant) {
        let Some(paused_at) = self.status_bar_paused_at.take() else {
            return;
        };
        if let Some(visible_until) = &mut self.status_bar_visible_until {
            *visible_until += now.saturating_duration_since(paused_at);
        }
    }

    pub fn resize(&mut self, width: u16, height: u16) {
        if width != self.viewport_width {
            self.viewport_width = width;
            self.timeline_context.viewport_width = width;
            self.timeline_context.max_width = self.style.resolved_max_width(width);
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

    pub fn effective_display_mode(&self) -> DisplayMode {
        if !self.terminal_unicode || self.style.reduced_motion {
            return DisplayMode::Normal;
        }
        if self.display_mode == DisplayMode::Cylinder
            && (self.viewport_width < cylinder::CYLINDER_MIN_WIDTH
                || self.viewport_height < cylinder::CYLINDER_MIN_HEIGHT)
        {
            return DisplayMode::Normal;
        }
        if self.display_mode == DisplayMode::Canvas3D
            && (self.viewport_width < canvas3d::CANVAS3D_MIN_WIDTH
                || self.viewport_height < canvas3d::CANVAS3D_MIN_HEIGHT)
        {
            return DisplayMode::Normal;
        }
        self.display_mode
    }

    pub fn set_cylinder_annotations(&mut self, mode: CylinderAnnotationMode) {
        self.cylinder_annotations = mode;
    }

    pub fn set_cylinder_speed_mode(&mut self, mode: CylinderSpeedMode) {
        self.cylinder_speed_mode = mode;
    }

    pub fn set_canvas3d_flip(&mut self, flip: bool) {
        self.canvas3d_flip = flip;
    }

    pub fn set_canvas3d_speed_pct(&mut self, pct: u16) {
        self.canvas3d_speed_pct = pct.clamp(
            canvas3d::CANVAS3D_MIN_SPEED_PCT,
            canvas3d::CANVAS3D_MAX_SPEED_PCT,
        );
    }

    pub fn adjust_canvas3d_speed(&mut self, delta_pct: i32) {
        let next = i32::from(self.canvas3d_speed_pct) + delta_pct;
        self.canvas3d_speed_pct = next.clamp(
            i32::from(canvas3d::CANVAS3D_MIN_SPEED_PCT),
            i32::from(canvas3d::CANVAS3D_MAX_SPEED_PCT),
        ) as u16;
    }

    pub fn reset_canvas3d_speed(&mut self) {
        self.canvas3d_speed_pct = 100;
    }

    pub fn set_canvas3d_tilt_deg(&mut self, tilt_deg: u16) {
        self.canvas3d_tilt_deg = tilt_deg.clamp(
            canvas3d::CANVAS3D_MIN_TILT_DEG,
            canvas3d::CANVAS3D_MAX_TILT_DEG,
        );
    }

    pub fn adjust_canvas3d_tilt_deg(&mut self, delta_deg: i32) {
        let next = i32::from(self.canvas3d_tilt_deg) + delta_deg;
        self.canvas3d_tilt_deg = next.clamp(
            i32::from(canvas3d::CANVAS3D_MIN_TILT_DEG),
            i32::from(canvas3d::CANVAS3D_MAX_TILT_DEG),
        ) as u16;
    }

    pub fn set_canvas3d_spawn_y_pct(&mut self, spawn_pct: u16) {
        self.canvas3d_spawn_y_pct = spawn_pct.clamp(
            canvas3d::CANVAS3D_MIN_SPAWN_PCT,
            canvas3d::CANVAS3D_MAX_SPAWN_PCT,
        );
    }

    pub fn adjust_canvas3d_spawn_y_pct(&mut self, delta_pct: i32) {
        let next = i32::from(self.canvas3d_spawn_y_pct) + delta_pct;
        self.canvas3d_spawn_y_pct = next.clamp(
            i32::from(canvas3d::CANVAS3D_MIN_SPAWN_PCT),
            i32::from(canvas3d::CANVAS3D_MAX_SPAWN_PCT),
        ) as u16;
    }

    pub fn set_canvas3d_zoom_pct(&mut self, zoom_pct: u16) {
        self.canvas3d_zoom_pct = zoom_pct.clamp(
            canvas3d::CANVAS3D_MIN_ZOOM_PCT,
            canvas3d::CANVAS3D_MAX_ZOOM_PCT,
        );
    }

    pub fn adjust_canvas3d_zoom_pct(&mut self, delta_pct: i32) {
        let next = i32::from(self.canvas3d_zoom_pct) + delta_pct;
        self.canvas3d_zoom_pct = next.clamp(
            i32::from(canvas3d::CANVAS3D_MIN_ZOOM_PCT),
            i32::from(canvas3d::CANVAS3D_MAX_ZOOM_PCT),
        ) as u16;
    }

    pub fn set_cylinder_entry_lead_ms(&mut self, lead_ms: u64) {
        self.cylinder_entry_lead_ms = lead_ms.min(cylinder::CYLINDER_MAX_ENTRY_LEAD_MS);
    }

    pub fn adjust_cylinder_speed(&mut self, delta_pct: i32) {
        let next = i32::from(self.cylinder_speed_pct) + delta_pct;
        self.cylinder_speed_pct = next.clamp(CYLINDER_MIN_SPEED_PCT, CYLINDER_MAX_SPEED_PCT) as u16;
    }

    pub fn reset_cylinder_speed(&mut self) {
        self.cylinder_speed_pct = 100;
    }

    pub fn toggle_display_mode(&mut self) {
        self.display_mode = self.display_mode.toggled();
    }

    pub fn toggle_word_highlight(&mut self) {
        self.word_highlight = !self.word_highlight;
    }

    /// Return whether word-level highlight or wipe is active under `[ui].word_highlight`.
    pub fn effective_word_highlight(&self) -> bool {
        self.word_highlight
            && matches!(
                self.style.highlight_mode,
                HighlightMode::Word | HighlightMode::Wipe
            )
    }

    /// Frame interval used only while an active animation needs ticks.
    pub fn frame_interval(&self) -> Duration {
        Duration::from_millis(1_000 / u64::from(self.fps_active.clamp(15, 120)))
    }

    /// Return effective header visibility respecting `fullscreen`, `[ui].show_header`, and `[header].show`.
    pub fn effective_show_header(&self) -> bool {
        if self.fullscreen || !self.show_header {
            return false;
        }
        self.header_override.unwrap_or(self.style.header_show)
    }

    /// Apply a newly resolved style profile and recompile timeline wrapping when layout inputs change.
    pub fn set_resolved_style(&mut self, style: ResolvedStyle) {
        self.timeline_context.viewport_width = self.viewport_width;
        self.timeline_context.padding_horizontal = style.padding[1];
        self.timeline_context.max_width = style.resolved_max_width(self.viewport_width);
        self.timeline_context.synthetic_word_timing = style.synthetic_word_timing;
        self.timeline_context.phrase_max_chars = style.big_text_max_chars_per_phrase;
        self.timeline_context.instrumental_marker = style.instrumental_marker;
        self.timeline_context.min_gap_ms = style.instrumental_min_gap_ms;
        self.style = style;
        self.recompile_if_context_changed();
        self.clamp_scroll();
    }

    /// Explicitly pin `[ui].show_header` override from config or interactive toggle.
    pub fn set_header_override(&mut self, override_val: Option<bool>) {
        self.header_override = override_val;
        self.clamp_scroll();
    }

    /// True if instrumental countdown should animate/display for current style.
    pub fn instrumental_countdown_enabled(&self) -> bool {
        self.style.instrumental_style != InstrumentalStyle::Hidden
    }

    pub fn timeline_context(&self) -> &TimelineContext {
        &self.timeline_context
    }

    pub fn configure_layout(
        &mut self,
        fullscreen: bool,
        show_header: bool,
        status_bar: StatusBarMode,
    ) {
        self.fullscreen = fullscreen;
        self.show_header = show_header;
        self.status_bar = status_bar;
        self.status_bar_last_visible = if status_bar == StatusBarMode::Off {
            StatusBarMode::Bottom
        } else {
            status_bar
        };
        self.status_bar_visible_until = None;
        self.status_bar_paused_at = None;
        self.clamp_scroll();
    }

    pub fn toggle_fullscreen(&mut self) {
        self.fullscreen = !self.fullscreen;
        self.status_bar_visible_until = None;
        self.clamp_scroll();
    }

    pub fn toggle_header(&mut self) {
        let next = !self.effective_show_header();
        self.fullscreen = false;
        self.show_header = next;
        self.header_override = Some(next);
        self.clamp_scroll();
    }

    pub fn toggle_status_bar(&mut self) {
        let visible = !self.fullscreen
            && self
                .status_bar_placement_for_height(self.viewport_height)
                .is_some();
        self.fullscreen = false;
        self.status_bar_visible_until = None;
        self.status_bar_paused_at = None;

        if visible {
            self.status_bar_last_visible = self.status_bar;
            self.status_bar = StatusBarMode::Off;
        } else {
            match self.status_bar {
                StatusBarMode::Off => {
                    self.status_bar = if self.status_bar_last_visible == StatusBarMode::Auto
                        && self.viewport_height < MIN_STATUS_BAR_AUTO_HEIGHT
                    {
                        StatusBarMode::Bottom
                    } else {
                        self.status_bar_last_visible
                    };
                }
                StatusBarMode::Auto if self.viewport_height < MIN_STATUS_BAR_AUTO_HEIGHT => {
                    self.status_bar = StatusBarMode::Bottom;
                }
                _ => {}
            }
        }
        self.clamp_scroll();
    }

    pub fn configure_status_bar(&mut self, mode: StatusBarMode) {
        self.status_bar = mode;
        self.status_bar_last_visible = if mode == StatusBarMode::Off {
            StatusBarMode::Bottom
        } else {
            mode
        };
        self.status_bar_visible_until = None;
        self.status_bar_paused_at = None;
        self.clamp_scroll();
    }

    pub fn peek_status_bar(&mut self, now: Instant) {
        if self.status_bar_placement().is_some() || self.viewport_height < 2 {
            return;
        }
        self.status_bar_visible_until = Some(now + STATUS_BAR_PEEK_DURATION);
        self.clamp_scroll();
    }

    pub fn expire_status_bar(&mut self, now: Instant) {
        if self.is_modal_open() {
            return;
        }
        if let Some(visible_until) = self.status_bar_visible_until
            && now >= visible_until
        {
            self.status_bar_visible_until = None;
            self.clamp_scroll();
        }
    }

    pub fn time_until_status_bar_expiry(&self, now: Instant) -> Option<Duration> {
        let visible_until = self.status_bar_visible_until?;
        Some(visible_until.saturating_duration_since(now))
    }

    pub fn status_bar_placement(&self) -> Option<StatusBarPlacement> {
        self.status_bar_placement_for_height(self.viewport_height)
    }

    pub fn status_bar_placement_for_height(&self, height: u16) -> Option<StatusBarPlacement> {
        if self.status_bar_visible_until.is_some() && height >= 2 {
            return Some(match self.status_bar_last_visible {
                StatusBarMode::Top => StatusBarPlacement::Top,
                _ => StatusBarPlacement::Bottom,
            });
        }
        if self.fullscreen {
            return None;
        }
        match self.status_bar {
            StatusBarMode::Top => Some(StatusBarPlacement::Top),
            StatusBarMode::Bottom => Some(StatusBarPlacement::Bottom),
            StatusBarMode::Auto if height >= MIN_STATUS_BAR_AUTO_HEIGHT => {
                Some(StatusBarPlacement::Bottom)
            }
            StatusBarMode::Auto | StatusBarMode::Off => None,
        }
    }

    pub fn status_bar_visible(&self) -> bool {
        self.status_bar_placement().is_some()
    }

    pub fn banner_visible(&self) -> bool {
        self.transient_banner.is_some()
    }

    pub fn is_fullscreen(&self) -> bool {
        self.fullscreen
    }

    pub fn persisted_layout(&self) -> (bool, bool, StatusBarMode) {
        (self.fullscreen, self.show_header, self.status_bar)
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
        self.pinned_source_label = None;
        self.pinned_style = None;
        self.close_context_menu();
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
        self.pinned_source_label = None;
        self.pinned_style = None;
        self.close_context_menu();
        self.scroll = 0;
    }

    pub fn set_no_track(&mut self) {
        self.track = None;
        self.lyrics = LyricsView::Idle;
        self.refetching = false;
        self.user_offset_ms = 0;
        self.pinned_source_label = None;
        self.pinned_style = None;
        self.close_context_menu();
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

    /// Status label for the loaded track, showing `line (est. word)` when synthetic word timings are active.
    pub fn track_status_label(&self) -> Option<&'static str> {
        let track = self.lyrics_track()?;
        if track.is_instrumental {
            Some("instrumental")
        } else if track.granularity == SyncGranularity::Line
            && self
                .timeline()
                .is_some_and(|timeline| timeline.has_synthetic_words)
        {
            Some("line (est. word)")
        } else {
            Some(track.status_label())
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
        let header_rows = u16::from(self.effective_show_header());
        let status_rows = u16::from(self.status_bar_visible());
        let banner_rows = u16::from(self.banner_visible());
        usize::from(
            self.viewport_height
                .saturating_sub(header_rows + status_rows + banner_rows)
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
        let v_padding = usize::from(self.style.padding[0].saturating_mul(2));
        let body_rows = self.body_rows().saturating_sub(v_padding).max(1);
        let mut rows = 0usize;
        let mut start = timeline.lines.len();

        for (index, line) in timeline.lines.iter().enumerate().rev() {
            let annotation_rows = if self.style.annotations_show {
                line.wrapped_annotations.len()
            } else {
                0
            };
            let spacing = if index + 1 < timeline.lines.len() {
                usize::from(self.style.line_spacing)
            } else {
                0
            };
            let line_rows = line.wrapped_main.len().max(1) + annotation_rows + spacing;
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
        self.timeline_context.synthetic_word_timing = self.style.synthetic_word_timing;
        self.timeline_context.phrase_max_chars = self.style.big_text_max_chars_per_phrase;
        self.timeline_context.instrumental_marker = self.style.instrumental_marker;
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
    use cylinder::{CYLINDER_MIN_HEIGHT, CYLINDER_MIN_WIDTH};

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
    fn display_mode_toggles_and_ascii_forces_normal_rendering() {
        let mut state = UiState::new();
        assert_eq!(state.display_mode, DisplayMode::Normal);
        assert_eq!(state.effective_display_mode(), DisplayMode::Normal);
        assert!(state.word_highlight);

        state.toggle_display_mode();
        assert_eq!(state.display_mode, DisplayMode::Block);
        assert_eq!(state.effective_display_mode(), DisplayMode::Block);

        state.set_terminal_unicode(false);
        assert_eq!(state.display_mode, DisplayMode::Block);
        assert_eq!(state.effective_display_mode(), DisplayMode::Normal);

        state.set_terminal_unicode(true);
        assert_eq!(state.effective_display_mode(), DisplayMode::Block);
        state.toggle_display_mode();
        assert_eq!(state.display_mode, DisplayMode::Kinetic);
        assert_eq!(state.effective_display_mode(), DisplayMode::Kinetic);

        state.set_terminal_unicode(false);
        assert_eq!(state.effective_display_mode(), DisplayMode::Normal);
        state.set_terminal_unicode(true);

        state.toggle_display_mode();
        assert_eq!(state.display_mode, DisplayMode::Cylinder);
        assert_eq!(state.effective_display_mode(), DisplayMode::Cylinder);

        state.set_terminal_unicode(false);
        assert_eq!(state.effective_display_mode(), DisplayMode::Normal);
        state.set_terminal_unicode(true);

        state.toggle_display_mode();
        assert_eq!(state.display_mode, DisplayMode::Canvas3D);
        assert_eq!(state.effective_display_mode(), DisplayMode::Canvas3D);

        state.set_terminal_unicode(false);
        assert_eq!(state.effective_display_mode(), DisplayMode::Normal);
        state.set_terminal_unicode(true);

        state.toggle_display_mode();
        assert_eq!(state.display_mode, DisplayMode::Normal);

        state.toggle_word_highlight();
        assert!(!state.word_highlight);
    }

    #[test]
    fn cylinder_mode_falls_back_to_normal_on_narrow_or_reduced_motion() {
        let mut state = UiState::new();
        state.display_mode = DisplayMode::Cylinder;
        state.viewport_width = 80;
        state.viewport_height = 24;
        assert_eq!(state.effective_display_mode(), DisplayMode::Cylinder);

        state.viewport_width = CYLINDER_MIN_WIDTH - 1;
        assert_eq!(state.effective_display_mode(), DisplayMode::Normal);
        state.viewport_width = CYLINDER_MIN_WIDTH;
        state.viewport_height = CYLINDER_MIN_HEIGHT - 1;
        assert_eq!(state.effective_display_mode(), DisplayMode::Normal);

        state.viewport_width = 80;
        state.viewport_height = 24;
        assert_eq!(state.effective_display_mode(), DisplayMode::Cylinder);

        state.style.reduced_motion = true;
        assert_eq!(state.effective_display_mode(), DisplayMode::Normal);
        state.style.reduced_motion = false;
        assert_eq!(state.effective_display_mode(), DisplayMode::Cylinder);
        assert_eq!(state.display_mode, DisplayMode::Cylinder);
    }

    #[test]
    fn cylinder_annotation_mode_names_are_stable() {
        assert_eq!(
            CylinderAnnotationMode::default(),
            CylinderAnnotationMode::Auto
        );
        assert_eq!(CylinderAnnotationMode::Auto.as_str(), "auto");
        assert_eq!(CylinderAnnotationMode::Show.as_str(), "show");
        assert_eq!(CylinderAnnotationMode::Hide.as_str(), "hide");
        assert_eq!(
            CylinderAnnotationMode::from_config_name("show"),
            Some(CylinderAnnotationMode::Show)
        );
        assert_eq!(CylinderAnnotationMode::from_config_name("bogus"), None);

        let mut state = UiState::new();
        assert_eq!(state.cylinder_annotations, CylinderAnnotationMode::Auto);
        state.set_cylinder_annotations(CylinderAnnotationMode::Hide);
        assert_eq!(state.cylinder_annotations, CylinderAnnotationMode::Hide);
    }

    #[test]
    fn display_mode_names_are_stable() {
        assert_eq!(DisplayMode::default(), DisplayMode::Normal);
        assert_eq!(DisplayMode::Normal.as_str(), "normal");
        assert_eq!(DisplayMode::Block.as_str(), "block");
        assert_eq!(DisplayMode::Kinetic.as_str(), "kinetic");
        assert_eq!(DisplayMode::Cylinder.as_str(), "cylinder");
        assert_eq!(
            DisplayMode::from_config_name("block"),
            Some(DisplayMode::Block)
        );
        assert_eq!(
            DisplayMode::from_config_name("kinetic"),
            Some(DisplayMode::Kinetic)
        );
        assert_eq!(
            DisplayMode::from_config_name("cylinder"),
            Some(DisplayMode::Cylinder)
        );
    }

    #[test]
    fn layout_toggles_restore_fullscreen_state() {
        let mut state = UiState::new();
        assert_eq!(
            state.persisted_layout(),
            (false, true, StatusBarMode::Bottom)
        );

        state.toggle_fullscreen();
        assert!(state.is_fullscreen());
        assert_eq!(
            state.persisted_layout(),
            (true, true, StatusBarMode::Bottom)
        );
        assert!(!state.effective_show_header());

        state.toggle_fullscreen();
        assert_eq!(
            (state.show_header, state.status_bar),
            (true, StatusBarMode::Bottom)
        );

        state.toggle_header();
        assert_eq!(
            (state.show_header, state.status_bar),
            (false, StatusBarMode::Bottom)
        );
        state.configure_layout(true, state.show_header, state.status_bar);
        assert!(state.is_fullscreen());
        assert_eq!(
            state.persisted_layout(),
            (true, false, StatusBarMode::Bottom)
        );

        state.toggle_fullscreen();
        assert_eq!(
            (state.show_header, state.status_bar),
            (false, StatusBarMode::Bottom)
        );
    }

    #[test]
    fn body_rows_follow_layout_and_banner() {
        let mut state = UiState::new();
        state.viewport_height = 24;
        assert_eq!(state.body_rows(), 22);

        state.toggle_header();
        assert_eq!(state.body_rows(), 23);

        state.toggle_status_bar();
        assert_eq!(state.body_rows(), 24);

        state.show_banner("Offline mode", Instant::now());
        assert_eq!(state.body_rows(), 23);

        state.expire_banner(Instant::now() + BANNER_DURATION);
        assert_eq!(state.body_rows(), 24);

        state.configure_status_bar(StatusBarMode::Top);
        assert_eq!(state.body_rows(), 23);

        state.show_banner("Offline mode", Instant::now());
        assert_eq!(state.body_rows(), 22);
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
    fn status_bar_modes_peek_and_breakpoint() {
        let mut state = UiState::new();
        let t0 = Instant::now();

        state.configure_status_bar(StatusBarMode::Auto);
        state.resize(80, 24);
        assert_eq!(
            state.status_bar_placement(),
            Some(StatusBarPlacement::Bottom)
        );
        state.configure_status_bar(StatusBarMode::Off);
        assert_eq!(state.status_bar_placement(), None);
        state.peek_status_bar(t0);
        assert_eq!(
            state.status_bar_placement(),
            Some(StatusBarPlacement::Bottom)
        );
        assert_eq!(
            state.time_until_status_bar_expiry(t0 + Duration::from_secs(1)),
            Some(Duration::from_secs(2))
        );
        state.expire_status_bar(t0 + Duration::from_secs(2));
        assert_eq!(
            state.status_bar_placement(),
            Some(StatusBarPlacement::Bottom)
        );
        state.expire_status_bar(t0 + STATUS_BAR_PEEK_DURATION);
        assert_eq!(state.status_bar_placement(), None);

        state.configure_status_bar(StatusBarMode::Off);
        assert_eq!(state.status_bar_placement(), None);
        state.peek_status_bar(t0);
        assert_eq!(
            state.status_bar_placement(),
            Some(StatusBarPlacement::Bottom)
        );
        state.expire_status_bar(t0 + STATUS_BAR_PEEK_DURATION);
        assert_eq!(state.status_bar_placement(), None);

        state.configure_status_bar(StatusBarMode::Top);
        assert_eq!(state.status_bar_placement(), Some(StatusBarPlacement::Top));
        assert_eq!(state.time_until_status_bar_expiry(t0), None);

        state.configure_status_bar(StatusBarMode::Bottom);
        assert_eq!(
            state.status_bar_placement(),
            Some(StatusBarPlacement::Bottom)
        );
        state.toggle_status_bar();
        assert_eq!(state.status_bar, StatusBarMode::Off);
        assert_eq!(state.status_bar_placement(), None);
        state.toggle_status_bar();
        assert_eq!(state.status_bar, StatusBarMode::Bottom);
        assert_eq!(
            state.status_bar_placement(),
            Some(StatusBarPlacement::Bottom)
        );

        state.configure_status_bar(StatusBarMode::Auto);
        state.resize(80, 7);
        assert_eq!(state.status_bar_placement(), None);
        state.toggle_status_bar();
        assert_eq!(state.status_bar, StatusBarMode::Bottom);
        assert_eq!(
            state.status_bar_placement(),
            Some(StatusBarPlacement::Bottom)
        );
        state.toggle_status_bar();
        assert_eq!(state.status_bar, StatusBarMode::Off);
        state.toggle_status_bar();
        state.resize(80, 8);
        assert_eq!(
            state.status_bar_placement(),
            Some(StatusBarPlacement::Bottom)
        );

        state.configure_status_bar(StatusBarMode::Bottom);
        state.toggle_fullscreen();
        assert_eq!(state.status_bar_placement(), None);
        state.toggle_status_bar();
        assert!(!state.is_fullscreen());
        assert_eq!(
            state.status_bar_placement(),
            Some(StatusBarPlacement::Bottom)
        );

        state.configure_status_bar(StatusBarMode::Bottom);
        state.toggle_fullscreen();
        assert_eq!(state.status_bar_placement(), None);
        state.peek_status_bar(t0);
        assert_eq!(
            state.status_bar_placement(),
            Some(StatusBarPlacement::Bottom)
        );
        state.expire_status_bar(t0 + STATUS_BAR_PEEK_DURATION);
        assert_eq!(state.status_bar_placement(), None);
        state.toggle_fullscreen();

        state.configure_status_bar(StatusBarMode::Off);
        state.peek_status_bar(t0);
        state.toggle_help_at(t0 + Duration::from_secs(2));
        state.expire_status_bar(t0 + Duration::from_secs(6));
        assert_eq!(
            state.status_bar_placement(),
            Some(StatusBarPlacement::Bottom)
        );
        state.close_modal_at(t0 + Duration::from_secs(6));
        assert_eq!(
            state.time_until_status_bar_expiry(t0 + Duration::from_secs(6)),
            Some(Duration::from_secs(1))
        );
        state.expire_status_bar(t0 + Duration::from_secs(7));
        assert_eq!(state.status_bar_placement(), None);
    }

    #[test]
    fn style_switch_updates_synthetic_word_timing_and_recompiles_without_mutating_track() {
        let mut state = UiState::new();
        let line_track = track(SyncGranularity::Line);
        state.set_lyrics(line_track.clone());
        assert!(!state.timeline().unwrap().has_synthetic_words);
        assert!(state.timeline().unwrap().lines[0].words.is_empty());

        let mut profile = crate::style::StyleProfile::default();
        profile.meta.synthetic_word_timing = true;
        let resolved = profile.resolve(
            "est-word",
            crate::style::color::ColorCapability::TrueColor,
            None,
        );
        state.set_resolved_style(resolved);

        assert!(state.timeline_context().synthetic_word_timing);
        assert!(state.timeline().unwrap().has_synthetic_words);
        assert_eq!(state.timeline().unwrap().lines[0].words.len(), 2);
        assert_eq!(state.lyrics_track(), Some(&line_track));
        assert_eq!(state.track_status_label(), Some("line (est. word)"));

        // Switching back to a style with synthetic_word_timing = false clears synthetic words.
        let off_resolved = crate::style::StyleProfile::default().resolve(
            "default",
            crate::style::color::ColorCapability::TrueColor,
            None,
        );
        state.set_resolved_style(off_resolved);
        assert!(!state.timeline_context().synthetic_word_timing);
        assert!(!state.timeline().unwrap().has_synthetic_words);
        assert!(state.timeline().unwrap().lines[0].words.is_empty());
        assert_eq!(state.track_status_label(), Some("line"));
    }
}

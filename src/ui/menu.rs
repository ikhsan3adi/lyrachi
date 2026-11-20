use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Padding, Paragraph},
};
use unicode_segmentation::UnicodeSegmentation;

use crate::core::text::{display_width, truncate_to_width};
use crate::style::ResolvedStyle;

use super::help::centered_rect;

const MAX_MODAL_WIDTH: u16 = 96;
const MIN_MODAL_WIDTH: u16 = 56;
const MAX_ACTIONS_WIDTH: u16 = 68;
const MIN_ACTIONS_WIDTH: u16 = 56;
const MIN_MODAL_HEIGHT: u16 = 6;
const HORIZONTAL_PADDING: u16 = 2;
const CHROME_WIDTH: u16 = 2 + HORIZONTAL_PADDING * 2;
pub const MIN_AREA_WIDTH: u16 = MIN_MODAL_WIDTH + 2;
pub const MIN_AREA_HEIGHT: u16 = MIN_MODAL_HEIGHT + 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextMenuAction {
    SelectCandidate,
    EditSearchQuery,
    ResetPreferredSource,
    TogglePinStyle,
}

impl ContextMenuAction {
    pub const ALL: [Self; 4] = [
        Self::SelectCandidate,
        Self::EditSearchQuery,
        Self::ResetPreferredSource,
        Self::TogglePinStyle,
    ];
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidateMenuItem {
    pub provider_label: String,
    pub title: String,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub duration_ms: Option<u64>,
    pub tier_label: &'static str,
    pub score_pct: u8,
    pub is_active: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchField {
    Artist,
    Title,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchInputState {
    pub artist: String,
    pub title: String,
    pub active_field: SearchField,
}

impl SearchInputState {
    pub fn push_char(&mut self, ch: char) {
        match self.active_field {
            SearchField::Artist => self.artist.push(ch),
            SearchField::Title => self.title.push(ch),
        }
    }

    pub fn pop_char(&mut self) {
        match self.active_field {
            SearchField::Artist => {
                self.artist.pop();
            }
            SearchField::Title => {
                self.title.pop();
            }
        }
    }

    pub fn switch_field(&mut self) {
        self.active_field = match self.active_field {
            SearchField::Artist => SearchField::Title,
            SearchField::Title => SearchField::Artist,
        };
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContextMenuView {
    Actions {
        selected: usize,
    },
    Candidates {
        query_label: String,
        items: Vec<CandidateMenuItem>,
        selected: usize,
        loading: bool,
        hydrating: bool,
        status_message: Option<String>,
    },
    SearchInput(SearchInputState),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextMenuState {
    pub view: ContextMenuView,
    pub track_label: String,
    pub pinned_source_label: Option<String>,
    pub pinned_style: Option<String>,
}

impl ContextMenuState {
    pub fn actions(
        track_label: String,
        pinned_source_label: Option<String>,
        pinned_style: Option<String>,
    ) -> Self {
        Self {
            view: ContextMenuView::Actions { selected: 0 },
            track_label,
            pinned_source_label,
            pinned_style,
        }
    }

    pub fn move_up(&mut self) {
        match &mut self.view {
            ContextMenuView::Actions { selected } => {
                if *selected == 0 {
                    *selected = ContextMenuAction::ALL.len().saturating_sub(1);
                } else {
                    *selected -= 1;
                }
            }
            ContextMenuView::Candidates {
                items,
                selected,
                hydrating,
                ..
            } => {
                if *hydrating || items.is_empty() {
                    return;
                }
                if *selected == 0 {
                    *selected = items.len() - 1;
                } else {
                    *selected -= 1;
                }
            }
            ContextMenuView::SearchInput(_) => {}
        }
    }

    pub fn move_down(&mut self) {
        match &mut self.view {
            ContextMenuView::Actions { selected } => {
                *selected = (*selected + 1) % ContextMenuAction::ALL.len();
            }
            ContextMenuView::Candidates {
                items,
                selected,
                hydrating,
                ..
            } => {
                if *hydrating || items.is_empty() {
                    return;
                }
                *selected = (*selected + 1) % items.len();
            }
            ContextMenuView::SearchInput(_) => {}
        }
    }

    pub fn selected_action(&self) -> Option<ContextMenuAction> {
        match self.view {
            ContextMenuView::Actions { selected } => ContextMenuAction::ALL.get(selected).copied(),
            _ => None,
        }
    }

    pub fn selected_candidate_index(&self) -> Option<usize> {
        match &self.view {
            ContextMenuView::Candidates {
                items,
                selected,
                loading,
                hydrating,
                ..
            } if !*loading && !*hydrating && *selected < items.len() => Some(*selected),
            _ => None,
        }
    }
}

pub fn render(frame: &mut Frame, area: Rect, menu: &ContextMenuState, style: &ResolvedStyle) {
    let available_width = area.width.saturating_sub(2);
    let available_height = area.height.saturating_sub(2);
    if available_width < MIN_MODAL_WIDTH || available_height < MIN_MODAL_HEIGHT {
        return;
    }

    match &menu.view {
        ContextMenuView::Actions { selected } => {
            render_actions(
                frame,
                area,
                menu,
                *selected,
                available_width,
                available_height,
                style,
            );
        }
        ContextMenuView::Candidates {
            query_label,
            items,
            selected,
            loading,
            hydrating,
            status_message,
        } => {
            render_candidates(
                frame,
                area,
                query_label,
                items,
                *selected,
                *loading,
                *hydrating,
                status_message.as_deref(),
                available_width,
                available_height,
                style,
            );
        }
        ContextMenuView::SearchInput(search) => {
            render_search_input(frame, area, search, available_height, style);
        }
    }
}

fn modal_width(area_width: u16) -> u16 {
    let available = area_width.saturating_sub(2);
    let preferred = area_width.saturating_mul(3) / 4;
    preferred
        .clamp(MIN_MODAL_WIDTH, MAX_MODAL_WIDTH)
        .min(available)
}

fn content_style(style: &ResolvedStyle) -> Style {
    let mut content = Style::default();
    if let Some(surface) = style.surface {
        content = content.bg(surface);
    }
    content
}

fn accent(style: &ResolvedStyle) -> Color {
    style.accent.unwrap_or(Color::Cyan)
}

fn full_row_line(text: &str, width: usize, style: Style) -> Line<'static> {
    let text = truncate_display(text, width);
    let padding = width.saturating_sub(usize::from(display_width(&text)));
    Line::from(Span::styled(
        format!("{text}{}", " ".repeat(padding)),
        style,
    ))
}

fn selected_style(style: &ResolvedStyle) -> Style {
    Style::default()
        .fg(accent(style))
        .add_modifier(Modifier::BOLD | Modifier::REVERSED)
}

fn render_actions(
    frame: &mut Frame,
    area: Rect,
    menu: &ContextMenuState,
    selected: usize,
    available_width: u16,
    available_height: u16,
    style: &ResolvedStyle,
) {
    let rows = build_action_rows(menu);
    let width = modal_width(area.width)
        .clamp(MIN_ACTIONS_WIDTH, MAX_ACTIONS_WIDTH)
        .min(available_width);

    let compact = available_height < u16::try_from(rows.len() + 6).unwrap_or(u16::MAX);
    let vertical_pad: u16 = if compact { 0 } else { 1 };
    let chrome_rows = 2 + vertical_pad * 2;
    let desired_rows = if compact {
        rows.len() + 1 + usize::from(chrome_rows)
    } else {
        rows.len() + 2 + usize::from(chrome_rows)
    };
    let height = u16::try_from(desired_rows)
        .unwrap_or(u16::MAX)
        .min(available_height);

    let popup = centered_rect(area, width, height);
    frame.render_widget(Clear, popup);

    let inner_width = usize::from(popup.width.saturating_sub(CHROME_WIDTH));
    let inner_height = usize::from(popup.height.saturating_sub(chrome_rows));
    let mut lines = Vec::with_capacity(inner_height.max(1));
    lines.push(Line::from(Span::styled(
        truncate_display(&menu.track_label, inner_width),
        Style::default().add_modifier(Modifier::BOLD),
    )));
    if !compact {
        lines.push(Line::raw(""));
    }

    let action_slots = inner_height.saturating_sub(lines.len()).max(1);
    let start = if selected >= action_slots {
        selected + 1 - action_slots
    } else {
        0
    };
    let end = (start + action_slots).min(rows.len());

    for (index, label) in rows.into_iter().enumerate().take(end).skip(start) {
        let row_style = if index == selected {
            selected_style(style)
        } else {
            Style::default()
        };
        lines.push(full_row_line(&label, inner_width, row_style));
    }

    let border_style = Style::default().fg(accent(style));
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(border_style)
        .padding(Padding::symmetric(HORIZONTAL_PADDING, vertical_pad))
        .title(Line::from(Span::styled(
            " Context Menu ",
            Style::default()
                .fg(accent(style))
                .add_modifier(Modifier::BOLD),
        )))
        .title_bottom(
            Line::from(Span::styled(
                " Up/Down move  Enter select  Esc close ",
                Style::default().add_modifier(Modifier::DIM),
            ))
            .alignment(Alignment::Right),
        );

    frame.render_widget(
        Paragraph::new(lines)
            .style(content_style(style))
            .block(block),
        popup,
    );
}

fn build_action_rows(menu: &ContextMenuState) -> Vec<String> {
    let source_suffix = match &menu.pinned_source_label {
        Some(provider) => format!(" (pinned: {provider})"),
        None => " (auto)".to_string(),
    };
    let style_row = match &menu.pinned_style {
        Some(style) => format!("Unpin style from track ({style})"),
        None => "Pin style to track".to_string(),
    };
    vec![
        "Select lyrics candidate...".to_string(),
        "Manual search query...".to_string(),
        format!("Reset preferred source{source_suffix}"),
        style_row,
    ]
}

fn muted_fg(style: &ResolvedStyle) -> Color {
    style.active_fg.or(style.inactive_fg).unwrap_or(Color::Gray)
}

#[allow(clippy::too_many_arguments)]
fn render_candidates(
    frame: &mut Frame,
    area: Rect,
    query_label: &str,
    items: &[CandidateMenuItem],
    selected: usize,
    loading: bool,
    hydrating: bool,
    status_message: Option<&str>,
    available_width: u16,
    available_height: u16,
    style: &ResolvedStyle,
) {
    let width = modal_width(area.width).min(available_width);
    let extra_rows: u16 = if hydrating || status_message.is_some() {
        1
    } else {
        0
    };
    let max_items = usize::from(available_height.saturating_sub(5 + extra_rows)).clamp(1, 16);
    let desired_rows = if loading || items.is_empty() {
        7
    } else {
        items.len().min(max_items) + 5 + usize::from(extra_rows)
    };
    let height = u16::try_from(desired_rows)
        .unwrap_or(u16::MAX)
        .min(available_height);

    let popup = centered_rect(area, width, height);
    frame.render_widget(Clear, popup);

    let inner_width = usize::from(popup.width.saturating_sub(CHROME_WIDTH));
    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(Line::from(Span::styled(
        truncate_display(&format!("Query: {query_label}"), inner_width),
        Style::default().fg(muted_fg(style)),
    )));

    if loading {
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled(
            "Searching providers...",
            Style::default().fg(accent(style)),
        )));
    } else if items.is_empty() {
        lines.push(Line::raw(""));
        let msg = status_message.unwrap_or("No candidates found.");
        lines.push(Line::from(Span::styled(
            truncate_display(msg, inner_width),
            Style::default().fg(Color::Yellow),
        )));
    } else {
        let available_slots = usize::from(popup.height.saturating_sub(5 + extra_rows)).clamp(1, 16);
        let start = if selected >= available_slots {
            selected + 1 - available_slots
        } else {
            0
        };
        let end = (start + available_slots).min(items.len());
        for (idx, item) in items.iter().enumerate().take(end).skip(start) {
            let row_style = if idx == selected {
                selected_style(style)
            } else {
                Style::default()
            };
            lines.push(full_row_line(
                &format_candidate_line(item),
                inner_width,
                row_style,
            ));
        }
        if hydrating {
            lines.push(Line::from(Span::styled(
                "Loading selected lyrics...",
                Style::default().fg(Color::Yellow),
            )));
        } else if let Some(msg) = status_message {
            lines.push(Line::from(Span::styled(
                truncate_display(msg, inner_width),
                Style::default().fg(Color::Red),
            )));
        }
    }

    let available_slots = usize::from(popup.height.saturating_sub(5 + extra_rows)).clamp(1, 16);
    let is_scrolling = items.len() > available_slots;
    let title = if is_scrolling {
        format!(" Select Candidate ({}/{}) ", selected + 1, items.len())
    } else {
        " Select Candidate ".to_string()
    };
    let bottom_hint = if is_scrolling {
        " [Arrows] move  Enter pin  Esc close "
    } else {
        " Up/Down move  Enter pin  Esc close "
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(accent(style)))
        .padding(Padding::symmetric(HORIZONTAL_PADDING, 1))
        .title(Line::from(Span::styled(
            title,
            Style::default()
                .fg(accent(style))
                .add_modifier(Modifier::BOLD),
        )))
        .title_bottom(
            Line::from(Span::styled(
                bottom_hint,
                Style::default()
                    .fg(muted_fg(style))
                    .add_modifier(Modifier::DIM),
            ))
            .alignment(Alignment::Right),
        );

    frame.render_widget(
        Paragraph::new(lines)
            .style(content_style(style))
            .block(block),
        popup,
    );
}

fn render_search_input(
    frame: &mut Frame,
    area: Rect,
    search: &SearchInputState,
    available_height: u16,
    style: &ResolvedStyle,
) {
    let width = modal_width(area.width);
    let height = 8u16.min(available_height);
    let popup = centered_rect(area, width, height);
    frame.render_widget(Clear, popup);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(accent(style)))
        .padding(Padding::symmetric(HORIZONTAL_PADDING, 1))
        .title(Line::from(Span::styled(
            " Manual Search ",
            Style::default()
                .fg(accent(style))
                .add_modifier(Modifier::BOLD),
        )))
        .title_bottom(
            Line::from(Span::styled(
                " Tab field  Enter search  Esc cancel ",
                Style::default()
                    .fg(muted_fg(style))
                    .add_modifier(Modifier::DIM),
            ))
            .alignment(Alignment::Right),
        );

    let inner = block.inner(popup);
    frame.render_widget(block.style(content_style(style)), popup);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(1),
        ])
        .split(inner);

    render_input_row(
        frame,
        rows[0],
        "Artist",
        &search.artist,
        search.active_field == SearchField::Artist,
        style,
    );
    render_input_row(
        frame,
        rows[2],
        "Title ",
        &search.title,
        search.active_field == SearchField::Title,
        style,
    );
    frame.render_widget(
        Paragraph::new(Span::styled(
            "Type query text directly; global hotkeys are locked.",
            Style::default()
                .fg(muted_fg(style))
                .add_modifier(Modifier::DIM),
        ))
        .style(content_style(style)),
        rows[3],
    );
}

fn render_input_row(
    frame: &mut Frame,
    area: Rect,
    label: &str,
    value: &str,
    active: bool,
    style: &ResolvedStyle,
) {
    let marker = if active { "> " } else { "  " };
    let cursor = if active { "_" } else { "" };
    let label_style = if active {
        Style::default()
            .fg(accent(style))
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(muted_fg(style))
    };
    let max_val_len = usize::from(area.width).saturating_sub(marker.len() + label.len() + 3);
    let visible = tail_display(value, max_val_len);
    let line = Line::from(vec![
        Span::styled(marker, label_style),
        Span::styled(format!("{label}: "), label_style),
        Span::raw(visible),
        Span::styled(cursor, Style::default().fg(accent(style))),
    ]);
    frame.render_widget(Paragraph::new(line).style(content_style(style)), area);
}

fn format_candidate_line(item: &CandidateMenuItem) -> String {
    let mut label = format!("[{}] {}", item.provider_label, item.title);
    if let Some(artist) = &item.artist
        && !artist.is_empty()
    {
        label.push_str(" - ");
        label.push_str(artist);
    }
    let duration = item.duration_ms.map(format_duration_ms);
    let active_tag = if item.is_active { " *" } else { "" };
    match duration {
        Some(dur) => format!(
            "{label} ({dur}, {}, {}%){active_tag}",
            item.tier_label, item.score_pct
        ),
        None => format!(
            "{label} ({}, {}%){active_tag}",
            item.tier_label, item.score_pct
        ),
    }
}

fn format_duration_ms(ms: u64) -> String {
    let secs = ms / 1_000;
    format!("{:02}:{:02}", secs / 60, secs % 60)
}

fn truncate_display(text: &str, max_cols: usize) -> String {
    if max_cols == 0 {
        return String::new();
    }
    if usize::from(display_width(text)) <= max_cols {
        return text.to_string();
    }
    let max_u16 = u16::try_from(max_cols).unwrap_or(u16::MAX);
    if max_cols <= 3 {
        return truncate_to_width(text, max_u16).to_string();
    }
    format!("{}...", truncate_to_width(text, max_u16 - 3))
}

fn tail_display(text: &str, max_cols: usize) -> String {
    if max_cols == 0 {
        return String::new();
    }
    if usize::from(display_width(text)) <= max_cols {
        return text.to_string();
    }
    let mut used = 0usize;
    let mut start_byte = text.len();
    for (byte_idx, grapheme) in text.grapheme_indices(true).rev() {
        let width = usize::from(display_width(grapheme));
        if used + width > max_cols {
            break;
        }
        used += width;
        start_byte = byte_idx;
    }
    text[start_byte..].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    #[test]
    fn format_candidate_line_and_duration() {
        let item = CandidateMenuItem {
            provider_label: "lyricsplus".to_string(),
            title: "Yellow".to_string(),
            artist: Some("Coldplay".to_string()),
            album: None,
            duration_ms: Some(267_000),
            tier_label: "word",
            score_pct: 98,
            is_active: true,
        };
        assert_eq!(
            format_candidate_line(&item),
            "[lyricsplus] Yellow - Coldplay (04:27, word, 98%) *"
        );

        let item_no_artist = CandidateMenuItem {
            provider_label: "local".to_string(),
            title: "Instrumental Track".to_string(),
            artist: None,
            album: None,
            duration_ms: None,
            tier_label: "line",
            score_pct: 75,
            is_active: false,
        };
        assert_eq!(
            format_candidate_line(&item_no_artist),
            "[local] Instrumental Track (line, 75%)"
        );
    }

    #[test]
    fn search_input_state_mutations() {
        let mut search = SearchInputState {
            artist: "Cold".to_string(),
            title: "Yel".to_string(),
            active_field: SearchField::Artist,
        };
        search.push_char('p');
        assert_eq!(search.artist, "Coldp");
        search.pop_char();
        assert_eq!(search.artist, "Cold");

        search.switch_field();
        assert_eq!(search.active_field, SearchField::Title);
        search.push_char('l');
        assert_eq!(search.title, "Yell");
    }

    #[test]
    fn menu_navigation_wraps_and_respects_hydrating() {
        let mut menu = ContextMenuState::actions("Artist - Title".to_string(), None, None);
        assert_eq!(
            menu.selected_action(),
            Some(ContextMenuAction::SelectCandidate)
        );
        menu.move_up();
        assert_eq!(
            menu.selected_action(),
            Some(ContextMenuAction::TogglePinStyle)
        );
        menu.move_down();
        assert_eq!(
            menu.selected_action(),
            Some(ContextMenuAction::SelectCandidate)
        );

        menu.view = ContextMenuView::Candidates {
            query_label: "Query".to_string(),
            items: vec![
                CandidateMenuItem {
                    provider_label: "lrclib".to_string(),
                    title: "Song 1".to_string(),
                    artist: None,
                    album: None,
                    duration_ms: None,
                    tier_label: "word",
                    score_pct: 90,
                    is_active: false,
                },
                CandidateMenuItem {
                    provider_label: "lrclib".to_string(),
                    title: "Song 2".to_string(),
                    artist: None,
                    album: None,
                    duration_ms: None,
                    tier_label: "word",
                    score_pct: 85,
                    is_active: false,
                },
            ],
            selected: 0,
            loading: false,
            hydrating: false,
            status_message: None,
        };
        assert_eq!(menu.selected_candidate_index(), Some(0));
        menu.move_down();
        assert_eq!(menu.selected_candidate_index(), Some(1));
        menu.move_down();
        assert_eq!(menu.selected_candidate_index(), Some(0));

        if let ContextMenuView::Candidates { hydrating, .. } = &mut menu.view {
            *hydrating = true;
        }
        assert_eq!(menu.selected_candidate_index(), None);
        menu.move_down();
        if let ContextMenuView::Candidates { selected, .. } = &menu.view {
            assert_eq!(*selected, 0);
        }
    }

    #[test]
    fn modal_width_follows_viewport_bounds() {
        assert_eq!(modal_width(120), 90);
        assert_eq!(modal_width(80), 60);
        assert_eq!(modal_width(40), 38);
        assert_eq!(modal_width(400), MAX_MODAL_WIDTH);
    }

    #[test]
    fn render_skips_tiny_areas_without_panic() {
        let backend = TestBackend::new(10, 5);
        let mut terminal = Terminal::new(backend).unwrap();
        let menu = ContextMenuState::actions("Artist - Title".to_string(), None, None);
        let style = ResolvedStyle::default();

        terminal
            .draw(|frame| {
                render(frame, frame.area(), &menu, &style);
            })
            .unwrap();
    }

    #[test]
    fn candidate_scrolling_window_renders_deep_selection() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();

        let items: Vec<CandidateMenuItem> = (0..20)
            .map(|i| CandidateMenuItem {
                provider_label: "lrclib".to_string(),
                title: format!("Candidate #{i}"),
                artist: Some("Artist".to_string()),
                album: None,
                duration_ms: Some(180_000),
                tier_label: "word",
                score_pct: (100 - i) as u8,
                is_active: i == 15,
            })
            .collect();

        let mut menu = ContextMenuState::actions("Artist - Title".to_string(), None, None);
        menu.view = ContextMenuView::Candidates {
            query_label: "Artist - Title".to_string(),
            items,
            selected: 15,
            loading: false,
            hydrating: false,
            status_message: None,
        };

        let style = ResolvedStyle::default();
        terminal
            .draw(|frame| {
                render(frame, frame.area(), &menu, &style);
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        let text: String = buffer.content().iter().map(|cell| cell.symbol()).collect();
        assert!(
            text.contains("Candidate #15"),
            "expected candidate #15 to be visible in scrolling window, got:\n{text}"
        );
        assert!(
            text.contains("(16/20)"),
            "expected title to display (16/20) scroll indicator, got:\n{text}"
        );
    }

    #[test]
    fn selected_row_fills_width_and_content_uses_surface() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let menu = ContextMenuState::actions("Artist - Title".to_string(), None, None);
        let style = ResolvedStyle {
            surface: Some(Color::Rgb(10, 20, 30)),
            accent: Some(Color::Rgb(0, 200, 200)),
            ..ResolvedStyle::default()
        };

        terminal
            .draw(|frame| {
                render(frame, frame.area(), &menu, &style);
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        let width = usize::from(buffer.area.width);
        let rows: Vec<String> = (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer.content()[usize::from(y) * width + usize::from(x)].symbol())
                    .collect()
            })
            .collect();
        let selected_y = rows
            .iter()
            .position(|row| row.contains("Select lyrics candidate"))
            .unwrap();
        let track_y = rows
            .iter()
            .position(|row| row.contains("Artist - Title"))
            .unwrap();
        let reversed = (0..buffer.area.width)
            .filter(|&x| {
                buffer.content()[selected_y * width + usize::from(x)]
                    .modifier
                    .contains(Modifier::REVERSED)
            })
            .count();
        assert!(
            reversed > 50,
            "selected row only highlighted {reversed} cells"
        );
        let track_x = rows[track_y].find('A').unwrap();
        assert_eq!(
            buffer.content()[track_y * width + track_x].bg,
            Color::Rgb(10, 20, 30)
        );
    }

    #[test]
    fn actions_fit_on_80x9_without_clipping_and_truncate_cjk_by_width() {
        let backend = TestBackend::new(80, 9);
        let mut terminal = Terminal::new(backend).unwrap();
        let menu = ContextMenuState::actions(
            "夜に駆ける - YOASOBI 日本語タイトル".to_string(),
            None,
            None,
        );

        let style = ResolvedStyle::default();
        terminal
            .draw(|frame| {
                render(frame, frame.area(), &menu, &style);
            })
            .unwrap();

        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(text.contains("Select lyrics candidate"), "{text}");
        assert!(text.contains("Manual search query"), "{text}");
        assert!(text.contains("Reset preferred source"), "{text}");
        assert!(text.contains("Pin style to track"), "{text}");

        let cjk = "日本語タイトルとても長い曲名テスト";
        let truncated = truncate_display(cjk, 10);
        assert!(display_width(&truncated) <= 10, "{truncated}");
        let tailed = tail_display(cjk, 8);
        assert!(display_width(&tailed) <= 8, "{tailed}");
    }
}

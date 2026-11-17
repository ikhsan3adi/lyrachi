use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Padding, Paragraph, Wrap},
};

use super::help::centered_rect;

const MAX_MODAL_WIDTH: u16 = 68;
const MIN_MODAL_WIDTH: u16 = 24;
const MIN_MODAL_HEIGHT: u16 = 7;

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

pub fn render(frame: &mut Frame, area: Rect, menu: &ContextMenuState) {
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
            );
        }
        ContextMenuView::SearchInput(search) => {
            render_search_input(frame, area, search, available_width, available_height);
        }
    }
}

fn render_actions(
    frame: &mut Frame,
    area: Rect,
    menu: &ContextMenuState,
    selected: usize,
    available_width: u16,
    available_height: u16,
) {
    let rows = build_action_rows(menu);
    let content_width = rows
        .iter()
        .map(|label| label.chars().count() + 4)
        .max()
        .unwrap_or(32)
        .max(menu.track_label.chars().count() + 4);
    let width = u16::try_from(content_width + 4)
        .unwrap_or(MAX_MODAL_WIDTH)
        .clamp(36, MAX_MODAL_WIDTH)
        .min(available_width);
    let height = u16::try_from(rows.len() + 6)
        .unwrap_or(u16::MAX)
        .min(available_height);

    let popup = centered_rect(area, width, height);
    frame.render_widget(Clear, popup);

    let mut lines = Vec::with_capacity(rows.len() + 2);
    lines.push(Line::from(Span::styled(
        truncate_chars(&menu.track_label, usize::from(width.saturating_sub(4))),
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::raw(""));

    for (index, label) in rows.into_iter().enumerate() {
        let is_selected = index == selected;
        let prefix = if is_selected { "> " } else { "  " };
        let style = if is_selected {
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        lines.push(Line::from(vec![
            Span::styled(prefix, style),
            Span::styled(label, style),
        ]));
    }

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::Cyan))
        .padding(Padding::symmetric(1, 1))
        .title(Line::from(Span::styled(
            " Context Menu ",
            Style::default().add_modifier(Modifier::BOLD),
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
            .block(block)
            .wrap(Wrap { trim: false }),
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
) {
    let width = MAX_MODAL_WIDTH.min(available_width);
    let extra_rows: u16 = if hydrating || status_message.is_some() {
        1
    } else {
        0
    };
    let desired_rows = if loading || items.is_empty() {
        7
    } else {
        items.len().min(10) + 5 + usize::from(extra_rows)
    };
    let height = u16::try_from(desired_rows)
        .unwrap_or(u16::MAX)
        .min(available_height);

    let popup = centered_rect(area, width, height);
    frame.render_widget(Clear, popup);

    let inner_width = usize::from(popup.width.saturating_sub(4));
    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(Line::from(Span::styled(
        truncate_chars(&format!("Query: {query_label}"), inner_width),
        Style::default().fg(Color::DarkGray),
    )));

    if loading {
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled(
            "Searching providers...",
            Style::default().fg(Color::Cyan),
        )));
    } else if items.is_empty() {
        lines.push(Line::raw(""));
        let msg = status_message.unwrap_or("No candidates found.");
        lines.push(Line::from(Span::styled(
            truncate_chars(msg, inner_width),
            Style::default().fg(Color::Yellow),
        )));
    } else {
        let available_slots = usize::from(popup.height.saturating_sub(5 + extra_rows)).max(1);
        let start = if selected >= available_slots {
            selected + 1 - available_slots
        } else {
            0
        };
        let end = (start + available_slots).min(items.len());
        for (idx, item) in items.iter().enumerate().take(end).skip(start) {
            let is_selected = idx == selected;
            let prefix = if is_selected { "> " } else { "  " };
            let formatted = format_candidate_line(item);
            let truncated = truncate_chars(&formatted, inner_width.saturating_sub(2));
            let style = if is_selected {
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            lines.push(Line::from(vec![
                Span::styled(prefix, style),
                Span::styled(truncated, style),
            ]));
        }
        if hydrating {
            lines.push(Line::from(Span::styled(
                "Loading selected lyrics...",
                Style::default().fg(Color::Yellow),
            )));
        } else if let Some(msg) = status_message {
            lines.push(Line::from(Span::styled(
                truncate_chars(msg, inner_width),
                Style::default().fg(Color::Red),
            )));
        }
    }

    let is_scrolling = items.len() > usize::from(popup.height.saturating_sub(5 + extra_rows));
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
        .border_style(Style::default().fg(Color::Cyan))
        .padding(Padding::symmetric(1, 1))
        .title(Line::from(Span::styled(
            title,
            Style::default().add_modifier(Modifier::BOLD),
        )))
        .title_bottom(
            Line::from(Span::styled(
                bottom_hint,
                Style::default().add_modifier(Modifier::DIM),
            ))
            .alignment(Alignment::Right),
        );

    frame.render_widget(Paragraph::new(lines).block(block), popup);
}

fn render_search_input(
    frame: &mut Frame,
    area: Rect,
    search: &SearchInputState,
    available_width: u16,
    available_height: u16,
) {
    let width = MAX_MODAL_WIDTH.min(available_width);
    let height = 8u16.min(available_height);
    let popup = centered_rect(area, width, height);
    frame.render_widget(Clear, popup);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::Cyan))
        .padding(Padding::symmetric(1, 1))
        .title(Line::from(Span::styled(
            " Manual Search ",
            Style::default().add_modifier(Modifier::BOLD),
        )))
        .title_bottom(
            Line::from(Span::styled(
                " Tab field  Enter search  Esc cancel ",
                Style::default().add_modifier(Modifier::DIM),
            ))
            .alignment(Alignment::Right),
        );

    let inner = block.inner(popup);
    frame.render_widget(block, popup);

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
    );
    render_input_row(
        frame,
        rows[2],
        "Title ",
        &search.title,
        search.active_field == SearchField::Title,
    );
    frame.render_widget(
        Paragraph::new(Span::styled(
            "Type query text directly; global hotkeys are locked.",
            Style::default().add_modifier(Modifier::DIM),
        )),
        rows[3],
    );
}

fn render_input_row(frame: &mut Frame, area: Rect, label: &str, value: &str, active: bool) {
    let marker = if active { "> " } else { "  " };
    let cursor = if active { "_" } else { "" };
    let label_style = if active {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let max_val_len = usize::from(area.width).saturating_sub(marker.len() + label.len() + 3);
    let visible = tail_chars(value, max_val_len);
    let line = Line::from(vec![
        Span::styled(marker, label_style),
        Span::styled(format!("{label}: "), label_style),
        Span::raw(visible),
        Span::styled(cursor, Style::default().fg(Color::Cyan)),
    ]);
    frame.render_widget(Paragraph::new(line), area);
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

fn truncate_chars(text: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    let count = text.chars().count();
    if count <= max_chars {
        return text.to_string();
    }
    if max_chars <= 3 {
        return text.chars().take(max_chars).collect();
    }
    let mut out: String = text.chars().take(max_chars - 3).collect();
    out.push_str("...");
    out
}

fn tail_chars(text: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    let count = text.chars().count();
    if count <= max_chars {
        return text.to_string();
    }
    text.chars().skip(count - max_chars).collect()
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
    fn render_skips_tiny_areas_without_panic() {
        let backend = TestBackend::new(10, 5);
        let mut terminal = Terminal::new(backend).unwrap();
        let menu = ContextMenuState::actions("Artist - Title".to_string(), None, None);

        terminal
            .draw(|frame| {
                render(frame, frame.area(), &menu);
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

        terminal
            .draw(|frame| {
                render(frame, frame.area(), &menu);
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
}

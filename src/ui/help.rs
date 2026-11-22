use ratatui::{
    Frame,
    layout::{Alignment, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Padding, Paragraph, Wrap},
};

use crate::app::keys::global_help_rows;

use super::UiState;

const MIN_MODAL_WIDTH: u16 = 56;
const MAX_MODAL_WIDTH: u16 = 80;
const MIN_KEY_COLUMN_WIDTH: usize = 10;
pub const MIN_AREA_WIDTH: u16 = 20;
pub const MIN_AREA_HEIGHT: u16 = 8;

struct HelpLayout {
    width: u16,
    height: u16,
    total_rows: usize,
}

fn layout(area: Rect) -> Option<HelpLayout> {
    let available_width = area.width.saturating_sub(2);
    let available_height = area.height.saturating_sub(2);
    if available_width < MIN_AREA_WIDTH - 2 || available_height < MIN_AREA_HEIGHT - 2 {
        return None;
    }

    let rows = global_help_rows();
    let key_width = rows
        .iter()
        .map(|(keys, _)| keys.len())
        .max()
        .unwrap_or(0)
        .max(MIN_KEY_COLUMN_WIDTH);
    let longest_row = rows
        .iter()
        .map(|(_keys, description)| key_width + 2 + description.len())
        .max()
        .unwrap_or(0);

    let width = modal_width(area.width, longest_row).min(available_width);

    // Borders (2)
    let content_width = width.saturating_sub(6);
    let mut wrapped_rows = 0usize;
    for (keys, description) in &rows {
        let keys = format!("{keys:<key_width$}");
        wrapped_rows = wrapped_rows.saturating_add(
            crate::core::text::wrap_text(&format!("{keys}  {description}"), content_width)
                .len()
                .max(1),
        );
    }
    let desired_height = u16::try_from(wrapped_rows.saturating_add(4)).unwrap_or(u16::MAX);
    let height = desired_height.min(available_height);

    Some(HelpLayout {
        width,
        height,
        total_rows: wrapped_rows,
    })
}

pub(crate) fn max_scroll(viewport_width: u16, viewport_height: u16) -> usize {
    let Some(layout) = layout(Rect::new(0, 0, viewport_width, viewport_height)) else {
        return 0;
    };
    let visible = usize::from(layout.height.saturating_sub(4));
    layout.total_rows.saturating_sub(visible)
}

pub fn render(frame: &mut Frame, area: Rect, state: &UiState) {
    let Some(layout) = layout(area) else {
        return;
    };

    let visible_rows = usize::from(layout.height.saturating_sub(4));
    let max_scroll = layout.total_rows.saturating_sub(visible_rows);
    let scroll = state.help_scroll.min(max_scroll);
    let scrollable = max_scroll > 0;

    let accent = state.style.accent.unwrap_or(Color::Cyan);
    let muted = state
        .style
        .inactive_fg
        .or(state.style.active_fg)
        .unwrap_or(Color::Gray);
    let content_style = if let Some(surface) = state.style.surface {
        Style::default().bg(surface)
    } else {
        Style::default()
    };

    let popup = centered_rect(area, layout.width, layout.height);
    frame.render_widget(Clear, popup);

    let separator = if state.terminal_unicode {
        " · "
    } else {
        " | "
    };
    let hint = if scrollable {
        let end = (scroll + visible_rows).min(layout.total_rows);
        format!(
            " {}/{}{}Up/Down scroll{}? / Esc close ",
            end, layout.total_rows, separator, separator,
        )
    } else {
        " ? / Esc close ".to_string()
    };

    let block = Block::default()
        .style(content_style)
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(accent))
        .padding(Padding::symmetric(2, 1))
        .title(Line::from(Span::styled(
            " Keybindings ",
            Style::default().fg(accent).add_modifier(Modifier::BOLD),
        )))
        .title_bottom(
            Line::from(Span::styled(
                hint,
                Style::default().fg(muted).add_modifier(Modifier::DIM),
            ))
            .alignment(Alignment::Right),
        );

    let rows = global_help_rows();
    let key_width = rows
        .iter()
        .map(|(keys, _)| keys.len())
        .max()
        .unwrap_or(0)
        .max(MIN_KEY_COLUMN_WIDTH);
    let lines: Vec<Line<'static>> = rows
        .into_iter()
        .map(|(keys, description)| {
            let keys = format!("{keys:<key_width$}");
            Line::from(vec![
                Span::styled(
                    keys,
                    Style::default().fg(accent).add_modifier(Modifier::BOLD),
                ),
                Span::raw("  "),
                Span::raw(description),
            ])
        })
        .collect();

    frame.render_widget(
        Paragraph::new(lines)
            .style(content_style)
            .block(block)
            .wrap(Wrap { trim: false })
            .scroll((u16::try_from(scroll).unwrap_or(u16::MAX), 0)),
        popup,
    );
}

fn modal_width(area_width: u16, longest_row: usize) -> u16 {
    let available = area_width.saturating_sub(2);
    let preferred = area_width.saturating_mul(3) / 4;
    let needed = u16::try_from(longest_row + 6).unwrap_or(MAX_MODAL_WIDTH);
    preferred
        .max(needed)
        .clamp(MIN_MODAL_WIDTH, MAX_MODAL_WIDTH)
        .min(available)
}

pub(crate) fn centered_rect(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    let x = area.x.saturating_add(area.width.saturating_sub(width) / 2);
    let y = area
        .y
        .saturating_add(area.height.saturating_sub(height) / 2);
    Rect {
        x,
        y,
        width,
        height,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::style::ResolvedStyle;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::style::Color;

    #[test]
    fn modal_width_grows_for_long_descriptions() {
        assert_eq!(modal_width(80, 40), 60);
        assert_eq!(modal_width(80, 54), 60);
        assert_eq!(modal_width(80, 60), 66);
        assert_eq!(modal_width(80, 200), 78);
        assert_eq!(modal_width(50, 60), 48);
    }

    #[test]
    fn centered_rect_clamps_within_bounds() {
        let area = Rect::new(0, 0, 80, 24);
        let popup = centered_rect(area, 50, 15);
        assert_eq!(popup.width, 50);
        assert_eq!(popup.height, 15);
        assert_eq!(popup.x, 15);
        assert_eq!(popup.y, 4);

        let clamped = centered_rect(area, 100, 30);
        assert_eq!(clamped.width, 80);
        assert_eq!(clamped.height, 24);
        assert_eq!(clamped.x, 0);
        assert_eq!(clamped.y, 0);
    }

    #[test]
    fn help_uses_profile_surface_and_accent() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let surface = Color::Rgb(10, 20, 30);
        let accent = Color::Rgb(0, 200, 200);
        let mut state = UiState::new();
        state.style = ResolvedStyle {
            surface: Some(surface),
            accent: Some(accent),
            ..ResolvedStyle::default()
        };

        terminal
            .draw(|frame| render(frame, frame.area(), &state))
            .unwrap();

        let buffer = terminal.backend().buffer();
        let mut saw_surface = false;
        let mut saw_accent = false;
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                let cell = buffer.cell((x, y)).unwrap();
                saw_surface |= cell.bg == surface;
                saw_accent |= cell.fg == accent;
            }
        }
        assert!(saw_surface);
        assert!(saw_accent);
    }

    #[test]
    fn render_skips_tiny_areas_without_panic() {
        let backend = TestBackend::new(10, 5);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                render(frame, frame.area(), &UiState::new());
            })
            .unwrap();

        // Screen buffer should remain empty (all spaces)
        let buffer = terminal.backend().buffer();
        for y in 0..5 {
            for x in 0..10 {
                assert_eq!(buffer.cell((x, y)).unwrap().symbol(), " ");
            }
        }
    }

    #[test]
    fn help_scrolls_to_the_last_binding() {
        let area = Rect::new(0, 0, 80, 24);
        let layout = layout(area).expect("help fits 80x24");
        let visible = usize::from(layout.height.saturating_sub(4));
        if layout.total_rows <= visible {
            panic!("expected the help overlay to overflow at 80x24");
        }

        let mut state = UiState::new();
        for _ in 0..layout.total_rows {
            state.help_scroll_down();
        }
        assert_eq!(state.help_scroll, layout.total_rows - visible);

        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render(frame, frame.area(), &state))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let text: String = (0..buffer.area.height)
            .flat_map(|y| (0..buffer.area.width).map(move |x| (x, y)))
            .filter_map(|(x, y)| {
                let cell = buffer.cell((x, y)).unwrap();
                if cell.symbol() == " " {
                    None
                } else {
                    Some(cell.symbol())
                }
            })
            .collect();
        assert!(text.contains("unsynced"), "last binding visible: {text}");
    }
}

use ratatui::{
    Frame,
    layout::{Alignment, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Padding, Paragraph, Wrap},
};

use crate::app::keys::global_help_rows;

const MAX_MODAL_WIDTH: u16 = 56;
const MIN_KEY_COLUMN_WIDTH: usize = 10;
pub const MIN_AREA_WIDTH: u16 = 20;
pub const MIN_AREA_HEIGHT: u16 = 8;

pub fn render(frame: &mut Frame, area: Rect) {
    let available_width = area.width.saturating_sub(2);
    let available_height = area.height.saturating_sub(2);
    if available_width < MIN_AREA_WIDTH - 2 || available_height < MIN_AREA_HEIGHT - 2 {
        return;
    }

    let rows = global_help_rows();
    let key_width = rows
        .iter()
        .map(|(keys, _)| keys.len())
        .max()
        .unwrap_or(0)
        .max(MIN_KEY_COLUMN_WIDTH);
    let content_width = rows
        .iter()
        .map(|(_, desc)| key_width + 2 + desc.len())
        .max()
        .unwrap_or(24);

    // Borders (2) + horizontal padding (2)
    let desired_width = u16::try_from(content_width + 4)
        .unwrap_or(MAX_MODAL_WIDTH)
        .min(MAX_MODAL_WIDTH);
    let width = desired_width.min(available_width);

    // Borders (2) + vertical padding (2)
    let desired_height = u16::try_from(rows.len() + 4).unwrap_or(u16::MAX);
    let height = desired_height.min(available_height);

    let popup = centered_rect(area, width, height);
    frame.render_widget(Clear, popup);

    let lines: Vec<Line<'static>> = rows
        .into_iter()
        .map(|(keys, description)| {
            Line::from(vec![
                Span::styled(
                    format!("{keys:<key_width$}"),
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw("  "),
                Span::raw(description),
            ])
        })
        .collect();

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::Cyan))
        .padding(Padding::symmetric(1, 1))
        .title(Line::from(Span::styled(
            " Keybindings ",
            Style::default().add_modifier(Modifier::BOLD),
        )))
        .title_bottom(
            Line::from(Span::styled(
                " ? / Esc close ",
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
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

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
    fn render_skips_tiny_areas_without_panic() {
        let backend = TestBackend::new(10, 5);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                render(frame, frame.area());
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
}

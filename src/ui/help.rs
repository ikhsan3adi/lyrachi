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

pub fn render(frame: &mut Frame, area: Rect) {
    if area.width < 4 || area.height < 3 {
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
    let width = desired_width.min(area.width.saturating_sub(2)).max(4);

    // Borders (2) + vertical padding (2)
    let desired_height = u16::try_from(rows.len() + 4).unwrap_or(u16::MAX);
    let height = desired_height.min(area.height.saturating_sub(2)).max(3);

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

fn centered_rect(area: Rect, width: u16, height: u16) -> Rect {
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

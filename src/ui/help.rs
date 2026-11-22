use ratatui::{
    Frame,
    layout::{Alignment, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Padding, Paragraph, Wrap},
};

use crate::app::keys::global_help_rows;
use crate::style::ResolvedStyle;

const MIN_MODAL_WIDTH: u16 = 56;
const MAX_MODAL_WIDTH: u16 = 80;
const MIN_KEY_COLUMN_WIDTH: usize = 10;
pub const MIN_AREA_WIDTH: u16 = 20;
pub const MIN_AREA_HEIGHT: u16 = 8;

pub fn render(frame: &mut Frame, area: Rect, style: &ResolvedStyle) {
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

    let width = modal_width(area.width).min(available_width);

    let accent = style.accent.unwrap_or(Color::Cyan);
    let content_style = if let Some(surface) = style.surface {
        Style::default().bg(surface)
    } else {
        Style::default()
    };

    // Borders (2)
    let content_width = width.saturating_sub(6);
    let mut wrapped_rows = 0usize;
    let lines: Vec<Line<'static>> = rows
        .into_iter()
        .map(|(keys, description)| {
            let keys = format!("{keys:<key_width$}");
            wrapped_rows = wrapped_rows.saturating_add(
                crate::core::text::wrap_text(&format!("{keys}  {description}"), content_width)
                    .len()
                    .max(1),
            );
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
    let desired_height = u16::try_from(wrapped_rows.saturating_add(2)).unwrap_or(u16::MAX);
    let height = desired_height.min(available_height);

    let popup = centered_rect(area, width, height);
    frame.render_widget(Clear, popup);

    let block = Block::default()
        .style(content_style)
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(accent))
        .padding(Padding::horizontal(2))
        .title(Line::from(Span::styled(
            " Keybindings ",
            Style::default().fg(accent).add_modifier(Modifier::BOLD),
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
            .style(content_style)
            .block(block)
            .wrap(Wrap { trim: false }),
        popup,
    );
}

fn modal_width(area_width: u16) -> u16 {
    let available = area_width.saturating_sub(2);
    let preferred = area_width.saturating_mul(3) / 4;
    preferred
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
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::style::Color;

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
        let style = ResolvedStyle {
            surface: Some(surface),
            accent: Some(accent),
            ..ResolvedStyle::default()
        };

        terminal
            .draw(|frame| render(frame, frame.area(), &style))
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
                render(frame, frame.area(), &ResolvedStyle::default());
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

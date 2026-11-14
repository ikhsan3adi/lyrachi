use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Layout},
    style::{Color, Modifier, Style},
    text::Line,
    widgets::Paragraph,
};

/// M0 placeholder screen. The lyric renderer replaces this in M5.
pub fn scaffold(frame: &mut Frame) {
    let area = frame.area();
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas(area);

    frame.render_widget(
        Paragraph::new("Lyrachi")
            .style(
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )
            .alignment(Alignment::Center),
        header,
    );

    frame.render_widget(
        Paragraph::new(vec![
            Line::from("M0 scaffold ready"),
            Line::from("MPRIS wiring arrives in M3"),
        ])
        .alignment(Alignment::Center),
        body,
    );

    frame.render_widget(
        Paragraph::new("q quit")
            .style(Style::default().fg(Color::DarkGray))
            .alignment(Alignment::Center),
        footer,
    );
}

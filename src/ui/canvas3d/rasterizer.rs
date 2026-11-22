use ratatui::style::Style;
use ratatui::text::{Line, Span};

/// 2x4 sub-pixel canvas per terminal cell, rendered as Braille characters.
#[derive(Debug)]
pub struct BrailleCanvas {
    width: u16,
    height: u16,
    dots: Vec<u8>,
    styles: Vec<Style>,
}

const BRAILLE_BASE: u32 = 0x2800;

fn dot_bit(sub_x: usize, sub_y: usize) -> u8 {
    match (sub_x, sub_y) {
        (0, 0) => 0x01,
        (1, 0) => 0x08,
        (0, 1) => 0x02,
        (1, 1) => 0x10,
        (0, 2) => 0x04,
        (1, 2) => 0x20,
        (0, 3) => 0x40,
        (1, 3) => 0x80,
        _ => 0,
    }
}

impl Default for BrailleCanvas {
    fn default() -> Self {
        Self::new(1, 1)
    }
}

impl BrailleCanvas {
    pub fn new(width: u16, height: u16) -> Self {
        let mut canvas = Self {
            width: 1,
            height: 1,
            dots: Vec::new(),
            styles: Vec::new(),
        };
        canvas.resize(width, height);
        canvas
    }

    pub fn resize(&mut self, width: u16, height: u16) {
        let len = usize::from(width) * usize::from(height);
        if self.width != width || self.height != height || self.dots.len() != len {
            self.width = width;
            self.height = height;
            self.dots.clear();
            self.dots.resize(len, 0);
            self.styles.clear();
            self.styles.resize(len, Style::default());
        } else {
            self.dots.iter_mut().for_each(|dots| *dots = 0);
        }
    }

    pub fn has_dots(&self) -> bool {
        self.dots.iter().any(|dots| *dots != 0)
    }

    /// Plots a virtual pixel, overwriting the cell style (newest wins).
    /// Returns false when the pixel falls outside the canvas.
    pub fn plot(&mut self, x: f32, y: f32, style: Style) -> bool {
        if !(x >= 0.0 && y >= 0.0) {
            return false;
        }
        let px = x as u16;
        let py = y as u16;
        if px >= self.width * 2 || py >= self.height * 4 {
            return false;
        }
        let cell_x = px / 2;
        let cell_y = py / 4;
        let bit = dot_bit(usize::from(px % 2), usize::from(py % 4));
        let index = Self::index(self.width, cell_x, cell_y);
        self.dots[index] |= bit;
        self.styles[index] = style;
        true
    }

    /// Renders the canvas as styled lines; empty cells become spaces.
    pub fn to_lines(&self) -> Vec<Line<'static>> {
        let mut lines = Vec::with_capacity(usize::from(self.height));
        for y in 0..self.height {
            let mut spans: Vec<Span<'static>> = Vec::new();
            let mut text = String::new();
            let mut style = Style::default();
            let mut started = false;
            for x in 0..self.width {
                let index = Self::index(self.width, x, y);
                let dots = self.dots[index];
                if dots == 0 {
                    if started && style != Style::default() {
                        flush_span(&mut spans, &mut text, style, &mut started);
                        style = Style::default();
                    }
                    text.push(' ');
                    started = true;
                    continue;
                }
                let cell_style = self.styles[index];
                if !started {
                    style = cell_style;
                    started = true;
                } else if cell_style != style {
                    flush_span(&mut spans, &mut text, style, &mut started);
                    style = cell_style;
                }
                text.push(char::from_u32(BRAILLE_BASE + u32::from(dots)).unwrap_or('\u{2800}'));
            }
            flush_span(&mut spans, &mut text, style, &mut started);
            if spans.last().is_some_and(|span| {
                span.style == Style::default() && span.content.chars().all(|c| c == ' ')
            }) {
                spans.pop();
            }
            lines.push(Line::from(spans));
        }
        lines
    }

    fn index(width: u16, x: u16, y: u16) -> usize {
        usize::from(y) * usize::from(width) + usize::from(x)
    }
}

fn flush_span(spans: &mut Vec<Span<'static>>, text: &mut String, style: Style, started: &mut bool) {
    if !*started || text.is_empty() {
        text.clear();
        return;
    }
    spans.push(Span::styled(std::mem::take(text), style));
}

#[cfg(test)]
impl BrailleCanvas {
    fn width(&self) -> u16 {
        self.width
    }

    fn height(&self) -> u16 {
        self.height
    }

    fn dots_at(&self, x: u16, y: u16) -> u8 {
        if x >= self.width || y >= self.height {
            return 0;
        }
        self.dots[Self::index(self.width, x, y)]
    }
}

#[cfg(test)]
mod tests {
    use ratatui::style::{Color, Modifier};

    use super::*;

    #[test]
    fn plot_sets_the_expected_braille_bit() {
        let mut canvas = BrailleCanvas::new(4, 2);
        assert!(canvas.plot(1.0, 5.0, Style::default()));
        // virtual (1, 5): cell (0, 1), sub (1, 1) -> bit 0x10
        assert_eq!(canvas.dots_at(0, 1), 0x10);
        assert_eq!(canvas.dots_at(0, 0), 0);
    }

    #[test]
    fn plot_rejects_out_of_bounds_pixels() {
        let mut canvas = BrailleCanvas::new(2, 2);
        assert!(!canvas.plot(-1.0, 0.0, Style::default()));
        assert!(!canvas.plot(0.0, -1.0, Style::default()));
        assert!(!canvas.plot(4.0, 0.0, Style::default()));
        assert!(!canvas.plot(0.0, 8.0, Style::default()));
    }

    #[test]
    fn braille_char_encodes_all_eight_dots() {
        let mut canvas = BrailleCanvas::new(1, 1);
        for py in 0..4u16 {
            for px in 0..2u16 {
                assert!(canvas.plot(f32::from(px), f32::from(py), Style::default()));
            }
        }
        let lines = canvas.to_lines();
        let symbol = lines[0].spans[0].content.as_ref();
        assert_eq!(symbol, "\u{28FF}");
    }

    #[test]
    fn empty_cells_render_as_spaces_and_group_by_style() {
        let mut canvas = BrailleCanvas::new(6, 1);
        let red = Style::default().fg(Color::Red);
        let blue = Style::default().fg(Color::Blue);
        assert!(canvas.plot(0.0, 0.0, red));
        assert!(canvas.plot(2.0, 0.0, red));
        assert!(canvas.plot(8.0, 0.0, blue));
        let lines = canvas.to_lines();
        assert_eq!(lines.len(), 1);
        let spans = &lines[0].spans;
        assert_eq!(spans.len(), 3);
        assert_eq!(spans[0].content.as_ref(), "\u{2801}\u{2801}");
        assert_eq!(spans[0].style, red);
        assert_eq!(spans[1].content.as_ref(), "  ");
        assert_eq!(spans[2].content.as_ref(), "\u{2801}");
        assert_eq!(spans[2].style, blue);
    }

    #[test]
    fn resize_keeps_dimensions_and_clears_dots() {
        let mut canvas = BrailleCanvas::new(4, 2);
        assert!(canvas.plot(0.0, 0.0, Style::default()));
        canvas.resize(4, 2);
        assert_eq!(canvas.dots_at(0, 0), 0);
        canvas.resize(8, 3);
        assert_eq!(canvas.width(), 8);
        assert_eq!(canvas.height(), 3);
        assert_eq!(canvas.dots_at(7, 2), 0);
    }

    #[test]
    fn later_plot_overwrites_cell_style() {
        let mut canvas = BrailleCanvas::new(1, 1);
        let base = Style::default().fg(Color::Red);
        let over = Style::default()
            .fg(Color::Blue)
            .add_modifier(Modifier::BOLD);
        assert!(canvas.plot(0.0, 0.0, base));
        assert!(canvas.plot(1.0, 3.0, over));
        assert_eq!(canvas.styles[0], over);
        assert_eq!(canvas.dots_at(0, 0), 0x01 | 0x80);
    }
}

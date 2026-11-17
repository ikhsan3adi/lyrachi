use ratatui::{
    layout::Alignment,
    style::Style,
    text::{Line, Span},
};

use crate::compiler::timeline::ActiveStageCursor;
use crate::core::model::WordToken;

use super::BlockLayout;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BlockCellState {
    Active,
    Sung,
    Unsung,
    Inactive,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct BlockStyles {
    pub active: Style,
    pub sung: Style,
    pub unsung: Style,
    pub inactive: Style,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct BlockRenderContext<'a> {
    pub words: &'a [WordToken],
    pub cursor: ActiveStageCursor,
    pub effective_ms: u64,
    pub active: bool,
    pub word_highlight: bool,
    pub styles: BlockStyles,
    pub alignment: Alignment,
}

pub(crate) fn render_lines(
    layout: &BlockLayout,
    context: BlockRenderContext<'_>,
) -> Vec<Line<'static>> {
    if layout.is_empty() {
        return Vec::new();
    }

    let states = cell_states(layout, context);
    let rows_per_line = usize::from(layout.rows_per_line().max(1));
    let mut lines = Vec::with_capacity(usize::from(layout.height));

    for (visual, line_states) in layout.lines.iter().zip(states) {
        for row_index in 0..rows_per_line {
            let mut spans = Vec::new();
            for (cell, state) in visual.cells.iter().zip(line_states.iter()) {
                let text = cell.rows.get(row_index).map_or(" ", String::as_str);
                push_span(&mut spans, text, state.style(context.styles));
            }
            lines.push(Line::from(spans).alignment(context.alignment));
        }
    }

    lines
}

impl BlockCellState {
    fn style(self, styles: BlockStyles) -> Style {
        match self {
            Self::Active => styles.active,
            Self::Sung => styles.sung,
            Self::Unsung => styles.unsung,
            Self::Inactive => styles.inactive,
        }
    }
}

fn cell_states(layout: &BlockLayout, context: BlockRenderContext<'_>) -> Vec<Vec<BlockCellState>> {
    let BlockRenderContext {
        words,
        cursor,
        effective_ms,
        active,
        word_highlight,
        ..
    } = context;
    let mut states = layout
        .lines
        .iter()
        .map(|visual| vec![BlockCellState::Inactive; visual.cells.len()])
        .collect::<Vec<_>>();

    if !active {
        return states;
    }

    if !word_highlight || words.is_empty() {
        for line_states in &mut states {
            line_states.fill(BlockCellState::Active);
        }
        return states;
    }

    let active_word_idx = cursor.active_word_idx.filter(|index| *index < words.len());
    let mut token_counts = vec![0usize; words.len()];
    let mut word_idx = 0usize;
    for visual in &layout.lines {
        for cell in &visual.cells {
            if let Some(index) = token_index(words, &mut word_idx, cell.source_byte_range) {
                token_counts[index] = token_counts[index].saturating_add(1);
            }
        }
    }

    let reveal_count = active_word_idx.map_or(0, |index| {
        let count = token_counts[index];
        if count == 0 {
            0
        } else {
            ((cursor.progress * count as f32).ceil() as usize).clamp(1, count)
        }
    });
    let mut token_seen = vec![0usize; words.len()];
    word_idx = 0;

    for (visual, line_states) in layout.lines.iter().zip(states.iter_mut()) {
        for (cell, state) in visual.cells.iter().zip(line_states.iter_mut()) {
            *state = match token_index(words, &mut word_idx, cell.source_byte_range) {
                Some(index) if Some(index) == active_word_idx => {
                    let ordinal = token_seen[index];
                    token_seen[index] = token_seen[index].saturating_add(1);
                    if ordinal < reveal_count {
                        BlockCellState::Active
                    } else {
                        BlockCellState::Unsung
                    }
                }
                Some(index)
                    if active_word_idx.is_some_and(|active| index < active)
                        || effective_ms > words[index].end_ms =>
                {
                    BlockCellState::Sung
                }
                Some(_) => BlockCellState::Unsung,
                None => gap_state(words, word_idx, effective_ms),
            };
        }
    }

    states
}

fn token_index(words: &[WordToken], word_idx: &mut usize, range: (usize, usize)) -> Option<usize> {
    while *word_idx < words.len() && words[*word_idx].byte_range.1 <= range.0 {
        *word_idx += 1;
    }
    words
        .get(*word_idx)
        .filter(|word| word.byte_range.0 < range.1)
        .map(|_| *word_idx)
}

fn gap_state(words: &[WordToken], next_word_idx: usize, effective_ms: u64) -> BlockCellState {
    if let Some(next) = words.get(next_word_idx) {
        if effective_ms >= next.start_ms {
            BlockCellState::Sung
        } else {
            BlockCellState::Unsung
        }
    } else if words.last().is_some_and(|last| effective_ms > last.end_ms) {
        BlockCellState::Sung
    } else {
        BlockCellState::Unsung
    }
}

fn push_span(spans: &mut Vec<Span<'static>>, text: &str, style: Style) {
    if let Some(last) = spans.last_mut()
        && last.style == style
    {
        last.content.to_mut().push_str(text);
        return;
    }
    spans.push(Span::styled(text.to_string(), style));
}

#[cfg(test)]
mod tests {
    use ratatui::{style::Color, text::Line};

    use super::super::{BlockFontSize, layout_text};
    use super::*;

    fn styles() -> BlockStyles {
        BlockStyles {
            active: Style::default().fg(Color::Red),
            sung: Style::default().fg(Color::Green),
            unsung: Style::default().fg(Color::Blue),
            inactive: Style::default().fg(Color::Gray),
        }
    }

    fn syllables() -> Vec<WordToken> {
        vec![
            WordToken {
                byte_range: (0, 3),
                start_ms: 1_000,
                end_ms: 1_400,
                display_width: 3,
            },
            WordToken {
                byte_range: (3, 5),
                start_ms: 1_400,
                end_ms: 1_800,
                display_width: 2,
            },
        ]
    }

    fn text_with_style(lines: &[Line<'static>], style: Style) -> String {
        let mut text = String::new();
        for line in lines {
            for span in &line.spans {
                if span.style == style {
                    text.push_str(&span.content);
                }
            }
        }
        text
    }

    #[test]
    fn reveals_syllable_cells_across_wrapped_visual_lines() {
        let layout = layout_text("Hello", 4, 20, 1);
        assert_eq!(layout.font_size, BlockFontSize::Plain);
        assert_eq!(layout.lines.len(), 2);

        let lines = render_lines(
            &layout,
            BlockRenderContext {
                words: &syllables(),
                cursor: ActiveStageCursor {
                    active_line_idx: Some(0),
                    active_word_idx: Some(1),
                    progress: 0.5,
                    waiting: None,
                },
                effective_ms: 1_600,
                active: true,
                word_highlight: true,
                styles: styles(),
                alignment: Alignment::Left,
            },
        );

        assert_eq!(text_with_style(&lines, styles().active), "l");
        assert_eq!(text_with_style(&lines, styles().sung), "Hel");
        assert_eq!(text_with_style(&lines, styles().unsung), "o");
    }

    #[test]
    fn word_highlight_off_uses_one_active_style() {
        let layout = layout_text("Hello", 4, 20, 1);
        let lines = render_lines(
            &layout,
            BlockRenderContext {
                words: &syllables(),
                cursor: ActiveStageCursor {
                    active_line_idx: Some(0),
                    active_word_idx: Some(1),
                    progress: 0.5,
                    waiting: None,
                },
                effective_ms: 1_600,
                active: true,
                word_highlight: false,
                styles: styles(),
                alignment: Alignment::Left,
            },
        );

        assert_eq!(text_with_style(&lines, styles().active), "Hello");
        assert!(text_with_style(&lines, styles().sung).is_empty());
        assert!(text_with_style(&lines, styles().unsung).is_empty());
    }

    #[test]
    fn inactive_lines_never_receive_word_styles() {
        let layout = layout_text("Hello", 4, 20, 1);
        let lines = render_lines(
            &layout,
            BlockRenderContext {
                words: &syllables(),
                cursor: ActiveStageCursor {
                    active_line_idx: Some(0),
                    active_word_idx: Some(0),
                    progress: 0.5,
                    waiting: None,
                },
                effective_ms: 1_200,
                active: false,
                word_highlight: true,
                styles: styles(),
                alignment: Alignment::Left,
            },
        );

        assert_eq!(text_with_style(&lines, styles().inactive), "Hello");
        assert!(text_with_style(&lines, styles().active).is_empty());
    }
}

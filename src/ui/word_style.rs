use ratatui::style::{Modifier, Style};
use unicode_segmentation::UnicodeSegmentation;

use crate::compiler::timeline::{ActiveStageCursor, CompiledLine};
use crate::core::model::LineRole;
use crate::style::{HighlightMode, ResolvedStyle};
use crate::ui::render::{WordState, word_state_style};

#[allow(clippy::too_many_arguments)]
pub(crate) fn style_for_byte(
    compiled: &CompiledLine,
    line_text: &str,
    byte_range: (usize, usize),
    cursor: &ActiveStageCursor,
    active: bool,
    holding: bool,
    effective_ms: u64,
    depth: f64,
    word_highlight: bool,
    style: &ResolvedStyle,
) -> Style {
    let style = if !active {
        style.line_style(compiled.role, 1, false)
    } else if holding {
        held_style(compiled, byte_range, word_highlight, style)
    } else if !word_highlight || compiled.words.is_empty() {
        active_line_style(style, compiled.role)
    } else {
        active_word_style(compiled, line_text, byte_range, cursor, effective_ms, style)
    };

    if depth < 0.45 {
        style.add_modifier(Modifier::DIM)
    } else {
        style
    }
}

fn active_line_style(style: &ResolvedStyle, role: LineRole) -> Style {
    match role {
        LineRole::Primary | LineRole::DuetSecondary => {
            word_state_style(style, role, WordState::Active)
        }
        LineRole::BackgroundVocal | LineRole::InstrumentalBreak => style.line_style(role, 0, true),
    }
}

fn held_style(
    compiled: &CompiledLine,
    byte_range: (usize, usize),
    word_highlight: bool,
    style: &ResolvedStyle,
) -> Style {
    if !word_highlight || compiled.words.is_empty() {
        return style.line_style(compiled.role, 0, true);
    }
    let last_word = compiled.words.len() - 1;
    match word_index_for_byte(compiled, byte_range.0) {
        Some(index) if index == last_word => {
            word_state_style(style, compiled.role, WordState::Active)
        }
        _ => word_state_style(style, compiled.role, WordState::Sung),
    }
}

fn active_word_style(
    compiled: &CompiledLine,
    line_text: &str,
    byte_range: (usize, usize),
    cursor: &ActiveStageCursor,
    effective_ms: u64,
    style: &ResolvedStyle,
) -> Style {
    let Some(word_idx) = word_index_for_byte(compiled, byte_range.0) else {
        return gap_style(compiled, byte_range, effective_ms, style);
    };
    let word = &compiled.words[word_idx];
    let state = if cursor.active_word_idx == Some(word_idx) {
        if style.highlight_mode == HighlightMode::Wipe && !style.reduced_motion {
            return wipe_word_style(compiled, line_text, byte_range, cursor, word_idx, style);
        }
        WordState::Active
    } else if cursor
        .active_word_idx
        .is_some_and(|active| word_idx < active)
        || effective_ms > word.end_ms
    {
        WordState::Sung
    } else {
        WordState::Unsung
    };
    word_state_style(style, compiled.role, state)
}

fn gap_style(
    compiled: &CompiledLine,
    byte_range: (usize, usize),
    effective_ms: u64,
    style: &ResolvedStyle,
) -> Style {
    let next_word = compiled
        .words
        .iter()
        .find(|word| word.byte_range.0 >= byte_range.0);
    let sung = match next_word {
        Some(word) => effective_ms >= word.start_ms,
        None => compiled
            .words
            .last()
            .is_some_and(|word| effective_ms > word.end_ms),
    };
    word_state_style(
        style,
        compiled.role,
        if sung {
            WordState::Sung
        } else {
            WordState::Unsung
        },
    )
}

fn wipe_word_style(
    compiled: &CompiledLine,
    line_text: &str,
    byte_range: (usize, usize),
    cursor: &ActiveStageCursor,
    word_idx: usize,
    style: &ResolvedStyle,
) -> Style {
    let word = &compiled.words[word_idx];
    let word_text = line_text
        .get(word.byte_range.0..word.byte_range.1)
        .unwrap_or("");
    let glyph_offset = byte_range
        .0
        .saturating_sub(word.byte_range.0)
        .min(word_text.len());
    let before = word_text.get(..glyph_offset).unwrap_or("");
    let ordinal = before.graphemes(true).count();
    let total = word_text.graphemes(true).count();
    if total == 0 {
        return word_state_style(style, compiled.role, WordState::Active);
    }

    let head = cursor.progress.clamp(0.0, 1.0) * total as f32;
    let head_index = head.floor() as usize;
    let trail = f32::from(style.highlight_trail);
    let distance = head - (ordinal as f32 + 0.5);
    let state = if ordinal == head_index {
        WordState::Active
    } else if distance < 0.0 {
        WordState::Unsung
    } else if trail > 0.0 && distance <= trail {
        return style.wipe_style(compiled.role, distance / trail);
    } else {
        WordState::Sung
    };
    word_state_style(style, compiled.role, state)
}

pub(crate) fn word_index_for_byte(compiled: &CompiledLine, byte: usize) -> Option<usize> {
    compiled
        .words
        .iter()
        .position(|word| word.byte_range.0 <= byte && byte < word.byte_range.1)
}

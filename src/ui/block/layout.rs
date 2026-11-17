use unicode_segmentation::UnicodeSegmentation;

use super::font::{
    BlockFontSize, GlyphBitmap, glyph_bitmap_for_cluster, placeholder_bitmap, space_bitmap,
};
use crate::core::text::{cannot_end_line, cannot_start_line, display_width_with_ambiguous, is_cjk};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Glyph {
    pub bitmap: GlyphBitmap,
    pub source_byte_range: (usize, usize),
    pub is_space: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockCell {
    pub rows: Vec<String>,
    pub width: u16,
    pub source_byte_range: (usize, usize),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisualLine {
    pub cells: Vec<BlockCell>,
    pub width: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockLayout {
    pub font_size: BlockFontSize,
    pub width: u16,
    pub height: u16,
    pub lines: Vec<VisualLine>,
    pub unsupported_clusters: usize,
    pub overflow: bool,
}

impl BlockLayout {
    pub fn new(text: &str, max_width: u16, max_height: u16, ambiguous_width: u8) -> Self {
        if max_width == 0 || max_height == 0 {
            return Self::empty();
        }
        let clusters = clusters(text);
        if clusters.is_empty() {
            return Self::empty();
        }
        let units = build_units(&clusters);
        if !units.iter().any(|unit| !unit.is_space) {
            return Self::empty();
        }

        for size in BlockFontSize::FALLBACKS {
            let (glyphs, unsupported, non_space) = glyphs_for(&clusters, size, ambiguous_width);
            let supported = non_space.saturating_sub(unsupported);
            if non_space > 0 && supported.saturating_mul(2) < non_space {
                continue;
            }
            if let Some(layout) =
                try_layout(&units, &glyphs, size, unsupported, max_width, max_height)
            {
                return layout;
            }
        }

        let (glyphs, _, _) = glyphs_for(&clusters, BlockFontSize::Plain, ambiguous_width);
        let (lines, overflow) = wrap_units(&units, &glyphs, true, max_width);
        build_layout(
            BlockFontSize::Plain,
            lines,
            &glyphs,
            max_width,
            max_height,
            0,
            overflow,
        )
    }

    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    pub fn rows_per_line(&self) -> u16 {
        self.font_size.rows()
    }

    fn empty() -> Self {
        Self {
            font_size: BlockFontSize::Plain,
            width: 0,
            height: 0,
            lines: Vec::new(),
            unsupported_clusters: 0,
            overflow: false,
        }
    }
}

pub fn layout_text(
    text: &str,
    max_width: u16,
    max_height: u16,
    ambiguous_width: u8,
) -> BlockLayout {
    BlockLayout::new(text, max_width, max_height, ambiguous_width)
}

#[derive(Debug, Clone)]
struct Cluster<'a> {
    text: &'a str,
    byte_range: (usize, usize),
    is_space: bool,
    is_cjk: bool,
    no_start: bool,
    no_end: bool,
}

#[derive(Debug, Clone)]
struct Unit {
    clusters: Vec<usize>,
    is_space: bool,
}

fn clusters(text: &str) -> Vec<Cluster<'_>> {
    text.grapheme_indices(true)
        .map(|(start, text)| {
            let first = text.chars().next().unwrap_or('\0');
            Cluster {
                text,
                byte_range: (start, start + text.len()),
                is_space: text.chars().all(char::is_whitespace),
                is_cjk: is_cjk(first),
                no_start: cannot_start_line(first),
                no_end: cannot_end_line(first),
            }
        })
        .collect()
}

fn build_units(clusters: &[Cluster<'_>]) -> Vec<Unit> {
    let mut units = Vec::new();
    let mut index = 0usize;

    while index < clusters.len() {
        if clusters[index].is_space {
            units.push(Unit {
                clusters: vec![index],
                is_space: true,
            });
            index += 1;
            continue;
        }

        let mut unit = vec![index];
        let mut next = index + 1;

        if clusters[index].no_end {
            while next < clusters.len() && !clusters[next].is_space {
                unit.push(next);
                let keep_joining = clusters[next].no_end;
                next += 1;
                if !keep_joining {
                    break;
                }
            }
        }

        while next < clusters.len() && !clusters[next].is_space && clusters[next].no_start {
            unit.push(next);
            next += 1;
        }

        if !clusters[index].is_cjk {
            while next < clusters.len() && !clusters[next].is_space && !clusters[next].is_cjk {
                unit.push(next);
                next += 1;
            }
        }

        units.push(Unit {
            clusters: unit,
            is_space: false,
        });
        index = next;
    }

    units
}

fn glyphs_for(
    clusters: &[Cluster<'_>],
    size: BlockFontSize,
    ambiguous_width: u8,
) -> (Vec<Glyph>, usize, usize) {
    let mut glyphs = Vec::with_capacity(clusters.len());
    let mut unsupported = 0usize;
    let mut non_space = 0usize;

    for cluster in clusters {
        if cluster.is_space {
            glyphs.push(Glyph {
                bitmap: space_bitmap(size),
                source_byte_range: cluster.byte_range,
                is_space: true,
            });
            continue;
        }

        non_space += 1;
        let glyph = if size == BlockFontSize::Plain {
            plain_glyph(cluster.text, cluster.byte_range, ambiguous_width)
        } else {
            match glyph_bitmap_for_cluster(cluster.text, size) {
                Some(bitmap) => Glyph {
                    bitmap,
                    source_byte_range: cluster.byte_range,
                    is_space: false,
                },
                None => {
                    unsupported += 1;
                    Glyph {
                        bitmap: placeholder_bitmap(size),
                        source_byte_range: cluster.byte_range,
                        is_space: false,
                    }
                }
            }
        };
        glyphs.push(glyph);
    }

    (glyphs, unsupported, non_space)
}

fn plain_glyph(text: &str, byte_range: (usize, usize), ambiguous_width: u8) -> Glyph {
    let width = display_width_with_ambiguous(text, ambiguous_width).max(1);
    Glyph {
        bitmap: GlyphBitmap::new(width, vec![text.to_string()]),
        source_byte_range: byte_range,
        is_space: false,
    }
}

fn try_layout(
    units: &[Unit],
    glyphs: &[Glyph],
    size: BlockFontSize,
    unsupported: usize,
    max_width: u16,
    max_height: u16,
) -> Option<BlockLayout> {
    for unit in units.iter().filter(|unit| !unit.is_space) {
        if unit_width(unit, glyphs) > max_width {
            return None;
        }
    }

    let (lines, overflow) = wrap_units(units, glyphs, false, max_width);
    if overflow {
        return None;
    }

    let height = u32::try_from(lines.len()).unwrap_or(u32::MAX) * u32::from(size.rows());
    if height > u32::from(max_height) {
        return None;
    }

    Some(build_layout(
        size,
        lines,
        glyphs,
        max_width,
        max_height,
        unsupported,
        false,
    ))
}

fn unit_width(unit: &Unit, glyphs: &[Glyph]) -> u16 {
    unit.clusters.iter().fold(0u16, |width, index| {
        width.saturating_add(glyphs[*index].bitmap.width)
    })
}

fn wrap_units(
    units: &[Unit],
    glyphs: &[Glyph],
    allow_split: bool,
    max_width: u16,
) -> (Vec<Vec<usize>>, bool) {
    let mut lines = Vec::new();
    let mut line = Vec::new();
    let mut line_width = 0u16;
    let mut pending_space = None;
    let mut overflow = false;

    for unit in units {
        if unit.is_space {
            if !line.is_empty() && pending_space.is_none() {
                pending_space = unit.clusters.first().copied();
            }
            continue;
        }

        let width = unit_width(unit, glyphs);
        let fits_with_pending = pending_space.is_none_or(|space| {
            line_width
                .saturating_add(glyphs[space].bitmap.width)
                .saturating_add(width)
                <= max_width
        });
        let fits_without_pending = line_width.saturating_add(width) <= max_width;
        let fits = if pending_space.is_some() {
            fits_with_pending
        } else {
            fits_without_pending
        };

        if !line.is_empty() && !fits {
            finalize_line(&mut lines, &mut line, &mut line_width);
            pending_space = None;
        }

        if line.is_empty() {
            pending_space = None;
        }

        if width <= max_width {
            if let Some(space) = pending_space.take() {
                line.push(space);
                line_width = line_width.saturating_add(glyphs[space].bitmap.width);
            }
            line.extend(unit.clusters.iter().copied());
            line_width = line_width.saturating_add(width);
        } else {
            if !allow_split {
                overflow = true;
            }
            for index in &unit.clusters {
                let glyph_width = glyphs[*index].bitmap.width;
                if glyph_width > max_width {
                    overflow = true;
                }
                if !line.is_empty() && line_width.saturating_add(glyph_width) > max_width {
                    finalize_line(&mut lines, &mut line, &mut line_width);
                }
                line.push(*index);
                line_width = line_width.saturating_add(glyph_width);
            }
        }
    }

    finalize_line(&mut lines, &mut line, &mut line_width);
    (lines, overflow)
}

fn finalize_line(lines: &mut Vec<Vec<usize>>, line: &mut Vec<usize>, width: &mut u16) {
    if !line.is_empty() {
        lines.push(std::mem::take(line));
        *width = 0;
    }
}

fn build_layout(
    size: BlockFontSize,
    line_indices: Vec<Vec<usize>>,
    glyphs: &[Glyph],
    max_width: u16,
    max_height: u16,
    unsupported: usize,
    mut overflow: bool,
) -> BlockLayout {
    let mut lines = Vec::with_capacity(line_indices.len());
    let mut layout_width = 0u16;

    for indices in line_indices {
        let mut cells = Vec::new();
        let mut width = 0u16;
        for index in indices {
            let glyph = &glyphs[index];
            if size == BlockFontSize::Plain {
                cells.push(BlockCell {
                    rows: glyph.bitmap.rows.clone(),
                    width: glyph.bitmap.width,
                    source_byte_range: glyph.source_byte_range,
                });
                width = width.saturating_add(glyph.bitmap.width);
            } else {
                for column in 0..usize::from(glyph.bitmap.width) {
                    let rows = glyph
                        .bitmap
                        .rows
                        .iter()
                        .map(|row| row.chars().nth(column).unwrap_or(' ').to_string())
                        .collect();
                    cells.push(BlockCell {
                        rows,
                        width: 1,
                        source_byte_range: glyph.source_byte_range,
                    });
                    width = width.saturating_add(1);
                }
            }
        }

        if width > max_width {
            overflow = true;
        }
        layout_width = layout_width.max(width);
        lines.push(VisualLine { cells, width });
    }

    let height = u32::try_from(lines.len()).unwrap_or(u32::MAX) * u32::from(size.rows());
    if height > u32::from(max_height) {
        overflow = true;
    }

    BlockLayout {
        font_size: size,
        width: layout_width,
        height: u16::try_from(height).unwrap_or(u16::MAX),
        lines,
        unsupported_clusters: unsupported,
        overflow,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rendered(layout: &BlockLayout) -> String {
        layout
            .lines
            .iter()
            .flat_map(|line| line.cells.iter())
            .flat_map(|cell| cell.rows.iter())
            .cloned()
            .collect()
    }

    fn assert_valid(layout: &BlockLayout, text: &str, max_width: u16, max_height: u16) {
        assert!(
            layout.width <= max_width,
            "{} > {}: {layout:?}",
            layout.width,
            max_width
        );
        assert!(
            layout.height <= max_height,
            "{} > {}: {layout:?}",
            layout.height,
            max_height
        );
        assert!(!layout.overflow, "layout overflowed: {layout:?}");
        assert_eq!(
            layout.height,
            u16::try_from(layout.lines.len()).unwrap() * layout.rows_per_line()
        );

        for line in &layout.lines {
            let cell_width = line
                .cells
                .iter()
                .map(|cell| cell.width)
                .fold(0u16, u16::saturating_add);
            assert_eq!(line.width, cell_width);
            assert!(line.width <= max_width);

            for cell in &line.cells {
                let (start, end) = cell.source_byte_range;
                assert!(start < end && end <= text.len());
                assert!(text.is_char_boundary(start));
                assert!(text.is_char_boundary(end));
                assert_eq!(cell.rows.len(), usize::from(layout.rows_per_line()));
                if layout.font_size != BlockFontSize::Plain {
                    for row in &cell.rows {
                        assert_eq!(row.chars().count(), usize::from(cell.width));
                    }
                }
            }
        }
    }

    #[test]
    fn zero_sized_viewport_returns_empty_layout() {
        for (width, height) in [(0, 24), (20, 0), (0, 0)] {
            let layout = BlockLayout::new("HELLO", width, height, 1);
            assert!(layout.is_empty());
            assert_eq!(layout.width, 0);
            assert_eq!(layout.height, 0);
        }
    }

    #[test]
    fn non_latin_and_emoji_layouts_stay_valid_at_supported_widths() {
        let text = "مرحبا สวัสดี 你好 👩🏽‍💻";
        for width in [20, 45, 80, 120] {
            let layout = BlockLayout::new(text, width, 24, 2);
            assert_valid(&layout, text, width, 24);
            assert!(!layout.lines.is_empty());
        }
    }

    #[test]
    fn lays_out_long_text_at_supported_widths_without_clipping() {
        let text = "The quick brown fox jumps over the lazy dog";
        for width in [20, 45, 80, 120] {
            let layout = BlockLayout::new(text, width, 24, 1);
            assert_valid(&layout, text, width, 24);
            assert!(!layout.lines.is_empty());
            assert!(layout.width > 0);
        }

        assert_eq!(
            BlockLayout::new(text, 20, 24, 1).font_size,
            BlockFontSize::Mini
        );
        assert_eq!(
            BlockLayout::new(text, 45, 24, 1).font_size,
            BlockFontSize::Compact
        );
        assert_eq!(
            BlockLayout::new(text, 120, 24, 1).font_size,
            BlockFontSize::Block
        );
    }

    #[test]
    fn falls_back_by_height_before_using_plain_text() {
        assert_eq!(
            BlockLayout::new("HELLO", 120, 3, 1).font_size,
            BlockFontSize::Compact
        );
        assert_eq!(
            BlockLayout::new("HELLO", 120, 2, 1).font_size,
            BlockFontSize::Mini
        );
        assert_eq!(
            BlockLayout::new("HELLO", 120, 1, 1).font_size,
            BlockFontSize::Plain
        );
    }

    #[test]
    fn uses_plain_for_majority_unsupported_text_without_losing_content() {
        let text = "你好，世界。こんにちは";
        let layout = BlockLayout::new(text, 20, 24, 2);
        assert_eq!(layout.font_size, BlockFontSize::Plain);
        assert_valid(&layout, text, 20, 24);
        assert!(rendered(&layout).contains('你'));
    }

    #[test]
    fn keeps_small_kana_and_opening_brackets_off_line_edges() {
        let text = "あああゃ「い";
        let layout = BlockLayout::new(text, 4, 24, 2);
        assert_eq!(layout.font_size, BlockFontSize::Plain);
        assert_valid(&layout, text, 4, 24);

        let lines: Vec<String> = layout
            .lines
            .iter()
            .map(|line| {
                line.cells
                    .iter()
                    .filter_map(|cell| cell.rows.first().cloned())
                    .collect()
            })
            .collect();
        assert!(
            !lines.iter().any(|line| line.starts_with('ゃ')),
            "{lines:?}"
        );
        assert!(!lines.iter().any(|line| line.ends_with('「')), "{lines:?}");
    }

    #[test]
    fn honors_ambiguous_width_in_plain_fallback() {
        let narrow = BlockLayout::new("あ··", 3, 24, 1);
        let wide = BlockLayout::new("あ··", 3, 24, 2);
        assert_eq!(narrow.font_size, BlockFontSize::Plain);
        assert_eq!(wide.font_size, BlockFontSize::Plain);
        assert_eq!(narrow.lines.len(), 2);
        assert_eq!(wide.lines.len(), 3);
        assert_valid(&narrow, "あ··", 3, 24);
        assert_valid(&wide, "あ··", 3, 24);
    }

    #[test]
    fn composes_nfd_without_changing_source_ranges() {
        let text = "cafe\u{301} ß 👩🏽‍💻";
        let layout = BlockLayout::new(text, 120, 24, 1);
        assert_valid(&layout, text, 120, 24);

        let cells = layout
            .lines
            .iter()
            .flat_map(|line| line.cells.iter())
            .collect::<Vec<_>>();
        assert!(cells.iter().any(|cell| {
            let (start, end) = cell.source_byte_range;
            &text[start..end] == "e\u{301}"
        }));
        assert!(cells.iter().any(|cell| {
            let (start, end) = cell.source_byte_range;
            &text[start..end] == "ß"
        }));
        assert!(cells.iter().any(|cell| {
            let (start, end) = cell.source_byte_range;
            &text[start..end] == "👩🏽‍💻"
        }));
    }

    #[test]
    fn falls_back_to_plain_when_a_combining_mark_has_no_glyph() {
        let text = "x\u{301}";
        let layout = BlockLayout::new(text, 80, 24, 1);
        assert_eq!(layout.font_size, BlockFontSize::Plain);
        assert!(rendered(&layout).contains(text));
    }

    #[test]
    fn preserves_multibyte_ranges_and_does_not_case_fold_eszett() {
        let text = "straße 👩🏽‍💻";
        let layout = BlockLayout::new(text, 120, 24, 1);
        assert_valid(&layout, text, 120, 24);

        let cells = layout
            .lines
            .iter()
            .flat_map(|line| line.cells.iter())
            .collect::<Vec<_>>();
        assert!(cells.iter().any(|cell| {
            let (start, end) = cell.source_byte_range;
            &text[start..end] == "ß"
        }));
        assert!(cells.iter().any(|cell| {
            let (start, end) = cell.source_byte_range;
            &text[start..end] == "👩🏽‍💻"
        }));

        let plain = BlockLayout::new(text, 10, 24, 1);
        assert_eq!(plain.font_size, BlockFontSize::Plain);
        let plain_text = rendered(&plain);
        assert!(plain_text.contains("straße"));
        assert!(!plain_text.contains("SS"));
    }
}

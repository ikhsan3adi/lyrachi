use font8x8::{BASIC_FONTS, GREEK_FONTS, LATIN_FONTS, UnicodeFonts};
use unicode_normalization::UnicodeNormalization;

pub const GLYPH_GAP: u16 = 1;

const SOURCE_HEIGHT: usize = 8;
const SOURCE_WIDTH: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockFontSize {
    Block,
    Compact,
    Mini,
    Plain,
}

impl BlockFontSize {
    pub const FALLBACKS: [Self; 3] = [Self::Block, Self::Compact, Self::Mini];

    pub const fn rows(self) -> u16 {
        match self {
            Self::Block => 4,
            Self::Compact => 3,
            Self::Mini => 2,
            Self::Plain => 1,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Block => "block",
            Self::Compact => "compact",
            Self::Mini => "mini",
            Self::Plain => "plain",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GlyphBitmap {
    pub width: u16,
    pub rows: Vec<String>,
}

impl GlyphBitmap {
    pub fn new(width: u16, rows: Vec<String>) -> Self {
        Self { width, rows }
    }

    pub fn height(&self) -> u16 {
        u16::try_from(self.rows.len()).unwrap_or(u16::MAX)
    }

    pub fn is_blank(&self) -> bool {
        self.rows
            .iter()
            .all(|row| row.chars().all(char::is_whitespace))
    }
}

pub fn glyph_bitmap(ch: char, size: BlockFontSize) -> Option<GlyphBitmap> {
    let raw = source_glyph(ch)?;
    match size {
        BlockFontSize::Block => block_bitmap(raw),
        BlockFontSize::Compact => compact_bitmap(raw, 4, 3),
        BlockFontSize::Mini => compact_bitmap(raw, 3, 2),
        BlockFontSize::Plain => None,
    }
}

pub fn glyph_bitmap_for_cluster(text: &str, size: BlockFontSize) -> Option<GlyphBitmap> {
    if size == BlockFontSize::Plain {
        return None;
    }
    let ch = text.nfc().next()?;
    glyph_bitmap(ch, size)
}

pub fn placeholder_bitmap(size: BlockFontSize) -> GlyphBitmap {
    glyph_bitmap('?', size).unwrap_or_else(|| solid_bitmap(size))
}

pub fn space_bitmap(size: BlockFontSize) -> GlyphBitmap {
    let width = match size {
        BlockFontSize::Block => 4,
        BlockFontSize::Compact => 2,
        BlockFontSize::Mini => 1,
        BlockFontSize::Plain => 1,
    };
    let rows = vec![" ".repeat(usize::from(width)); usize::from(size.rows())];
    GlyphBitmap::new(width, rows)
}

fn source_glyph(ch: char) -> Option<[u8; 8]> {
    BASIC_FONTS
        .get(ch)
        .or_else(|| LATIN_FONTS.get(ch))
        .or_else(|| GREEK_FONTS.get(ch))
}

fn ink_bounds(raw: [u8; 8]) -> Option<(usize, usize)> {
    let mut min = SOURCE_WIDTH;
    let mut max = 0usize;
    for row in raw {
        for (x, bit) in (0..SOURCE_WIDTH).map(|x| (x, 1u8 << x)) {
            if row & bit != 0 {
                min = min.min(x);
                max = max.max(x);
            }
        }
    }
    (min < SOURCE_WIDTH).then_some((min, max))
}

fn block_bitmap(raw: [u8; 8]) -> Option<GlyphBitmap> {
    let (min, max) = ink_bounds(raw)?;
    let ink_width = max - min + 1;
    let width = u16::try_from(ink_width).ok()? + GLYPH_GAP;
    let mut rows = Vec::with_capacity(usize::from(BlockFontSize::Block.rows()));

    for y in (0..SOURCE_HEIGHT).step_by(2) {
        let top = raw[y];
        let bottom = raw.get(y + 1).copied().unwrap_or(0);
        let mut row = String::with_capacity(usize::from(width));
        for x in 0..ink_width {
            let bit = 1u8 << (min + x);
            row.push(match (top & bit != 0, bottom & bit != 0) {
                (true, true) => '█',
                (true, false) => '▀',
                (false, true) => '▄',
                (false, false) => ' ',
            });
        }
        row.push(' ');
        rows.push(row);
    }

    Some(GlyphBitmap::new(width, rows))
}

fn compact_bitmap(raw: [u8; 8], max_width: u16, height: u16) -> Option<GlyphBitmap> {
    let (min, max) = ink_bounds(raw)?;
    let ink_width = max - min + 1;
    let out_width = u16::try_from(ink_width).ok()?.min(max_width);
    if out_width == 0 {
        return None;
    }

    let width = out_width + GLYPH_GAP;
    let mut rows = Vec::with_capacity(usize::from(height));
    for y in 0..usize::from(height) {
        let row_start = y * SOURCE_HEIGHT / usize::from(height);
        let row_end = ((y + 1) * SOURCE_HEIGHT / usize::from(height)).max(row_start + 1);
        let mut row = String::with_capacity(usize::from(width));
        for x in 0..usize::from(out_width) {
            let col_start = x * ink_width / usize::from(out_width);
            let col_end = ((x + 1) * ink_width / usize::from(out_width)).max(col_start + 1);
            let mut set = false;
            for source_row in &raw[row_start..row_end.min(SOURCE_HEIGHT)] {
                for source_x in col_start..col_end.min(ink_width) {
                    if source_row & (1u8 << (min + source_x)) != 0 {
                        set = true;
                        break;
                    }
                }
                if set {
                    break;
                }
            }
            row.push(if set { '█' } else { ' ' });
        }
        row.push(' ');
        rows.push(row);
    }

    Some(GlyphBitmap::new(width, rows))
}

fn solid_bitmap(size: BlockFontSize) -> GlyphBitmap {
    let width = 2;
    let rows = vec!["█".repeat(usize::from(width)); usize::from(size.rows())];
    GlyphBitmap::new(width, rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_font_uses_half_blocks_and_four_rows() {
        let glyph = glyph_bitmap('A', BlockFontSize::Block).unwrap();
        assert_eq!(glyph.height(), 4);
        assert_eq!(
            glyph.width,
            u16::try_from(glyph.rows[0].chars().count()).unwrap()
        );
        assert!(glyph.rows.iter().any(|row| row.contains(['▀', '▄', '█'])));
        assert!(glyph.width < 9, "horizontal trimming did not run");
    }

    #[test]
    fn trims_horizontally_and_keeps_compact_sizes_monochrome() {
        let narrow = glyph_bitmap('I', BlockFontSize::Block).unwrap();
        let wide = glyph_bitmap('W', BlockFontSize::Block).unwrap();
        assert!(narrow.width < wide.width);

        for size in [BlockFontSize::Compact, BlockFontSize::Mini] {
            let glyph = glyph_bitmap('A', size).unwrap();
            assert!(
                glyph
                    .rows
                    .iter()
                    .all(|row| { row.chars().all(|ch| ch == '█' || ch == ' ') })
            );
        }
    }

    #[test]
    fn compact_and_mini_have_the_expected_row_counts() {
        let compact = glyph_bitmap('A', BlockFontSize::Compact).unwrap();
        let mini = glyph_bitmap('A', BlockFontSize::Mini).unwrap();
        assert_eq!(compact.rows.len(), 3);
        assert_eq!(mini.rows.len(), 2);
        assert!(compact.rows.iter().all(|row| row.contains('█')));
        assert!(mini.rows.iter().all(|row| row.contains('█')));
    }

    #[test]
    fn composes_nfd_latin_clusters_before_glyph_lookup() {
        let direct = glyph_bitmap('é', BlockFontSize::Block).unwrap();
        assert_eq!(
            glyph_bitmap_for_cluster("e\u{301}", BlockFontSize::Block),
            Some(direct)
        );
        assert!(glyph_bitmap_for_cluster("ß", BlockFontSize::Block).is_some());
        assert!(glyph_bitmap_for_cluster("👩🏽‍💻", BlockFontSize::Block).is_none());
    }

    #[test]
    fn covers_basic_latin_extended_latin_and_greek() {
        assert!(glyph_bitmap('A', BlockFontSize::Block).is_some());
        assert!(glyph_bitmap('ß', BlockFontSize::Block).is_some());
        assert!(glyph_bitmap('Ω', BlockFontSize::Block).is_some());
        assert!(glyph_bitmap('世', BlockFontSize::Block).is_none());
        assert!(glyph_bitmap('👩', BlockFontSize::Block).is_none());
    }

    #[test]
    fn space_bitmaps_match_each_size() {
        assert_eq!(space_bitmap(BlockFontSize::Block).height(), 4);
        assert_eq!(space_bitmap(BlockFontSize::Compact).height(), 3);
        assert_eq!(space_bitmap(BlockFontSize::Mini).height(), 2);
        assert_eq!(space_bitmap(BlockFontSize::Plain).height(), 1);
        assert!(space_bitmap(BlockFontSize::Block).is_blank());
    }
}

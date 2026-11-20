use font8x8::{BASIC_FONTS, GREEK_FONTS, LATIN_FONTS, UnicodeFonts};
use unicode_normalization::UnicodeNormalization;

pub const GLYPH_GAP: u16 = 1;

const SOURCE_HEIGHT: usize = 8;
const SOURCE_WIDTH: usize = 8;

const MUSIC_NOTE_SINGLE: [u8; 8] = [
    0b00001100, 0b00010100, 0b00010000, 0b00010000, 0b00010000, 0b00010000, 0b00011100, 0b00111110,
];

const MUSIC_NOTE_BEAMED: [u8; 8] = [
    0b00111100, 0b00100100, 0b00100100, 0b00100100, 0b00100100, 0b00100100, 0b00110110, 0b00111111,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockFontSize {
    Block,
    Plain,
}

impl BlockFontSize {
    pub const FALLBACKS: [Self; 1] = [Self::Block];

    pub const fn rows(self) -> u16 {
        match self {
            Self::Block => 4,
            Self::Plain => 1,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Block => "block",
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
        BlockFontSize::Plain => None,
    }
}

pub fn glyph_bitmap_for_cluster(text: &str, size: BlockFontSize) -> Option<GlyphBitmap> {
    if size == BlockFontSize::Plain {
        return None;
    }
    let mut normalized = text.nfc();
    let ch = normalized.next()?;
    if normalized.next().is_some() {
        return None;
    }
    glyph_bitmap(ch, size)
}

pub fn placeholder_bitmap(size: BlockFontSize) -> GlyphBitmap {
    glyph_bitmap('?', size).unwrap_or_else(|| solid_bitmap(size))
}

pub fn space_bitmap(size: BlockFontSize) -> GlyphBitmap {
    let width = match size {
        BlockFontSize::Block => 4,
        BlockFontSize::Plain => 1,
    };
    let rows = vec![" ".repeat(usize::from(width)); usize::from(size.rows())];
    GlyphBitmap::new(width, rows)
}

fn source_glyph(ch: char) -> Option<[u8; 8]> {
    match ch {
        '♪' => Some(MUSIC_NOTE_SINGLE),
        '♫' => Some(MUSIC_NOTE_BEAMED),
        _ => BASIC_FONTS
            .get(ch)
            .or_else(|| LATIN_FONTS.get(ch))
            .or_else(|| GREEK_FONTS.get(ch)),
    }
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
    fn trims_horizontally() {
        let narrow = glyph_bitmap('I', BlockFontSize::Block).unwrap();
        let wide = glyph_bitmap('W', BlockFontSize::Block).unwrap();
        assert!(narrow.width < wide.width);
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
    fn rejects_clusters_that_do_not_collapse_to_one_glyph() {
        assert!(glyph_bitmap_for_cluster("x\u{301}", BlockFontSize::Block).is_none());
        assert!(glyph_bitmap_for_cluster("1\u{fe0f}\u{20e3}", BlockFontSize::Block).is_none());
    }

    #[test]
    fn covers_basic_latin_extended_latin_and_greek() {
        assert!(glyph_bitmap('A', BlockFontSize::Block).is_some());
        assert!(glyph_bitmap('ß', BlockFontSize::Block).is_some());
        assert!(glyph_bitmap('Ω', BlockFontSize::Block).is_some());
        assert!(glyph_bitmap('♪', BlockFontSize::Block).is_some());
        assert!(glyph_bitmap('♫', BlockFontSize::Block).is_some());
        assert!(glyph_bitmap('世', BlockFontSize::Block).is_none());
        assert!(glyph_bitmap('👩', BlockFontSize::Block).is_none());
    }

    #[test]
    fn space_bitmaps_match_each_size() {
        assert_eq!(space_bitmap(BlockFontSize::Block).height(), 4);
        assert_eq!(space_bitmap(BlockFontSize::Plain).height(), 1);
        assert!(space_bitmap(BlockFontSize::Block).is_blank());
    }
}

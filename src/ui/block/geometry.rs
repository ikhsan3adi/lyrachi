use font8x8::{BASIC_FONTS, GREEK_FONTS, LATIN_FONTS, UnicodeFonts};
use unicode_normalization::UnicodeNormalization;

pub const SOURCE_HEIGHT: usize = 8;
pub const SOURCE_WIDTH: usize = 8;

const MUSIC_NOTE_SINGLE: [u8; 8] = [
    0b00001100, 0b00010100, 0b00010000, 0b00010000, 0b00010000, 0b00010000, 0b00011100, 0b00111110,
];

const MUSIC_NOTE_BEAMED: [u8; 8] = [
    0b00111100, 0b00100100, 0b00100100, 0b00100100, 0b00100100, 0b00100100, 0b00110110, 0b00111111,
];

pub fn source_bitmap(ch: char) -> Option<[u8; 8]> {
    match ch {
        '♪' => Some(MUSIC_NOTE_SINGLE),
        '♫' => Some(MUSIC_NOTE_BEAMED),
        _ => BASIC_FONTS
            .get(ch)
            .or_else(|| LATIN_FONTS.get(ch))
            .or_else(|| GREEK_FONTS.get(ch)),
    }
}

pub fn source_bitmap_for_cluster(text: &str) -> Option<[u8; 8]> {
    let mut normalized = text.nfc();
    let ch = normalized.next()?;
    if normalized.next().is_some() {
        return None;
    }
    source_bitmap(ch)
}

pub fn ink_width(bitmap: [u8; 8]) -> u16 {
    bitmap
        .iter()
        .map(|row| (8 - row.leading_zeros().min(8)) as u16)
        .max()
        .unwrap_or(0)
}

pub fn push_source_points(bitmap: [u8; 8], out: &mut Vec<(u8, u8)>) {
    for (y, row) in bitmap.iter().enumerate() {
        for x in 0..SOURCE_WIDTH {
            if row & (1 << x) != 0 {
                out.push((x as u8, y as u8));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_bitmap_covers_latin_greek_and_notes() {
        assert!(source_bitmap('A').is_some());
        assert!(source_bitmap('ß').is_some());
        assert!(source_bitmap('Ω').is_some());
        assert!(source_bitmap('♪').is_some());
        assert!(source_bitmap('♫').is_some());
        assert!(source_bitmap('世').is_none());
        assert!(source_bitmap('👩').is_none());
    }

    #[test]
    fn source_bitmap_for_cluster_rejects_multi_char_clusters() {
        assert!(source_bitmap_for_cluster("q\u{0323}").is_none());
        assert!(source_bitmap_for_cluster("A").is_some());
    }

    #[test]
    fn ink_width_measures_the_rightmost_ink_bit() {
        assert_eq!(ink_width([0; 8]), 0);
        assert_eq!(ink_width([0b00000100; 8]), 3);
        assert_eq!(ink_width([0b10000000; 8]), 8);
    }

    #[test]
    fn push_source_points_lists_ink_pixels_row_major() {
        let mut points = Vec::new();
        push_source_points(
            [
                0b00000000, 0b00000000, 0b00000000, 0b00000000, 0b10000001, 0b00000000, 0b00000000,
                0b00000000,
            ],
            &mut points,
        );
        assert_eq!(points, vec![(0, 4), (7, 4)]);
    }

    #[test]
    fn push_source_points_covers_every_ink_bit_of_a_glyph() {
        let bitmap = source_bitmap('A').unwrap();
        let mut points = Vec::new();
        push_source_points(bitmap, &mut points);
        let ink_bits: usize = bitmap.iter().map(|row| row.count_ones() as usize).sum();
        assert_eq!(points.len(), ink_bits);
        assert!(
            points
                .iter()
                .all(|(x, y)| (*x as usize) < SOURCE_WIDTH && (*y as usize) < SOURCE_HEIGHT)
        );
    }
}

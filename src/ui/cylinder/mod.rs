use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::mem::{size_of, size_of_val};

use ratatui::style::Style;

use crate::ui::block::GlyphBitmap;

pub(crate) mod layout;
pub(crate) mod render;

pub const CYLINDER_MIN_WIDTH: u16 = 60;
pub const CYLINDER_MIN_HEIGHT: u16 = 12;
pub const CYLINDER_TRAVERSE_SECONDS: f64 = 6.0;
pub const CYLINDER_MIN_SPEED: f64 = 8.0;
pub const CYLINDER_MAX_SPEED: f64 = 40.0;
pub const CYLINDER_MAX_VISIBLE_LINES: usize = 32;
pub const CYLINDER_MAX_ANNOTATIONS: usize = 2;
pub const CYLINDER_MIN_MAIN_HEIGHT: u16 = 5;
pub const CYLINDER_DEFAULT_ENTRY_LEAD_MS: u64 = 1_000;
pub const CYLINDER_MAX_ENTRY_LEAD_MS: u64 = 5_000;
pub const CYLINDER_MAX_STRETCH: f64 = 0.75;

pub(crate) const CYLINDER_CACHE_MAX_BYTES: usize = 2 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CylinderGlyph {
    pub text: String,
    pub byte_range: (usize, usize),
    pub display_width: u16,
    pub bitmap: GlyphBitmap,
}

#[derive(Debug, Clone, PartialEq)]
pub struct GridCell {
    pub text: String,
    pub style: Style,
}

#[derive(Debug, Clone)]
pub struct AnnotationRow {
    pub text: String,
    pub style: Style,
}

#[derive(Debug, Clone)]
pub(crate) struct CachedCylinderLine {
    pub(crate) text_hash: u64,
    pub(crate) ambiguous_width: u8,
    pub(crate) glyphs: Vec<CylinderGlyph>,
    pub(crate) text_width: u16,
    bytes: usize,
}

#[derive(Debug, Default)]
pub struct CylinderStageCache {
    options_hash: Option<u64>,
    entries: HashMap<usize, CachedCylinderLine>,
    estimated_bytes: usize,
    grid_width: u16,
    grid_height: u16,
    grid: Vec<Option<GridCell>>,
}

impl CylinderStageCache {
    pub fn prepare(&mut self, options_hash: u64) {
        if self.options_hash != Some(options_hash) {
            self.entries.clear();
            self.estimated_bytes = 0;
            self.options_hash = Some(options_hash);
        }
    }

    pub fn cache_line(
        &mut self,
        line_index: usize,
        text: &str,
        ambiguous_width: u8,
        glyphs: Vec<CylinderGlyph>,
        text_width: u16,
    ) {
        self.insert_line(line_index, text, ambiguous_width, glyphs, text_width);
    }

    pub(crate) fn ensure_line(
        &mut self,
        line_index: usize,
        text: &str,
        ambiguous_width: u8,
        build: impl FnOnce() -> (Vec<CylinderGlyph>, u16),
    ) -> u16 {
        if let Some(existing) = self.entries.get(&line_index)
            && existing.text_hash == text_hash(text)
            && existing.ambiguous_width == normalize_ambiguous(ambiguous_width)
        {
            return existing.text_width;
        }
        let (glyphs, text_width) = build();
        self.insert_line(line_index, text, ambiguous_width, glyphs, text_width);
        text_width
    }

    fn insert_line(
        &mut self,
        line_index: usize,
        text: &str,
        ambiguous_width: u8,
        glyphs: Vec<CylinderGlyph>,
        text_width: u16,
    ) {
        self.entries.remove(&line_index);
        self.recompute_bytes();
        let bytes = line_bytes(&glyphs);
        self.entries.insert(
            line_index,
            CachedCylinderLine {
                text_hash: text_hash(text),
                ambiguous_width: normalize_ambiguous(ambiguous_width),
                glyphs,
                text_width,
                bytes,
            },
        );
        self.estimated_bytes = self.estimated_bytes.saturating_add(bytes);
        self.enforce_limit(line_index);
    }

    pub(crate) fn line(
        &self,
        line_index: usize,
        text_hash: u64,
        ambiguous_width: u8,
    ) -> Option<&CachedCylinderLine> {
        let line = self.entries.get(&line_index)?;
        if line.text_hash != text_hash
            || line.ambiguous_width != normalize_ambiguous(ambiguous_width)
        {
            return None;
        }
        Some(line)
    }

    pub fn retain_visible(&mut self, visible: &[usize]) {
        self.entries.retain(|index, _| visible.contains(index));
        self.recompute_bytes();
    }

    pub fn estimated_bytes(&self) -> usize {
        self.estimated_bytes
    }

    pub fn cached_line_count(&self) -> usize {
        self.entries.len()
    }

    pub fn prepare_grid(&mut self, width: u16, height: u16) {
        let width = width.max(1);
        let height = height.max(1);
        if self.grid_width != width || self.grid_height != height {
            self.grid_width = width;
            self.grid_height = height;
            let len = usize::from(width) * usize::from(height);
            self.grid.clear();
            self.grid.resize_with(len, || None);
        } else {
            self.grid.iter_mut().for_each(|cell| *cell = None);
        }
    }

    pub fn cell(&self, x: u16, y: u16) -> Option<&GridCell> {
        if x >= self.grid_width || y >= self.grid_height {
            return None;
        }
        self.grid[grid_index(self.grid_width, x, y)].as_ref()
    }

    pub fn cell_mut(&mut self, x: u16, y: u16) -> Option<&mut GridCell> {
        if x >= self.grid_width || y >= self.grid_height {
            return None;
        }
        let index = grid_index(self.grid_width, x, y);
        self.grid[index].as_mut()
    }

    pub fn cell_slot_mut(&mut self, x: u16, y: u16) -> Option<&mut Option<GridCell>> {
        if x >= self.grid_width || y >= self.grid_height {
            return None;
        }
        let index = grid_index(self.grid_width, x, y);
        self.grid.get_mut(index)
    }

    pub fn grid_width(&self) -> u16 {
        self.grid_width
    }

    pub fn grid_height(&self) -> u16 {
        self.grid_height
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.estimated_bytes = 0;
        self.options_hash = None;
        self.grid.iter_mut().for_each(|cell| *cell = None);
    }

    fn enforce_limit(&mut self, keep: usize) {
        while self.entries.len() > CYLINDER_MAX_VISIBLE_LINES {
            let Some(candidate) = self.entries.keys().copied().find(|key| *key != keep) else {
                break;
            };
            if let Some(entry) = self.entries.remove(&candidate) {
                self.estimated_bytes = self.estimated_bytes.saturating_sub(entry.bytes);
            }
        }
        while self.estimated_bytes > CYLINDER_CACHE_MAX_BYTES {
            let Some(candidate) = self.entries.keys().copied().find(|key| *key != keep) else {
                break;
            };
            if let Some(entry) = self.entries.remove(&candidate) {
                self.estimated_bytes = self.estimated_bytes.saturating_sub(entry.bytes);
            }
        }
    }

    fn recompute_bytes(&mut self) {
        self.estimated_bytes = self.entries.values().map(|entry| entry.bytes).sum();
    }
}

fn normalize_ambiguous(ambiguous_width: u8) -> u8 {
    if ambiguous_width == 2 { 2 } else { 1 }
}

fn line_bytes(glyphs: &[CylinderGlyph]) -> usize {
    let mut bytes = size_of::<CachedCylinderLine>();
    bytes = bytes.saturating_add(size_of_val(glyphs));
    for glyph in glyphs {
        bytes = bytes.saturating_add(glyph.text.capacity());
        bytes = bytes.saturating_add(glyph.bitmap.width.into());
        for row in &glyph.bitmap.rows {
            bytes = bytes.saturating_add(row.capacity());
        }
    }
    bytes
}

fn grid_index(grid_width: u16, x: u16, y: u16) -> usize {
    usize::from(y) * usize::from(grid_width) + usize::from(x)
}

pub(crate) fn text_hash(text: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn glyph(text: &str, byte_range: (usize, usize), display_width: u16) -> CylinderGlyph {
        CylinderGlyph {
            text: text.to_string(),
            byte_range,
            display_width,
            bitmap: GlyphBitmap::new(
                display_width,
                vec![" ".repeat(usize::from(display_width)); 4],
            ),
        }
    }

    #[test]
    fn prepare_grid_resizes_and_clears_stale_cells() {
        let mut cache = CylinderStageCache::default();
        cache.prepare_grid(4, 2);
        assert_eq!(cache.grid_width(), 4);
        assert_eq!(cache.grid_height(), 2);
        if let Some(slot) = cache.cell_slot_mut(0, 0) {
            *slot = Some(GridCell {
                text: "x".to_string(),
                style: Style::default(),
            });
        }
        assert!(cache.cell(0, 0).is_some());

        cache.prepare_grid(4, 2);
        assert!(cache.cell(0, 0).is_none());

        cache.prepare_grid(8, 3);
        assert_eq!(cache.grid_width(), 8);
        assert_eq!(cache.grid_height(), 3);
        assert!(cache.cell(0, 0).is_none());
        assert!(cache.cell(7, 2).is_none());
        assert!(cache.cell(8, 0).is_none());
        assert!(cache.cell(0, 3).is_none());
    }

    #[test]
    fn cache_reuses_lines_and_resets_on_hash_change() {
        let mut cache = CylinderStageCache::default();
        cache.prepare(7);
        cache.cache_line(2, "HELLO", 1, vec![glyph("H", (0, 1), 1)], 5);
        assert_eq!(cache.cached_line_count(), 1);
        assert!(cache.estimated_bytes() > 0);
        assert!(cache.estimated_bytes() <= CYLINDER_CACHE_MAX_BYTES);

        cache.prepare(7);
        assert_eq!(cache.cached_line_count(), 1);

        cache.prepare(9);
        assert_eq!(cache.cached_line_count(), 0);
        assert_eq!(cache.estimated_bytes(), 0);
    }

    #[test]
    fn cache_line_rejects_stale_hash_or_width() {
        let mut cache = CylinderStageCache::default();
        cache.prepare(1);
        cache.cache_line(0, "OLD", 1, vec![glyph("O", (0, 1), 1)], 3);
        assert!(cache.line(0, text_hash("OLD"), 1).is_some());
        assert!(cache.line(0, text_hash("NEW"), 1).is_none());
        assert!(cache.line(0, text_hash("OLD"), 2).is_none());
    }

    #[test]
    fn cache_replacement_does_not_inflate_byte_estimate() {
        let text = "A much longer replacement line that scrolls across the wall";
        let mut cache = CylinderStageCache::default();
        cache.prepare(5);
        cache.cache_line(0, "OLD", 1, vec![glyph("O", (0, 1), 1)], 3);
        cache.cache_line(0, text, 1, vec![glyph("A", (0, 1), 1); 8], 8);

        let mut fresh = CylinderStageCache::default();
        fresh.prepare(5);
        fresh.cache_line(0, text, 1, vec![glyph("A", (0, 1), 1); 8], 8);

        assert_eq!(cache.estimated_bytes(), fresh.estimated_bytes());
    }

    #[test]
    fn retain_visible_keeps_only_the_window_regardless_of_order() {
        let mut cache = CylinderStageCache::default();
        cache.prepare(3);
        for index in 0..6 {
            cache.cache_line(index, "line", 1, vec![glyph("l", (0, 1), 1)], 4);
        }
        cache.retain_visible(&[4, 2, 3]);
        assert_eq!(cache.cached_line_count(), 3);
        assert!(cache.line(2, text_hash("line"), 1).is_some());
        assert!(cache.line(3, text_hash("line"), 1).is_some());
        assert!(cache.line(4, text_hash("line"), 1).is_some());
        assert!(cache.line(5, text_hash("line"), 1).is_none());
    }
}

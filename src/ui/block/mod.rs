use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::mem::size_of;

pub mod font;
pub mod layout;

pub use font::{BlockFontSize, GlyphBitmap};
pub use layout::{BlockCell, BlockLayout, Glyph, VisualLine, layout_text};

pub const BLOCK_CACHE_MAX_BYTES: usize = 8 * 1024 * 1024;
pub const BLOCK_WINDOW_BEFORE: usize = 1;
pub const BLOCK_WINDOW_AFTER: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct BlockCacheKey {
    line_index: usize,
    text_hash: u64,
    max_width: u16,
    max_height: u16,
    ambiguous_width: u8,
}

#[derive(Debug)]
struct CachedBlockLine {
    layout: BlockLayout,
    bytes: usize,
}

#[derive(Debug, Default)]
pub struct BlockStageCache {
    options_hash: Option<u64>,
    entries: HashMap<BlockCacheKey, CachedBlockLine>,
    estimated_bytes: usize,
}

impl BlockStageCache {
    pub fn prepare(&mut self, options_hash: u64, active_line_idx: Option<usize>) {
        if self.options_hash != Some(options_hash) {
            self.entries.clear();
            self.estimated_bytes = 0;
            self.options_hash = Some(options_hash);
        }

        let Some(active_idx) = active_line_idx else {
            self.entries.clear();
            self.estimated_bytes = 0;
            return;
        };
        let start = active_idx.saturating_sub(BLOCK_WINDOW_BEFORE);
        let end = active_idx.saturating_add(BLOCK_WINDOW_AFTER);
        self.entries
            .retain(|key, _| (start..=end).contains(&key.line_index));
        self.recompute_bytes();
    }

    pub fn layout_for(
        &mut self,
        line_index: usize,
        text: &str,
        max_width: u16,
        max_height: u16,
        ambiguous_width: u8,
    ) -> &BlockLayout {
        let key = BlockCacheKey {
            line_index,
            text_hash: text_hash(text),
            max_width: max_width.max(1),
            max_height: max_height.max(1),
            ambiguous_width: if ambiguous_width == 2 { 2 } else { 1 },
        };

        if !self.entries.contains_key(&key) {
            self.entries
                .retain(|existing, _| existing.line_index != line_index);
            self.recompute_bytes();
            let layout = layout_text(text, key.max_width, key.max_height, key.ambiguous_width);
            let bytes = layout_bytes(&layout);
            self.entries.insert(key, CachedBlockLine { layout, bytes });
            self.estimated_bytes = self.estimated_bytes.saturating_add(bytes);
            self.enforce_limit(key);
        }

        &self.entries.get(&key).expect("inserted layout").layout
    }

    pub fn estimated_bytes(&self) -> usize {
        self.estimated_bytes
    }

    pub fn cached_line_count(&self) -> usize {
        self.entries.len()
    }

    pub fn contains_line(&self, line_index: usize) -> bool {
        self.entries.keys().any(|key| key.line_index == line_index)
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.estimated_bytes = 0;
        self.options_hash = None;
    }

    fn enforce_limit(&mut self, keep: BlockCacheKey) {
        while self.estimated_bytes > BLOCK_CACHE_MAX_BYTES {
            let Some(candidate) = self
                .entries
                .keys()
                .copied()
                .find(|candidate| *candidate != keep)
            else {
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

fn text_hash(text: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

fn layout_bytes(layout: &BlockLayout) -> usize {
    let mut bytes = size_of::<BlockLayout>();
    bytes = bytes.saturating_add(layout.lines.capacity() * size_of::<VisualLine>());
    for line in &layout.lines {
        bytes = bytes.saturating_add(line.cells.capacity() * size_of::<BlockCell>());
        for cell in &line.cells {
            bytes = bytes.saturating_add(cell.rows.capacity() * size_of::<String>());
            for row in &cell.rows {
                bytes = bytes.saturating_add(row.capacity());
            }
        }
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_reuses_layouts_and_resets_on_hash_change() {
        let mut cache = BlockStageCache::default();
        cache.prepare(7, Some(2));
        cache.layout_for(2, "HELLO", 80, 2, 1);
        cache.layout_for(2, "HELLO", 80, 2, 1);
        assert_eq!(cache.cached_line_count(), 1);
        assert!(cache.estimated_bytes() > 0);
        assert!(cache.estimated_bytes() <= BLOCK_CACHE_MAX_BYTES);

        cache.prepare(7, None);
        assert_eq!(cache.cached_line_count(), 0);
        assert_eq!(cache.estimated_bytes(), 0);
    }

    #[test]
    fn cache_keeps_only_the_active_window() {
        let mut cache = BlockStageCache::default();
        cache.prepare(11, Some(10));
        for line_index in 0..20 {
            cache.layout_for(line_index, "window", 80, 2, 1);
        }
        assert_eq!(cache.cached_line_count(), 20);

        cache.prepare(11, Some(10));
        assert_eq!(cache.cached_line_count(), 5);
        for line_index in 9..=13 {
            assert!(cache.contains_line(line_index));
        }
        assert!(!cache.contains_line(8));
        assert!(!cache.contains_line(14));
    }

    #[test]
    fn cache_replaces_layout_when_line_text_changes() {
        let mut cache = BlockStageCache::default();
        cache.prepare(3, Some(0));
        cache.layout_for(0, "OLD", 80, 2, 1);
        cache.layout_for(0, "NEW TEXT", 80, 2, 1);
        assert_eq!(cache.cached_line_count(), 1);
    }

    #[test]
    fn cache_replacement_does_not_inflate_byte_estimate() {
        let text = "A much longer replacement line that wraps across cells";
        let mut cache = BlockStageCache::default();
        cache.prepare(5, Some(0));
        cache.layout_for(0, "OLD", 80, 8, 1);
        cache.layout_for(0, text, 80, 8, 1);

        let mut fresh = BlockStageCache::default();
        fresh.prepare(5, Some(0));
        fresh.layout_for(0, text, 80, 8, 1);

        assert_eq!(cache.estimated_bytes(), fresh.estimated_bytes());
    }
}

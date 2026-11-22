use ratatui::style::Style;
use ratatui::text::Line;
use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::mem::{size_of, size_of_val};

pub(crate) mod camera;
pub(crate) mod rasterizer;
pub(crate) mod render;

use rasterizer::BrailleCanvas;

pub const CANVAS3D_MIN_WIDTH: u16 = 60;
pub const CANVAS3D_MIN_HEIGHT: u16 = 16;
pub const CANVAS3D_DEFAULT_TILT_DEG: u16 = 60;
pub const CANVAS3D_MIN_TILT_DEG: u16 = 35;
pub const CANVAS3D_MAX_TILT_DEG: u16 = 60;
pub const CANVAS3D_TILT_STEP_DEG: i32 = 5;
pub const CANVAS3D_FOV_Y_DEG: f32 = 60.0;
pub const CANVAS3D_PLANE_Y_OFFSET: f32 = 3.0;
pub const CANVAS3D_DEFAULT_SPAWN_PCT: u16 = 82;
pub const CANVAS3D_MIN_SPAWN_PCT: u16 = 50;
pub const CANVAS3D_MAX_SPAWN_PCT: u16 = 95;
pub const CANVAS3D_SPAWN_STEP_PCT: i32 = 2;
pub const CANVAS3D_V_ENTER: f32 = 34.0;
pub const CANVAS3D_MIN_SPAWN_V: f32 = 6.0;
pub const CANVAS3D_MAX_SPAWN_V: f32 = 120.0;
pub const CANVAS3D_CROSS_SECONDS: f32 = 4.0;
pub const CANVAS3D_GLYPH_SCALE: f32 = 0.15;
pub const CANVAS3D_Y_STRETCH: f32 = 2.5;
pub const CANVAS3D_SPACE_ADVANCE: u16 = 4;
pub const CANVAS3D_GLYPH_GAP: u16 = 1;
pub const CANVAS3D_FLIP_MS: u64 = 400;
pub const CANVAS3D_ZPOP_SCALE: f32 = 1.15;
pub const CANVAS3D_DIM_DEPTH: f32 = 26.0;
pub const CANVAS3D_CACHE_MAX_BYTES: usize = 2 * 1024 * 1024;
pub const CANVAS3D_MIN_SPEED_PCT: u16 = 50;
pub const CANVAS3D_MAX_SPEED_PCT: u16 = 300;
pub const CANVAS3D_MAX_VISIBLE_LINES: usize = 32;

#[derive(Debug, Clone, PartialEq)]
pub struct CanvasGlyph {
    pub text: String,
    pub byte_range: (usize, usize),
    pub points: Vec<(u8, u8)>,
    pub advance: u16,
}

#[derive(Debug, Clone)]
pub(crate) struct CachedCanvasLine {
    pub(crate) text_hash: u64,
    pub(crate) ambiguous_width: u8,
    pub(crate) glyphs: Vec<CanvasGlyph>,
    pub(crate) text_width: u16,
    bytes: usize,
}

#[derive(Debug, Default)]
pub struct CanvasStageCache {
    options_hash: Option<u64>,
    entries: HashMap<usize, CachedCanvasLine>,
    estimated_bytes: usize,
    canvas: BrailleCanvas,
}

impl CanvasStageCache {
    pub fn prepare(&mut self, options_hash: u64) {
        if self.options_hash != Some(options_hash) {
            self.entries.clear();
            self.estimated_bytes = 0;
            self.options_hash = Some(options_hash);
        }
    }

    pub(crate) fn ensure_line(
        &mut self,
        line_index: usize,
        text: &str,
        ambiguous_width: u8,
        build: impl FnOnce() -> (Vec<CanvasGlyph>, u16),
    ) -> u16 {
        let hash = text_hash(text);
        let ambiguous = normalize_ambiguous(ambiguous_width);
        if let Some(existing) = self.entries.get(&line_index)
            && existing.text_hash == hash
            && existing.ambiguous_width == ambiguous
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
        glyphs: Vec<CanvasGlyph>,
        text_width: u16,
    ) {
        self.entries.remove(&line_index);
        self.recompute_bytes();
        let bytes = line_bytes(&glyphs);
        self.entries.insert(
            line_index,
            CachedCanvasLine {
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
    ) -> Option<&CachedCanvasLine> {
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

    pub(crate) fn prepare_canvas(&mut self, width: u16, height: u16) {
        self.canvas.resize(width, height);
    }

    pub(crate) fn plot(&mut self, x: f32, y: f32, style: Style) -> bool {
        self.canvas.plot(x, y, style)
    }

    pub(crate) fn has_dots(&self) -> bool {
        self.canvas.has_dots()
    }

    pub(crate) fn canvas_lines(&self) -> Vec<Line<'static>> {
        self.canvas.to_lines()
    }

    pub(crate) fn clear(&mut self) {
        self.entries.clear();
        self.estimated_bytes = 0;
        self.options_hash = None;
        self.canvas = BrailleCanvas::new(1, 1);
    }

    fn enforce_limit(&mut self, keep: usize) {
        while self.entries.len() > CANVAS3D_MAX_VISIBLE_LINES {
            let Some(candidate) = self.entries.keys().copied().find(|key| *key != keep) else {
                break;
            };
            if let Some(entry) = self.entries.remove(&candidate) {
                self.estimated_bytes = self.estimated_bytes.saturating_sub(entry.bytes);
            }
        }
        while self.estimated_bytes > CANVAS3D_CACHE_MAX_BYTES {
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

pub(crate) fn text_hash(text: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

fn normalize_ambiguous(ambiguous_width: u8) -> u8 {
    if ambiguous_width == 2 { 2 } else { 1 }
}
/// Spawn distance on the crawl plane, in plane units.
///
/// `spawn_pct` is the fraction of the screen height (measured from the top)
/// where a line appears. Solving the projection for that height gives
/// `v = y_offset * f / (f * cos(tilt) - p * sin(tilt))`, which works for any
/// tilt. Near the asymptote (`tilt >= 90 - fov/2`) the target height becomes
/// unreachable, so the result clamps to [`CANVAS3D_MAX_SPAWN_V`]; flat tilts
/// clamp to [`CANVAS3D_MIN_SPAWN_V`].
pub(crate) fn spawn_v_for(tilt_deg: f32, spawn_pct: u16) -> f32 {
    let p = f32::from(spawn_pct) / 100.0;
    let tilt = tilt_deg.to_radians();
    let f = 1.0 / (CANVAS3D_FOV_Y_DEG.to_radians() / 2.0).tan();
    let denom = f * tilt.cos() - p * tilt.sin();
    if denom <= 0.01 {
        return CANVAS3D_MAX_SPAWN_V;
    }
    (CANVAS3D_PLANE_Y_OFFSET * f / denom).clamp(CANVAS3D_MIN_SPAWN_V, CANVAS3D_MAX_SPAWN_V)
}

/// Depth past which lines render dimmed, scaled to the current spawn so the
/// dim band stays proportional at every spawn distance.
pub(crate) fn dim_depth_for(v_enter: f32) -> f32 {
    v_enter * (CANVAS3D_DIM_DEPTH / CANVAS3D_V_ENTER)
}

fn line_bytes(glyphs: &[CanvasGlyph]) -> usize {
    size_of_val(glyphs)
        + glyphs
            .iter()
            .map(|glyph| {
                size_of_val(&glyph.text)
                    + glyph.text.len()
                    + size_of_val(&glyph.points)
                    + glyph.points.len() * size_of::<(u8, u8)>()
            })
            .sum::<usize>()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawn_v_at_the_default_matches_the_reference() {
        let v = spawn_v_for(
            f32::from(CANVAS3D_DEFAULT_TILT_DEG),
            CANVAS3D_DEFAULT_SPAWN_PCT,
        );
        assert!((v - CANVAS3D_V_ENTER).abs() < 1.5, "{v}");
    }

    #[test]
    fn spawn_v_caps_when_the_target_height_is_unreachable() {
        assert_eq!(spawn_v_for(60.0, 100), CANVAS3D_MAX_SPAWN_V);
        assert_eq!(spawn_v_for(70.0, 100), CANVAS3D_MAX_SPAWN_V);
    }

    #[test]
    fn spawn_v_grows_with_the_requested_height() {
        let high = spawn_v_for(55.0, 90);
        let low = spawn_v_for(55.0, 60);
        assert!(high > low, "{high} vs {low}");
        assert!(spawn_v_for(45.0, 82) < spawn_v_for(60.0, 82));
    }

    #[test]
    fn spawn_v_clamps_flat_tilts() {
        assert_eq!(spawn_v_for(0.0, 82), CANVAS3D_MIN_SPAWN_V);
    }

    #[test]
    fn dim_depth_matches_default_geometry() {
        assert!((dim_depth_for(CANVAS3D_V_ENTER) - CANVAS3D_DIM_DEPTH).abs() < 1e-3);
        let v = spawn_v_for(
            f32::from(CANVAS3D_DEFAULT_TILT_DEG),
            CANVAS3D_DEFAULT_SPAWN_PCT,
        );
        assert!(dim_depth_for(v) < v);
    }
}

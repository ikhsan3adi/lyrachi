use super::{
    CYLINDER_MAX_SPEED, CYLINDER_MAX_STRETCH, CYLINDER_MIN_SPEED, CYLINDER_TRAVERSE_SECONDS,
};
use crate::ui::CylinderSpeedMode;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CylinderLinePlacement {
    pub first_col: f64,
    pub visible: bool,
    pub fully_left: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct CylinderGeometry {
    width: u16,
    height: u16,
    speed_mode: CylinderSpeedMode,
    speed_multiplier: f64,
}

impl CylinderGeometry {
    pub fn new(
        width: u16,
        height: u16,
        speed_mode: CylinderSpeedMode,
        speed_multiplier: f64,
    ) -> Self {
        let width = width.max(1);
        let height = height.max(1);
        let speed_multiplier = if speed_multiplier.is_finite() && speed_multiplier > 0.0 {
            speed_multiplier
        } else {
            1.0
        };
        Self {
            width,
            height,
            speed_mode,
            speed_multiplier,
        }
    }

    pub fn belt_y(&self) -> u16 {
        self.height / 2
    }

    fn speed_for(&self, text_width: u16) -> f64 {
        let base = match self.speed_mode {
            CylinderSpeedMode::Constant => f64::from(self.width) / CYLINDER_TRAVERSE_SECONDS,
            CylinderSpeedMode::LineLength => {
                (f64::from(self.width) + f64::from(text_width)) / CYLINDER_TRAVERSE_SECONDS
            }
        };
        base.clamp(CYLINDER_MIN_SPEED, CYLINDER_MAX_SPEED) * self.speed_multiplier
    }

    pub fn first_col(&self, start_ms: u64, effective_ms: u64, text_width: u16) -> f64 {
        let elapsed_sec = (effective_ms as f64 - start_ms as f64) / 1000.0;
        f64::from(self.width - 1) - elapsed_sec * self.speed_for(text_width)
    }

    pub fn place(
        &self,
        start_ms: u64,
        effective_ms: u64,
        text_width: u16,
    ) -> CylinderLinePlacement {
        let started = effective_ms >= start_ms;
        let first_col = self.first_col(start_ms, effective_ms, text_width);
        let tail = first_col + f64::from(text_width);
        let visible = started && first_col < f64::from(self.width) && tail > 0.0;
        CylinderLinePlacement {
            first_col,
            visible,
            fully_left: tail <= 0.0,
        }
    }

    pub fn stretch_rows(&self, screen_x: i32, base_rows: u16) -> u16 {
        self.stretch(screen_x, base_rows)
    }

    pub fn stretch_cols(&self, screen_x: i32, base_cols: u16) -> u16 {
        self.stretch(screen_x, base_cols)
    }

    pub fn glyph_top(&self, out_rows: u16) -> u16 {
        self.belt_y().saturating_sub(out_rows.saturating_sub(1) / 2)
    }

    pub fn depth(&self, screen_x: i32) -> f64 {
        1.0 - self.bend_t(screen_x) * self.bend_t(screen_x)
    }

    fn stretch(&self, screen_x: i32, base: u16) -> u16 {
        let t = self.bend_t(screen_x);
        let factor = 1.0 + CYLINDER_MAX_STRETCH * t * t;
        let rows = (f64::from(base) * factor).round();
        (rows as u16).max(1)
    }

    fn bend_t(&self, screen_x: i32) -> f64 {
        let center_x = f64::from(self.width) / 2.0;
        let half_width = f64::from(self.width / 2).max(1.0);
        ((f64::from(screen_x) - center_x) / half_width).clamp(-1.0, 1.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn geometry() -> CylinderGeometry {
        CylinderGeometry::new(80, 24, CylinderSpeedMode::Constant, 1.0)
    }

    #[test]
    fn first_character_enters_at_right_edge_at_start_ms() {
        let geometry = geometry();
        let first_col = geometry.first_col(5_000, 5_000, 10);
        assert!((first_col - 79.0).abs() < 1e-9, "{first_col}");
        assert!(geometry.place(5_000, 5_000, 10).visible);
    }

    #[test]
    fn one_second_moves_the_line_by_speed_columns() {
        let geometry = geometry();
        let before = geometry.first_col(5_000, 5_000, 10);
        let after = geometry.first_col(5_000, 6_000, 10);
        let speed = before - after;
        assert!((speed - (80.0 / 6.0)).abs() < 1e-9, "{speed}");
    }

    #[test]
    fn constant_mode_speed_is_clamped_across_terminal_sizes() {
        let speed_of = |geometry: &CylinderGeometry| {
            geometry.first_col(0, 0, 10) - geometry.first_col(0, 1_000, 10)
        };
        assert!(
            (speed_of(&CylinderGeometry::new(
                40,
                12,
                CylinderSpeedMode::Constant,
                1.0
            )) - CYLINDER_MIN_SPEED)
                .abs()
                < 1e-9
        );
        assert!(
            (speed_of(&CylinderGeometry::new(
                400,
                24,
                CylinderSpeedMode::Constant,
                1.0
            )) - CYLINDER_MAX_SPEED)
                .abs()
                < 1e-9
        );
        assert!((speed_of(&geometry()) - 80.0 / 6.0).abs() < 1e-9);
    }

    #[test]
    fn line_length_mode_crosses_in_traverse_seconds() {
        let geometry = CylinderGeometry::new(80, 24, CylinderSpeedMode::LineLength, 1.0);
        let speed = geometry.first_col(0, 0, 40) - geometry.first_col(0, 1_000, 40);
        assert!((speed - (120.0 / 6.0)).abs() < 1e-9, "{speed}");
        let huge = geometry.first_col(0, 0, 400) - geometry.first_col(0, 1_000, 400);
        assert!((huge - CYLINDER_MAX_SPEED).abs() < 1e-9, "{huge}");
    }

    #[test]
    fn speed_multiplier_scales_both_modes() {
        let doubled = CylinderGeometry::new(80, 24, CylinderSpeedMode::Constant, 2.0);
        let speed = doubled.first_col(0, 0, 10) - doubled.first_col(0, 1_000, 10);
        assert!((speed - 2.0 * 80.0 / 6.0).abs() < 1e-9, "{speed}");
        let by_length = CylinderGeometry::new(80, 24, CylinderSpeedMode::LineLength, 0.5);
        let speed = by_length.first_col(0, 0, 40) - by_length.first_col(0, 1_000, 40);
        assert!((speed - 10.0).abs() < 1e-9, "{speed}");
    }

    #[test]
    fn invalid_multiplier_falls_back_to_one() {
        let geometry = CylinderGeometry::new(80, 24, CylinderSpeedMode::Constant, 0.0);
        let speed = geometry.first_col(0, 0, 10) - geometry.first_col(0, 1_000, 10);
        assert!((speed - 80.0 / 6.0).abs() < 1e-9, "{speed}");
    }

    #[test]
    fn long_line_stays_visible_until_tail_exits() {
        let geometry = geometry();
        let text_width = 200;
        let start = 5_000;
        let speed = geometry.first_col(0, 0, text_width) - geometry.first_col(0, 1_000, text_width);
        let traverse = (80.0 + 200.0) / speed;
        let mid = start + (traverse * 0.5 * 1000.0) as u64;
        assert!(geometry.place(start, mid, text_width).visible);
        let end = start + (traverse * 1000.0) as u64 + 1;
        let placement = geometry.place(start, end, text_width);
        assert!(!placement.visible);
        assert!(placement.fully_left);
    }

    #[test]
    fn line_fully_left_of_area_is_not_visible() {
        let geometry = geometry();
        let placement = geometry.place(5_000, 5_000 + 60_000, 10);
        assert!(!placement.visible);
        assert!(placement.fully_left);
    }

    #[test]
    fn zero_and_negative_elapsed_time_is_not_visible() {
        let geometry = geometry();
        let before = geometry.place(5_000, 4_999, 10);
        assert!(!before.visible);
        assert!(!before.fully_left);
    }

    #[test]
    fn belt_is_flat_and_centered() {
        let geometry = geometry();
        assert_eq!(geometry.belt_y(), 12);
        assert_eq!(
            geometry.belt_y(),
            CylinderGeometry::new(80, 24, CylinderSpeedMode::Constant, 1.0).belt_y()
        );
    }

    #[test]
    fn stretch_rows_are_native_at_center_and_maxed_at_edges() {
        let geometry = geometry();
        assert_eq!(geometry.stretch_rows(40, 4), 4);
        assert_eq!(geometry.stretch_rows(0, 4), 7);
        assert_eq!(geometry.stretch_rows(79, 4), 7);
        assert!(geometry.stretch_rows(60, 4) > 4);
        assert!(geometry.stretch_rows(60, 4) < 7);
        assert_eq!(geometry.stretch_cols(40, 8), 8);
        assert_eq!(geometry.stretch_cols(0, 8), 14);
        assert_eq!(geometry.stretch_cols(79, 8), 14);
    }

    #[test]
    fn stretch_grows_symmetrically_around_the_belt() {
        let geometry = geometry();
        let center = geometry.glyph_top(4);
        assert_eq!(center, geometry.belt_y() - 1);
        let edge_top = geometry.glyph_top(7);
        let belt = geometry.belt_y();
        assert_eq!(belt - edge_top, 3);
        assert_eq!(edge_top + 6 - belt, 3);
    }

    #[test]
    fn glyph_top_never_underflows_at_the_top_row() {
        let geometry = CylinderGeometry::new(80, 2, CylinderSpeedMode::Constant, 1.0);
        assert_eq!(geometry.belt_y(), 1);
        assert_eq!(geometry.glyph_top(7), 0);
    }

    #[test]
    fn depth_is_one_at_center_and_zero_at_edges() {
        let geometry = geometry();
        assert!((geometry.depth(40) - 1.0).abs() < 1e-9);
        assert!(geometry.depth(0).abs() < 1e-9);
        assert!(geometry.depth(79) < 0.1);
        assert!(geometry.depth(20) > 0.0 && geometry.depth(20) < 1.0);
    }
}

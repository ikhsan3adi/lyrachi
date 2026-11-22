use super::{
    CYLINDER_MAX_SPEED, CYLINDER_MAX_STRETCH, CYLINDER_MIN_SPEED, CYLINDER_TRAVERSE_SECONDS,
};

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
    speed: f64,
}

impl CylinderGeometry {
    pub fn new(width: u16, height: u16) -> Self {
        let width = width.max(1);
        let height = height.max(1);
        let speed = (f64::from(width) / CYLINDER_TRAVERSE_SECONDS)
            .clamp(CYLINDER_MIN_SPEED, CYLINDER_MAX_SPEED);
        Self {
            width,
            height,
            speed,
        }
    }

    pub fn bend_rows(&self) -> u16 {
        (self.height / 16).clamp(1, 3)
    }

    pub fn first_col(&self, start_ms: u64, effective_ms: u64) -> f64 {
        let elapsed_sec = (effective_ms as f64 - start_ms as f64) / 1000.0;
        f64::from(self.width - 1) - elapsed_sec * self.speed
    }

    pub fn place(
        &self,
        start_ms: u64,
        effective_ms: u64,
        text_width: u16,
    ) -> CylinderLinePlacement {
        let started = effective_ms >= start_ms;
        let first_col = self.first_col(start_ms, effective_ms);
        let tail = first_col + f64::from(text_width);
        let visible = started && first_col < f64::from(self.width) && tail > 0.0;
        CylinderLinePlacement {
            first_col,
            visible,
            fully_left: tail <= 0.0,
        }
    }

    pub fn screen_y(&self, screen_x: i32) -> u16 {
        let t = self.bend_t(screen_x);
        let y_offset = (f64::from(self.bend_rows()) * t * t).round();
        let center_y = i32::from(self.height / 2);
        (center_y - y_offset as i32).clamp(0, i32::from(self.height - 1)) as u16
    }

    pub fn depth(&self, screen_x: i32) -> f64 {
        1.0 - self.bend_t(screen_x) * self.bend_t(screen_x)
    }

    pub fn stretch_rows(&self, screen_x: i32, base_rows: u16) -> u16 {
        let t = self.bend_t(screen_x);
        let factor = 1.0 + CYLINDER_MAX_STRETCH * t * t;
        let rows = (f64::from(base_rows) * factor).round();
        (rows as u16).max(1)
    }

    pub fn glyph_top(&self, screen_x: i32, out_rows: u16) -> u16 {
        self.screen_y(screen_x)
            .saturating_sub(out_rows.saturating_sub(1) / 2)
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
        CylinderGeometry::new(80, 24)
    }

    #[test]
    fn first_character_enters_at_right_edge_at_start_ms() {
        let geometry = geometry();
        let first_col = geometry.first_col(5_000, 5_000);
        assert!((first_col - 79.0).abs() < 1e-9, "{first_col}");
        assert!(geometry.place(5_000, 5_000, 10).visible);
    }

    #[test]
    fn one_second_moves_the_line_by_speed_columns() {
        let geometry = geometry();
        let before = geometry.first_col(5_000, 5_000);
        let after = geometry.first_col(5_000, 6_000);
        let speed = before - after;
        assert!((speed - (80.0 / 6.0)).abs() < 1e-9, "{speed}");
    }

    #[test]
    fn speed_is_clamped_across_terminal_sizes() {
        let speed_of =
            |geometry: &CylinderGeometry| geometry.first_col(0, 0) - geometry.first_col(0, 1_000);
        assert!((speed_of(&CylinderGeometry::new(40, 12)) - CYLINDER_MIN_SPEED).abs() < 1e-9);
        assert!((speed_of(&CylinderGeometry::new(400, 24)) - CYLINDER_MAX_SPEED).abs() < 1e-9);
        assert!((speed_of(&geometry()) - 80.0 / 6.0).abs() < 1e-9);
    }

    #[test]
    fn long_line_stays_visible_until_tail_exits() {
        let geometry = geometry();
        let text_width = 200;
        let start = 5_000;
        let speed = geometry.first_col(0, 0) - geometry.first_col(0, 1_000);
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
    fn edges_are_higher_than_center_by_bend_rows() {
        let geometry = CylinderGeometry::new(80, 24);
        let bend_rows = geometry.bend_rows();
        assert_eq!(bend_rows, 1);
        let center_y = geometry.screen_y(40);
        let left_y = geometry.screen_y(0);
        let right_y = geometry.screen_y(79);
        assert_eq!(center_y, 12);
        assert_eq!(center_y - left_y, bend_rows);
        assert_eq!(center_y - right_y, bend_rows);
        assert_eq!(left_y, right_y);
    }

    #[test]
    fn stretch_rows_are_native_at_center_and_maxed_at_edges() {
        let geometry = geometry();
        assert_eq!(geometry.stretch_rows(40, 4), 4);
        assert_eq!(geometry.stretch_rows(0, 4), 7);
        assert_eq!(geometry.stretch_rows(79, 4), 7);
        assert!(geometry.stretch_rows(60, 4) > 4);
        assert!(geometry.stretch_rows(60, 4) < 7);
    }

    #[test]
    fn stretch_grows_symmetrically_around_the_belt() {
        let geometry = geometry();
        let center = geometry.glyph_top(40, 4);
        assert_eq!(center, geometry.screen_y(40) - 1);
        let edge_top = geometry.glyph_top(0, 7);
        let edge_belt = geometry.screen_y(0);
        assert_eq!(edge_belt - edge_top, 3);
        assert_eq!(edge_top + 6 - edge_belt, 3);
    }

    #[test]
    fn glyph_top_never_underflows_at_the_top_row() {
        let geometry = CylinderGeometry::new(80, 2);
        assert_eq!(geometry.screen_y(0), 0);
        assert_eq!(geometry.glyph_top(0, 7), 0);
    }

    #[test]
    fn bend_rows_clamp_between_one_and_three() {
        assert_eq!(CylinderGeometry::new(80, 12).bend_rows(), 1);
        assert_eq!(CylinderGeometry::new(80, 32).bend_rows(), 2);
        assert_eq!(CylinderGeometry::new(80, 48).bend_rows(), 3);
        assert_eq!(CylinderGeometry::new(80, 200).bend_rows(), 3);
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

use glam::camera::rh::proj::opengl as proj;
use glam::{Mat4, Vec3};

use super::{CANVAS3D_FOV_Y_DEG, CANVAS3D_PLANE_Y_OFFSET, CANVAS3D_TILT_DEG};

/// Perspective camera over the tilted crawl plane.
///
/// The camera sits at the origin looking down the -Z axis. Plane-local
/// coordinates `(u, v)` have `u` pointing right and `v` increasing away
/// from the viewer along the tilted plane; the plane is pushed down by
/// `CANVAS3D_PLANE_Y_OFFSET` so near text exits through the bottom edge.
pub struct CrawlCamera {
    width_px: f32,
    height_px: f32,
    projection: Mat4,
    cos_tilt: f32,
    sin_tilt: f32,
}

impl CrawlCamera {
    pub fn new(width: u16, height: u16) -> Self {
        let width_px = f32::from(width) * 2.0;
        let height_px = f32::from(height) * 4.0;
        let aspect = width_px / height_px;
        let fov = CANVAS3D_FOV_Y_DEG.to_radians();
        let tilt = CANVAS3D_TILT_DEG.to_radians();
        Self {
            width_px,
            height_px,
            projection: proj::perspective(fov, aspect, 0.1, 400.0),
            cos_tilt: tilt.cos(),
            sin_tilt: tilt.sin(),
        }
    }

    /// Projects a plane-local point to virtual pixel coordinates
    /// (`x` right, `y` down) plus the view distance, or `None` when the
    /// point is clipped by the near plane or the NDC frustum. `w` is the
    /// out-of-plane offset, positive toward the viewer.
    pub fn project(&self, u: f32, v: f32, w: f32) -> Option<(f32, f32, f32)> {
        let world = Vec3::new(
            u,
            v * self.cos_tilt + w * self.sin_tilt - CANVAS3D_PLANE_Y_OFFSET,
            -(v * self.sin_tilt - w * self.cos_tilt),
        );
        let clip = self.projection * world.extend(1.0);
        if clip.w <= 0.1 {
            return None;
        }
        let ndc = clip.truncate() / clip.w;
        if !(ndc.x > -1.0 && ndc.x < 1.0 && ndc.y > -1.0 && ndc.y < 1.0) {
            return None;
        }
        let x = (ndc.x * 0.5 + 0.5) * self.width_px;
        let y = (1.0 - (ndc.y * 0.5 + 0.5)) * self.height_px;
        Some((x, y, -world.z))
    }
}

#[cfg(test)]
mod tests {
    use glam::Vec4;

    use super::*;

    fn camera() -> CrawlCamera {
        CrawlCamera::new(120, 40)
    }

    #[test]
    fn far_points_render_higher_and_smaller() {
        let camera = camera();
        let (near_x, near_y, _) = camera.project(0.0, 8.0, 0.0).expect("near point visible");
        let (far_x, far_y, _) = camera.project(0.0, 25.0, 0.0).expect("far point visible");
        assert!(far_y < near_y, "far text sits higher: {far_y} vs {near_y}");
        let spread_near = camera.project(1.0, 8.0, 0.0).expect("visible").0 - near_x;
        let spread_far = camera.project(1.0, 25.0, 0.0).expect("visible").0 - far_x;
        assert!(
            spread_near > spread_far * 1.5,
            "near text must be magnified: {spread_near} vs {spread_far}"
        );
    }

    #[test]
    fn points_beyond_the_frustum_are_clipped() {
        let camera = camera();
        assert!(camera.project(0.0, 0.0, 0.0).is_none(), "near plane clip");
        assert!(camera.project(500.0, 15.0, 0.0).is_none(), "side clip");
        assert!(camera.project(-500.0, 15.0, 0.0).is_none(), "side clip");
    }

    #[test]
    fn center_point_maps_near_the_virtual_center() {
        let camera = camera();
        let (x, y, _) = camera.project(0.0, 10.0, 0.0).expect("visible");
        assert!((x - camera_width(&camera) / 2.0).abs() < 2.0, "{x}");
        assert!(y > camera_height(&camera) * 0.15, "below the top: {y}");
        assert!(y < camera_height(&camera) * 0.8, "above the bottom: {y}");
    }

    #[test]
    fn virtual_resolution_is_two_by_four_subpixels_per_cell() {
        let camera = CrawlCamera::new(60, 16);
        assert_eq!(camera_width(&camera), 120.0);
        assert_eq!(camera_height(&camera), 64.0);
    }

    fn camera_width(camera: &CrawlCamera) -> f32 {
        camera.width_px
    }

    fn camera_height(camera: &CrawlCamera) -> f32 {
        camera.height_px
    }

    #[test]
    fn projection_is_horizontally_symmetric_around_the_center() {
        let camera = camera();
        let width_px = camera_width(&camera);
        let left = camera.project(-8.0, 18.0, 0.0).expect("visible").0;
        let right = camera.project(8.0, 18.0, 0.0).expect("visible").0;
        assert!((left + right - width_px).abs() < 1e-3, "{left} + {right}");
    }

    #[test]
    fn out_of_plane_offset_moves_points_toward_the_viewer() {
        let camera = camera();
        let (_, _, flat) = camera.project(0.0, 10.0, 0.0).expect("visible");
        let (x, y, lifted) = camera.project(0.0, 10.0, 1.0).expect("visible");
        assert!(lifted < flat, "lifted point is closer: {lifted} vs {flat}");
        assert!((x - camera_width(&camera) / 2.0).abs() < 2.0);
        assert!(y > 0.0);
    }

    #[test]
    fn vec4_helper_keeps_homogeneous_division_stable() {
        let clip = Vec4::new(2.0, 4.0, 8.0, 2.0);
        let ndc = clip.truncate() / clip.w;
        assert_eq!(ndc, Vec3::new(1.0, 2.0, 4.0));
    }
}

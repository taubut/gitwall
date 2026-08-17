//! Drawing sheared slices as textured geometry.
//!
//! The one non-obvious trick in this file: a slice is a parallelogram, but the
//! wallpaper inside it must stay upright. If you map the texture's unit square
//! onto a sheared quad, the image shears with it. So instead we compute each
//! vertex's UV from its *screen position* through an axis-aligned mapping — the
//! net texture-to-screen transform then has no shear in it, and the
//! parallelogram behaves as a window onto an upright image.
//!
//! The CSS version needed a counter-skewed wrapper plus `overflow: hidden` to
//! fake this. Here it falls out of the vertex maths for free.

use egui::epaint::{Mesh, Vertex};
use egui::{Color32, Pos2, TextureId, Vec2};

/// Slice corners, in winding order: top-left, top-right, bottom-right,
/// bottom-left. The top edge is offset right of the bottom edge, matching
/// skwd-wall's positive `skewOffset`.
pub type Quad = [Pos2; 4];

pub fn slice_quad(left: f32, top: f32, w: f32, h: f32, skew: f32) -> Quad {
    [
        Pos2::new(left + skew, top),
        Pos2::new(left + skew + w, top),
        Pos2::new(left + w, top + h),
        Pos2::new(left, top + h),
    ]
}

/// Where an image sits in screen space when scaled to cover a region.
pub struct Cover {
    origin: Pos2,
    size: Vec2,
}

impl Cover {
    /// Scale `img` (its pixel dimensions) so it covers at least `need`, centred
    /// on `centre`.
    ///
    /// `need` must include the shear's horizontal sweep, otherwise the sheared
    /// corners sample outside the texture and clamp-to-edge smears them.
    pub fn new(centre: Pos2, need: Vec2, img: Vec2) -> Self {
        let img = Vec2::new(img.x.max(1.0), img.y.max(1.0));
        let scale = (need.x / img.x).max(need.y / img.y);
        let size = img * scale;
        Self {
            origin: centre - size * 0.5,
            size,
        }
    }

    pub fn uv(&self, p: Pos2) -> Pos2 {
        Pos2::new(
            (p.x - self.origin.x) / self.size.x,
            (p.y - self.origin.y) / self.size.y,
        )
    }
}

/// Outline of a quad with rounded corners.
///
/// The corners of a parallelogram are not right angles, so this rounds with a
/// quadratic Bezier that uses the corner as its control point — visually the
/// same as a radius, and it degrades gracefully on the sharp corners.
pub fn rounded_outline(q: &Quad, radius: f32, segments: usize) -> Vec<Pos2> {
    let n = q.len();
    let mut out = Vec::with_capacity(n * (segments + 1));

    for i in 0..n {
        let prev = q[(i + n - 1) % n];
        let cur = q[i];
        let next = q[(i + 1) % n];

        let to_prev = prev - cur;
        let to_next = next - cur;

        // Never inset more than half an edge, or narrow slices produce
        // crossing arcs.
        let limit = to_prev.length().min(to_next.length()) * 0.5;
        let r = radius.min(limit);

        if r <= 0.01 {
            out.push(cur);
            continue;
        }

        let a = cur + to_prev.normalized() * r;
        let b = cur + to_next.normalized() * r;

        for s in 0..=segments {
            let t = s as f32 / segments as f32;
            let mt = 1.0 - t;
            out.push(Pos2::new(
                mt * mt * a.x + 2.0 * mt * t * cur.x + t * t * b.x,
                mt * mt * a.y + 2.0 * mt * t * cur.y + t * t * b.y,
            ));
        }
    }

    out
}

/// Fill a convex outline with a texture, deriving each vertex's UV from its
/// position. Fanning from the centroid is safe because the outline is convex.
pub fn textured_polygon(
    outline: &[Pos2],
    texture: TextureId,
    cover: &Cover,
    tint: Color32,
) -> Mesh {
    let mut mesh = Mesh::with_texture(texture);
    if outline.len() < 3 {
        return mesh;
    }

    let mut centroid = Vec2::ZERO;
    for p in outline {
        centroid += p.to_vec2();
    }
    let centroid = (centroid / outline.len() as f32).to_pos2();

    mesh.vertices.push(Vertex {
        pos: centroid,
        uv: cover.uv(centroid),
        color: tint,
    });
    for p in outline {
        mesh.vertices.push(Vertex {
            pos: *p,
            uv: cover.uv(*p),
            color: tint,
        });
    }

    let n = outline.len() as u32;
    for i in 0..n {
        mesh.add_triangle(0, 1 + i, 1 + (i + 1) % n);
    }

    mesh
}

/// Flat-coloured convex polygon, for scrims and shadows.
pub fn solid_polygon(outline: &[Pos2], color: Color32) -> Mesh {
    let mut mesh = Mesh::default();
    if outline.len() < 3 {
        return mesh;
    }

    let mut centroid = Vec2::ZERO;
    for p in outline {
        centroid += p.to_vec2();
    }
    let centroid = (centroid / outline.len() as f32).to_pos2();

    mesh.vertices.push(Vertex {
        pos: centroid,
        uv: egui::epaint::WHITE_UV,
        color,
    });
    for p in outline {
        mesh.vertices.push(Vertex {
            pos: *p,
            uv: egui::epaint::WHITE_UV,
            color,
        });
    }

    let n = outline.len() as u32;
    for i in 0..n {
        mesh.add_triangle(0, 1 + i, 1 + (i + 1) % n);
    }

    mesh
}

/// Soft radial glow: a triangle fan with the colour at the centre and full
/// transparency at the rim.
///
/// egui has no gradient shape, and the start screen is otherwise a flat void.
/// Premultiplied alpha means the rim must fade to transparent *black*, not to a
/// transparent version of the colour, or the edge shows a ring.
pub fn radial_glow(centre: Pos2, radius: f32, colour: Color32, segments: usize) -> Mesh {
    let mut mesh = Mesh::default();
    let uv = egui::epaint::WHITE_UV;
    let segments = segments.max(6);

    mesh.vertices.push(Vertex {
        pos: centre,
        uv,
        color: colour,
    });
    for i in 0..segments {
        let a = i as f32 / segments as f32 * std::f32::consts::TAU;
        mesh.vertices.push(Vertex {
            pos: centre + Vec2::new(a.cos(), a.sin()) * radius,
            uv,
            color: Color32::TRANSPARENT,
        });
    }
    let n = segments as u32;
    for i in 0..n {
        mesh.add_triangle(0, 1 + i, 1 + (i + 1) % n);
    }
    mesh
}

/// Vertical linear gradient as a single quad with per-vertex colours — the
/// cheapest way to get a gradient out of egui, which has no gradient shape.
pub fn vertical_gradient(rect: egui::Rect, top: Color32, bottom: Color32) -> Mesh {
    let mut mesh = Mesh::default();
    let uv = egui::epaint::WHITE_UV;
    for (pos, color) in [
        (rect.left_top(), top),
        (rect.right_top(), top),
        (rect.right_bottom(), bottom),
        (rect.left_bottom(), bottom),
    ] {
        mesh.vertices.push(Vertex { pos, uv, color });
    }
    mesh.add_triangle(0, 1, 2);
    mesh.add_triangle(0, 2, 3);
    mesh
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slice_quad_leans_top_edge_to_the_right() {
        let q = slice_quad(100.0, 10.0, 50.0, 200.0, 20.0);
        assert_eq!(q[0], Pos2::new(120.0, 10.0), "top-left carries the skew");
        assert_eq!(q[3], Pos2::new(100.0, 210.0), "bottom-left does not");
        assert!(q[0].x > q[3].x, "top edge must sit right of the bottom edge");
    }

    #[test]
    fn cover_uvs_stay_inside_the_texture_across_the_whole_sheared_quad() {
        // A wide slice of a 16:9 image, sheared hard: every corner must still
        // sample inside [0,1] or the edges smear.
        let (left, top, w, h, skew) = (0.0, 0.0, 300.0, 400.0, 80.0);
        let q = slice_quad(left, top, w, h, skew);

        let centre = Pos2::new(left + w * 0.5 + skew * 0.5, top + h * 0.5);
        let cover = Cover::new(
            centre,
            Vec2::new(w + skew, h),
            Vec2::new(1920.0, 1080.0),
        );

        for p in rounded_outline(&q, 12.0, 4) {
            let uv = cover.uv(p);
            assert!(
                (-0.001..=1.001).contains(&uv.x) && (-0.001..=1.001).contains(&uv.y),
                "uv {uv:?} escaped the texture for corner {p:?}"
            );
        }
    }

    #[test]
    fn uv_mapping_has_no_shear_so_the_image_stays_upright() {
        let q = slice_quad(0.0, 0.0, 200.0, 300.0, 60.0);
        let cover = Cover::new(
            Pos2::new(130.0, 150.0),
            Vec2::new(260.0, 300.0),
            Vec2::new(1000.0, 1000.0),
        );

        // Two points at the same screen height must share a v, and two at the
        // same screen x must share a u. That is precisely "no shear".
        let a = cover.uv(Pos2::new(10.0, 100.0));
        let b = cover.uv(Pos2::new(190.0, 100.0));
        assert!((a.y - b.y).abs() < 1e-6, "same y must give same v");

        let c = cover.uv(Pos2::new(50.0, 20.0));
        let d = cover.uv(Pos2::new(50.0, 280.0));
        assert!((c.x - d.x).abs() < 1e-6, "same x must give same u");
    }

    #[test]
    fn rounding_never_collapses_a_narrow_slice() {
        // A 12px-wide slice with a 12px radius must not fold in on itself.
        let q = slice_quad(0.0, 0.0, 12.0, 400.0, 5.0);
        let outline = rounded_outline(&q, 12.0, 4);
        let xs: Vec<f32> = outline.iter().map(|p| p.x).collect();
        let width = xs.iter().cloned().fold(f32::MIN, f32::max)
            - xs.iter().cloned().fold(f32::MAX, f32::min);
        assert!(width > 10.0, "outline width collapsed to {width}");
    }

    #[test]
    fn glow_fades_to_transparent_at_the_rim() {
        let g = radial_glow(Pos2::new(0.0, 0.0), 100.0, Color32::from_rgb(80, 90, 120), 12);
        assert_eq!(g.vertices.len(), 13, "centre plus one ring");
        assert!(g.vertices[0].color.a() > 0, "centre is visible");
        for v in &g.vertices[1..] {
            assert_eq!(v.color, Color32::TRANSPARENT, "rim must be fully transparent");
        }
        assert_eq!(g.indices.len(), 12 * 3);
    }

    #[test]
    fn polygon_mesh_is_fully_triangulated() {
        let q = slice_quad(0.0, 0.0, 100.0, 100.0, 10.0);
        let outline = rounded_outline(&q, 8.0, 3);
        let mesh = solid_polygon(&outline, Color32::WHITE);
        assert_eq!(mesh.vertices.len(), outline.len() + 1, "centroid + ring");
        assert_eq!(mesh.indices.len(), outline.len() * 3, "one triangle per edge");
    }
}

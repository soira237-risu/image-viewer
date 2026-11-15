//! EXIF orientation as a texture-coordinate transform, so rotated photos are shown upright
//! without touching their pixels.

use iv_core::Orientation;

/// Affine map from displayed UV (0..1 over the upright image) to stored-texture UV:
/// `stored = [a b c; d e f] · [u v 1]`.
pub type UvTransform = [[f32; 3]; 2];

pub fn uv_transform(o: Orientation) -> UvTransform {
    match o {
        Orientation::Normal => [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
        Orientation::FlipH => [[-1.0, 0.0, 1.0], [0.0, 1.0, 0.0]],
        Orientation::Rotate180 => [[-1.0, 0.0, 1.0], [0.0, -1.0, 1.0]],
        Orientation::FlipV => [[1.0, 0.0, 0.0], [0.0, -1.0, 1.0]],
        Orientation::Transpose => [[0.0, 1.0, 0.0], [1.0, 0.0, 0.0]],
        Orientation::Rotate90 => [[0.0, 1.0, 0.0], [-1.0, 0.0, 1.0]],
        Orientation::Transverse => [[0.0, -1.0, 1.0], [-1.0, 0.0, 1.0]],
        Orientation::Rotate270 => [[0.0, -1.0, 1.0], [1.0, 0.0, 0.0]],
    }
}

pub fn apply(t: &UvTransform, u: f32, v: f32) -> (f32, f32) {
    (t[0][0] * u + t[0][1] * v + t[0][2], t[1][0] * u + t[1][1] * v + t[1][2])
}

#[cfg(test)]
mod tests {
    use super::*;

    type Grid = Vec<Vec<u32>>;

    fn mirror_h(g: &Grid) -> Grid {
        g.iter().map(|row| row.iter().rev().copied().collect()).collect()
    }

    /// Rotate 90° clockwise.
    fn rot_cw(g: &Grid) -> Grid {
        let (h, w) = (g.len(), g[0].len());
        (0..w).map(|x| (0..h).rev().map(|y| g[y][x]).collect()).collect()
    }

    /// The EXIF definitions, built independently from mirror + clockwise rotations.
    fn display(stored: &Grid, o: Orientation) -> Grid {
        let rot = |g: &Grid, n: usize| (0..n).fold(g.clone(), |acc, _| rot_cw(&acc));
        match o {
            Orientation::Normal => stored.clone(),
            Orientation::FlipH => mirror_h(stored),
            Orientation::Rotate180 => rot(stored, 2),
            Orientation::FlipV => rot(&mirror_h(stored), 2),
            Orientation::Transpose => rot(&mirror_h(stored), 3),
            Orientation::Rotate90 => rot(stored, 1),
            Orientation::Transverse => rot(&mirror_h(stored), 1),
            Orientation::Rotate270 => rot(stored, 3),
        }
    }

    #[test]
    fn every_orientation_matches_the_exif_definition() {
        // 3 wide, 2 tall, every pixel distinct
        let stored: Grid = vec![vec![0, 1, 2], vec![3, 4, 5]];
        for value in 1..=8 {
            let o = Orientation::from_exif(value);
            let expected = display(&stored, o);
            let (dh, dw) = (expected.len(), expected[0].len());
            assert_eq!(o.swaps_dimensions(), dw == 2, "{o:?}");

            let t = uv_transform(o);
            for (y, row) in expected.iter().enumerate() {
                for (x, want) in row.iter().enumerate() {
                    // sample at the pixel centre
                    let (s, tt) = apply(&t, (x as f32 + 0.5) / dw as f32, (y as f32 + 0.5) / dh as f32);
                    let sx = (s * 3.0).floor() as usize;
                    let sy = (tt * 2.0).floor() as usize;
                    assert_eq!(stored[sy][sx], *want, "{o:?} at display ({x},{y})");
                }
            }
        }
    }
}

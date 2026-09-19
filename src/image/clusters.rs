// transcode/src/image/clusters.rs — Connected-component labelling on a diff mask.
//
// Two-pass CCL (Suzuki-Abe-style) over a byte mask, then a 4-connectivity
// bounding-box extraction per component. This module is the spatial clustering
// primitive that mirrors upstream `pixelhog`'s `Comparison::clusters()` method,
// on the fold substrate: zero dependencies beyond the fold substrate, output
// is a `Vec<BoundingBox>` sorted by `(y, x)` for deterministic rendering.
//
// Quality column: cluster count + tightness. Speed column: pixels/sec.

/// Axis-aligned bounding box on the original image's pixel grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BoundingBox {
    /// Inclusive left column.
    pub x_min: u32,
    /// Inclusive top row.
    pub y_min: u32,
    /// Inclusive right column.
    pub x_max: u32,
    /// Inclusive bottom row.
    pub y_max: u32,
}

impl BoundingBox {
    /// Width of the box in pixels.
    pub fn width(&self) -> u32 {
        self.x_max - self.x_min + 1
    }

    /// Height of the box in pixels.
    pub fn height(&self) -> u32 {
        self.y_max - self.y_min + 1
    }

    /// Number of pixels in the box (area).
    pub fn area(&self) -> u32 {
        self.width() * self.height()
    }

    /// Expand the box to include `(x, y)`. No-op if `(x, y)` is already inside.
    pub fn expand(&mut self, x: u32, y: u32) {
        if x < self.x_min { self.x_min = x; }
        if y < self.y_min { self.y_min = y; }
        if x > self.x_max { self.x_max = x; }
        if y > self.y_max { self.y_max = y; }
    }
}

/// One connected component on the diff mask.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cluster {
    /// Background label (a component the mask never uses).
    pub id: u32,
    /// Bounding box of all mask pixels in this component.
    pub bbox: BoundingBox,
}

/// Two-pass connected-component labelling on a `width x height` byte mask.
///
/// `mask[w + h*width]` is truthy if the pixel at `(w, h)` belongs to a
/// change-region. 4-connectivity is used (no diagonal links).
///
/// Returns a `Vec<Cluster>` sorted by `(y_min, x_min)`. Background label `0`
/// is reserved and never returned.
pub fn clusters(mask: &[u8], width: u32, height: u32) -> Vec<Cluster> {
    let w = width as usize;
    let h = height as usize;
    // Probe-before-merge (BND-4): reject malformed masks at the boundary, never panic on input.
    if w == 0 || h == 0 || mask.len() != w.saturating_mul(h) {
        return Vec::new();
    }

    // Phase 1: raster scan, assign provisional labels and record equivalences.
    let mut labels = vec![0u32; w * h];
    let mut parents: Vec<u32> = vec![0]; // parents[label] = parent, root has itself
    let mut next_label = 1u32;

    let find = |parents: &mut Vec<u32>, mut x: u32| -> u32 {
        while parents[x as usize] != x {
            parents[x as usize] = parents[parents[x as usize] as usize]; // path halving
            x = parents[x as usize];
        }
        x
    };
    let union = |parents: &mut Vec<u32>, a: u32, b: u32| {
        let ra = find(parents, a);
        let rb = find(parents, b);
        if ra != rb {
            let (lo, hi) = if ra < rb { (ra, rb) } else { (rb, ra) };
            parents[hi as usize] = lo;
        }
    };

    for y in 0..h {
        for x in 0..w {
            if mask[y * w + x] == 0 {
                continue;
            }
            let left = if x > 0 { labels[y * w + (x - 1)] } else { 0 };
            let up = if y > 0 { labels[(y - 1) * w + x] } else { 0 };

            match (left, up) {
                (0, 0) => {
                    labels[y * w + x] = next_label;
                    parents.push(next_label);
                    next_label += 1;
                }
                (l, 0) => labels[y * w + x] = l,
                (0, u) => labels[y * w + x] = u,
                (l, u) => {
                    labels[y * w + x] = l;
                    union(&mut parents, l, u);
                }
            }
        }
    }

    // Resolve to root labels and compute a bbox per root.
    //
    // This used a `hashtrinity::Table` before the module was lost in the repo
    // split; that dependency was incidental — the type is only ever used as a
    // map. A std BTreeMap keeps transcode dependency-free and, unlike a hash
    // map, iterates in a deterministic order, so cluster ids are reproducible
    // run to run rather than merely sorted after the fact.
    let mut bboxes: std::collections::BTreeMap<u32, BoundingBox> =
        std::collections::BTreeMap::new();
    for y in 0..h {
        for x in 0..w {
            let l = labels[y * w + x];
            if l == 0 {
                continue;
            }
            let root = find(&mut parents, l);
            match bboxes.get_mut(&root) {
                Some(bbox) => bbox.expand(x as u32, y as u32),
                None => {
                    bboxes.insert(
                        root,
                        BoundingBox {
                            x_min: x as u32,
                            y_min: y as u32,
                            x_max: x as u32,
                            y_max: y as u32,
                        },
                    );
                }
            }
        }
    }

    let mut out: Vec<Cluster> = bboxes
        .into_iter()
        .map(|(id, bbox)| Cluster { id, bbox })
        .collect();
    out.sort_by_key(|c| (c.bbox.y_min, c.bbox.x_min));
    out
}

/// Pixel-perfect diff mask: 1 where the pixel's RGBA differs, 0 where it matches.
/// One byte per pixel, length = `width * height`.
pub fn diff_mask(a: &crate::Image, b: &crate::Image) -> Option<Vec<u8>> {
    if a.width != b.width || a.height != b.height {
        return None;
    }
    let npix = (a.width as usize) * (a.height as usize);
    if a.pixels.len() != npix * 4 || b.pixels.len() != npix * 4 {
        return None;
    }
    let mut m = Vec::with_capacity(npix);
    for i in 0..npix {
        let p = i * 4;
        let diff = a.pixels[p] != b.pixels[p]
            || a.pixels[p + 1] != b.pixels[p + 1]
            || a.pixels[p + 2] != b.pixels[p + 2]
            || a.pixels[p + 3] != b.pixels[p + 3];
        m.push(if diff { 1 } else { 0 });
    }
    Some(m)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(w: u32, h: u32, fill: [u8; 4]) -> crate::Image {
        let mut pixels = Vec::with_capacity((w * h * 4) as usize);
        for _ in 0..(w * h) {
            pixels.extend_from_slice(&fill);
        }
        crate::Image {
            width: w,
            height: h,
            pixels,
        }
    }

    fn mask_str(mask: &[u8], width: u32) -> String {
        let mut s = String::new();
        for row in mask.chunks(width as usize) {
            for &b in row {
                s.push(if b != 0 { '#' } else { '.' });
            }
            s.push('\n');
        }
        s
    }

    #[test]
    fn single_rectangle_one_cluster() {
        // 6x4 image, 2x2 red square at (2,1)
        let mut img = image(6, 4, [0, 0, 0, 255]);
        for y in 1..3 {
            for x in 2..4 {
                let p = ((y * 6 + x) * 4) as usize;
                img.pixels[p] = 255;
            }
        }
        let mask = diff_mask(&image(6, 4, [0, 0, 0, 255]), &img).unwrap();
        let cs = clusters(&mask, 6, 4);
        assert_eq!(cs.len(), 1);
        assert_eq!(cs[0].bbox, BoundingBox { x_min: 2, y_min: 1, x_max: 3, y_max: 2 });
    }

    #[test]
    fn two_disjoint_rectangles_two_clusters() {
        let mut img = image(8, 4, [0, 0, 0, 255]);
        for y in 0..2 {
            for x in 0..2 {
                img.pixels[((y * 8 + x) * 4) as usize] = 255;
            }
        }
        for y in 2..4 {
            for x in 6..8 {
                img.pixels[((y * 8 + x) * 4) as usize] = 255;
            }
        }
        let mask = diff_mask(&image(8, 4, [0, 0, 0, 255]), &img).unwrap();
        assert_eq!(mask_str(&mask, 8).trim().lines().count(), 4);
        let cs = clusters(&mask, 8, 4);
        assert_eq!(cs.len(), 2, "expected two disjoint clusters, got {cs:?}");
    }

    #[test]
    fn diagonal_pixels_four_clusters_under_4_connectivity() {
        // 4-connectivity, not 8: diagonals are NOT joined. Four diagonal pixels must
        // remain four separate clusters.
        let mut img = image(4, 4, [0, 0, 0, 255]);
        for (y, x) in [(0, 0), (1, 1), (2, 2), (3, 3)] {
            img.pixels[((y * 4 + x) * 4) as usize] = 255;
        }
        let mask = diff_mask(&image(4, 4, [0, 0, 0, 255]), &img).unwrap();
        assert_eq!(mask.len(), 16, "mask must be one byte per pixel");
        let cs = clusters(&mask, 4, 4);
        assert_eq!(
            cs.len(),
            4,
            "diagonal pixels must remain 4 separate clusters under 4-connectivity, got {cs:?}"
        );
    }

    #[test]
    fn empty_mask_zero_clusters() {
        let mask = vec![0u8; 6 * 4];
        let cs = clusters(&mask, 6, 4);
        assert_eq!(cs.len(), 0);
    }
}

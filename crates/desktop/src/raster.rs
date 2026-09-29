//! Anti-aliased glyph rasteriser.
//!
//! Outlines are flattened to line segments and each segment adds its exact
//! signed area to an accumulation buffer; a running sum across every row then
//! yields coverage. This is the technique popularised by `font-rs`: no
//! supersampling, exact analytic anti-aliasing, and about a hundred lines.

use ttf_parser::{Face, GlyphId, OutlineBuilder};

/// A line segment in pixels.
type Segment = ((f32, f32), (f32, f32));

/// Maximum distance, in pixels, between a curve and its flattened polyline.
const TOLERANCE: f32 = 0.1;

/// A rasterised glyph: 8-bit coverage plus placement relative to the pen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bitmap {
    /// Width in pixels.
    pub w: u32,
    /// Height in pixels.
    pub h: u32,
    /// Left edge relative to the pen position.
    pub xmin: i32,
    /// Bottom edge relative to the baseline, y up.
    pub ymin: i32,
    /// Coverage, row-major from the top.
    pub data: Vec<u8>,
}

/// Rasterises `glyph` at `px` pixels per em. `None` for glyphs without ink.
pub fn rasterize(face: &Face<'_>, glyph: GlyphId, px: f32) -> Option<Bitmap> {
    let mut path = Flattener { scale: px / f32::from(face.units_per_em()), ..Flattener::default() };
    face.outline_glyph(glyph, &mut path)?;
    path.close();
    rasterize_lines(&path.lines)
}

/// Rasterises closed polygons given as line segments in pixels, y up.
fn rasterize_lines(lines: &[Segment]) -> Option<Bitmap> {
    // Bounds come from the flattened geometry itself, so no point can fall
    // outside the buffer whatever the font's bounding box claims.
    let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
    for &(a, b) in lines {
        for (x, y) in [a, b] {
            (x0, y0, x1, y1) = (x0.min(x), y0.min(y), x1.max(x), y1.max(y));
        }
    }
    if lines.is_empty() || !(x0.is_finite() && y0.is_finite() && x1.is_finite() && y1.is_finite()) {
        return None;
    }
    let (xmin, ymin) = (x0.floor() as i32, y0.floor() as i32);
    let (xmax, ymax) = (x1.ceil() as i32, y1.ceil() as i32);
    let (w, h) = ((xmax - xmin).max(0) as usize, (ymax - ymin).max(0) as usize);
    if w == 0 || h == 0 || w > 4096 || h > 4096 {
        return None;
    }
    let mut acc = Accumulator::new(w, h);
    let to_buffer = |(x, y): (f32, f32)| (x - xmin as f32, ymax as f32 - y);
    for &(a, b) in lines {
        acc.line(to_buffer(a), to_buffer(b));
    }
    Some(Bitmap { w: w as u32, h: h as u32, xmin, ymin, data: acc.finish() })
}

/// Collects an outline as line segments in pixel space.
#[derive(Default)]
struct Flattener {
    scale: f32,
    lines: Vec<Segment>,
    start: (f32, f32),
    current: (f32, f32),
}

impl Flattener {
    fn point(&self, x: f32, y: f32) -> (f32, f32) {
        (x * self.scale, y * self.scale)
    }

    fn push(&mut self, to: (f32, f32)) {
        if to != self.current {
            self.lines.push((self.current, to));
        }
        self.current = to;
    }
}

/// Segments needed so a curve with control-polygon deviation `dd` (pixels)
/// stays within [`TOLERANCE`].
fn segments(dd: f32, factor: f32) -> u32 {
    ((dd * factor / TOLERANCE).sqrt().ceil() as u32).clamp(1, 64)
}

impl OutlineBuilder for Flattener {
    fn move_to(&mut self, x: f32, y: f32) {
        self.close();
        self.start = self.point(x, y);
        self.current = self.start;
    }

    fn line_to(&mut self, x: f32, y: f32) {
        let p = self.point(x, y);
        self.push(p);
    }

    #[allow(clippy::many_single_char_names, reason = "Bezier maths reads best in textbook notation")]
    fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
        let (p0, p1, p2) = (self.current, self.point(x1, y1), self.point(x, y));
        let dd = (p0.0 - 2.0 * p1.0 + p2.0).hypot(p0.1 - 2.0 * p1.1 + p2.1);
        let n = segments(dd, 0.125);
        for i in 1..=n {
            let t = i as f32 / n as f32;
            let u = 1.0 - t;
            self.push((u * u * p0.0 + 2.0 * u * t * p1.0 + t * t * p2.0, u * u * p0.1 + 2.0 * u * t * p1.1 + t * t * p2.1));
        }
    }

    #[allow(clippy::many_single_char_names, reason = "Bezier maths reads best in textbook notation")]
    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
        let (p0, p1, p2, p3) = (self.current, self.point(x1, y1), self.point(x2, y2), self.point(x, y));
        let dd1 = (p0.0 - 2.0 * p1.0 + p2.0).hypot(p0.1 - 2.0 * p1.1 + p2.1);
        let dd2 = (p1.0 - 2.0 * p2.0 + p3.0).hypot(p1.1 - 2.0 * p2.1 + p3.1);
        let n = segments(dd1.max(dd2), 0.75);
        for i in 1..=n {
            let t = i as f32 / n as f32;
            let u = 1.0 - t;
            let (a, b, c, d) = (u * u * u, 3.0 * u * u * t, 3.0 * u * t * t, t * t * t);
            self.push((a * p0.0 + b * p1.0 + c * p2.0 + d * p3.0, a * p0.1 + b * p1.1 + c * p2.1 + d * p3.1));
        }
    }

    fn close(&mut self) {
        let start = self.start;
        self.push(start);
    }
}

/// Signed-area accumulation buffer. Rows carry two spare cells so a segment
/// touching the right edge never writes into the next row.
struct Accumulator {
    w: usize,
    h: usize,
    stride: usize,
    cells: Vec<f32>,
}

impl Accumulator {
    fn new(w: usize, h: usize) -> Self {
        Self { w, h, stride: w + 2, cells: vec![0.0; (w + 2) * h] }
    }

    /// Adds the segment `p0 -> p1` (buffer coordinates, y down).
    fn line(&mut self, p0: (f32, f32), p1: (f32, f32)) {
        if (p0.1 - p1.1).abs() <= f32::EPSILON {
            return;
        }
        let (dir, (x0, y0), (x1, y1)) = if p0.1 < p1.1 { (1.0, p0, p1) } else { (-1.0, p1, p0) };
        let dxdy = (x1 - x0) / (y1 - y0);
        let (top, bottom) = (y0.max(0.0), y1.min(self.h as f32));
        if top >= bottom {
            return;
        }
        let width = self.w as f32;
        let mut x = x0 + (top - y0) * dxdy;
        for row in top as usize..(bottom.ceil() as usize).min(self.h) {
            let row_top = row as f32;
            let dy = (row_top + 1.0).min(bottom) - row_top.max(top);
            let x_next = x + dxdy * dy;
            let d = dy * dir;
            let (xa, xb) = if x < x_next { (x, x_next) } else { (x_next, x) };
            let (xa, xb) = (xa.clamp(0.0, width), xb.clamp(0.0, width));
            let base = row * self.stride;
            let xa_floor = xa.floor();
            let xai = xa_floor as usize;
            let xb_ceil = xb.ceil();
            let xbi = xb_ceil as usize;
            let cells = &mut self.cells[base..base + self.stride];
            if xbi <= xai + 1 {
                // The segment stays within one pixel column in this row.
                let xmf = 0.5 * (xa + xb) - xa_floor;
                cells[xai] += d - d * xmf;
                cells[xai + 1] += d * xmf;
            } else {
                let s = (xb - xa).recip();
                let x0f = xa - xa_floor;
                let a0 = 0.5 * s * (1.0 - x0f) * (1.0 - x0f);
                let x1f = xb - xb_ceil + 1.0;
                let am = 0.5 * s * x1f * x1f;
                cells[xai] += d * a0;
                if xbi == xai + 2 {
                    cells[xai + 1] += d * (1.0 - a0 - am);
                } else {
                    let a1 = s * (1.5 - x0f);
                    cells[xai + 1] += d * (a1 - a0);
                    for cell in &mut cells[xai + 2..xbi - 1] {
                        *cell += d * s;
                    }
                    let a2 = a1 + (xbi - xai - 3) as f32 * s;
                    cells[xbi - 1] += d * (1.0 - a2 - am);
                }
                cells[xbi] += d * am;
            }
            x = x_next;
        }
    }

    /// Integrates each row into 8-bit coverage (non-zero winding).
    fn finish(self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.w * self.h);
        for row in self.cells.chunks_exact(self.stride) {
            let mut acc = 0.0f32;
            for &cell in &row[..self.w] {
                acc += cell;
                out.push((acc.abs().min(1.0) * 255.0 + 0.5) as u8);
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn polygon(points: &[(f32, f32)]) -> Vec<Segment> {
        points.iter().zip(points.iter().cycle().skip(1)).map(|(&a, &b)| (a, b)).collect()
    }

    fn area(bitmap: &Bitmap) -> f32 {
        bitmap.data.iter().map(|&c| f32::from(c) / 255.0).sum()
    }

    #[test]
    fn exact_area_for_axis_aligned_and_slanted_shapes() {
        let square = rasterize_lines(&polygon(&[(0.5, 0.5), (8.5, 0.5), (8.5, 4.5), (0.5, 4.5)])).unwrap();
        assert_eq!((square.w, square.h, square.xmin, square.ymin), (9, 5, 0, 0));
        assert!((area(&square) - 32.0).abs() < 0.1, "{}", area(&square));
        // Interior pixels are fully covered, edges half.
        assert_eq!(square.data[2 * 9 + 4], 255);
        assert!((i32::from(square.data[2 * 9]) - 128).abs() <= 1);

        let triangle = rasterize_lines(&polygon(&[(0.0, 0.0), (10.0, 0.0), (0.0, 7.0)])).unwrap();
        assert!((area(&triangle) - 35.0).abs() < 0.15, "{}", area(&triangle));
    }

    #[test]
    fn winding_direction_does_not_matter() {
        let cw = rasterize_lines(&polygon(&[(0.0, 0.0), (0.0, 6.0), (6.0, 6.0), (6.0, 0.0)])).unwrap();
        let ccw = rasterize_lines(&polygon(&[(0.0, 0.0), (6.0, 0.0), (6.0, 6.0), (0.0, 6.0)])).unwrap();
        assert_eq!(cw, ccw);
        assert!(rasterize_lines(&[]).is_none());
    }
}

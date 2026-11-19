//! The fallback for terminals without Kitty graphics: each cell shows two
//! pixels, the top one as the foreground of "▀" and the bottom one as its background.

use std::io::Write;

use image::{DynamicImage, RgbaImage};

use crate::files;
use crate::view::View;

/// Top and bottom pixel of a cell; `None` is transparent or outside the image.
pub type Cell = [Option<[u8; 3]>; 2];

/// The image at halving sizes, so a zoomed-out view samples a level close to
/// its own scale instead of skipping over most pixels.
pub struct Pyramid {
    levels: Vec<RgbaImage>,
    sx: f64,
    sy: f64,
}

impl Pyramid {
    pub fn new(img: &DynamicImage) -> Pyramid {
        let base = files::cap(img, 4096, 16_000_000).to_rgba8();
        let sx = base.width() as f64 / img.width().max(1) as f64;
        let sy = base.height() as f64 / img.height().max(1) as f64;
        let mut levels = vec![base];
        loop {
            let last = levels.last().unwrap();
            if last.width() <= 1 && last.height() <= 1 {
                break;
            }
            let next = half(last);
            levels.push(next);
        }
        Pyramid { levels, sx, sy }
    }

    pub fn bytes(&self) -> u64 {
        self.levels.iter().map(|l| l.as_raw().len() as u64).sum()
    }

    /// `footprint` is how many original image pixels one screen sample covers.
    fn level_for(&self, footprint: f64) -> usize {
        let f = footprint * self.sx.max(self.sy);
        if f <= 1.0 {
            0
        } else {
            (f.log2().floor() as usize).min(self.levels.len() - 1)
        }
    }

    /// Samples at a point in original image pixels.
    fn sample(&self, level: usize, x: f64, y: f64, smooth: bool) -> Option<[u8; 3]> {
        let base = &self.levels[0];
        let (x, y) = (x * self.sx, y * self.sy);
        if x < 0.0 || y < 0.0 || x >= base.width() as f64 || y >= base.height() as f64 {
            return None;
        }
        let img = &self.levels[level];
        let f = (1u32 << level) as f64;
        let (w, h) = (img.width() as i64, img.height() as i64);
        let px = |x: i64, y: i64| {
            img.get_pixel(x.clamp(0, w - 1) as u32, y.clamp(0, h - 1) as u32)
                .0
        };
        let rgba = if smooth {
            let (lx, ly) = (x / f - 0.5, y / f - 0.5);
            let (x0, y0) = (lx.floor(), ly.floor());
            let (tx, ty) = (lx - x0, ly - y0);
            let (x0, y0) = (x0 as i64, y0 as i64);
            let (a, b, c, d) = (
                px(x0, y0),
                px(x0 + 1, y0),
                px(x0, y0 + 1),
                px(x0 + 1, y0 + 1),
            );
            let mut out = [0u8; 4];
            for i in 0..4 {
                let top = a[i] as f64 * (1.0 - tx) + b[i] as f64 * tx;
                let bottom = c[i] as f64 * (1.0 - tx) + d[i] as f64 * tx;
                out[i] = (top * (1.0 - ty) + bottom * ty).round() as u8;
            }
            out
        } else {
            px((x / f) as i64, (y / f) as i64)
        };
        (rgba[3] >= 128).then_some([rgba[0], rgba[1], rgba[2]])
    }
}

fn half(src: &RgbaImage) -> RgbaImage {
    let (w, h) = src.dimensions();
    let (nw, nh) = (w.div_ceil(2), h.div_ceil(2));
    RgbaImage::from_fn(nw, nh, |x, y| {
        let (x0, y0) = (2 * x, 2 * y);
        let (x1, y1) = ((x0 + 1).min(w - 1), (y0 + 1).min(h - 1));
        let p = [
            src.get_pixel(x0, y0),
            src.get_pixel(x1, y0),
            src.get_pixel(x0, y1),
            src.get_pixel(x1, y1),
        ];
        let mut out = [0u8; 4];
        for (i, v) in out.iter_mut().enumerate() {
            *v = ((p.iter().map(|q| q.0[i] as u32).sum::<u32>() + 2) / 4) as u8;
        }
        image::Rgba(out)
    })
}

pub fn render(pyr: &Pyramid, view: &View) -> Vec<Cell> {
    let g = view.grid;
    // Each cell half covers cw × ch/2 screen pixels.
    let footprint = g.cw.max(g.ch / 2.0) / view.zoom;
    let level = pyr.level_for(footprint);
    // Zoomed in far enough to see single pixels: keep their edges sharp.
    let smooth = footprint > 0.5;
    let mut cells = Vec::with_capacity((g.cols * g.rows) as usize);
    for row in 0..g.rows {
        for col in 0..g.cols {
            let x = view.ox + (col as f64 + 0.5) * g.cw / view.zoom;
            let half = |k: f64| {
                let y = view.oy + (row as f64 + 0.25 + 0.5 * k) * g.ch / view.zoom;
                pyr.sample(level, x, y, smooth)
            };
            cells.push([half(0.0), half(1.0)]);
        }
    }
    cells
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Color {
    Rgb(u8, u8, u8),
    Indexed(u8),
}

fn color(c: [u8; 3], truecolor: bool) -> Color {
    if truecolor {
        Color::Rgb(c[0], c[1], c[2])
    } else {
        Color::Indexed(to_256(c))
    }
}

/// The nearest color in the 6×6×6 cube or the gray ramp of the 256-color palette.
fn to_256(c: [u8; 3]) -> u8 {
    const LEVELS: [i32; 6] = [0, 95, 135, 175, 215, 255];
    let nearest = |v: u8| -> usize {
        (0..6)
            .min_by_key(|&i| (LEVELS[i] - v as i32).abs())
            .unwrap()
    };
    let (r, g, b) = (nearest(c[0]), nearest(c[1]), nearest(c[2]));
    let dist = |x: [i32; 3]| -> i32 { (0..3).map(|i| (x[i] - c[i] as i32).pow(2)).sum() };
    let cube = dist([LEVELS[r], LEVELS[g], LEVELS[b]]);
    let avg = (c[0] as i32 + c[1] as i32 + c[2] as i32) / 3;
    let step = ((avg - 8 + 5) / 10).clamp(0, 23);
    let gray_level = 8 + 10 * step;
    if dist([gray_level; 3]) < cube {
        (232 + step) as u8
    } else {
        (16 + 36 * r + 6 * g + b) as u8
    }
}

fn sgr(out: &mut Vec<u8>, fg: bool, c: Color) {
    let layer = if fg { 38 } else { 48 };
    let _ = match c {
        Color::Rgb(r, g, b) => write!(out, "\x1b[{layer};2;{r};{g};{b}m"),
        Color::Indexed(i) => write!(out, "\x1b[{layer};5;{i}m"),
    };
}

/// Writes the cells that differ from `prev` (all of them if `prev` is empty).
pub fn encode(out: &mut Vec<u8>, cells: &[Cell], prev: &[Cell], cols: u32, truecolor: bool) {
    let cols = cols.max(1) as usize;
    let same_layout = prev.len() == cells.len();
    let mut fg: Option<Color> = None;
    let mut bg: Option<Color> = None;
    let mut cursor: Option<usize> = None;
    out.extend_from_slice(b"\x1b[0m");
    for (i, cell) in cells.iter().enumerate() {
        if same_layout && prev[i] == *cell {
            continue;
        }
        if cursor != Some(i) {
            let _ = write!(out, "\x1b[{};{}H", i / cols + 1, i % cols + 1);
        }
        let (glyph, f, b) = match *cell {
            [Some(t), Some(b)] if t == b => (" ", None, Some(color(b, truecolor))),
            [Some(t), Some(b)] => ("▀", Some(color(t, truecolor)), Some(color(b, truecolor))),
            [Some(t), None] => ("▀", Some(color(t, truecolor)), None),
            [None, Some(b)] => ("▄", Some(color(b, truecolor)), None),
            [None, None] => (" ", None, None),
        };
        if bg != b {
            match b {
                Some(c) => sgr(out, false, c),
                None => out.extend_from_slice(b"\x1b[49m"),
            }
            bg = b;
        }
        if let Some(f) = f.filter(|&f| fg != Some(f)) {
            sgr(out, true, f);
            fg = Some(f);
        }
        out.extend_from_slice(glyph.as_bytes());
        // With autowrap off, the cursor stays put after the last column.
        cursor = ((i + 1) % cols != 0).then_some(i + 1);
    }
    out.extend_from_slice(b"\x1b[0m");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::view::Grid;

    #[test]
    fn palette() {
        assert_eq!(to_256([0, 0, 0]), 16);
        assert_eq!(to_256([255, 0, 0]), 196);
        assert_eq!(to_256([128, 128, 128]), 244);
    }

    #[test]
    fn renders_an_image_that_fills_the_grid() {
        let img = DynamicImage::ImageRgba8(RgbaImage::from_fn(40, 40, |x, _| {
            if x < 20 {
                image::Rgba([255, 0, 0, 255])
            } else {
                image::Rgba([0, 0, 255, 255])
            }
        }));
        let pyr = Pyramid::new(&img);
        let grid = Grid {
            cols: 4,
            rows: 2,
            cw: 10.0,
            ch: 20.0,
        };
        let view = View::new(40, 40, grid);
        let cells = render(&pyr, &view);
        assert_eq!(cells.len(), 8);
        assert_eq!(cells[0], [Some([255, 0, 0]); 2]);
        assert_eq!(cells[3], [Some([0, 0, 255]); 2]);
    }

    #[test]
    fn encode_skips_unchanged_cells() {
        let a: Cell = [Some([1, 2, 3]), Some([4, 5, 6])];
        let b: Cell = [None, None];
        let mut out = Vec::new();
        encode(&mut out, &[a, b], &[a, a], 2, true);
        let s = String::from_utf8(out).unwrap();
        assert_eq!(s, "\x1b[0m\x1b[1;2H \x1b[0m");
    }
}

//! Zoom and pan, in screen pixels. Both drawing backends share this model.

/// The image area of the terminal: its size in cells and the size of a cell in pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Grid {
    pub cols: u32,
    pub rows: u32,
    pub cw: f64,
    pub ch: f64,
}

impl Grid {
    pub fn width(&self) -> f64 {
        self.cols as f64 * self.cw
    }

    pub fn height(&self) -> f64 {
        self.rows as f64 * self.ch
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Shrink large images to fit; show small ones at 100%.
    BestFit,
    /// Fit the window, enlarging small images.
    Fit,
    /// Wherever the user zoomed to.
    Free,
}

#[derive(Clone, Debug)]
pub struct View {
    pub img_w: f64,
    pub img_h: f64,
    pub grid: Grid,
    /// Screen pixels per image pixel.
    pub zoom: f64,
    /// The image point at the top-left corner of the viewport. Negative when
    /// the image is smaller than the viewport and centered in it.
    pub ox: f64,
    pub oy: f64,
    pub mode: Mode,
}

/// Where to draw the visible part of the image, in the terms the Kitty
/// graphics protocol uses: a starting cell, a pixel offset inside it, a size in
/// whole cells, and the source rectangle in image pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Placement {
    pub col: u32,
    pub row: u32,
    pub x_off: u32,
    pub y_off: u32,
    pub cols: u32,
    pub rows: u32,
    pub src_x: f64,
    pub src_y: f64,
    pub src_w: f64,
    pub src_h: f64,
}

impl View {
    pub fn new(w: u32, h: u32, grid: Grid) -> View {
        let mut v = View {
            img_w: w.max(1) as f64,
            img_h: h.max(1) as f64,
            grid,
            zoom: 1.0,
            ox: 0.0,
            oy: 0.0,
            mode: Mode::BestFit,
        };
        v.apply_mode();
        v
    }

    pub fn fit_zoom(&self) -> f64 {
        (self.grid.width() / self.img_w)
            .min(self.grid.height() / self.img_h)
            .max(1e-6)
    }

    fn min_zoom(&self) -> f64 {
        self.fit_zoom().min(1.0) / 4.0
    }

    fn max_zoom(&self) -> f64 {
        (self.fit_zoom() * 4.0).max(64.0)
    }

    pub fn set_mode(&mut self, mode: Mode) {
        self.mode = mode;
        self.apply_mode();
    }

    fn apply_mode(&mut self) {
        self.zoom = match self.mode {
            Mode::BestFit => self.fit_zoom().min(1.0),
            Mode::Fit => self.fit_zoom(),
            Mode::Free => return,
        };
        self.clamp();
    }

    /// Changes the viewport size, keeping the point at its center in place.
    pub fn set_grid(&mut self, grid: Grid) {
        let (cx, cy) = self.to_image(self.grid.width() / 2.0, self.grid.height() / 2.0);
        self.grid = grid;
        if self.mode != Mode::Free {
            return self.apply_mode();
        }
        self.zoom = self.zoom.clamp(self.min_zoom(), self.max_zoom());
        self.ox = cx - grid.width() / 2.0 / self.zoom;
        self.oy = cy - grid.height() / 2.0 / self.zoom;
        self.clamp();
    }

    pub fn to_image(&self, sx: f64, sy: f64) -> (f64, f64) {
        (self.ox + sx / self.zoom, self.oy + sy / self.zoom)
    }

    /// Zooms by `factor`, keeping the image point under screen point (sx, sy) still.
    pub fn zoom_at(&mut self, factor: f64, sx: f64, sy: f64) {
        self.zoom_to(self.zoom * factor, sx, sy);
    }

    pub fn zoom_to(&mut self, zoom: f64, sx: f64, sy: f64) {
        let (ix, iy) = self.to_image(sx, sy);
        self.zoom = zoom.clamp(self.min_zoom(), self.max_zoom());
        self.ox = ix - sx / self.zoom;
        self.oy = iy - sy / self.zoom;
        self.mode = Mode::Free;
        self.clamp();
    }

    pub fn center(&self) -> (f64, f64) {
        (self.grid.width() / 2.0, self.grid.height() / 2.0)
    }

    /// Moves the image by a screen distance, as dragging it does.
    pub fn pan_by(&mut self, dx: f64, dy: f64) {
        self.ox -= dx / self.zoom;
        self.oy -= dy / self.zoom;
        self.clamp();
    }

    pub fn fits_width(&self) -> bool {
        self.img_w * self.zoom <= self.grid.width() + 0.5
    }

    /// Centers an axis the image fits in, and keeps the image's edges at the
    /// viewport's edges on an axis it overflows.
    fn clamp(&mut self) {
        self.ox = clamp_axis(self.ox, self.img_w, self.grid.width() / self.zoom);
        self.oy = clamp_axis(self.oy, self.img_h, self.grid.height() / self.zoom);
    }

    pub fn placement(&self) -> Option<Placement> {
        if self.grid.cols == 0 || self.grid.rows == 0 {
            return None;
        }
        let x = place_axis(self.img_w, self.grid.cols, self.grid.cw, self.zoom, self.ox);
        let y = place_axis(self.img_h, self.grid.rows, self.grid.ch, self.zoom, self.oy);
        Some(Placement {
            col: x.cell,
            row: y.cell,
            x_off: x.off,
            y_off: y.off,
            cols: x.cells,
            rows: y.cells,
            src_x: x.src,
            src_y: y.src,
            src_w: x.len,
            src_h: y.len,
        })
    }
}

/// `span` is how many image pixels fit across the viewport.
fn clamp_axis(o: f64, img: f64, span: f64) -> f64 {
    if img <= span {
        (img - span) / 2.0
    } else {
        o.clamp(0.0, img - span)
    }
}

struct Axis {
    cell: u32,
    off: u32,
    cells: u32,
    src: f64,
    len: f64,
}

fn place_axis(img: f64, cells: u32, cell_px: f64, zoom: f64, o: f64) -> Axis {
    let view = cells as f64 * cell_px;
    let span = view / zoom;
    if img > span {
        // The image overflows: the visible part fills the viewport exactly.
        return Axis {
            cell: 0,
            off: 0,
            cells,
            src: o,
            len: span,
        };
    }
    // The image fits. Its size has to be whole cells, so round that, then
    // place it with pixel precision, never past the viewport's far edge.
    let n = ((img * zoom / cell_px).round() as u32).clamp(1, cells);
    let start = (-o * zoom).min(view - n as f64 * cell_px).max(0.0);
    let mut cell = (start / cell_px).floor() as u32;
    let mut off = (start - cell as f64 * cell_px).round() as u32;
    if off as f64 >= cell_px.floor() {
        cell += 1;
        off = 0;
    }
    Axis {
        cell: cell.min(cells - n),
        off,
        cells: n,
        src: 0.0,
        len: img,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GRID: Grid = Grid {
        cols: 100,
        rows: 40,
        cw: 10.0,
        ch: 20.0,
    };

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-6
    }

    #[test]
    fn large_images_shrink_to_fit_and_center() {
        let v = View::new(4000, 1000, GRID);
        assert!(close(v.zoom, 0.25));
        let p = v.placement().unwrap();
        assert_eq!((p.col, p.cols, p.x_off), (0, 100, 0));
        // 250 screen pixels tall in an 800 pixel viewport: 275 pixels above it.
        assert_eq!((p.row, p.y_off), (13, 15));
        assert!(close(p.src_h, 1000.0));
        assert_eq!(p.rows, 13); // 250 / 20 = 12.5, rounded
    }

    #[test]
    fn small_images_stay_at_actual_size() {
        let v = View::new(100, 50, GRID);
        assert!(close(v.zoom, 1.0));
        let p = v.placement().unwrap();
        assert_eq!((p.cols, p.rows), (10, 3));
        assert_eq!((p.col, p.x_off), (45, 0));
    }

    #[test]
    fn zoom_keeps_the_point_under_the_pointer() {
        let mut v = View::new(4000, 3000, GRID);
        let before = v.to_image(300.0, 200.0);
        v.zoom_at(3.0, 300.0, 200.0);
        let after = v.to_image(300.0, 200.0);
        assert!(close(before.0, after.0) && close(before.1, after.1));
        assert_eq!(v.mode, Mode::Free);
    }

    #[test]
    fn panning_stops_at_the_edges() {
        let mut v = View::new(4000, 3000, GRID);
        v.zoom_to(1.0, 0.0, 0.0);
        v.pan_by(1e6, 1e6);
        assert!(close(v.ox, 0.0) && close(v.oy, 0.0));
        v.pan_by(-1e7, -1e7);
        assert!(close(v.ox, 3000.0) && close(v.oy, 2200.0));
        let p = v.placement().unwrap();
        assert!(close(p.src_x, 3000.0) && close(p.src_w, 1000.0));
        assert_eq!((p.cols, p.rows), (100, 40));
    }

    #[test]
    fn fitting_axis_never_passes_the_far_edge() {
        for w in [1, 7, 333, 995, 999, 1000] {
            let v = View::new(w, 10, GRID);
            let p = v.placement().unwrap();
            assert!(p.col + p.cols <= GRID.cols, "w={w}");
            assert!(
                (p.col as f64 * 10.0 + p.x_off as f64) + p.cols as f64 * 10.0 <= 1000.0,
                "w={w}"
            );
            assert!(p.x_off < 10);
        }
    }

    #[test]
    fn resizing_keeps_the_center() {
        let mut v = View::new(4000, 3000, GRID);
        v.zoom_to(2.0, 500.0, 400.0);
        let c = v.to_image(500.0, 400.0);
        v.set_grid(Grid {
            cols: 50,
            rows: 20,
            ..GRID
        });
        let c2 = v.to_image(250.0, 200.0);
        assert!(close(c.0, c2.0) && close(c.1, c2.1));
    }
}

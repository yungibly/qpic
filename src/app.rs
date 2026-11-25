//! The viewer: probing the terminal, the event loop, and drawing.

use std::borrow::Cow;
use std::collections::VecDeque;
use std::fs::File;
use std::io::{self, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};

use image::DynamicImage;
use image::imageops::FilterType;

use crate::blocks::{self, Cell, Pyramid};
use crate::files;
use crate::input::{Button, Event, Key, Mouse, MouseKind, Parser};
use crate::kitty::{self, Transport};
use crate::term::{self, Tty};
use crate::view::{Grid, Mode, View};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DrawMode {
    Auto,
    Kitty,
    Blocks,
}

const KEY_ZOOM: f64 = 1.25;
const WHEEL_ZOOM: f64 = 1.12;
const DOUBLE_CLICK: Duration = Duration::from_millis(400);
const ESC_TIMEOUT: Duration = Duration::from_millis(40);
const PROBE_TIMEOUT: Duration = Duration::from_millis(1500);
/// The largest image sent through a temporary file, and directly: (side, pixels).
const FILE_LIMIT: (u32, u64) = (8192, 40_000_000);
const DIRECT_LIMIT: (u32, u64) = (4096, 16_000_000);
/// Direct uploads bigger than this many pixels show a quick preview first.
const PREVIEW_ABOVE: u64 = 1_000_000;
/// Small images are enlarged to about this size before uploading.
const SMALL_IMAGE_TARGET: u32 = 2048;
/// Images kept after moving on, so going back is instant.
const CACHE_IMAGES: usize = 8;
/// Terminals limit the memory images take, counting four bytes a pixel.
/// Ghostty and Kitty allow 320 MB by default; Rex allows 128 MiB.
const KNOWN_STORAGE: u64 = 300_000_000;
const DEFAULT_STORAGE: u64 = 120 << 20;
const MIN_STORAGE: u64 = 16 << 20;
/// How many times to send the current image again, smaller each time, when
/// the terminal rejects or drops it.
const RETRIES: u8 = 3;
/// How long a temporary file may outlive its freed image before qpic deletes it.
const TEMP_FILE_GRACE: Duration = Duration::from_secs(30);

const HELP: &[(&str, &str)] = &[
    ("Scroll wheel", "Zoom at the pointer"),
    ("Drag", "Pan"),
    ("Double-click", "Toggle 100% and fit"),
    ("+  -", "Zoom in, zoom out"),
    ("0", "Fit large images in the window"),
    ("f", "Fill the window"),
    ("1", "Actual size"),
    ("Arrows  h j k l", "Pan"),
    ("n  p  Space", "Next, previous image"),
    ("Home  End", "First, last image"),
    ("i", "Show or hide the status bar"),
    ("q  Esc", "Quit"),
];

#[derive(Default, Debug)]
struct Caps {
    kitty: bool,
    /// A temporary file directory the terminal reads images from, if any.
    temp_dir: Option<PathBuf>,
    sgr_pixels: bool,
    cell_px: Option<(f64, f64)>,
    window_px: Option<(f64, f64)>,
}

enum Backend {
    Kitty {
        transport: Transport,
        /// Images to hide, keeping their data, once the next frame is drawn.
        hidden: Vec<u32>,
        /// Images to free once the next frame is drawn.
        stale: Vec<u32>,
        /// The placement command last sent, so unchanged frames send nothing.
        last: Vec<u8>,
    },
    Blocks {
        truecolor: bool,
        prev: Vec<Cell>,
    },
}

/// What it takes to show an image again without decoding or uploading it.
enum Held {
    /// In the terminal. `complete` is false while only a preview is there.
    Kitty {
        img: kitty::Image,
        complete: bool,
    },
    Blocks(Pyramid),
}

/// A decoded image, shown now or kept in the cache.
struct Entry {
    index: usize,
    /// The file's modification time when loaded; a changed file is loaded again.
    modified: Option<SystemTime>,
    w: u32,
    h: u32,
    held: Held,
}

impl Entry {
    fn bytes(&self) -> u64 {
        match &self.held {
            Held::Kitty { img, .. } => img.w as u64 * img.h as u64 * 4,
            Held::Blocks(p) => p.bytes(),
        }
    }

    fn kitty_id(&self) -> Option<u32> {
        match &self.held {
            Held::Kitty { img, .. } => Some(img.id),
            Held::Blocks(_) => None,
        }
    }
}

enum Picture {
    Empty,
    Ready(Entry),
    Failed(String),
}

pub fn run(files: Vec<PathBuf>, start: usize, mode: DrawMode) -> Result<(), String> {
    let tty = Tty::open().map_err(|e| format!("can't use the terminal: {e}"))?;
    term::install_panic_hook();
    let mut app = App::new(tty, files, start);
    let result = app.run(mode);
    app.shutdown();
    result.map_err(|e| e.to_string())
}

struct App {
    tty: Tty,
    parser: Parser,
    pending: VecDeque<Event>,
    out: Vec<u8>,
    backend: Backend,
    files: Vec<PathBuf>,
    index: usize,
    picture: Picture,
    /// Images viewed earlier, least recently viewed first.
    cache: Vec<Entry>,
    /// How many times the current image was sent again after the terminal lost or refused it.
    retries: u8,
    /// Bytes of terminal image storage qpic lets itself use, cache included.
    budget: u64,
    view: View,
    cols: u32,
    rows: u32,
    cw: f64,
    ch: f64,
    /// Whether the kernel reports the window's pixel size, which beats asking the terminal.
    ioctl_px: bool,
    status: bool,
    help: bool,
    message: Option<String>,
    drag: Option<(f64, f64)>,
    last_click: Option<(Instant, f64, f64)>,
    sgr_pixels: bool,
    dirty: bool,
    clear: bool,
    resized: bool,
    quit: bool,
    next_id: u32,
    /// Temporary files sent to the terminal, which deletes each one once it reads
    /// it. Some terminals read them lazily, so a file stays even after its image
    /// is freed; the time it was freed lets qpic clean up files left behind.
    temp_files: Vec<(u32, PathBuf, Option<Instant>)>,
    temp_dir: PathBuf,
    log: Option<File>,
}

impl App {
    fn new(tty: Tty, files: Vec<PathBuf>, index: usize) -> App {
        let grid = Grid {
            cols: 80,
            rows: 23,
            cw: 10.0,
            ch: 20.0,
        };
        let log = std::env::var_os("QPIC_LOG").and_then(|p| {
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(p)
                .ok()
        });
        App {
            tty,
            parser: Parser::default(),
            pending: VecDeque::new(),
            out: Vec::new(),
            backend: Backend::Blocks {
                truecolor: true,
                prev: Vec::new(),
            },
            files,
            index,
            picture: Picture::Empty,
            cache: Vec::new(),
            retries: 0,
            budget: storage_budget(),
            view: View::new(1, 1, grid),
            cols: 80,
            rows: 24,
            cw: 10.0,
            ch: 20.0,
            ioctl_px: false,
            status: true,
            help: false,
            message: None,
            drag: None,
            last_click: None,
            sgr_pixels: false,
            dirty: true,
            clear: true,
            resized: false,
            quit: false,
            next_id: ((std::process::id() & 0x7fff) << 16) + 1,
            temp_files: Vec::new(),
            temp_dir: PathBuf::from("/tmp"),
            log,
        }
    }

    fn log(&mut self, args: std::fmt::Arguments) {
        if let Some(f) = &mut self.log {
            let _ = writeln!(f, "{args}");
        }
    }

    fn alloc_id(&mut self) -> u32 {
        self.next_id += 1;
        self.next_id
    }

    fn run(&mut self, mode: DrawMode) -> io::Result<()> {
        self.tty.write_all(term::ENTER.as_bytes())?;
        let caps = self.probe()?;
        self.log(format_args!("caps {caps:?} size {:?}", self.tty.size()));

        let use_kitty = match mode {
            DrawMode::Kitty => true,
            DrawMode::Blocks => false,
            DrawMode::Auto => caps.kitty,
        };
        self.backend = if use_kitty {
            let transport = match std::env::var("QPIC_TRANSPORT").as_deref() {
                Ok("direct") => Transport::Direct,
                Ok("file") => Transport::TempFile,
                _ if caps.temp_dir.is_some() && !relays_images() => Transport::TempFile,
                _ => Transport::Direct,
            };
            self.temp_dir = caps
                .temp_dir
                .clone()
                .unwrap_or_else(|| PathBuf::from("/tmp"));
            Backend::Kitty {
                transport,
                hidden: Vec::new(),
                stale: Vec::new(),
                last: Vec::new(),
            }
        } else {
            let truecolor = caps.kitty
                || matches!(
                    std::env::var("COLORTERM").as_deref(),
                    Ok("truecolor" | "24bit")
                );
            Backend::Blocks {
                truecolor,
                prev: Vec::new(),
            }
        };

        self.sgr_pixels = caps.sgr_pixels;
        let mut modes = String::from("\x1b[?1000h\x1b[?1002h\x1b[?1006h");
        if caps.sgr_pixels {
            modes.push_str("\x1b[?1016h");
        }
        self.tty.write_all(modes.as_bytes())?;

        let size = self.tty.size();
        if let Some((w, h)) = caps.cell_px {
            (self.cw, self.ch) = (w, h);
        } else if let Some((w, h)) = caps.window_px {
            (self.cw, self.ch) = (w / size.cols.max(1) as f64, h / size.rows.max(1) as f64);
        }
        self.update_size();
        self.load()?;
        self.main_loop()
    }

    /// Asks the terminal what it supports. Every terminal answers the device
    /// attributes query last, so its reply ends the wait.
    fn probe(&mut self) -> io::Result<Caps> {
        let direct_id = self.alloc_id();
        let mut q = Vec::new();
        kitty::query(&mut q, direct_id, None);
        let mut probes = Vec::new();
        for dir in kitty::temp_dirs() {
            let id = self.alloc_id();
            let path = kitty::temp_path(&dir, id);
            if std::fs::write(&path, [0u8; 3]).is_ok() {
                kitty::query(&mut q, id, Some(&path));
                probes.push((id, dir, path));
            }
        }
        q.extend_from_slice(b"\x1b[?1016$p\x1b[16t\x1b[14t\x1b[c");
        self.tty.write_all(&q)?;

        let mut caps = Caps::default();
        let deadline = Instant::now() + PROBE_TIMEOUT;
        'wait: loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                self.log(format_args!("probe timed out"));
                break;
            }
            if !self.tty.wait(Some(left))?.0 {
                continue;
            }
            self.read_input()?;
            while let Some(event) = self.parser.next(false) {
                match event {
                    Event::KittyReply { id, ok, .. } if id == direct_id => caps.kitty = ok,
                    Event::KittyReply { id, ok, .. } if probes.iter().any(|p| p.0 == id) => {
                        if ok && caps.temp_dir.is_none() {
                            caps.temp_dir = probes.iter().find(|p| p.0 == id).map(|p| p.1.clone());
                        }
                    }
                    Event::ModeReport { mode: 1016, value } => {
                        caps.sgr_pixels = matches!(value, 1..=3)
                    }
                    Event::CellSize { w, h } if w > 0 && h > 0 => {
                        caps.cell_px = Some((w as f64, h as f64))
                    }
                    Event::WindowPx { w, h } if w > 0 && h > 0 => {
                        caps.window_px = Some((w as f64, h as f64))
                    }
                    Event::DeviceAttrs => break 'wait,
                    other => self.pending.push_back(other),
                }
            }
        }
        for (_, _, path) in probes {
            let _ = std::fs::remove_file(path);
        }
        Ok(caps)
    }

    fn image_grid(&self) -> Grid {
        let rows = self.rows.saturating_sub(self.status as u32).max(1);
        Grid {
            cols: self.cols,
            rows,
            cw: self.cw,
            ch: self.ch,
        }
    }

    fn update_size(&mut self) {
        let size = self.tty.size();
        self.cols = size.cols.max(1);
        self.rows = size.rows.max(1);
        self.ioctl_px = size.width_px > 0 && size.height_px > 0;
        if self.ioctl_px {
            self.cw = size.width_px as f64 / self.cols as f64;
            self.ch = size.height_px as f64 / self.rows as f64;
        } else {
            // Ask; the reply updates the cell size when it arrives.
            self.out.extend_from_slice(b"\x1b[16t");
        }
        self.view.set_grid(self.image_grid());
        self.clear = true;
        self.dirty = true;
    }

    fn read_input(&mut self) -> io::Result<()> {
        let mut buf = [0u8; 65536];
        match self.tty.read(&mut buf) {
            Ok(0) => self.quit = true,
            Ok(n) => self.parser.feed(&buf[..n]),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
        Ok(())
    }

    fn main_loop(&mut self) -> io::Result<()> {
        while !self.quit {
            while let Some(event) = self.pending.pop_front() {
                self.handle(event)?;
                if self.quit {
                    return Ok(());
                }
            }
            if self.resized {
                self.resized = false;
                self.update_size();
            }
            if self.dirty {
                self.render()?;
            } else if !self.out.is_empty() {
                self.tty.write_all(&self.out)?;
                self.out.clear();
            }

            let timeout = self.parser.has_pending().then_some(ESC_TIMEOUT);
            let (input, signal) = self.tty.wait(timeout)?;
            if signal {
                if self.tty.take_signals() {
                    self.quit = true;
                }
                self.resized = true;
            }
            if input {
                self.read_input()?;
            }
            let flush = !input && !signal;
            while let Some(event) = self.parser.next(flush) {
                self.pending.push_back(event);
            }
        }
        Ok(())
    }

    fn handle(&mut self, event: Event) -> io::Result<()> {
        match event {
            Event::Key(k) => self.key(k)?,
            Event::Mouse(m) => self.mouse(m),
            Event::CellSize { w, h } if w > 0 && h > 0 && !self.ioctl_px => {
                (self.cw, self.ch) = (w as f64, h as f64);
                self.view.set_grid(self.image_grid());
                self.clear = true;
                self.dirty = true;
            }
            Event::KittyReply {
                ok: false,
                message,
                id,
            } => {
                self.log(format_args!("kitty error for {id}: {message}"));
                let current = match &self.picture {
                    Picture::Ready(entry) => entry.kitty_id(),
                    _ => None,
                };
                if current != Some(id) {
                    return Ok(());
                }
                if (message.starts_with("ENOENT") || message.starts_with("ENOMEM"))
                    && self.retries < RETRIES
                {
                    // The terminal dropped the image to make room, or refused it as
                    // too big: it allows less than we assumed. Send it again, smaller.
                    self.retries += 1;
                    self.budget = (self.budget / 4 * 3).max(MIN_STORAGE);
                    let budget = self.budget;
                    self.log(format_args!("storage budget now {budget} bytes"));
                    if let Picture::Ready(entry) =
                        std::mem::replace(&mut self.picture, Picture::Empty)
                    {
                        self.discard(entry);
                    }
                    self.make_room(0);
                    return self.load();
                }
                self.message = Some(format!("terminal: {message}"));
                self.dirty = true;
            }
            _ => {}
        }
        Ok(())
    }

    fn key(&mut self, k: Key) -> io::Result<()> {
        if self.help {
            self.help = false;
            self.clear = true;
            self.dirty = true;
            if !matches!(k, Key::Char('q') | Key::Ctrl('c')) {
                return Ok(());
            }
        }
        let ready = matches!(self.picture, Picture::Ready(_));
        let (cx, cy) = self.view.center();
        let step_x = (self.view.grid.width() / 8.0).max(self.cw);
        let step_y = (self.view.grid.height() / 8.0).max(self.ch);
        let before = (self.view.zoom, self.view.ox, self.view.oy);
        match k {
            Key::Char('q') | Key::Ctrl('c') | Key::Esc => self.quit = true,
            Key::Char('?') => {
                self.help = true;
                self.clear = true;
            }
            Key::Char('+' | '=') => self.view.zoom_at(KEY_ZOOM, cx, cy),
            Key::Char('-' | '_') => self.view.zoom_at(1.0 / KEY_ZOOM, cx, cy),
            Key::Char('0') => self.view.set_mode(Mode::BestFit),
            Key::Char('f') => self.view.set_mode(Mode::Fit),
            Key::Char('1') => self.view.zoom_to(1.0, cx, cy),
            Key::Left if self.view.fits_width() => self.go(self.prev_index())?,
            Key::Right if self.view.fits_width() => self.go(self.next_index())?,
            Key::Left | Key::Char('h') => self.view.pan_by(step_x, 0.0),
            Key::Right | Key::Char('l') => self.view.pan_by(-step_x, 0.0),
            Key::Up | Key::Char('k') => self.view.pan_by(0.0, step_y),
            Key::Down | Key::Char('j') => self.view.pan_by(0.0, -step_y),
            Key::Char('n' | ' ') | Key::PageDown | Key::Enter => self.go(self.next_index())?,
            Key::Char('p') | Key::Backspace | Key::PageUp => self.go(self.prev_index())?,
            Key::Home | Key::Char('g') => self.go(0)?,
            Key::End | Key::Char('G') => self.go(self.files.len() - 1)?,
            Key::Char('i') => {
                self.status = !self.status;
                self.view.set_grid(self.image_grid());
                self.clear = true;
            }
            _ => {}
        }
        if ready && before != (self.view.zoom, self.view.ox, self.view.oy) {
            self.dirty = true;
        }
        self.dirty |= self.clear;
        Ok(())
    }

    fn next_index(&self) -> usize {
        (self.index + 1) % self.files.len()
    }

    fn prev_index(&self) -> usize {
        (self.index + self.files.len() - 1) % self.files.len()
    }

    fn go(&mut self, index: usize) -> io::Result<()> {
        if index != self.index {
            self.stash();
            self.index = index;
            self.retries = 0;
            self.load()?;
        }
        Ok(())
    }

    /// Moves the current image into the cache. It stays on screen until the
    /// next frame, so the screen doesn't go blank while the next one decodes.
    fn stash(&mut self) {
        let Picture::Ready(entry) = std::mem::replace(&mut self.picture, Picture::Empty) else {
            return;
        };
        if let Held::Kitty {
            complete: false, ..
        } = entry.held
        {
            return self.discard(entry);
        }
        if let (Backend::Kitty { hidden, last, .. }, Some(id)) =
            (&mut self.backend, entry.kitty_id())
        {
            hidden.push(id);
            last.clear();
        }
        self.cache.push(entry);
        self.make_room(0);
    }

    fn discard(&mut self, entry: Entry) {
        if let (Backend::Kitty { stale, .. }, Some(id)) = (&mut self.backend, entry.kitty_id()) {
            stale.push(id);
        }
    }

    /// Drops the least recently viewed images until `incoming` more bytes fit.
    fn make_room(&mut self, incoming: u64) {
        let current = match &self.picture {
            Picture::Ready(entry) => entry.bytes(),
            _ => 0,
        };
        loop {
            let held: u64 = self.cache.iter().map(Entry::bytes).sum::<u64>() + current + incoming;
            if self.cache.is_empty() || (held <= self.budget && self.cache.len() <= CACHE_IMAGES) {
                break;
            }
            let entry = self.cache.remove(0);
            self.log(format_args!(
                "evicted image {} (id {:?})",
                entry.index,
                entry.kitty_id()
            ));
            self.discard(entry);
        }
    }

    /// Turns a mouse report into screen pixels within the image area.
    fn mouse_px(&self, x: i32, y: i32) -> (f64, f64) {
        let (x, y) = ((x - 1).max(0) as f64, (y - 1).max(0) as f64);
        if self.sgr_pixels {
            (x, y)
        } else {
            ((x + 0.5) * self.cw, (y + 0.5) * self.ch)
        }
    }

    fn mouse(&mut self, m: Mouse) {
        if self.help {
            if matches!(m.kind, MouseKind::Press(_)) {
                self.help = false;
                self.clear = true;
                self.dirty = true;
            }
            return;
        }
        if !matches!(self.picture, Picture::Ready(_)) {
            return;
        }
        let (x, y) = self.mouse_px(m.x, m.y);
        let before = (self.view.zoom, self.view.ox, self.view.oy);
        let wheel_pan = self.view.grid.width() / 16.0;
        match m.kind {
            MouseKind::Press(b @ (Button::Left | Button::Middle)) => {
                if b == Button::Left {
                    let double = self.last_click.is_some_and(|(t, px, py)| {
                        t.elapsed() < DOUBLE_CLICK
                            && (x - px).abs() <= self.cw * 2.0
                            && (y - py).abs() <= self.ch
                    });
                    if double {
                        self.last_click = None;
                        self.toggle_actual_size(x, y);
                    } else {
                        self.last_click = Some((Instant::now(), x, y));
                    }
                }
                self.drag = Some((x, y));
                self.out.extend_from_slice(b"\x1b]22;grabbing\x1b\\");
            }
            MouseKind::Drag(Button::Left | Button::Middle) => {
                if let Some((px, py)) = self.drag {
                    self.view.pan_by(x - px, y - py);
                }
                self.drag = Some((x, y));
            }
            MouseKind::Release(_) => {
                if self.drag.take().is_some() {
                    self.out.extend_from_slice(b"\x1b]22;default\x1b\\");
                }
            }
            MouseKind::WheelUp if m.shift => self.view.pan_by(wheel_pan, 0.0),
            MouseKind::WheelDown if m.shift => self.view.pan_by(-wheel_pan, 0.0),
            MouseKind::WheelUp => self.view.zoom_at(WHEEL_ZOOM, x, y),
            MouseKind::WheelDown => self.view.zoom_at(1.0 / WHEEL_ZOOM, x, y),
            MouseKind::WheelLeft => self.view.pan_by(wheel_pan, 0.0),
            MouseKind::WheelRight => self.view.pan_by(-wheel_pan, 0.0),
            _ => {}
        }
        if before != (self.view.zoom, self.view.ox, self.view.oy) {
            self.dirty = true;
        }
    }

    /// Double-click: actual size at the pointer, or back to fitting the window.
    fn toggle_actual_size(&mut self, x: f64, y: f64) {
        if (self.view.zoom - 1.0).abs() < 1e-3 {
            let mode = if self.view.fit_zoom() > 1.0 {
                Mode::Fit
            } else {
                Mode::BestFit
            };
            self.view.set_mode(mode);
        } else {
            self.view.zoom_to(1.0, x, y);
        }
    }

    fn load(&mut self) -> io::Result<()> {
        let index = self.index;
        let path = self.files[index].clone();
        let modified = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        if let Some(pos) = self.cache.iter().position(|e| e.index == index) {
            let entry = self.cache.remove(pos);
            if modified.is_some() && entry.modified == modified {
                self.log(format_args!(
                    "cached {} (id {:?})",
                    path.display(),
                    entry.kitty_id()
                ));
                self.view = View::new(entry.w, entry.h, self.image_grid());
                self.set_current(entry);
                self.message = None;
                self.clear = true;
                self.dirty = true;
                return Ok(());
            }
            self.discard(entry);
        }

        self.message = Some("loading…".into());
        self.flash_status()?;
        let started = Instant::now();
        match files::load(&path) {
            Ok(img) => {
                let (w, h) = (img.width(), img.height());
                self.view = View::new(w, h, self.image_grid());
                self.message = None;
                let entry = |held| Entry {
                    index,
                    modified,
                    w,
                    h,
                    held,
                };
                if let Backend::Kitty { .. } = self.backend {
                    self.upload(&img, |img, complete| entry(Held::Kitty { img, complete }))?;
                } else {
                    self.set_current(entry(Held::Blocks(Pyramid::new(&img))));
                    self.make_room(0);
                }
            }
            Err(e) => {
                self.picture = Picture::Failed(e);
                self.view = View::new(1, 1, self.image_grid());
                self.message = None;
            }
        }
        self.log(format_args!(
            "loaded {} in {:?}",
            path.display(),
            started.elapsed()
        ));
        self.clear = true;
        self.dirty = true;
        Ok(())
    }

    /// Shows `entry`, freeing the preview it replaces, if any.
    fn set_current(&mut self, entry: Entry) {
        if let Picture::Ready(old) = std::mem::replace(&mut self.picture, Picture::Ready(entry)) {
            self.discard(old);
        }
        if let Backend::Kitty { last, .. } = &mut self.backend {
            last.clear();
        }
    }

    fn flash_status(&mut self) -> io::Result<()> {
        if self.status {
            let mut out = Vec::new();
            self.draw_status(&mut out);
            self.tty.write_all(&out)?;
        }
        Ok(())
    }

    /// Frees dropped images now, before an upload that needs their room.
    fn free_stale_now(&mut self) -> io::Result<()> {
        let mut out = Vec::new();
        self.free_stale(&mut out);
        self.tty.write_all(&out)
    }

    fn free_stale(&mut self, out: &mut Vec<u8>) {
        if let Backend::Kitty { stale, .. } = &mut self.backend {
            for id in stale.drain(..) {
                kitty::free(out, id);
                if let Some(file) = self.temp_files.iter_mut().find(|(t, ..)| *t == id) {
                    file.2 = Some(Instant::now());
                }
            }
        }
    }

    fn upload(
        &mut self,
        img: &DynamicImage,
        entry: impl Fn(kitty::Image, bool) -> Entry,
    ) -> io::Result<()> {
        let Backend::Kitty { transport, .. } = self.backend else {
            return Ok(());
        };
        let (side, pixels) = if transport == Transport::TempFile {
            FILE_LIMIT
        } else {
            DIRECT_LIMIT
        };
        let mut full = files::cap(img, side, pixels.min(self.budget / 4));
        // The terminal scales images smoothly, which blurs pixel art when zoomed
        // in. Enlarge small images by a whole number first so pixels stay square.
        let longest = full.width().max(full.height());
        let factor = (SMALL_IMAGE_TARGET / longest.max(1)).min(64);
        if factor >= 2 {
            let (w, h) = (full.width() * factor, full.height() * factor);
            full = Cow::Owned(full.resize_exact(w, h, FilterType::Nearest));
        }
        self.make_room(full.width() as u64 * full.height() as u64 * 4);
        self.free_stale_now()?;

        if transport == Transport::Direct
            && full.width() as u64 * full.height() as u64 > PREVIEW_ABOVE
        {
            let g = self.view.grid;
            let preview = full.thumbnail(g.width().max(1.0) as u32, g.height().max(1.0) as u32);
            if preview.width() < full.width() {
                let id = self.alloc_id();
                if !self.send(id, &preview, None)? {
                    return Ok(());
                }
                let (w, h) = (preview.width(), preview.height());
                self.set_current(entry(kitty::Image { id, w, h }, false));
                self.message = Some("loading full size…".into());
                self.clear = true;
                self.render()?;
            }
        }
        let id = self.alloc_id();
        if self.send(id, &full, Some("loading full size"))? {
            self.set_current(entry(
                kitty::Image {
                    id,
                    w: full.width(),
                    h: full.height(),
                },
                true,
            ));
        }
        self.message = None;
        Ok(())
    }

    /// Uploads an image. Returns false if a key asked to quit or move on partway through.
    fn send(&mut self, id: u32, img: &DynamicImage, progress: Option<&str>) -> io::Result<bool> {
        let Backend::Kitty { transport, .. } = self.backend else {
            return Ok(false);
        };
        let (w, h) = (img.width(), img.height());
        let alpha = img.color().has_alpha();
        let (format, data) = if alpha {
            (32, img.to_rgba8().into_raw())
        } else {
            (24, img.to_rgb8().into_raw())
        };
        let started = Instant::now();

        if transport == Transport::TempFile {
            let path = kitty::temp_path(&self.temp_dir, id);
            match std::fs::write(&path, &data) {
                Ok(()) => {
                    let mut cmd = Vec::new();
                    kitty::transmit_file(&mut cmd, id, format, w, h, &path);
                    self.tty.write_all(&cmd)?;
                    self.temp_files.retain(|(_, path, freed)| {
                        let read = !path.exists();
                        let abandoned = freed.is_some_and(|t| t.elapsed() > TEMP_FILE_GRACE);
                        if abandoned && !read {
                            let _ = std::fs::remove_file(path);
                        }
                        !(read || abandoned)
                    });
                    self.temp_files.push((id, path, None));
                    self.log(format_args!(
                        "sent {id} {w}x{h} as file in {:?}",
                        started.elapsed()
                    ));
                    return Ok(true);
                }
                Err(e) => self.log(format_args!("temp file failed, sending directly: {e}")),
            }
        }

        let chunks = kitty::direct_chunks(id, format, w, h, &data);
        drop(data);
        let total: usize = chunks.iter().map(Vec::len).sum::<usize>().max(1);
        let (mut sent, mut since) = (0, 0);
        for chunk in &chunks {
            self.tty.write_all(chunk)?;
            sent += chunk.len();
            since += chunk.len();
            if since < 256 * 1024 {
                continue;
            }
            since = 0;
            if self.interrupted()? {
                self.tty.write_all(kitty::ABORT)?;
                self.log(format_args!("upload of {w}x{h} interrupted"));
                return Ok(false);
            }
            if let Some(label) = progress {
                self.message = Some(format!("{label} {}%", sent * 100 / total));
                self.flash_status()?;
            }
        }
        self.log(format_args!(
            "sent {id} {w}x{h} directly, {total} bytes in {:?}",
            started.elapsed()
        ));
        Ok(true)
    }

    /// Checks for keys that quit or switch images during a long upload,
    /// keeping all input for the main loop to handle afterwards.
    fn interrupted(&mut self) -> io::Result<bool> {
        let (input, signal) = self.tty.wait(Some(Duration::ZERO))?;
        if signal {
            if self.tty.take_signals() {
                self.quit = true;
                return Ok(true);
            }
            self.resized = true;
        }
        if input {
            self.read_input()?;
            while let Some(event) = self.parser.next(false) {
                self.pending.push_back(event);
            }
        }
        Ok(self.quit || self.pending.iter().any(interrupts_upload))
    }

    fn render(&mut self) -> io::Result<()> {
        let mut out = std::mem::take(&mut self.out);
        out.extend_from_slice(b"\x1b[?2026h");
        let clear = std::mem::take(&mut self.clear);
        if clear {
            for row in 1..=self.rows {
                let _ = write!(out, "\x1b[{row};1H\x1b[0m\x1b[2K");
            }
        }
        self.draw_image(&mut out, clear);
        if self.help {
            self.draw_help(&mut out);
        } else if let Picture::Failed(msg) = &self.picture {
            self.draw_error(&mut out, msg);
        }
        if self.status {
            self.draw_status(&mut out);
        }
        out.extend_from_slice(b"\x1b[?2026l");
        self.tty.write_all(&out)?;
        out.clear();
        self.out = out;
        self.dirty = false;
        Ok(())
    }

    fn draw_image(&mut self, out: &mut Vec<u8>, clear: bool) {
        let current = match &self.picture {
            Picture::Ready(entry) if !self.help => Some(&entry.held),
            _ => None,
        };
        match &mut self.backend {
            Backend::Kitty { hidden, last, .. } => {
                let shown = match current {
                    Some(Held::Kitty { img, .. }) => Some(img),
                    _ => None,
                };
                let mut cmd = Vec::new();
                if let (Some(img), Some(pl)) = (shown, self.view.placement()) {
                    kitty::place(&mut cmd, img, &pl, self.view.img_w, self.view.img_h);
                }
                if cmd.is_empty() {
                    if let (true, Picture::Ready(entry)) =
                        (clear || !last.is_empty(), &self.picture)
                    {
                        hidden.extend(entry.kitty_id());
                    }
                } else if clear || cmd != *last {
                    out.extend_from_slice(&cmd);
                    if let Some(f) = &mut self.log {
                        let _ =
                            writeln!(f, "place {}", String::from_utf8_lossy(&cmd).escape_debug());
                    }
                }
                *last = cmd;
                // Going back and forth quickly can hide and show an image in one frame.
                for id in hidden
                    .drain(..)
                    .filter(|&id| Some(id) != shown.map(|i| i.id))
                {
                    kitty::delete_placements(out, id);
                }
            }
            Backend::Blocks { truecolor, prev } => {
                if clear || current.is_none() {
                    prev.clear();
                }
                if let Some(Held::Blocks(p)) = current {
                    let cells = blocks::render(p, &self.view);
                    blocks::encode(out, &cells, prev, self.view.grid.cols, *truecolor);
                    *prev = cells;
                }
            }
        }
        self.free_stale(out);
    }

    fn file_name(&self) -> String {
        let path = &self.files[self.index];
        path.file_name()
            .unwrap_or(path.as_os_str())
            .to_string_lossy()
            .into_owned()
    }

    fn draw_status(&self, out: &mut Vec<u8>) {
        let mut left = format!(" {}", self.file_name());
        match &self.picture {
            Picture::Ready(e) => {
                left += &format!("   {}×{}   {}", e.w, e.h, zoom_label(self.view.zoom))
            }
            Picture::Failed(_) => left += "   can't open",
            Picture::Empty => {}
        }
        if self.files.len() > 1 {
            left += &format!("   {}/{}", self.index + 1, self.files.len());
        }
        if let Some(m) = &self.message {
            left += "   ";
            left += m;
        }
        let line = fit_line(&left, "? help ", self.cols as usize);
        let _ = write!(out, "\x1b[{};1H\x1b[0;7m{line}\x1b[0m", self.rows);
    }

    fn draw_lines(&self, out: &mut Vec<u8>, lines: &[String]) {
        let area = self.image_grid();
        let width = lines.iter().map(|l| l.chars().count()).max().unwrap_or(0) as u32;
        let top = area.rows.saturating_sub(lines.len() as u32) / 2;
        let left = self.cols.saturating_sub(width) / 2;
        for (i, line) in lines.iter().enumerate() {
            let text: String = line.chars().take(self.cols as usize).collect();
            let _ = write!(out, "\x1b[{};{}H{text}", top + i as u32 + 1, left + 1);
        }
    }

    fn draw_help(&self, out: &mut Vec<u8>) {
        let mut lines = vec!["\x1b[1mqpic\x1b[0m".to_string(), String::new()];
        lines.extend(HELP.iter().map(|(keys, what)| format!("{keys:<18}{what}")));
        lines.extend([
            String::new(),
            "\x1b[2mPress any key to close\x1b[0m".to_string(),
        ]);
        // Escape codes don't take up columns; measure without them.
        let width = HELP
            .iter()
            .map(|(_, w)| 18 + w.chars().count())
            .max()
            .unwrap_or(0) as u32;
        let area = self.image_grid();
        let top = area.rows.saturating_sub(lines.len() as u32) / 2;
        let left = self.cols.saturating_sub(width) / 2;
        for (i, line) in lines.iter().enumerate() {
            let _ = write!(out, "\x1b[{};{}H{line}", top + i as u32 + 1, left + 1);
        }
    }

    fn draw_error(&self, out: &mut Vec<u8>, msg: &str) {
        self.draw_lines(
            out,
            &[format!("Can't open {}", self.file_name()), msg.to_string()],
        );
    }

    fn shutdown(&mut self) {
        let mut out = Vec::new();
        if let Picture::Ready(entry) = std::mem::replace(&mut self.picture, Picture::Empty) {
            self.discard(entry);
        }
        for entry in std::mem::take(&mut self.cache) {
            self.discard(entry);
        }
        self.free_stale(&mut out);
        out.extend_from_slice(term::LEAVE.as_bytes());
        let _ = self.tty.write_all(&out);
        let _ = self.tty.flush();
        for (_, path, _) in self.temp_files.drain(..) {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// How much image memory to assume the terminal allows. Ghostty and Kitty
/// say who they are; anything else gets a size that fits Rex, and qpic shrinks
/// it further if the terminal turns out to allow less.
fn storage_budget() -> u64 {
    let known = terminal_is("ghostty")
        || terminal_is("kitty")
        || std::env::var_os("KITTY_WINDOW_ID").is_some();
    if known {
        KNOWN_STORAGE
    } else {
        DEFAULT_STORAGE
    }
}

/// Whether the terminal says it is `name`, such as "ghostty".
fn terminal_is(name: &str) -> bool {
    let var = |key| std::env::var(key).unwrap_or_default();
    var("TERM_PROGRAM") == name || var("TERM").strip_prefix("xterm-") == Some(name)
}

/// Whether the terminal passes images on to a display that may be on another
/// machine. Rex's server reads a temporary file where qpic runs, then sends
/// its app the pixels uncompressed: 5.8 MB over the network for a 1440×900
/// screenshot that takes 0.3 MB inline.
fn relays_images() -> bool {
    terminal_is("rex")
}

fn interrupts_upload(event: &Event) -> bool {
    matches!(
        event,
        Event::Key(
            Key::Char('q' | 'n' | 'p' | ' ' | 'g' | 'G')
                | Key::Ctrl('c')
                | Key::Left
                | Key::Right
                | Key::PageUp
                | Key::PageDown
                | Key::Home
                | Key::End
                | Key::Enter
                | Key::Backspace
        )
    )
}

fn zoom_label(zoom: f64) -> String {
    let pct = zoom * 100.0;
    if pct >= 9.95 {
        format!("{pct:.0}%")
    } else {
        format!("{pct:.1}%")
    }
}

/// Puts `right` at the end of a line `width` columns wide, shortening `left` if needed.
fn fit_line(left: &str, right: &str, width: usize) -> String {
    let (lw, rw) = (left.chars().count(), right.chars().count());
    if lw + rw < width {
        return format!("{left}{}{right}", " ".repeat(width - lw - rw));
    }
    let mut s: String = left.chars().take(width.saturating_sub(1)).collect();
    if lw >= width {
        s.push('…');
    }
    let n = s.chars().count();
    s + &" ".repeat(width.saturating_sub(n))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_line_fits_the_width() {
        assert_eq!(fit_line(" a.png", "? help ", 20), " a.png       ? help ");
        assert_eq!(
            fit_line(" a-very-long-name.png", "? help ", 10),
            " a-very-l…"
        );
        assert_eq!(zoom_label(1.0), "100%");
        assert_eq!(zoom_label(0.05), "5.0%");
    }
}

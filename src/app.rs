//! The viewer: probing the terminal, the event loop, and drawing.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{self, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};

use crate::blocks::{self, Cell, Pyramid};
use crate::input::{Button, Event, Key, Mouse, MouseKind, Parser};
use crate::kitty::{self, Payload, Transport};
use crate::loader::{Form, Loader, Reply, Request};
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
/// While keys that switch images come closer together than this, qpic waits
/// for them to stop before it loads anything. Keyboards repeat faster than
/// this, so holding a key down loads only the image it ends on.
const SETTLE: Duration = Duration::from_millis(150);
/// The largest image sent through a temporary file, and directly: (side, pixels).
const FILE_LIMIT: (u32, u64) = (8192, 40_000_000);
const DIRECT_LIMIT: (u32, u64) = (4096, 16_000_000);
/// A preview is at most this many pixels across, and no bigger than the window.
const PREVIEW_SIDE: f64 = 512.0;
/// How many bytes of a direct upload go between updates of its progress.
const PROGRESS_STEP: usize = 256 * 1024;
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

    /// False for a preview, which stands in until the image itself arrives.
    fn complete(&self) -> bool {
        !matches!(
            self.held,
            Held::Kitty {
                complete: false,
                ..
            }
        )
    }
}

/// What is on screen. It stays there while another image loads, so it isn't
/// always the selected one.
enum Picture {
    Empty,
    Ready(Entry),
    Failed { index: usize, error: String },
}

/// The selected image, on its way from the loader to the screen.
struct Loading {
    /// The request's serial; replies to earlier requests are ignored.
    serial: u64,
    index: usize,
    modified: Option<SystemTime>,
    started: Instant,
    /// Whether a preview of it is on screen already.
    previewed: bool,
}

/// A prepared image going to the terminal a piece at a time, so that keys
/// are read in between.
struct Upload {
    id: u32,
    /// The size of the image as decoded; `image` may hold it at another size.
    w: u32,
    h: u32,
    image: Payload,
    preview: bool,
    /// How much of an encoded image has been sent.
    offset: usize,
    started: Option<Instant>,
}

pub fn run(files: Vec<PathBuf>, start: usize, mode: DrawMode) -> Result<(), String> {
    let tty = Tty::open().map_err(|e| format!("can't use the terminal: {e}"))?;
    term::install_panic_hook();
    let loader = tty
        .waker()
        .and_then(Loader::start)
        .map_err(|e| format!("can't start loading images: {e}"))?;
    let mut app = App::new(tty, loader, files, start);
    let result = app.run(mode);
    app.shutdown();
    result.map_err(|e| e.to_string())
}

struct App {
    tty: Tty,
    parser: Parser,
    /// When the parser was left holding the start of a sequence. If the rest
    /// doesn't follow, it was a key press on its own, such as Esc.
    partial_since: Option<Instant>,
    pending: VecDeque<Event>,
    out: Vec<u8>,
    backend: Backend,
    files: Vec<PathBuf>,
    /// The selected image: the one the status bar names.
    index: usize,
    picture: Picture,
    loader: Loader,
    /// The serial of the last request made of the loader.
    serial: u64,
    loading: Option<Loading>,
    /// When to load the selected image, while keys are still switching images.
    load_at: Option<Instant>,
    /// When a key last switched images.
    last_move: Option<Instant>,
    /// Prepared images on their way to the terminal, the first of them in part.
    uploads: VecDeque<Upload>,
    /// The status bar as last drawn, so that it is only redrawn when it changes.
    last_status: String,
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
    fn new(tty: Tty, loader: Loader, files: Vec<PathBuf>, index: usize) -> App {
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
            partial_since: None,
            pending: VecDeque::new(),
            out: Vec::new(),
            backend: Backend::Blocks {
                truecolor: true,
                prev: Vec::new(),
            },
            files,
            index,
            picture: Picture::Empty,
            loader,
            serial: 0,
            loading: None,
            load_at: None,
            last_move: None,
            uploads: VecDeque::new(),
            last_status: String::new(),
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

    fn log(&self, args: std::fmt::Arguments) {
        if let Some(mut f) = self.log.as_ref() {
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
        self.start_load();
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
            if !self.tty.wait(Some(left))?.input {
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
            self.take_replies();
            if self.load_at.is_some_and(|at| at <= Instant::now()) {
                self.start_load();
            }
            // An upload goes a piece per turn of this loop, which looks for
            // input in between without waiting for any.
            let sending = self.send_some()?;
            self.draw()?;

            let timeout = if sending {
                Some(Duration::ZERO)
            } else {
                let settle = self
                    .load_at
                    .map(|at| at.saturating_duration_since(Instant::now()));
                let escape = self
                    .partial_since
                    .map(|since| ESC_TIMEOUT.saturating_sub(since.elapsed()));
                match (settle, escape) {
                    (Some(a), Some(b)) => Some(a.min(b)),
                    (a, b) => a.or(b),
                }
            };
            let ready = self.tty.wait(timeout)?;
            if ready.signal {
                if self.tty.take_signals() {
                    self.quit = true;
                }
                self.resized = true;
            }
            if ready.input {
                self.read_input()?;
            }
            // The start of a sequence with nothing after it for a while was a
            // key on its own.
            let flush = !ready.input
                && self
                    .partial_since
                    .is_some_and(|since| since.elapsed() >= ESC_TIMEOUT);
            while let Some(event) = self.parser.next(flush) {
                self.pending.push_back(event);
            }
            self.partial_since = match self.partial_since {
                _ if !self.parser.has_pending() => None,
                Some(since) if !ready.input => Some(since),
                _ => Some(Instant::now()),
            };
        }
        Ok(())
    }

    /// Draws what changed. In the middle of an upload the terminal takes text
    /// but no other graphics commands, so the picture waits for the upload
    /// while the status bar stays current.
    fn draw(&mut self) -> io::Result<()> {
        if self.dirty && !self.mid_upload() {
            return self.render();
        }
        if !self.out.is_empty() {
            self.tty.write_all(&self.out)?;
            self.out.clear();
        }
        if self.dirty {
            self.flash_status()?;
        }
        Ok(())
    }

    fn mid_upload(&self) -> bool {
        self.uploads.front().is_some_and(|up| up.offset > 0)
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
                let shown = match &self.picture {
                    Picture::Ready(entry) if entry.kitty_id() == Some(id) => entry.index,
                    _ => return Ok(()),
                };
                // The terminal dropped the image to make room, or refused it as
                // too big: it allows less than we assumed.
                let lost = message.starts_with("ENOENT") || message.starts_with("ENOMEM");
                if lost && (shown != self.index || self.retries < RETRIES) {
                    if let Picture::Ready(entry) =
                        std::mem::replace(&mut self.picture, Picture::Empty)
                    {
                        self.discard(entry);
                    }
                    self.clear = true;
                    self.dirty = true;
                    if shown != self.index {
                        // It was only on screen until the selected image
                        // arrives, which is on its way.
                        return Ok(());
                    }
                    // Send it again, smaller.
                    self.retries += 1;
                    self.budget = (self.budget / 4 * 3).max(MIN_STORAGE);
                    let budget = self.budget;
                    self.log(format_args!("storage budget now {budget} bytes"));
                    self.cancel_load()?;
                    self.make_room(0);
                    self.start_load();
                    return Ok(());
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
        // Left and right pan a picture too wide for the window, unless it is
        // only on screen until the selected image arrives.
        let arrows_switch = self.view.fits_width() || self.shown() != Some(self.index);
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
            Key::Left if arrows_switch => self.go(self.prev_index())?,
            Key::Right if arrows_switch => self.go(self.next_index())?,
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
            self.index = index;
            self.retries = 0;
            self.select()?;
        }
        Ok(())
    }

    /// The image on screen, which isn't the selected one while that loads.
    fn shown(&self) -> Option<usize> {
        match &self.picture {
            Picture::Ready(entry) => Some(entry.index),
            Picture::Failed { index, .. } => Some(*index),
            Picture::Empty => None,
        }
    }

    /// Brings the selected image to the screen. One that is cached shows at
    /// once. Any other has to load, and the status bar names it meanwhile:
    /// straight away after a single key, but only once the keys stop when
    /// they come quickly, so the images passed over cost nothing.
    fn select(&mut self) -> io::Result<()> {
        self.cancel_load()?;
        let now = Instant::now();
        let rapid = self
            .last_move
            .is_some_and(|last| now.duration_since(last) < SETTLE);
        self.last_move = Some(now);
        self.message = None;
        self.dirty = true;
        match &self.picture {
            Picture::Ready(entry) if entry.index == self.index && entry.complete() => {
                return Ok(());
            }
            // An error about another image has nothing to say any more.
            Picture::Failed { index, .. } if *index != self.index => {
                self.picture = Picture::Empty;
                self.clear = true;
            }
            _ => {}
        }
        if self.show_cached() {
            return Ok(());
        }
        if rapid {
            self.message = Some("loading…".into());
            self.load_at = Some(now + SETTLE);
        } else {
            self.start_load();
        }
        Ok(())
    }

    /// Shows the selected image from the cache, if it is there and its file
    /// hasn't changed since.
    fn show_cached(&mut self) -> bool {
        let index = self.index;
        let Some(pos) = self.cache.iter().position(|e| e.index == index) else {
            return false;
        };
        let entry = self.cache.remove(pos);
        let modified = std::fs::metadata(&self.files[index])
            .and_then(|m| m.modified())
            .ok();
        if modified.is_none() || entry.modified != modified {
            self.discard(entry);
            return false;
        }
        let id = entry.kitty_id();
        self.log(format_args!(
            "cached {} (id {id:?})",
            self.files[index].display()
        ));
        self.show(entry);
        true
    }

    /// Asks the loader for the selected image.
    fn start_load(&mut self) {
        self.load_at = None;
        let index = self.index;
        let path = self.files[index].clone();
        let modified = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        // A load that was cut short may have left its preview on screen.
        let previewed = matches!(
            &self.picture,
            Picture::Ready(entry)
                if !entry.complete() && entry.index == index && entry.modified == modified
        );
        let form = match self.backend {
            Backend::Kitty { transport, .. } => {
                let direct = transport == Transport::Direct;
                let (max_side, pixels) = if direct { DIRECT_LIMIT } else { FILE_LIMIT };
                let g = self.view.grid;
                let bounds = |px: f64| px.clamp(1.0, PREVIEW_SIDE) as u32;
                Form::Kitty {
                    transport,
                    max_side,
                    max_pixels: pixels.min(self.budget / 4),
                    // A temporary file arrives whole, with no wait to fill.
                    preview: (direct && !previewed)
                        .then(|| (bounds(g.width()), bounds(g.height()))),
                }
            }
            Backend::Blocks { .. } => Form::Blocks,
        };
        self.serial += 1;
        self.loader.request(Request {
            serial: self.serial,
            path,
            form,
        });
        self.loading = Some(Loading {
            serial: self.serial,
            index,
            modified,
            started: Instant::now(),
            previewed,
        });
        let message = if previewed {
            "loading full size…"
        } else {
            "loading…"
        };
        self.message = Some(message.into());
        self.dirty = true;
    }

    /// Stops loading: the loader drops its work, and an upload under way ends
    /// where it is.
    fn cancel_load(&mut self) -> io::Result<()> {
        self.load_at = None;
        if self.loading.take().is_some() {
            self.loader.abandon();
        }
        if self.mid_upload() {
            self.tty.write_all(kitty::ABORT)?;
            self.log(format_args!("upload interrupted"));
        }
        self.uploads.clear();
        Ok(())
    }

    /// Takes what the loader has prepared for the selected image.
    fn take_replies(&mut self) {
        while let Some(reply) = self.loader.poll() {
            let Some(loading) = &self.loading else {
                continue;
            };
            if reply.serial() != loading.serial {
                continue;
            }
            match reply {
                Reply::Preview { w, h, image, .. } => {
                    if !loading.previewed {
                        self.queue(w, h, image, true);
                    }
                }
                Reply::Image { w, h, image, .. } => self.queue(w, h, image, false),
                Reply::Blocks { w, h, pyramid, .. } => self.arrived(w, h, Held::Blocks(pyramid)),
                Reply::Failed { error, .. } => self.failed(error),
            }
        }
    }

    fn queue(&mut self, w: u32, h: u32, image: Payload, preview: bool) {
        let id = self.alloc_id();
        self.uploads.push_back(Upload {
            id,
            w,
            h,
            image,
            preview,
            offset: 0,
            started: None,
        });
    }

    /// Sends the next piece of the image being uploaded. Returns whether more
    /// is waiting to go.
    fn send_some(&mut self) -> io::Result<bool> {
        let Some(mut up) = self.uploads.pop_front() else {
            return Ok(false);
        };
        if up.started.is_none() {
            up.started = Some(Instant::now());
            // The terminal needs the room before the first byte arrives.
            let (w, h) = (up.image.w as u64, up.image.h as u64);
            self.make_room(if up.preview { 0 } else { w * h * 4 });
            self.free_stale_now()?;
            if !up.image.encoded {
                if self.send_file(&up)? {
                    self.sent(up);
                    return Ok(!self.uploads.is_empty());
                }
                up.image.encode();
            }
        }
        let before = up.offset;
        let mut code = Vec::with_capacity(kitty::CHUNK + 64);
        up.offset = kitty::direct_chunk(&mut code, up.id, &up.image, up.offset);
        self.tty.write_all(&code)?;
        let total = up.image.data.len();
        if up.offset >= total {
            self.sent(up);
        } else {
            if !up.preview && up.offset / PROGRESS_STEP != before / PROGRESS_STEP {
                let percent = up.offset * 100 / total;
                self.message = Some(format!("loading full size {percent}%"));
                self.dirty = true;
            }
            self.uploads.push_front(up);
        }
        Ok(!self.uploads.is_empty())
    }

    /// Hands the terminal an image as a temporary file. Returns false if the
    /// file couldn't be written.
    fn send_file(&mut self, up: &Upload) -> io::Result<bool> {
        let path = kitty::temp_path(&self.temp_dir, up.id);
        if let Err(e) = std::fs::write(&path, &up.image.data) {
            self.log(format_args!("temp file failed, sending directly: {e}"));
            return Ok(false);
        }
        let mut cmd = Vec::new();
        let Payload { format, w, h, .. } = up.image;
        kitty::transmit_file(&mut cmd, up.id, format, w, h, &path);
        self.tty.write_all(&cmd)?;
        self.temp_files.retain(|(_, path, freed)| {
            let read = !path.exists();
            let abandoned = freed.is_some_and(|t| t.elapsed() > TEMP_FILE_GRACE);
            if abandoned && !read {
                let _ = std::fs::remove_file(path);
            }
            !(read || abandoned)
        });
        self.temp_files.push((up.id, path, None));
        Ok(true)
    }

    /// An upload has finished: its image can be shown.
    fn sent(&mut self, up: Upload) {
        let Payload { w, h, .. } = up.image;
        let took = up.started.map(|t| t.elapsed()).unwrap_or_default();
        let (id, bytes) = (up.id, up.image.data.len());
        let how = if up.image.encoded {
            "directly"
        } else {
            "as file"
        };
        self.log(format_args!(
            "sent {id} {w}x{h} {how}, {bytes} bytes in {took:?}"
        ));
        let img = kitty::Image { id, w, h };
        let complete = !up.preview;
        self.arrived(up.w, up.h, Held::Kitty { img, complete });
    }

    /// The selected image, or a preview of it, is ready to show.
    fn arrived(&mut self, w: u32, h: u32, held: Held) {
        let (index, modified) = match &self.loading {
            Some(loading) => (loading.index, loading.modified),
            None => (self.index, None),
        };
        let entry = Entry {
            index,
            modified,
            w,
            h,
            held,
        };
        let Some(loading) = &mut self.loading else {
            // Nothing is waiting for it any more, so the terminal needn't keep it.
            return self.discard(entry);
        };
        if entry.complete() {
            let (index, took) = (loading.index, loading.started.elapsed());
            self.loading = None;
            self.message = None;
            self.log(format_args!(
                "loaded {} in {took:?}",
                self.files[index].display()
            ));
        } else {
            loading.previewed = true;
            self.message = Some("loading full size…".into());
        }
        self.show(entry);
    }

    /// The selected image couldn't be opened.
    fn failed(&mut self, error: String) {
        let Some(loading) = self.loading.take() else {
            return;
        };
        let index = loading.index;
        self.log(format_args!(
            "failed {}: {error}",
            self.files[index].display()
        ));
        // A preview may have been on its way when the image itself failed.
        if self.mid_upload() {
            self.out.extend_from_slice(kitty::ABORT);
        }
        self.uploads.clear();
        self.message = None;
        let old = std::mem::replace(&mut self.picture, Picture::Failed { index, error });
        self.retire(old);
        self.view = View::new(1, 1, self.image_grid());
        self.clear = true;
        self.dirty = true;
    }

    /// Puts an image on screen. The picture it replaces stays in the terminal
    /// for later if it is worth keeping.
    fn show(&mut self, entry: Entry) {
        // An image that replaces its own preview keeps the zoom and position.
        let upgrade = matches!(
            &self.picture,
            Picture::Ready(old)
                if !old.complete() && (old.index, old.w, old.h) == (entry.index, entry.w, entry.h)
        );
        if !upgrade {
            self.view = View::new(entry.w, entry.h, self.image_grid());
        }
        let old = std::mem::replace(&mut self.picture, Picture::Ready(entry));
        self.retire(old);
        if let Backend::Kitty { last, .. } = &mut self.backend {
            last.clear();
        }
        self.clear = true;
        self.dirty = true;
    }

    /// Moves a picture that has left the screen into the cache, or frees it
    /// if it was only a preview. Its placement goes with the next frame, so
    /// the screen is never blank in between.
    fn retire(&mut self, old: Picture) {
        let Picture::Ready(entry) = old else {
            return;
        };
        if !entry.complete() || self.shown() == Some(entry.index) {
            return self.discard(entry);
        }
        if let (Backend::Kitty { hidden, .. }, Some(id)) = (&mut self.backend, entry.kitty_id()) {
            hidden.push(id);
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
    /// If that isn't enough, a picture that is only on screen until the
    /// selected image arrives goes too.
    fn make_room(&mut self, incoming: u64) {
        loop {
            let (current, passing) = match &self.picture {
                Picture::Ready(entry) => (entry.bytes(), entry.index != self.index),
                _ => (0, false),
            };
            let held: u64 = self.cache.iter().map(Entry::bytes).sum::<u64>() + current + incoming;
            if held <= self.budget && self.cache.len() <= CACHE_IMAGES {
                break;
            }
            let entry = if !self.cache.is_empty() {
                self.cache.remove(0)
            } else if passing && incoming > 0 {
                let Picture::Ready(entry) = std::mem::replace(&mut self.picture, Picture::Empty)
                else {
                    break;
                };
                self.clear = true;
                self.dirty = true;
                entry
            } else {
                break;
            };
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

    /// Redraws the status bar alone, if it has changed.
    fn flash_status(&mut self) -> io::Result<()> {
        if self.status && self.status_line() != self.last_status {
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
        } else if let Picture::Failed { index, error } = &self.picture {
            self.draw_error(&mut out, *index, error);
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

    fn file_name(&self, index: usize) -> String {
        let path = &self.files[index];
        path.file_name()
            .unwrap_or(path.as_os_str())
            .to_string_lossy()
            .into_owned()
    }

    /// The status bar, which is about the selected image. Its size and zoom
    /// show once it is the one on screen.
    fn status_line(&self) -> String {
        let mut left = format!(" {}", self.file_name(self.index));
        match &self.picture {
            Picture::Ready(e) if e.index == self.index => {
                left += &format!("   {}×{}   {}", e.w, e.h, zoom_label(self.view.zoom))
            }
            Picture::Failed { index, .. } if *index == self.index => left += "   can't open",
            _ => {}
        }
        if self.files.len() > 1 {
            left += &format!("   {}/{}", self.index + 1, self.files.len());
        }
        if let Some(m) = &self.message {
            left += "   ";
            left += m;
        }
        fit_line(&left, "? help ", self.cols as usize)
    }

    fn draw_status(&mut self, out: &mut Vec<u8>) {
        let line = self.status_line();
        let _ = write!(out, "\x1b[{};1H\x1b[0;7m{line}\x1b[0m", self.rows);
        self.last_status = line;
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

    fn draw_error(&self, out: &mut Vec<u8>, index: usize, msg: &str) {
        self.draw_lines(
            out,
            &[
                format!("Can't open {}", self.file_name(index)),
                msg.to_string(),
            ],
        );
    }

    fn shutdown(&mut self) {
        let mut out = Vec::new();
        self.loader.abandon();
        if self.mid_upload() {
            out.extend_from_slice(kitty::ABORT);
        }
        self.uploads.clear();
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

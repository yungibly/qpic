//! Decoding and compressing on a second thread, so a key never waits for an
//! image. The viewer asks for one image at a time. Asking for another, or
//! abandoning the one asked for, makes the loader drop its work at the next step.

use std::borrow::Cow;
use std::io;
use std::panic::{self, AssertUnwindSafe};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};

use image::DynamicImage;
use image::imageops::FilterType;

use crate::blocks::Pyramid;
use crate::files;
use crate::kitty::{Payload, Transport};
use crate::term::Waker;

/// The loader thread's name, which the panic hook knows it by.
pub const THREAD: &str = "qpic-loader";

/// Decoders run on the loader thread with as much stack as the main thread has.
const STACK: usize = 8 << 20;
/// Small images are enlarged to about this size before uploading.
const SMALL_IMAGE_TARGET: u32 = 2048;
/// Images with more pixels than this can have a preview sent ahead of them.
const PREVIEW_ABOVE: u64 = 1_000_000;

pub struct Request {
    /// Names the request in its replies. Counts up from 1, never repeating.
    pub serial: u64,
    pub path: PathBuf,
    pub form: Form,
}

/// What the drawing backend needs an image turned into.
#[derive(Clone, Copy, Debug)]
pub enum Form {
    /// Pixels to upload to the terminal.
    Kitty {
        transport: Transport,
        /// The largest image the terminal takes, in pixels across and in total.
        max_side: u32,
        max_pixels: u64,
        /// The size a preview has to fit in, if one may go first.
        preview: Option<(u32, u32)>,
    },
    /// Half-blocks.
    Blocks,
}

/// The loader's answers. `w` and `h` are the size of the image as decoded,
/// whatever size its pixels are sent at.
pub enum Reply {
    /// A small version, to show while the image itself is prepared and sent.
    Preview {
        serial: u64,
        w: u32,
        h: u32,
        image: Payload,
    },
    Image {
        serial: u64,
        w: u32,
        h: u32,
        image: Payload,
    },
    Blocks {
        serial: u64,
        w: u32,
        h: u32,
        pyramid: Pyramid,
    },
    Failed {
        serial: u64,
        error: String,
    },
}

impl Reply {
    pub fn serial(&self) -> u64 {
        match self {
            Reply::Preview { serial, .. }
            | Reply::Image { serial, .. }
            | Reply::Blocks { serial, .. }
            | Reply::Failed { serial, .. } => *serial,
        }
    }
}

pub struct Loader {
    shared: Arc<Shared>,
    replies: Receiver<Reply>,
}

struct Shared {
    waiting: Mutex<Waiting>,
    changed: Condvar,
    /// The serial of the request still wanted, or 0 for none. The thread
    /// checks it between steps and drops work on any other request.
    wanted: AtomicU64,
}

#[derive(Default)]
struct Waiting {
    request: Option<Request>,
    closed: bool,
}

impl Shared {
    fn waiting(&self) -> MutexGuard<'_, Waiting> {
        // The lock is only held to swap a value, so it can't be left poisoned
        // in a state worth refusing.
        self.waiting.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl Loader {
    /// Starts the thread. It uses `waker` after sending each reply.
    pub fn start(waker: Waker) -> io::Result<Loader> {
        let shared = Arc::new(Shared {
            waiting: Mutex::new(Waiting::default()),
            changed: Condvar::new(),
            wanted: AtomicU64::new(0),
        });
        let (sender, replies) = mpsc::channel();
        let theirs = shared.clone();
        std::thread::Builder::new()
            .name(THREAD.into())
            .stack_size(STACK)
            .spawn(move || run(&theirs, &sender, &waker))?;
        Ok(Loader { shared, replies })
    }

    /// Asks for an image, instead of any asked for before.
    pub fn request(&self, request: Request) {
        self.shared.wanted.store(request.serial, Ordering::Relaxed);
        self.shared.waiting().request = Some(request);
        self.shared.changed.notify_one();
    }

    /// Gives up on the image asked for.
    pub fn abandon(&self) {
        self.shared.wanted.store(0, Ordering::Relaxed);
        self.shared.waiting().request = None;
    }

    /// The next reply, if one has arrived. Replies to requests since replaced
    /// or abandoned arrive too; their serial tells them apart.
    pub fn poll(&self) -> Option<Reply> {
        self.replies.try_recv().ok()
    }
}

impl Drop for Loader {
    fn drop(&mut self) {
        self.abandon();
        self.shared.waiting().closed = true;
        self.shared.changed.notify_one();
    }
}

fn run(shared: &Shared, replies: &Sender<Reply>, waker: &Waker) {
    loop {
        let request = {
            let mut waiting = shared.waiting();
            loop {
                if waiting.closed {
                    return;
                }
                if let Some(request) = waiting.request.take() {
                    break request;
                }
                waiting = shared
                    .changed
                    .wait(waiting)
                    .unwrap_or_else(|e| e.into_inner());
            }
        };
        let serial = request.serial;
        let wanted = || shared.wanted.load(Ordering::Relaxed) == serial;
        let mut send = |reply| {
            let _ = replies.send(reply);
            waker.wake();
        };
        // A decoder that panics on a strange file costs only that file.
        let outcome =
            panic::catch_unwind(AssertUnwindSafe(|| prepare(&request, &wanted, &mut send)));
        let error = match outcome {
            Ok(Ok(())) => continue,
            Ok(Err(error)) => error,
            Err(panic) => {
                let why = panic
                    .downcast_ref::<String>()
                    .map(String::as_str)
                    .or_else(|| panic.downcast_ref::<&str>().copied())
                    .unwrap_or("no details");
                format!("the decoder failed: {why}")
            }
        };
        send(Reply::Failed { serial, error });
    }
}

fn prepare(
    request: &Request,
    wanted: &dyn Fn() -> bool,
    send: &mut dyn FnMut(Reply),
) -> Result<(), String> {
    let serial = request.serial;
    let img = files::load(&request.path)?;
    if !wanted() {
        return Ok(());
    }
    let (w, h) = (img.width(), img.height());
    match request.form {
        Form::Blocks => {
            let pyramid = Pyramid::new(&img);
            send(Reply::Blocks {
                serial,
                w,
                h,
                pyramid,
            });
        }
        Form::Kitty {
            transport,
            max_side,
            max_pixels,
            preview,
        } => {
            let full = fit(&img, max_side, max_pixels);
            if let Some((pw, ph)) = preview.and_then(|bounds| preview_size(&full, bounds)) {
                // Always direct: a preview only helps where sending is slow.
                let image = payload(&full.thumbnail_exact(pw, ph), Transport::Direct);
                send(Reply::Preview {
                    serial,
                    w,
                    h,
                    image,
                });
                if !wanted() {
                    return Ok(());
                }
            }
            let image = payload(&full, transport);
            send(Reply::Image {
                serial,
                w,
                h,
                image,
            });
        }
    }
    Ok(())
}

/// Sizes an image for the terminal: shrunk to its limits, or enlarged if small.
fn fit(img: &DynamicImage, max_side: u32, max_pixels: u64) -> Cow<'_, DynamicImage> {
    let full = files::cap(img, max_side, max_pixels);
    // The terminal scales images smoothly, which blurs pixel art when zoomed
    // in. Enlarge small images by a whole number first so pixels stay square.
    let longest = full.width().max(full.height());
    let factor = (SMALL_IMAGE_TARGET / longest.max(1)).min(64);
    if factor < 2 {
        return full;
    }
    let (w, h) = (full.width() * factor, full.height() * factor);
    Cow::Owned(full.resize_exact(w, h, FilterType::Nearest))
}

/// The size of a preview of `full` that fits in `bounds`, if one is worth
/// sending first: `full` has to be large, and the preview at most a quarter
/// of its pixels.
fn preview_size(full: &DynamicImage, bounds: (u32, u32)) -> Option<(u32, u32)> {
    let (w, h) = (full.width() as f64, full.height() as f64);
    if (w * h) as u64 <= PREVIEW_ABOVE {
        return None;
    }
    let scale = (bounds.0 as f64 / w).min(bounds.1 as f64 / h);
    if scale > 0.5 {
        return None;
    }
    let side = |px: f64| ((px * scale).round() as u32).max(1);
    Some((side(w), side(h)))
}

fn payload(img: &DynamicImage, transport: Transport) -> Payload {
    let (format, pixels) = if img.color().has_alpha() {
        (32, img.to_rgba8().into_raw())
    } else {
        (24, img.to_rgb8().into_raw())
    };
    Payload::new(format, img.width(), img.height(), pixels, transport)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::time::Duration;

    const KITTY: Form = Form::Kitty {
        transport: Transport::Direct,
        max_side: 4096,
        max_pixels: 16_000_000,
        preview: Some((512, 512)),
    };

    struct Fixture {
        dir: PathBuf,
        loader: Loader,
        // Kept so the loader's wakes have somewhere to go.
        _wakes: std::os::unix::net::UnixStream,
    }

    impl Fixture {
        fn new(name: &str) -> Fixture {
            let dir = std::env::temp_dir().join(format!("qpic-{name}-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let (wakes, waker) = Waker::pair().unwrap();
            Fixture {
                dir,
                loader: Loader::start(waker).unwrap(),
                _wakes: wakes,
            }
        }

        fn image(&self, name: &str, w: u32, h: u32) -> PathBuf {
            let path = self.dir.join(name);
            let img = image::RgbImage::from_fn(w, h, |x, y| image::Rgb([x as u8, y as u8, 7]));
            img.save(&path).unwrap();
            path
        }

        fn ask(&self, serial: u64, path: &Path, form: Form) {
            self.loader.request(Request {
                serial,
                path: path.to_path_buf(),
                form,
            });
        }

        fn reply(&self) -> Reply {
            self.loader
                .replies
                .recv_timeout(Duration::from_secs(30))
                .expect("the loader should have replied")
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn a_large_image_comes_as_a_preview_then_in_full() {
        let f = Fixture::new("loader-large");
        f.ask(1, &f.image("large.png", 1600, 1200), KITTY);
        match f.reply() {
            Reply::Preview {
                serial: 1,
                w: 1600,
                h: 1200,
                image,
            } => assert_eq!((image.w, image.h, image.encoded), (512, 384, true)),
            _ => panic!("expected a preview first"),
        }
        match f.reply() {
            Reply::Image {
                serial: 1,
                w: 1600,
                h: 1200,
                image,
            } => assert_eq!((image.w, image.h, image.format), (1600, 1200, 24)),
            _ => panic!("expected the image after its preview"),
        }
    }

    #[test]
    fn a_small_image_is_enlarged_and_a_file_payload_stays_raw() {
        let f = Fixture::new("loader-small");
        let form = Form::Kitty {
            transport: Transport::TempFile,
            max_side: 8192,
            max_pixels: 40_000_000,
            preview: None,
        };
        f.ask(1, &f.image("small.png", 100, 50), form);
        match f.reply() {
            Reply::Image {
                w: 100,
                h: 50,
                image,
                ..
            } => {
                assert_eq!((image.w, image.h, image.encoded), (2000, 1000, false));
                assert_eq!(image.data.len(), 2000 * 1000 * 3);
            }
            _ => panic!("expected the image alone"),
        }
    }

    #[test]
    fn half_blocks_get_a_pyramid() {
        let f = Fixture::new("loader-blocks");
        f.ask(1, &f.image("any.png", 64, 48), Form::Blocks);
        assert!(matches!(
            f.reply(),
            Reply::Blocks {
                serial: 1,
                w: 64,
                h: 48,
                ..
            }
        ));
    }

    #[test]
    fn a_file_that_will_not_decode_fails_and_the_next_still_loads() {
        let f = Fixture::new("loader-failed");
        let broken = f.dir.join("broken.png");
        std::fs::write(&broken, b"not an image").unwrap();
        f.ask(1, &broken, KITTY);
        assert!(matches!(f.reply(), Reply::Failed { serial: 1, .. }));
        f.ask(2, &f.image("fine.png", 300, 200), Form::Blocks);
        assert!(matches!(f.reply(), Reply::Blocks { serial: 2, .. }));
    }

    #[test]
    fn only_large_images_get_previews_and_only_much_smaller_ones() {
        let sized = |w, h| DynamicImage::new_rgb8(w, h);
        assert_eq!(
            preview_size(&sized(1440, 900), (512, 512)),
            Some((512, 320))
        );
        assert_eq!(
            preview_size(&sized(900, 1440), (2000, 368)),
            Some((230, 368))
        );
        // Not large enough to need one.
        assert_eq!(preview_size(&sized(1000, 1000), (512, 512)), None);
        // The preview would be more than a quarter of the image.
        assert_eq!(preview_size(&sized(1440, 900), (1000, 1000)), None);
    }
}

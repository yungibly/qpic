//! Runs the real viewer on a pseudo-terminal, with the test playing the
//! terminal: it answers the startup query, presses keys, and reads what
//! qpic draws and uploads.

use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use base64::Engine;

const ROWS: u16 = 40;
const COLS: u16 = 120;
/// Far longer than anything here takes, so a slow machine doesn't fail a test.
const PATIENCE: Duration = Duration::from_secs(60);
const RIGHT: &[u8] = b"\x1b[C";
const LEFT: &[u8] = b"\x1b[D";
/// What qpic ends an unfinished upload with.
const ABORT: &[u8] = b"\x1b_Gm=0,q=2;\x1b\\";
/// How qpic leaves the alternate screen on its way out.
const LEAVE: &[u8] = b"\x1b[?1049l";

#[derive(Default)]
struct Seen {
    bytes: Vec<u8>,
    /// Stop reading once this appears, leaving qpic to fill the terminal's
    /// buffer and wait, as it would on a slow connection.
    pause_at: Option<Vec<u8>>,
    paused: bool,
}

struct Shared {
    seen: Mutex<Seen>,
    changed: Condvar,
}

impl Shared {
    fn seen(&self) -> MutexGuard<'_, Seen> {
        self.seen.lock().unwrap()
    }
}

struct Terminal {
    child: Child,
    keys: File,
    shared: Arc<Shared>,
    reader: Option<JoinHandle<()>>,
    dir: PathBuf,
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn count(haystack: &[u8], needle: &[u8]) -> usize {
    haystack
        .windows(needle.len())
        .filter(|w| *w == needle)
        .count()
}

/// The status bar as last drawn.
fn status(seen: &[u8]) -> String {
    let start = format!("\x1b[{ROWS};1H\x1b[0;7m");
    let Some(at) = seen
        .windows(start.len())
        .rposition(|w| w == start.as_bytes())
    else {
        return String::new();
    };
    let rest = &seen[at + start.len()..];
    let end = find(rest, b"\x1b[0m").unwrap_or(rest.len());
    String::from_utf8_lossy(&rest[..end]).into_owned()
}

/// The size of every image qpic started to upload, in order.
fn uploads(seen: &[u8]) -> Vec<(u32, u32)> {
    let text = String::from_utf8_lossy(seen);
    text.split("\x1b_Ga=t,")
        .skip(1)
        .map(|header| {
            let number = |key: &str| -> u32 {
                let at = header.find(key).unwrap() + key.len();
                let digits: String = header[at..]
                    .chars()
                    .take_while(char::is_ascii_digit)
                    .collect();
                digits.parse().unwrap()
            };
            (number(",s="), number(",v="))
        })
        .collect()
}

/// The id qpic gave the image it uploaded last.
fn last_upload_id(seen: &[u8]) -> String {
    let text = String::from_utf8_lossy(seen);
    let header = text.rsplit("\x1b_Ga=t,i=").next().unwrap();
    header.chars().take_while(char::is_ascii_digit).collect()
}

/// The temporary file qpic named in its last upload.
fn last_upload_file(seen: &[u8]) -> PathBuf {
    let text = String::from_utf8_lossy(seen);
    let named = text.rsplit(",t=t,q=1;").next().unwrap();
    let encoded = &named[..named.find('\x1b').unwrap()];
    let path = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .unwrap();
    PathBuf::from(String::from_utf8(path).unwrap())
}

/// A folder for a test's images, emptied first.
fn folder(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("qpic-viewer-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Writes `count` plain images named 01.png, 02.png and so on. Image `n` is
/// 1200 + n pixels wide, which tells them apart in what qpic uploads.
fn plain_images(dir: &Path, count: u32) {
    for n in 1..=count {
        let img = image::RgbImage::from_pixel(1200 + n, 800, image::Rgb([n as u8 * 20, 90, 160]));
        img.save(dir.join(format!("{n:02}.png"))).unwrap();
    }
}

/// Writes an image of noise, which doesn't compress: 1300 × 1000 pixels come
/// to about five megabytes on their way to the terminal.
fn noise_image(path: &Path) {
    let mut state = 0x2545_f491_4f6c_dd1d_u64;
    let img = image::RgbImage::from_fn(1300, 1000, |_, _| {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        image::Rgb([state as u8, (state >> 8) as u8, (state >> 16) as u8])
    });
    img.save(path).unwrap();
}

impl Terminal {
    /// Starts qpic with `args` in `dir`, and answers its startup query the
    /// way a terminal without graphics would. `--mode kitty` makes qpic use
    /// graphics all the same, sent inline.
    fn start(dir: PathBuf, args: &[&str], stdin: Option<&[u8]>) -> Terminal {
        Terminal::start_with(dir, args, stdin, "direct")
    }

    /// As `start`, with images sent by `transport`: "direct" or "file".
    fn start_with(dir: PathBuf, args: &[&str], stdin: Option<&[u8]>, transport: &str) -> Terminal {
        let size = libc::winsize {
            ws_row: ROWS,
            ws_col: COLS,
            ws_xpixel: COLS * 10,
            ws_ypixel: ROWS * 20,
        };
        let (mut master, mut slave) = (0, 0);
        // SAFETY: openpty fills in the two descriptors and reads the size.
        let opened = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &size as *const libc::winsize as *mut libc::winsize,
            )
        };
        assert_eq!(opened, 0, "openpty failed");
        // SAFETY: both descriptors are open and owned by nothing else.
        let (master, slave) =
            unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };

        let mut command = Command::new(env!("CARGO_BIN_EXE_qpic"));
        command
            .args(args)
            .current_dir(&dir)
            .env_clear()
            .env("TERM", "xterm-256color")
            .env("QPIC_TRANSPORT", transport)
            .stdin(match stdin {
                Some(_) => Stdio::piped(),
                None => Stdio::from(slave.try_clone().unwrap()),
            })
            .stdout(Stdio::from(slave.try_clone().unwrap()))
            .stderr(Stdio::from(slave));
        // SAFETY: the closure only makes system calls. It gives the child its
        // own session with the pseudo-terminal, now its standard error, as the
        // controlling terminal, which is what /dev/tty then opens.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 || libc::ioctl(2, libc::TIOCSCTTY as _, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command.spawn().unwrap();
        if let Some(list) = stdin {
            child.stdin.take().unwrap().write_all(list).unwrap();
        }

        let shared = Arc::new(Shared {
            seen: Mutex::new(Seen::default()),
            changed: Condvar::new(),
        });
        let keys = File::from(master.try_clone().unwrap());
        let mut output = File::from(master);
        let theirs = shared.clone();
        let reader = std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                {
                    let mut seen = theirs.seen();
                    while seen.paused {
                        seen = theirs.changed.wait(seen).unwrap();
                    }
                }
                // Reading fails or returns nothing once qpic has exited.
                let n = match output.read(&mut buf) {
                    Ok(n) if n > 0 => n,
                    _ => break,
                };
                let mut seen = theirs.seen();
                seen.bytes.extend_from_slice(&buf[..n]);
                if let Some(pattern) = &seen.pause_at
                    && find(&seen.bytes, pattern).is_some()
                {
                    seen.pause_at = None;
                    seen.paused = true;
                }
                theirs.changed.notify_all();
            }
        });

        let mut terminal = Terminal {
            child,
            keys,
            shared,
            reader: Some(reader),
            dir,
        };
        terminal.wait_for("the startup query", |seen| find(seen, b"\x1b[c").is_some());
        terminal.press(b"\x1b[?62;c");
        terminal
    }

    fn press(&mut self, keys: &[u8]) {
        self.keys.write_all(keys).unwrap();
    }

    /// Waits until what qpic has written so far satisfies `done`.
    fn wait_for(&self, what: &str, done: impl Fn(&[u8]) -> bool) {
        let deadline = Instant::now() + PATIENCE;
        let mut seen = self.shared.seen();
        while !done(&seen.bytes) {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                let tail = seen.bytes.len().saturating_sub(600);
                panic!(
                    "never saw {what}. The status bar was {:?}, and the output ended with {:?}",
                    status(&seen.bytes),
                    String::from_utf8_lossy(&seen.bytes[tail..])
                );
            }
            seen = self.shared.changed.wait_timeout(seen, left).unwrap().0;
        }
    }

    /// Waits for the status bar to name an image with its size, which it does
    /// once that image is on screen in full.
    fn wait_for_image(&self, name: &str) {
        self.wait_for(&format!("{name} on screen"), |seen| {
            let bar = status(seen);
            bar.contains(name) && bar.contains('×') && !bar.contains("loading")
        });
    }

    fn seen(&self) -> Vec<u8> {
        self.shared.seen().bytes.clone()
    }

    fn pause_at(&self, pattern: &[u8]) {
        self.shared.seen().pause_at = Some(pattern.to_vec());
    }

    fn wait_until_paused(&self) {
        let deadline = Instant::now() + PATIENCE;
        let mut seen = self.shared.seen();
        while !seen.paused {
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(!left.is_zero(), "the output never reached the pause point");
            seen = self.shared.changed.wait_timeout(seen, left).unwrap().0;
        }
    }

    fn resume(&self) {
        self.shared.seen().paused = false;
        self.shared.changed.notify_all();
    }

    /// Waits for qpic to exit, and returns how it went and all it wrote.
    fn finish(mut self) -> (ExitStatus, Vec<u8>) {
        let deadline = Instant::now() + PATIENCE;
        let status = loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                break status;
            }
            assert!(Instant::now() < deadline, "qpic didn't exit");
            std::thread::sleep(Duration::from_millis(5));
        };
        // Closing our end too lets the reader see the end of the output.
        drop(std::mem::replace(
            &mut self.keys,
            File::open("/dev/null").unwrap(),
        ));
        self.reader.take().unwrap().join().unwrap();
        (status, self.seen())
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.resume();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn keys_in_quick_succession_load_only_the_image_they_end_on() {
    let dir = folder("burst");
    plain_images(&dir, 8);
    let mut term = Terminal::start(dir, &["--mode", "kitty", "."], None);
    term.wait_for_image("01.png");

    // Six presses arrive together, as they do when a key is held down.
    term.press(&RIGHT.repeat(6));
    term.wait_for("the last image named while it loads", |seen| {
        let bar = status(seen);
        bar.contains("07.png") && bar.contains("7/8") && bar.contains("loading")
    });
    term.wait_for_image("07.png");
    assert_eq!(uploads(&term.seen()), [(1201, 800), (1207, 800)]);

    term.press(b"q");
    let (exit, seen) = term.finish();
    assert!(exit.success());
    assert!(seen.ends_with(LEAVE));
}

#[test]
fn keys_one_at_a_time_load_each_image_and_going_back_reuses_it() {
    let dir = folder("steps");
    plain_images(&dir, 4);
    let mut term = Terminal::start(dir, &["--mode", "kitty", "."], None);
    term.wait_for_image("01.png");
    for name in ["02.png", "03.png"] {
        term.press(RIGHT);
        term.wait_for_image(name);
    }
    let three = [(1201, 800), (1202, 800), (1203, 800)];
    assert_eq!(uploads(&term.seen()), three);

    // The terminal still has the second image, so only its placement is sent.
    term.press(LEFT);
    term.wait_for_image("02.png");
    assert_eq!(uploads(&term.seen()), three);

    term.press(b"q");
    assert!(term.finish().0.success());
}

#[test]
fn a_key_ends_an_upload_under_way() {
    let dir = folder("interrupt");
    noise_image(&dir.join("01.bmp"));
    plain_images(&dir, 2);
    std::fs::remove_file(dir.join("01.png")).unwrap();
    let mut term = Terminal::start(dir, &["--mode", "kitty", "."], None);

    // Stop reading as the full image starts to arrive. qpic fills the
    // terminal's buffer and has to wait, megabytes short of the end.
    let full = b",s=1300,v=1000,";
    term.pause_at(full);
    term.wait_until_paused();
    term.press(RIGHT);
    term.resume();
    term.wait_for_image("02.png");

    let seen = term.seen();
    assert_eq!(uploads(&seen), [(512, 394), (1300, 1000), (1202, 800)]);
    let start = find(&seen, full).unwrap();
    let sent = find(&seen[start..], ABORT).expect("the upload should have been ended early");
    assert!(
        sent < 1_000_000,
        "{sent} bytes of the image went out after its upload began"
    );

    term.press(b"q");
    assert!(term.finish().0.success());
}

#[test]
fn quitting_ends_an_upload_under_way_and_restores_the_terminal() {
    let dir = folder("quit");
    noise_image(&dir.join("01.bmp"));
    let mut term = Terminal::start(dir, &["--mode", "kitty", "01.bmp"], None);
    term.pause_at(b",s=1300,v=1000,");
    term.wait_until_paused();
    term.press(b"q");
    term.resume();
    let (exit, seen) = term.finish();
    assert!(exit.success());
    assert_eq!(count(&seen, ABORT), 1);
    assert!(seen.ends_with(LEAVE));
}

#[test]
fn a_file_that_is_not_an_image_says_so_and_the_next_one_still_opens() {
    let dir = folder("broken");
    plain_images(&dir, 3);
    std::fs::write(dir.join("02.png"), b"not an image").unwrap();
    let mut term = Terminal::start(dir, &["--mode", "kitty", "."], None);
    term.wait_for_image("01.png");

    term.press(RIGHT);
    term.wait_for("the error", |seen| {
        status(seen).contains("02.png   can't open") && find(seen, b"Can't open 02.png").is_some()
    });
    term.press(RIGHT);
    term.wait_for_image("03.png");
    assert_eq!(uploads(&term.seen()), [(1201, 800), (1203, 800)]);

    term.press(b"q");
    assert!(term.finish().0.success());
}

#[test]
fn half_blocks_load_the_same_way() {
    let dir = folder("blocks");
    plain_images(&dir, 6);
    let mut term = Terminal::start(dir, &["--mode", "blocks", "."], None);
    term.wait_for_image("01.png");
    term.press(&RIGHT.repeat(4));
    term.wait_for_image("05.png");
    term.press(LEFT);
    term.wait_for_image("04.png");
    let seen = term.seen();
    assert!(uploads(&seen).is_empty());
    assert!(find(&seen, "▀".as_bytes()).is_some() || find(&seen, b"\x1b[48;5;").is_some());

    term.press(b"q");
    assert!(term.finish().0.success());
}

#[test]
fn a_list_on_standard_input_opens_in_its_order() {
    let dir = folder("list");
    plain_images(&dir, 3);
    let list = b"03.png\ngone.png\n01.png\n";
    let mut term = Terminal::start(dir, &["--mode", "kitty", "-"], Some(list));
    term.wait_for_image("03.png");
    assert!(status(&term.seen()).contains("1/2"));
    term.press(RIGHT);
    term.wait_for_image("01.png");
    assert_eq!(uploads(&term.seen()), [(1203, 800), (1201, 800)]);

    term.press(b"q");
    assert!(term.finish().0.success());
}

#[test]
fn coming_straight_back_to_a_previewed_image_sends_only_the_rest() {
    let dir = folder("back");
    noise_image(&dir.join("01.bmp"));
    plain_images(&dir, 2);
    std::fs::remove_file(dir.join("01.png")).unwrap();
    let mut term = Terminal::start(dir, &["--mode", "kitty", "."], None);
    term.pause_at(b",s=1300,v=1000,");
    term.wait_until_paused();

    // Away and back before the other image has loaded: the preview is still
    // on screen, so only the full image is sent again.
    term.press(&[RIGHT, LEFT].concat());
    term.resume();
    term.wait_for_image("01.bmp");
    let seen = term.seen();
    assert_eq!(uploads(&seen), [(512, 394), (1300, 1000), (1300, 1000)]);
    assert_eq!(count(&seen, ABORT), 1);

    term.press(b"q");
    assert!(term.finish().0.success());
}

#[test]
fn escape_quits_at_rest_and_during_an_upload() {
    let dir = folder("escape-rest");
    plain_images(&dir, 1);
    let mut term = Terminal::start(dir, &["--mode", "kitty", "01.png"], None);
    term.wait_for_image("01.png");
    term.press(b"\x1b");
    let (exit, seen) = term.finish();
    assert!(exit.success());
    assert!(seen.ends_with(LEAVE));

    let dir = folder("escape-upload");
    noise_image(&dir.join("01.bmp"));
    let mut term = Terminal::start(dir, &["--mode", "kitty", "01.bmp"], None);
    term.pause_at(b",s=1300,v=1000,");
    term.wait_until_paused();
    // Esc only counts as a key once nothing has followed it for a moment,
    // and the upload may well finish in that time, so it needn't be cut short.
    term.press(b"\x1b");
    term.resume();
    let (exit, seen) = term.finish();
    assert!(exit.success());
    assert!(seen.ends_with(LEAVE));
}

#[test]
fn a_temporary_file_carries_the_image_when_asked() {
    let dir = folder("file");
    plain_images(&dir, 2);
    let mut term = Terminal::start_with(dir, &["--mode", "kitty", "."], None, "file");
    term.wait_for_image("01.png");
    let seen = term.seen();
    assert_eq!(uploads(&seen), [(1201, 800)]);
    // The file holds the pixels as they are: three bytes each.
    let first = last_upload_file(&seen);
    assert_eq!(std::fs::metadata(&first).unwrap().len(), 1201 * 800 * 3);

    term.press(RIGHT);
    term.wait_for_image("02.png");
    let second = last_upload_file(&term.seen());
    assert_ne!(first, second);
    assert_eq!(std::fs::metadata(&second).unwrap().len(), 1202 * 800 * 3);

    // A terminal deletes each file as it reads it. This one never did, so
    // qpic cleans up after it.
    term.press(b"q");
    assert!(term.finish().0.success());
    assert!(!first.exists() && !second.exists());
}

#[test]
fn an_image_the_terminal_refuses_is_sent_again_and_then_reported() {
    let dir = folder("refused");
    plain_images(&dir, 1);
    let mut term = Terminal::start(dir, &["--mode", "kitty", "01.png"], None);
    term.wait_for_image("01.png");
    let refuse = |term: &mut Terminal| {
        let id = last_upload_id(&term.seen());
        term.press(format!("\x1b_Gi={id};ENOMEM:too big\x1b\\").as_bytes());
    };
    for sent_again in 1..=3 {
        refuse(&mut term);
        term.wait_for("the image sent again", |seen| {
            uploads(seen).len() == sent_again + 1
        });
        term.wait_for_image("01.png");
    }
    // After three tries qpic says what the terminal said.
    refuse(&mut term);
    term.wait_for("the refusal in the status bar", |seen| {
        status(seen).contains("terminal: ENOMEM:too big")
    });
    assert_eq!(uploads(&term.seen()).len(), 4);

    term.press(b"q");
    assert!(term.finish().0.success());
}

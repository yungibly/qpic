//! The controlling terminal: raw mode, size, waiting for input and signals.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM, SIGWINCH};

/// Alternate screen, hidden cursor, no autowrap (so a long status line can
/// never scroll the screen and drag the image with it), cleared screen.
pub const ENTER: &str = "\x1b[?1049h\x1b[?25l\x1b[?7l\x1b[2J";

/// Undoes everything qpic turns on, including the mouse modes and pointer shape.
pub const LEAVE: &str = "\x1b[?2026l\x1b[?1016l\x1b[?1006l\x1b[?1002l\x1b[?1000l\
\x1b]22;default\x1b\\\x1b[0m\x1b[?7h\x1b[?25h\x1b[?1049l";

/// The terminal's original settings, for restoring them from the panic hook.
static SAVED: OnceLock<(i32, libc::termios)> = OnceLock::new();

pub struct Tty {
    file: File,
    orig: libc::termios,
    signals: UnixStream,
    /// Becomes readable when a `Waker` is used.
    wakes: UnixStream,
    waker: Waker,
    quit: Arc<AtomicBool>,
}

/// What a wait found: input from the terminal, or a signal. A wait that a
/// `Waker` or the timeout ended finds neither.
#[derive(Clone, Copy, Debug, Default)]
pub struct Ready {
    pub input: bool,
    pub signal: bool,
}

/// Ends the main loop's wait from another thread.
pub struct Waker(UnixStream);

impl Waker {
    /// A waker and the socket that becomes readable when it is used.
    pub fn pair() -> io::Result<(UnixStream, Waker)> {
        let (wakes, waker) = UnixStream::pair()?;
        wakes.set_nonblocking(true)?;
        waker.set_nonblocking(true)?;
        Ok((wakes, Waker(waker)))
    }

    pub fn wake(&self) {
        // A full socket means a wake is already waiting to be noticed.
        let _ = (&self.0).write(&[0]);
    }

    pub fn try_clone(&self) -> io::Result<Waker> {
        Ok(Waker(self.0.try_clone()?))
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Size {
    pub cols: u32,
    pub rows: u32,
    pub width_px: u32,
    pub height_px: u32,
}

impl Tty {
    /// Opens /dev/tty in raw mode, so qpic works even when stdin or stdout is a pipe.
    pub fn open() -> io::Result<Tty> {
        let file = OpenOptions::new().read(true).write(true).open("/dev/tty")?;
        let fd = file.as_raw_fd();
        // SAFETY: termios is plain data and tcgetattr fills it in.
        let mut orig: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(fd, &mut orig) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let mut raw = orig;
        unsafe { libc::cfmakeraw(&mut raw) };
        if unsafe { libc::tcsetattr(fd, libc::TCSAFLUSH, &raw) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let _ = SAVED.set((fd, orig));

        // Signals wake the main loop through a socket pair, so poll() sees them.
        let (rx, tx) = UnixStream::pair()?;
        rx.set_nonblocking(true)?;
        tx.set_nonblocking(true)?;
        let quit = Arc::new(AtomicBool::new(false));
        for sig in [SIGWINCH, SIGTERM, SIGHUP, SIGINT] {
            signal_hook::low_level::pipe::register(sig, tx.try_clone()?)?;
        }
        for sig in [SIGTERM, SIGHUP, SIGINT] {
            signal_hook::flag::register(sig, quit.clone())?;
        }
        let (wakes, waker) = Waker::pair()?;
        Ok(Tty {
            file,
            orig,
            signals: rx,
            wakes,
            waker,
            quit,
        })
    }

    pub fn waker(&self) -> io::Result<Waker> {
        self.waker.try_clone()
    }

    pub fn size(&self) -> Size {
        // SAFETY: winsize is plain data and TIOCGWINSZ fills it in.
        let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
        unsafe { libc::ioctl(self.file.as_raw_fd(), libc::TIOCGWINSZ, &mut ws) };
        Size {
            cols: ws.ws_col as u32,
            rows: ws.ws_row as u32,
            width_px: ws.ws_xpixel as u32,
            height_px: ws.ws_ypixel as u32,
        }
    }

    /// Waits for input, a signal, or a `Waker`. `None` waits forever.
    ///
    /// This uses select() rather than poll(): macOS's poll() doesn't support
    /// devices and reports /dev/tty as invalid, while select() works everywhere.
    pub fn wait(&self, timeout: Option<Duration>) -> io::Result<Ready> {
        let (tty, signals) = (self.file.as_raw_fd(), self.signals.as_raw_fd());
        let wakes = self.wakes.as_raw_fd();
        // SAFETY: fd_set is plain data, the descriptors are open and far below
        // FD_SETSIZE, and select() only touches the sets and timeval given.
        let (input, signal, woken) = unsafe {
            let mut read: libc::fd_set = std::mem::zeroed();
            libc::FD_ZERO(&mut read);
            libc::FD_SET(tty, &mut read);
            libc::FD_SET(signals, &mut read);
            libc::FD_SET(wakes, &mut read);
            let mut tv = timeout.map(|d| libc::timeval {
                tv_sec: d.as_secs().min(i32::MAX as u64) as libc::time_t,
                tv_usec: d.subsec_micros() as libc::suseconds_t,
            });
            let tv_ptr = tv
                .as_mut()
                .map_or(std::ptr::null_mut(), |t| t as *mut libc::timeval);
            let n = libc::select(
                tty.max(signals).max(wakes) + 1,
                &mut read,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                tv_ptr,
            );
            if n < 0 {
                let err = io::Error::last_os_error();
                return if err.kind() == io::ErrorKind::Interrupted {
                    Ok(Ready {
                        signal: true,
                        ..Ready::default()
                    })
                } else {
                    Err(err)
                };
            }
            (
                libc::FD_ISSET(tty, &read),
                libc::FD_ISSET(signals, &read),
                libc::FD_ISSET(wakes, &read),
            )
        };
        if woken {
            let mut buf = [0u8; 64];
            while matches!((&self.wakes).read(&mut buf), Ok(n) if n > 0) {}
        }
        Ok(Ready { input, signal })
    }

    /// Drains pending signal notifications. Returns true if one asked qpic to quit.
    pub fn take_signals(&mut self) -> bool {
        let mut buf = [0u8; 64];
        while matches!((&self.signals).read(&mut buf), Ok(n) if n > 0) {}
        self.quit.load(Ordering::Relaxed)
    }

    pub fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.file.read(buf)
    }
}

impl Write for Tty {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.file.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

impl Drop for Tty {
    fn drop(&mut self) {
        unsafe { libc::tcsetattr(self.file.as_raw_fd(), libc::TCSAFLUSH, &self.orig) };
    }
}

/// Restores the terminal before a panic message prints, so the message is
/// readable. A panic in the loader thread only costs the image it was
/// decoding, which the viewer reports itself, so the terminal stays as it is.
pub fn install_panic_hook() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if std::thread::current().name() == Some(crate::loader::THREAD) {
            return;
        }
        if let Some((fd, orig)) = SAVED.get() {
            unsafe {
                libc::write(*fd, LEAVE.as_ptr().cast(), LEAVE.len());
                libc::tcsetattr(*fd, libc::TCSAFLUSH, orig);
            }
        }
        default(info);
    }));
}

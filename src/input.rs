//! Turns terminal input bytes into keys, mouse events, and replies to queries.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Ctrl(char),
    Enter,
    Tab,
    Backspace,
    Esc,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Delete,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Button {
    Left,
    Middle,
    Right,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseKind {
    Press(Button),
    Release(Button),
    Drag(Button),
    Move,
    WheelUp,
    WheelDown,
    WheelLeft,
    WheelRight,
}

/// A mouse event at 1-based cell coordinates, or pixel coordinates when the
/// terminal reports pixels (mode 1016).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mouse {
    pub kind: MouseKind,
    pub x: i32,
    pub y: i32,
    pub shift: bool,
    pub ctrl: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    Key(Key),
    Mouse(Mouse),
    /// A Kitty graphics reply, such as `OK` or `ENOENT:...`.
    KittyReply {
        id: u32,
        ok: bool,
        message: String,
    },
    /// A DECRPM reply: 0 unknown, 1 set, 2 reset, 3 always set, 4 always reset.
    ModeReport {
        mode: u32,
        value: u32,
    },
    CellSize {
        w: u32,
        h: u32,
    },
    WindowPx {
        w: u32,
        h: u32,
    },
    /// The primary device attributes reply, which ends a round of queries.
    DeviceAttrs,
}

enum Step {
    /// Consumed this many bytes, producing an event or nothing.
    Done(Option<Event>, usize),
    /// The bytes so far are the start of a longer sequence.
    Incomplete,
}

#[derive(Default)]
pub struct Parser {
    buf: Vec<u8>,
}

impl Parser {
    pub fn feed(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// Whether bytes are waiting for the rest of a sequence.
    pub fn has_pending(&self) -> bool {
        !self.buf.is_empty()
    }

    /// Returns the next complete event. With `flush`, an unfinished sequence is
    /// taken as it stands, which is how a lone Esc key press is recognized.
    pub fn next(&mut self, flush: bool) -> Option<Event> {
        while !self.buf.is_empty() {
            match step(&self.buf, flush) {
                Step::Done(event, n) => {
                    self.buf.drain(..n.clamp(1, self.buf.len()));
                    if event.is_some() {
                        return event;
                    }
                }
                Step::Incomplete => return None,
            }
        }
        None
    }
}

fn key(k: Key, n: usize) -> Step {
    Step::Done(Some(Event::Key(k)), n)
}

fn esc_or_wait(flush: bool) -> Step {
    if flush {
        key(Key::Esc, 1)
    } else {
        Step::Incomplete
    }
}

fn step(b: &[u8], flush: bool) -> Step {
    match b[0] {
        0x1b => match b.get(1) {
            None => esc_or_wait(flush),
            Some(b'[') => csi(b, flush),
            Some(b'O') => match b.get(2) {
                None => esc_or_wait(flush),
                Some(&c) => Step::Done(ss3(c), 3),
            },
            Some(b'_' | b']' | b'P' | b'^' | b'X') => string(b, flush),
            Some(0x1b) => key(Key::Esc, 1),
            // Alt+key, or Esc and a key typed in quick succession: take the key.
            Some(&c @ 0x20..=0x7e) => key(Key::Char(c as char), 2),
            Some(_) => Step::Done(None, 2),
        },
        b'\r' | b'\n' => key(Key::Enter, 1),
        b'\t' => key(Key::Tab, 1),
        0x7f | 0x08 => key(Key::Backspace, 1),
        c @ 0x01..=0x1a => key(Key::Ctrl((b'a' + c - 1) as char), 1),
        c if c < 0x20 => Step::Done(None, 1),
        c if c < 0x80 => key(Key::Char(c as char), 1),
        c => {
            let len = match c {
                0xc0..=0xdf => 2,
                0xe0..=0xef => 3,
                0xf0..=0xf7 => 4,
                _ => return Step::Done(None, 1),
            };
            if b.len() < len {
                return if flush {
                    Step::Done(None, b.len())
                } else {
                    Step::Incomplete
                };
            }
            match std::str::from_utf8(&b[..len])
                .ok()
                .and_then(|s| s.chars().next())
            {
                Some(ch) => key(Key::Char(ch), len),
                None => Step::Done(None, 1),
            }
        }
    }
}

fn ss3(c: u8) -> Option<Event> {
    let k = match c {
        b'A' => Key::Up,
        b'B' => Key::Down,
        b'C' => Key::Right,
        b'D' => Key::Left,
        b'H' => Key::Home,
        b'F' => Key::End,
        _ => return None,
    };
    Some(Event::Key(k))
}

fn num(s: &[u8]) -> u32 {
    s.iter()
        .take_while(|c| c.is_ascii_digit())
        .fold(0u32, |n, &c| {
            n.saturating_mul(10).saturating_add((c - b'0') as u32)
        })
}

fn csi(b: &[u8], flush: bool) -> Step {
    let mut i = 2;
    loop {
        match b.get(i) {
            None => return esc_or_wait(flush),
            Some(0x40..=0x7e) => break,
            Some(0x20..=0x3f) => i += 1,
            // Not a valid sequence: drop what we have.
            Some(_) => return Step::Done(None, i),
        }
    }
    let fin = b[i];
    let body = &b[2..i];
    let (prefix, body) = match body.first() {
        Some(&p @ (b'<' | b'?' | b'=' | b'>')) => (Some(p), &body[1..]),
        _ => (None, body),
    };
    let split = body
        .iter()
        .position(|c| (0x20..=0x2f).contains(c))
        .unwrap_or(body.len());
    let (params, inter) = body.split_at(split);
    let p: Vec<u32> = params.split(|&c| c == b';').map(num).collect();
    let get = |i: usize| p.get(i).copied().unwrap_or(0);

    let k = |k: Key| Some(Event::Key(k));
    let event = match (prefix, inter, fin) {
        (Some(b'<'), b"", b'M' | b'm') => mouse(get(0), get(1), get(2), fin == b'M'),
        (Some(b'?'), b"", b'c') => Some(Event::DeviceAttrs),
        (Some(b'?'), b"$", b'y') => Some(Event::ModeReport {
            mode: get(0),
            value: get(1),
        }),
        (None, b"", b't') => match get(0) {
            6 => Some(Event::CellSize {
                h: get(1),
                w: get(2),
            }),
            4 => Some(Event::WindowPx {
                h: get(1),
                w: get(2),
            }),
            _ => None,
        },
        (None, b"", b'A') => k(Key::Up),
        (None, b"", b'B') => k(Key::Down),
        (None, b"", b'C') => k(Key::Right),
        (None, b"", b'D') => k(Key::Left),
        (None, b"", b'H') => k(Key::Home),
        (None, b"", b'F') => k(Key::End),
        (None, b"", b'~') => match get(0) {
            1 | 7 => k(Key::Home),
            4 | 8 => k(Key::End),
            3 => k(Key::Delete),
            5 => k(Key::PageUp),
            6 => k(Key::PageDown),
            _ => None,
        },
        _ => None,
    };
    Step::Done(event, i + 1)
}

/// Decodes an SGR mouse report: `ESC [ < button ; x ; y M` (press) or `m` (release).
fn mouse(code: u32, x: u32, y: u32, press: bool) -> Option<Event> {
    let shift = code & 4 != 0;
    let ctrl = code & 16 != 0;
    let motion = code & 32 != 0;
    let base = code & !(4 | 8 | 16 | 32);
    let button = match base & 3 {
        _ if base >= 128 => Button::Other,
        0 => Button::Left,
        1 => Button::Middle,
        2 => Button::Right,
        _ => Button::Other,
    };
    let kind = if (64..128).contains(&base) {
        if !press {
            return None;
        }
        match base - 64 {
            0 => MouseKind::WheelUp,
            1 => MouseKind::WheelDown,
            2 => MouseKind::WheelLeft,
            _ => MouseKind::WheelRight,
        }
    } else if motion {
        if base & 3 == 3 && base < 128 {
            MouseKind::Move
        } else {
            MouseKind::Drag(button)
        }
    } else if press {
        MouseKind::Press(button)
    } else {
        MouseKind::Release(button)
    };
    Some(Event::Mouse(Mouse {
        kind,
        x: x as i32,
        y: y as i32,
        shift,
        ctrl,
    }))
}

/// APC, OSC, DCS, PM and SOS strings, ending in ST or BEL. Only Kitty graphics
/// replies (APC starting with `G`) mean anything to qpic.
fn string(b: &[u8], flush: bool) -> Step {
    let unfinished = || {
        if flush {
            Step::Done(None, b.len())
        } else {
            Step::Incomplete
        }
    };
    let mut i = 2;
    let (end, len) = loop {
        match b.get(i) {
            None => return unfinished(),
            Some(0x07) => break (i, i + 1),
            Some(0x1b) => match b.get(i + 1) {
                None => return unfinished(),
                Some(b'\\') => break (i, i + 2),
                // A new sequence began without a terminator: end the string here.
                Some(_) => break (i, i),
            },
            Some(_) => i += 1,
        }
    };
    let event = if b[1] == b'_' {
        kitty_reply(&b[2..end])
    } else {
        None
    };
    Step::Done(event, len)
}

fn kitty_reply(s: &[u8]) -> Option<Event> {
    let s = s.strip_prefix(b"G")?;
    let (keys, msg) = match s.iter().position(|&c| c == b';') {
        Some(p) => (&s[..p], &s[p + 1..]),
        None => (s, &b""[..]),
    };
    let id = keys
        .split(|&c| c == b',')
        .find_map(|kv| kv.strip_prefix(b"i="))
        .map(num)
        .unwrap_or(0);
    let message = String::from_utf8_lossy(msg).into_owned();
    Some(Event::KittyReply {
        id,
        ok: message == "OK",
        message,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all(bytes: &[u8], flush: bool) -> Vec<Event> {
        let mut p = Parser::default();
        p.feed(bytes);
        std::iter::from_fn(|| p.next(flush)).collect()
    }

    fn m(kind: MouseKind, x: i32, y: i32) -> Event {
        Event::Mouse(Mouse {
            kind,
            x,
            y,
            shift: false,
            ctrl: false,
        })
    }

    #[test]
    fn keys() {
        assert_eq!(
            all(b"q\x1b[A\x1bOB\x1b[5~\x03 \x7f", false),
            vec![
                Event::Key(Key::Char('q')),
                Event::Key(Key::Up),
                Event::Key(Key::Down),
                Event::Key(Key::PageUp),
                Event::Key(Key::Ctrl('c')),
                Event::Key(Key::Char(' ')),
                Event::Key(Key::Backspace),
            ]
        );
        assert_eq!(all("é".as_bytes(), false), vec![Event::Key(Key::Char('é'))]);
    }

    #[test]
    fn lone_escape_needs_flush() {
        assert_eq!(all(b"\x1b", false), vec![]);
        assert_eq!(all(b"\x1b", true), vec![Event::Key(Key::Esc)]);
        assert_eq!(all(b"\x1bn", false), vec![Event::Key(Key::Char('n'))]);
    }

    #[test]
    fn mouse_events() {
        assert_eq!(
            all(
                b"\x1b[<0;10;5M\x1b[<32;12;6M\x1b[<0;12;6m\x1b[<64;3;4M\x1b[<65;3;4M\x1b[<35;1;1M",
                false
            ),
            vec![
                m(MouseKind::Press(Button::Left), 10, 5),
                m(MouseKind::Drag(Button::Left), 12, 6),
                m(MouseKind::Release(Button::Left), 12, 6),
                m(MouseKind::WheelUp, 3, 4),
                m(MouseKind::WheelDown, 3, 4),
                m(MouseKind::Move, 1, 1),
            ]
        );
        let shifted = all(b"\x1b[<68;1;1M", false);
        assert!(matches!(
            shifted[0],
            Event::Mouse(Mouse {
                kind: MouseKind::WheelUp,
                shift: true,
                ..
            })
        ));
    }

    #[test]
    fn replies() {
        assert_eq!(
            all(b"\x1b_Gi=7;OK\x1b\\\x1b_Gi=8;ENOENT:nope\x1b\\\x1b[?1016;2$y\x1b[6;20;10t\x1b[?62;22c", false),
            vec![
                Event::KittyReply { id: 7, ok: true, message: "OK".into() },
                Event::KittyReply { id: 8, ok: false, message: "ENOENT:nope".into() },
                Event::ModeReport { mode: 1016, value: 2 },
                Event::CellSize { w: 10, h: 20 },
                Event::DeviceAttrs,
            ]
        );
    }

    #[test]
    fn split_sequences_wait_for_the_rest() {
        let mut p = Parser::default();
        p.feed(b"\x1b[<0;1");
        assert_eq!(p.next(false), None);
        p.feed(b"0;5M\x1b_Gi=1;O");
        assert_eq!(
            p.next(false),
            Some(m(MouseKind::Press(Button::Left), 10, 5))
        );
        assert_eq!(p.next(false), None);
        p.feed(b"K\x1b\\");
        assert!(matches!(
            p.next(false),
            Some(Event::KittyReply {
                id: 1,
                ok: true,
                ..
            })
        ));
    }

    #[test]
    fn other_strings_are_ignored() {
        assert_eq!(
            all(b"\x1b]11;rgb:0/0/0\x07x", false),
            vec![Event::Key(Key::Char('x'))]
        );
    }
}

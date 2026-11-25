//! The Kitty graphics protocol. qpic uploads each image once, then pans and
//! zooms by moving a placement: a source rectangle the terminal scales into
//! a block of cells. That keeps every frame to a few dozen bytes.

use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use flate2::Compression;
use flate2::write::ZlibEncoder;

use crate::view::Placement;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transport {
    /// Base64 inside the escape codes, which works over ssh.
    Direct,
    /// A temporary file the terminal reads and deletes, when it shares our filesystem.
    TempFile,
}

/// An image uploaded to the terminal, at the size it was uploaded at.
#[derive(Clone, Copy, Debug)]
pub struct Image {
    pub id: u32,
    pub w: u32,
    pub h: u32,
}

/// Ends a direct upload early; the terminal discards the incomplete image.
pub const ABORT: &[u8] = b"\x1b_Gm=0,q=2;\x1b\\";

fn encode_path(path: &Path) -> String {
    STANDARD.encode(path.as_os_str().as_bytes())
}

/// Asks whether the terminal accepts a one-pixel image, directly or from `file`.
pub fn query(out: &mut Vec<u8>, id: u32, file: Option<&Path>) {
    let _ = match file {
        None => write!(out, "\x1b_Gi={id},s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\"),
        Some(p) => write!(
            out,
            "\x1b_Gi={id},s=1,v=1,a=q,t=t,f=24;{}\x1b\\",
            encode_path(p)
        ),
    };
}

/// Directories a terminal may accept temporary files from. Terminals check
/// for a temp directory, and they differ in which: on macOS, /tmp is a link to
/// /private/tmp, so some accept only $TMPDIR. qpic asks about each in turn.
pub fn temp_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    for dir in [PathBuf::from("/tmp"), std::env::temp_dir()] {
        if dir.is_dir() && !dirs.contains(&dir) {
            dirs.push(dir);
        }
    }
    dirs
}

/// A path the protocol allows for temporary files: in a temp directory, with
/// "tty-graphics-protocol" in its name.
pub fn temp_path(dir: &Path, id: u32) -> PathBuf {
    dir.join(format!(
        "qpic-tty-graphics-protocol-{}-{id}",
        std::process::id()
    ))
}

/// `format` is 24 (RGB) or 32 (RGBA).
pub fn transmit_file(out: &mut Vec<u8>, id: u32, format: u32, w: u32, h: u32, path: &Path) {
    let _ = write!(
        out,
        "\x1b_Ga=t,i={id},f={format},s={w},v={h},t=t,q=1;{}\x1b\\",
        encode_path(path)
    );
}

/// An image's pixels, ready to send.
pub struct Payload {
    /// 24 (RGB) or 32 (RGBA).
    pub format: u32,
    pub w: u32,
    pub h: u32,
    /// The pixels as they are, for a temporary file, or what `encode` made of
    /// them, for a direct upload.
    pub data: Vec<u8>,
    pub encoded: bool,
}

impl Payload {
    pub fn new(format: u32, w: u32, h: u32, pixels: Vec<u8>, transport: Transport) -> Payload {
        let encoded = transport == Transport::Direct;
        let data = if encoded { encode(&pixels) } else { pixels };
        Payload {
            format,
            w,
            h,
            data,
            encoded,
        }
    }

    /// Turns raw pixels into a direct upload, for when a temporary file fails.
    pub fn encode(&mut self) {
        if !self.encoded {
            self.data = encode(&self.data);
            self.encoded = true;
        }
    }
}

/// Compresses pixels into the text a direct upload carries.
fn encode(pixels: &[u8]) -> Vec<u8> {
    let mut z = ZlibEncoder::new(Vec::with_capacity(pixels.len() / 2), Compression::fast());
    let _ = z.write_all(pixels);
    STANDARD.encode(z.finish().unwrap_or_default()).into_bytes()
}

/// How much of an encoded image one escape code of a direct upload carries.
pub const CHUNK: usize = 4096;

/// Writes the escape code that carries an encoded image's data from `offset`
/// on, and returns where the next one starts: the data's length once it has
/// all gone. Text may go between the codes, but no other graphics commands.
pub fn direct_chunk(out: &mut Vec<u8>, id: u32, image: &Payload, offset: usize) -> usize {
    let end = (offset + CHUNK).min(image.data.len());
    let more = (end < image.data.len()) as u8;
    let _ = if offset == 0 {
        let Payload { format, w, h, .. } = image;
        write!(
            out,
            "\x1b_Ga=t,i={id},f={format},s={w},v={h},o=z,t=d,q=1,m={more};"
        )
    } else {
        write!(out, "\x1b_Gm={more},q=1;")
    };
    out.extend_from_slice(&image.data[offset..end]);
    out.extend_from_slice(b"\x1b\\");
    end
}

/// Shows `img` according to `pl`, whose source rectangle is in the pixels of
/// the original `orig_w` × `orig_h` image. Replaces the image's previous placement.
pub fn place(out: &mut Vec<u8>, img: &Image, pl: &Placement, orig_w: f64, orig_h: f64) {
    let span = |start: f64, len: f64, scale: f64, size: u32| {
        let a = (start * scale)
            .round()
            .clamp(0.0, size.saturating_sub(1) as f64);
        let b = ((start + len) * scale).round().clamp(a + 1.0, size as f64);
        (a as u32, (b - a) as u32)
    };
    let (x, w) = span(pl.src_x, pl.src_w, img.w as f64 / orig_w, img.w);
    let (y, h) = span(pl.src_y, pl.src_h, img.h as f64 / orig_h, img.h);
    let _ = write!(
        out,
        "\x1b[{};{}H\x1b_Ga=p,i={},p=1,x={x},y={y},w={w},h={h},c={},r={},X={},Y={},C=1,q=1\x1b\\",
        pl.row + 1,
        pl.col + 1,
        img.id,
        pl.cols,
        pl.rows,
        pl.x_off,
        pl.y_off,
    );
}

/// Hides an image, keeping its data for later.
pub fn delete_placements(out: &mut Vec<u8>, id: u32) {
    let _ = write!(out, "\x1b_Ga=d,d=i,i={id},q=2\x1b\\");
}

/// Hides an image and frees its data.
pub fn free(out: &mut Vec<u8>, id: u32) {
    let _ = write!(out, "\x1b_Ga=d,d=I,i={id},q=2\x1b\\");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunks_mark_all_but_the_last_as_continued() {
        let pixels: Vec<u8> = (0..200_000u32)
            .map(|i| (i.wrapping_mul(2654435761) >> 13) as u8)
            .collect();
        let image = Payload::new(24, 400, 166, pixels.clone(), Transport::Direct);
        let (mut chunks, mut offset) = (Vec::new(), 0);
        while offset < image.data.len() {
            let mut chunk = Vec::new();
            offset = direct_chunk(&mut chunk, 5, &image, offset);
            chunks.push(chunk);
        }
        assert!(chunks.len() > 2);
        assert!(chunks[0].starts_with(b"\x1b_Ga=t,i=5,f=24,s=400,v=166,o=z,t=d,q=1,m=1;"));
        assert!(chunks[1].starts_with(b"\x1b_Gm=1,q=1;"));
        assert!(chunks.last().unwrap().starts_with(b"\x1b_Gm=0,q=1;"));
        assert!(chunks.iter().all(|c| c.ends_with(b"\x1b\\")));

        // The chunks carry the pixels, compressed and base64-encoded.
        let text: Vec<u8> = chunks
            .iter()
            .flat_map(|c| {
                let start = c.iter().position(|&b| b == b';').unwrap() + 1;
                c[start..c.len() - 2].to_vec()
            })
            .collect();
        let mut unpacked = Vec::new();
        let compressed = STANDARD.decode(text).unwrap();
        std::io::Read::read_to_end(
            &mut flate2::read::ZlibDecoder::new(&compressed[..]),
            &mut unpacked,
        )
        .unwrap();
        assert_eq!(unpacked, pixels);
    }

    #[test]
    fn a_temporary_file_payload_keeps_the_pixels_until_encoded() {
        let mut image = Payload::new(24, 2, 1, vec![1, 2, 3, 4, 5, 6], Transport::TempFile);
        assert_eq!((image.encoded, image.data.len()), (false, 6));
        image.encode();
        assert!(image.encoded);
        let mut chunk = Vec::new();
        assert_eq!(direct_chunk(&mut chunk, 9, &image, 0), image.data.len());
        assert!(chunk.starts_with(b"\x1b_Ga=t,i=9,f=24,s=2,v=1,o=z,t=d,q=1,m=0;"));
    }

    #[test]
    fn placement_scales_the_source_to_the_uploaded_size() {
        let img = Image {
            id: 9,
            w: 1000,
            h: 500,
        };
        let pl = Placement {
            col: 0,
            row: 2,
            x_off: 0,
            y_off: 4,
            cols: 80,
            rows: 20,
            src_x: 1000.0,
            src_y: 0.0,
            src_w: 2000.0,
            src_h: 1000.0,
        };
        let mut out = Vec::new();
        place(&mut out, &img, &pl, 4000.0, 2000.0);
        let s = String::from_utf8(out).unwrap();
        assert_eq!(
            s,
            "\x1b[3;1H\x1b_Ga=p,i=9,p=1,x=250,y=0,w=500,h=250,c=80,r=20,X=0,Y=4,C=1,q=1\x1b\\"
        );
    }
}

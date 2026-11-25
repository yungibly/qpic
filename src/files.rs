//! Finding images to show, and decoding them.

use std::borrow::Cow;
use std::cmp::Ordering;
use std::ffi::OsStr;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use image::metadata::Orientation;
use image::{DynamicImage, ImageDecoder, ImageReader};

const EXTENSIONS: &[&str] = &[
    "bmp", "gif", "ico", "jfif", "jpe", "jpeg", "jpg", "pam", "pbm", "pgm", "png", "pnm", "ppm",
    "qoi", "tga", "tif", "tiff", "webp",
];

pub fn is_image(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| EXTENSIONS.iter().any(|x| x.eq_ignore_ascii_case(e)))
}

/// Splits a list of paths read from a pipe: one per line, or NUL-separated
/// when it contains a NUL, as `find -print0` and `fd -0` write it.
pub fn split_list(bytes: &[u8]) -> Vec<PathBuf> {
    let lines = !bytes.contains(&0);
    bytes
        .split(|&b| b == if lines { b'\n' } else { 0 })
        .map(|path| match path.strip_suffix(b"\r") {
            Some(rest) if lines => rest,
            _ => path,
        })
        .filter(|path| !path.is_empty())
        .map(|path| PathBuf::from(OsStr::from_bytes(path)))
        .collect()
}

/// Builds the list of images to browse and the one to start at. One file
/// opens with the other images in its folder alongside it, like a GUI viewer,
/// unless `as_listed` says the paths are a list to show as it stands.
pub fn collect(args: &[PathBuf], as_listed: bool) -> Result<(Vec<PathBuf>, usize), String> {
    let missing = |p: &Path| format!("{}: no such file or directory", p.display());
    if let ([only], false) = (args, as_listed) {
        if only.is_dir() {
            let list = list_dir(only).map_err(|e| format!("{}: {e}", only.display()))?;
            if list.is_empty() {
                return Err(format!("{}: no images found", only.display()));
            }
            return Ok((list, 0));
        }
        if !only.exists() {
            return Err(missing(only));
        }
        let dir = match only.parent() {
            Some(p) if !p.as_os_str().is_empty() => p,
            _ => Path::new("."),
        };
        if let (Ok(list), Some(name)) = (list_dir(dir), only.file_name())
            && let Some(i) = list.iter().position(|p| p.file_name() == Some(name))
        {
            return Ok((list, i));
        }
        return Ok((vec![only.clone()], 0));
    }

    let mut list = Vec::new();
    for arg in args {
        if arg.is_dir() {
            list.extend(list_dir(arg).map_err(|e| format!("{}: {e}", arg.display()))?);
        } else if arg.exists() {
            list.push(arg.clone());
        } else {
            return Err(missing(arg));
        }
    }
    if list.is_empty() {
        return Err("no images found".into());
    }
    Ok((list, 0))
}

fn list_dir(dir: &Path) -> io::Result<Vec<PathBuf>> {
    let mut list: Vec<PathBuf> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
        .map(|e| e.path())
        .filter(|p| is_image(p) && p.is_file())
        .collect();
    list.sort_by(|a, b| {
        let (a, b) = (
            a.file_name().unwrap_or_default(),
            b.file_name().unwrap_or_default(),
        );
        natural_cmp(&a.to_string_lossy(), &b.to_string_lossy()).then_with(|| a.cmp(b))
    });
    Ok(list)
}

/// Compares names the way people count: "img2" before "img10", ignoring case.
pub fn natural_cmp(a: &str, b: &str) -> Ordering {
    let (mut a, mut b) = (a.as_bytes(), b.as_bytes());
    loop {
        match (a.first(), b.first()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let da = a.iter().take_while(|c| c.is_ascii_digit()).count();
                let db = b.iter().take_while(|c| c.is_ascii_digit()).count();
                let (na, ra) = a.split_at(da);
                let (nb, rb) = b.split_at(db);
                let trim = |n: &'_ [u8]| -> usize { n.iter().take_while(|&&c| c == b'0').count() };
                let (na, nb) = (&na[trim(na)..], &nb[trim(nb)..]);
                let ord = na.len().cmp(&nb.len()).then_with(|| na.cmp(nb));
                if ord != Ordering::Equal {
                    return ord;
                }
                (a, b) = (ra, rb);
            }
            (Some(x), Some(y)) => {
                let ord = x.to_ascii_lowercase().cmp(&y.to_ascii_lowercase());
                if ord != Ordering::Equal {
                    return ord;
                }
                (a, b) = (&a[1..], &b[1..]);
            }
        }
    }
}

/// Decodes an image, turned upright according to its EXIF orientation.
pub fn load(path: &Path) -> Result<DynamicImage, String> {
    let mut reader = ImageReader::open(path)
        .map_err(|e| e.to_string())?
        .with_guessed_format()
        .map_err(|e| e.to_string())?;
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(1 << 30);
    reader.limits(limits);
    let mut decoder = reader.into_decoder().map_err(|e| e.to_string())?;
    let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
    let mut img = DynamicImage::from_decoder(decoder).map_err(|e| e.to_string())?;
    img.apply_orientation(orientation);
    Ok(img)
}

/// Shrinks an image to at most `max_side` pixels across and `max_pixels` in total.
pub fn cap(img: &DynamicImage, max_side: u32, max_pixels: u64) -> Cow<'_, DynamicImage> {
    let (w, h) = (img.width().max(1) as f64, img.height().max(1) as f64);
    let scale = (max_side as f64 / w.max(h)).min((max_pixels as f64 / (w * h)).sqrt());
    if scale >= 1.0 {
        return Cow::Borrowed(img);
    }
    let (nw, nh) = (((w * scale) as u32).max(1), ((h * scale) as u32).max(1));
    Cow::Owned(img.thumbnail(nw, nh))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn natural_order() {
        let mut names = vec!["img10.png", "IMG2.png", "img1.png", "a.png", "img02b.png"];
        names.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(
            names,
            vec!["a.png", "img1.png", "IMG2.png", "img02b.png", "img10.png"]
        );
    }

    #[test]
    fn decodes_the_common_formats() {
        let dir = std::env::temp_dir().join(format!("qpic-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let img = DynamicImage::ImageRgb8(image::RgbImage::from_fn(6, 4, |x, y| {
            image::Rgb([x as u8 * 40, y as u8 * 60, 128])
        }));
        for ext in ["png", "jpg", "gif", "webp", "bmp", "tiff", "qoi"] {
            let path = dir.join(format!("sample.{ext}"));
            img.save(&path).unwrap();
            let loaded = load(&path).unwrap_or_else(|e| panic!("{ext}: {e}"));
            assert_eq!((loaded.width(), loaded.height()), (6, 4), "{ext}");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn lists_split_on_lines_or_nuls() {
        let paths = |bytes: &[u8]| -> Vec<String> {
            split_list(bytes)
                .iter()
                .map(|p| p.to_string_lossy().into_owned())
                .collect()
        };
        assert_eq!(
            paths(b"b.png\n/tmp/a b.png\r\n\nc.png"),
            ["b.png", "/tmp/a b.png", "c.png"]
        );
        assert_eq!(paths(b"one\nname.png\0b.png\0"), ["one\nname.png", "b.png"]);
        assert!(paths(b"\n").is_empty());
    }

    #[test]
    fn a_list_keeps_its_order_and_leaves_out_the_folder() {
        let dir = std::env::temp_dir().join(format!("qpic-list-test-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        for name in ["a.png", "b.png", "c.png", "sub/d.png"] {
            std::fs::write(dir.join(name), b"").unwrap();
        }
        let names = |list: &[PathBuf]| -> Vec<String> {
            list.iter()
                .map(|p| p.strip_prefix(&dir).unwrap().to_string_lossy().into_owned())
                .collect()
        };

        // As an argument, one file brings its folder along.
        let (list, start) = collect(&[dir.join("b.png")], false).unwrap();
        assert_eq!(
            (names(&list), start),
            (vec!["a.png".into(), "b.png".into(), "c.png".into()], 1)
        );

        // As a list, it is the only image.
        let (list, start) = collect(&[dir.join("b.png")], true).unwrap();
        assert_eq!((names(&list), start), (vec!["b.png".into()], 0));

        let (list, _) = collect(
            &[dir.join("c.png"), dir.join("sub"), dir.join("a.png")],
            true,
        )
        .unwrap();
        assert_eq!(names(&list), ["c.png", "sub/d.png", "a.png"]);

        assert_eq!(collect(&[], true).unwrap_err(), "no images found");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn image_extensions() {
        assert!(is_image(Path::new("a/B.JPG")));
        assert!(!is_image(Path::new("notes.txt")));
        assert!(!is_image(Path::new("png")));
    }
}

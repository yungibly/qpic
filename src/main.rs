mod app;
mod blocks;
mod files;
mod input;
mod kitty;
mod loader;
mod term;
mod view;

use std::io::Read;
use std::path::PathBuf;
use std::process::ExitCode;

use app::DrawMode;

const USAGE: &str = "\
qpic: view images in the terminal, zooming and panning with the mouse

Usage: qpic [options] <image|folder>...
       <command> | qpic [options] -

Opening one image lets you browse the other images in its folder.
A - reads paths from standard input, one per line, and shows them in that order.

Options:
  -m, --mode <mode>  How to draw: auto, kitty or blocks (default: auto)
  -h, --help         Show this help
  -V, --version      Show the version

In the viewer:
  Scroll wheel       Zoom at the pointer
  Drag               Pan
  Double-click       Toggle 100% and fit
  + - 0 1 f          Zoom in, zoom out, fit, actual size, fill the window
  Arrows, h j k l    Pan (left and right switch images when the image fits)
  n p Space          Next and previous image
  i                  Show or hide the status bar
  ?                  Help
  q                  Quit
";

fn parse_mode(s: &str) -> Option<DrawMode> {
    match s {
        "auto" => Some(DrawMode::Auto),
        "kitty" => Some(DrawMode::Kitty),
        "blocks" => Some(DrawMode::Blocks),
        _ => None,
    }
}

fn main() -> ExitCode {
    let mut args = std::env::args_os().skip(1);
    let mut paths = Vec::new();
    let mut mode = DrawMode::Auto;
    let mut only_paths = false;
    let mut listed = false;
    while let Some(arg) = args.next() {
        let text = arg.to_str().unwrap_or("");
        if only_paths || !text.starts_with('-') {
            paths.push(PathBuf::from(arg));
            continue;
        }
        if text == "-" {
            // The viewer talks to /dev/tty, which leaves standard input free
            // for a list. A path that is gone by now is skipped: whatever
            // wrote the list may have raced a file being deleted.
            let mut bytes = Vec::new();
            if let Err(e) = std::io::stdin().lock().read_to_end(&mut bytes) {
                eprintln!("qpic: can't read standard input: {e}");
                return ExitCode::FAILURE;
            }
            paths.extend(files::split_list(&bytes).into_iter().filter(|p| p.exists()));
            listed = true;
            continue;
        }
        let value = match text {
            "-h" | "--help" => {
                print!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            "-V" | "--version" => {
                println!("qpic {}", env!("CARGO_PKG_VERSION"));
                return ExitCode::SUCCESS;
            }
            "--" => {
                only_paths = true;
                continue;
            }
            "-m" | "--mode" => args.next().and_then(|v| v.into_string().ok()),
            _ if text.starts_with("--mode=") => Some(text["--mode=".len()..].to_string()),
            _ => {
                eprintln!("qpic: unknown option {text}\nTry 'qpic --help'.");
                return ExitCode::from(2);
            }
        };
        match value.as_deref().and_then(parse_mode) {
            Some(m) => mode = m,
            None => {
                eprintln!("qpic: --mode takes auto, kitty or blocks");
                return ExitCode::from(2);
            }
        }
    }
    if paths.is_empty() && !listed {
        eprint!("{USAGE}");
        return ExitCode::from(2);
    }
    let (list, start) = match files::collect(&paths, listed) {
        Ok(found) => found,
        Err(e) => {
            eprintln!("qpic: {e}");
            return ExitCode::FAILURE;
        }
    };
    match app::run(list, start, mode) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("qpic: {e}");
            ExitCode::FAILURE
        }
    }
}

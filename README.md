# qpic

[![CI](https://github.com/yungibly/qpic/actions/workflows/ci.yml/badge.svg)](https://github.com/yungibly/qpic/actions/workflows/ci.yml)

A terminal image viewer that works like a GUI one: scroll to zoom at the pointer, drag to pan, double-click for 100%.

```sh
brew install yungibly/tap/qpic
qpic photo.jpg
```

Images are drawn at full resolution in terminals with the Kitty graphics protocol, such as Ghostty, Kitty, and Rex. Other terminals get a lower-resolution version made of colored half-blocks. Both work over ssh.

## Install

`brew install yungibly/tap/qpic` works on macOS (Apple Silicon and Intel) and Linux x86_64. [Release archives](https://github.com/yungibly/qpic/releases) contain the same binaries: extract one and put `qpic` on your PATH. Linux needs glibc 2.35 or newer. There are no Linux ARM or Windows builds yet.

## Opening images

```sh
qpic photo.jpg            # the other images in its folder are a key away
qpic ~/Pictures           # every image in a folder
qpic a.png b.png c.png    # just these
ls -t *.png | qpic -      # paths from another program, in its order
```

`-` reads paths from standard input, one per line, so another program can choose what to show. `fd -e png --changed-within 1h . /tmp | qpic -` opens every PNG written under `/tmp` in the last hour. Paths separated by NUL bytes, as `find -print0` writes them, work too, and a path that no longer exists is skipped.

qpic reads PNG, JPEG, GIF (the first frame), WebP, BMP, TIFF, ICO, QOI, TGA, and PNM, and turns photos upright according to their EXIF orientation.

## Mouse and keys

| Input | Action |
| --- | --- |
| Scroll wheel | Zoom in or out at the pointer |
| Drag | Pan |
| Double-click | Toggle 100% and fit |
| `+` `-` | Zoom in, zoom out |
| `0` | Fit large images in the window; small ones stay at 100% |
| `f` | Fill the window, enlarging small images |
| `1` | Actual size |
| Arrows, `h` `j` `k` `l` | Pan; left and right switch images while the whole width fits |
| `n` `p`, Space, Backspace | Next, previous image |
| Home, End | First, last image |
| `i` | Show or hide the status bar |
| `?` | Help |
| `q`, Esc | Quit |

`--mode blocks` forces half-blocks; `--mode kitty` forces Kitty graphics when a terminal supports them without saying so.

## How it draws

With Kitty graphics, qpic sends each image once and then only tells the terminal which part to show and how big, so panning and zooming cost a few bytes a frame even over ssh. When the terminal can read files on qpic's machine, images go through a temporary file; otherwise they travel inline, compressed, with a quick preview first. Rex gets them inline too: its app can be on another machine, and only inline images reach it compressed.

Images load in the background, so keys never wait for one. Switching images names the new one in the status bar at once, and the picture follows when it has loaded. While keys come in quick succession, as they do when one is held down, qpic waits for them to stop and loads only the image they end on. A key that quits or switches images ends an upload under way.

Images you have viewed stay in the terminal, so going back is instant. Terminals cap the memory their images use: Ghostty and Kitty allow 320 MB, and Rex 128 MiB, which holds about two 12-megapixel photos. qpic stays within the cap, forgetting the least recently viewed images first, and adjusts if a terminal turns out to allow less. Very large images are scaled down to fit.

tmux and other multiplexers that don't pass Kitty graphics through get half-blocks.

## Development

Building needs **Rust 1.89 or newer**.

```sh
cargo build --locked --release
cargo test --locked
QPIC_LOG=/tmp/qpic.log target/release/qpic image.png   # log terminal replies and placements
```

`QPIC_TRANSPORT=direct` sends images inline even when a temporary file would work, which is how they travel over ssh. `QPIC_TRANSPORT=file` uses temporary files even where qpic would send inline, such as in Rex.

- [Release guide](docs/releases.md): CI, archives, and the Homebrew tap.

qpic's code is [MIT licensed](LICENSE); Rust dependencies keep their own notices in release archives.

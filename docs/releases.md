# Releasing qpic

The source repository is [`yungibly/qpic`](https://github.com/yungibly/qpic), and the binary formula lives in [`yungibly/homebrew-tap`](https://github.com/yungibly/homebrew-tap) as `Formula/qpic.rb`. The process follows pica's.

## CI

Pushes and pull requests run locked tests on Linux and macOS with Rust stable and the minimum toolchain, 1.89.0. Stable also checks formatting, strict all-target Clippy, and the release tool tests.

## Release archives

Release builds use Rust 1.98.0 on three native runners:

| Runner | Archive target | Compatibility |
| --- | --- | --- |
| `macos-15` | `aarch64-apple-darwin` | Apple Silicon; macOS 11.0 or newer |
| `macos-15-intel` | `x86_64-apple-darwin` | Intel; macOS 11.0 or newer |
| `ubuntu-22.04` | `x86_64-unknown-linux-gnu` | Linux x86_64; glibc 2.35 or newer |

1. Update the version in `Cargo.toml` and `Cargo.lock` together, and wait for green CI on that commit.
2. Push a matching tag, such as `v0.1.0`. The **Release archives** workflow checks the tag against Cargo's version and builds that exact commit.
3. Each native build repeats the tests and lint, copies the actual license notices of every locked dependency into `third_party/rust/`, packs `qpic-{version}-{target}.tar.gz`, and smoke-tests the extracted binary.
4. Assembly requires all three archives, verifies them, and writes `SHA256SUMS`. The last job creates a draft release and refuses to replace an existing one.
5. Check the draft's assets, then publish it with a maintainer account. A release published with the workflow's own token doesn't trigger the Homebrew workflow; dispatch it by hand in that case.

Locally, with a release binary built for the host target:

```sh
python3 -m unittest discover -s tools/release -p 'test_*.py'
python3 tools/release/package.py validate --tag v0.1.0
python3 tools/release/notices.py --target x86_64-unknown-linux-gnu --output target/rust-notices
python3 tools/release/package.py pack --tag v0.1.0 --target x86_64-unknown-linux-gnu \
  --binary target/release/qpic --notices target/rust-notices --output-dir target/packages --epoch 0
```

Use fresh output directories; the tools never overwrite.

## Homebrew

Publishing a stable release runs the **Homebrew** workflow; its manual dispatch takes a published tag, for retrying a failed tap update. It downloads the three archives and `SHA256SUMS` anonymously, verifies them, and renders `Formula/qpic.rb`. Native jobs on all three platforms install the public archives with that formula and run its test. Only then does the updater receive `BREW_TAP_TOKEN`, and it changes only `Formula/qpic.rb` on the tap's `main` branch. Repeating a release is a no-op, an older release can't replace a newer formula, and a released version's formula can't change.

`BREW_TAP_TOKEN` is an Actions secret in this repository with contents-write access to `yungibly/homebrew-tap`. The root `.env` is a local, ignored setup input; workflows never read it.

If a Homebrew test or the tap update fails after publishing, leave the tap alone, fix the failure, and rerun the workflow. Don't replace a published archive to fit an old formula.

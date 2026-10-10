# KVM-Control Plus

A **laptop-as-KVM** tool for Windows: plug a USB HDMI capture dongle into the
target machine and a CH9329 serial HID bridge into your own, and drive the target
from this app — its screen in the preview, your keyboard and mouse forwarded over
the serial link.

This is a rewrite of the C#/WinUI3 `Control3`, rebuilt in Rust around a hard
constraint: **two dependencies, total.**

## Why it is lean, on purpose

```toml
eframe  = "0.35"   # GUI
windows = "0.62"   # every bit of platform plumbing
```

That is the entire dependency list. Everything the original pulled in — a serial
crate, a capture crate, an image crate, serde, anyhow, a channel crate — has been
replaced by a few hundred lines of code on top of the `windows` crate:

| Normally you'd use | What this project does instead |
|---|---|
| `serialport` | `CreateFileW` + `BuildCommDCBW` + `PurgeComm` + `ReadFile`/`WriteFile` |
| `nokhwa` / `capture` | MediaFoundation source reader (`MFCreateSourceReaderFromMediaSource`) |
| `image` | a BMP writer, and MediaFoundation's own MJPG decoder |
| `serde` / `serde_json` | a hand-rolled `key = value` settings file |
| `anyhow` | the `windows` crate already returns `Result` with rich codes |

`eframe` links statically through OpenGL, so the result is one `.exe` with no
runtime, no DLL folder, and no installer.

### What actually ships

**One file: `KVM-Control-Plus.exe`, 7.3 MB.** No DLLs, no assets folder, no config to
install, no runtime to download.

Verified against the PE import table: the binary imports **23 libraries, every
one of them a Windows system DLL** (`kernel32`, `user32`, `gdi32`, `opengl32`,
`mf`/`mfplat`/`mfreadwrite` for MediaFoundation, `setupapi`, `ole32`, …). The C
runtime is linked statically — see `.cargo/config.toml` — so there is *no*
dependency on `VCRUNTIME140.dll` or the VC++ redistributable. That matters: with
the default dynamic CRT, "one file" would only be true on a machine that already
happened to have the redistributable installed. Static CRT costs ~170 KB.

Build with plain `cargo build --release`; the flags come from `.cargo/config.toml`
and apply automatically.

## The icon

The mark is a bolted porthole — a window into another machine, with light coming
through it. The window is the product's whole idea, whatever it ends up called. It is **generated, not drawn**: `tools/make_icon.py` renders it with
pure stdlib math (no Pillow, no SVG tooling) and writes the whole set.

```sh
python tools/make_icon.py     # assets/icon.ico, icon.rgba, previews
```

- `assets/icon.ico` — 16/24/32/48/64/128/256, 32-bit entries. Sizes **below 48px
  are rendered without the bolts**: eight bolt heads average into grey noise at
  that scale, so the small sizes get a simpler mark rather than a smudge.
- `assets/icon.rgba` — flat 256×256 RGBA for the window and taskbar icon, handed
  straight to `eframe::IconData`. Raw bytes rather than a PNG, because decoding a
  PNG would mean adding an image crate for one file.
- `assets/icon_preview.png` — every size on a dark and a light band, so the set
  can be judged the way Windows will actually draw it.

The `.ico` is compiled into the exe by `build.rs` driving the SDK's `rc.exe`
directly — no `winres`/`embed-resource` build dependency — along with version
info, so `KVM-Control-Plus.exe` reports a real product name and version in Explorer's
Properties. If `rc.exe` is absent the build warns and continues; cosmetics must
never break the build.

## Build

```sh
cargo build --release      # target/release/KVM-Control-Plus.exe
cargo test                 # 58 tests
```

Requires the MSVC toolchain (Rust 1.93 + VS 2022 verified). The `windows` crate
features are listed in `Cargo.toml`; add none you do not need.

## Use

Run it with no arguments for the GUI. The **Devices** tab carries a
*"Link & formats — why the picture looks like it does"* report that answers the
two questions that actually matter when the preview is black:

- **Which USB mode is the dongle running in?** Measured from the hub, not
  guessed — SuperSpeed (5 Gbit/s) or High Speed (480 Mbit/s).
- **Which formats can this link physically carry?** Every format the device
  offers, with the bandwidth each one costs and whether it fits.

Uncompressed 1080p needs 155–207 MB/s. A USB 2.0 link carries about 40 MB/s. On
a USB 2.0 link **only MJPEG can work** — an uncompressed selection will "stream"
fine and show you filler frames forever. The app says so instead of leaving you
to work it out.

### Headless modes

Everything the GUI knows is reachable from the command line, which is how the
capture problem was diagnosed in the first place:

| Flag | Does |
|---|---|
| `--version` / `-V` | version + build timestamp |
| `--list-devices` | video capture devices |
| `--list-formats <n>` | every native format of device *n* |
| `--list-ports` | serial ports, with reasons when empty |
| `--probe <n>` | open device *n* through MediaFoundation |
| `--usb` | **measured** USB link speed + the bandwidth arithmetic |
| `--snapshot <out.bmp> [n]` | save a frame (waits for a non-black one) |
| `--raw-probe <count> <n>` | walk all native formats, classify varying vs uniform |
| `--grab-native <index> <n>` | dump one raw, undecoded sample to disk |
| `--level <lvl>` `-v` `--trace` `-q` | log verbosity |

## Documentation

- [`docs/TODO.md`](docs/TODO.md) — living status: what is done, verified, and next.
- [`docs/CODE_MAP.md`](docs/CODE_MAP.md) — every module, the data flow, and the
  rules that keep the codebase lean.
- [`docs/CAPTURE_DIAGNOSIS.md`](docs/CAPTURE_DIAGNOSIS.md) — the full measured
  investigation into why a black picture happens, and what each cause looks like.

## Project policy

**Verbose logging is permanent.** Log lines are never removed to tidy up — if a
quieter view is wanted, add a level filter. The same applies to the UI: when the
app knows why something is wrong, it explains it on screen rather than showing a
bare error.

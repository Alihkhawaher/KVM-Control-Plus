# KVM-Control Plus — TODO / status

Living document. Updated as work lands. `[x]` = done and verified on real
hardware, `[~]` = coded but not yet exercised against a live source, `[ ]` = not
started.

Last updated: round 3 (GUI link/format report + USB speed measurement).

---

## Where this project is

A Rust rewrite of the C#/WinUI3 `Control3` (laptop-as-KVM: MS2130 capture dongle
+ CH9329 serial HID bridge). The whole app is **two direct dependencies** —
`eframe` (GUI) and `windows` (all platform plumbing). Nothing else.

Current state: **builds clean, 0 warnings, 58/58 tests pass**, and the full path
has been exercised against the real Hagibis MS2130 dongle on this machine.

---

## Done

- [x] **Research** — original C# app read; CH9329 protocol nailed down from
      datasheets + three open-source implementations; MS2130 UVC format lists
      confirmed from the datasheet.
- [x] **Lean manifest** — 2 direct deps. `serialport`, `nokhwa`, `image`,
      `serde`, `serde_json`, `anyhow`, `crossbeam-channel`, `egui_extras`,
      `tracing` all deliberately dropped and re-implemented in-tree.
- [x] **CH9329 protocol layer** (`src/kvm/ch9329.rs`) — pure, no I/O, byte-exact
      test vectors. GET_INFO (`57 AB 00 81`), keyboard, (absolute) mouse.
- [x] **Serial I/O by hand** (`src/kvm/serial_port.rs`) — raw `CreateFileW` /
      `BuildCommDCBW` / `PurgeComm`, manual `Debug`.
- [x] **Port enumeration** (`src/kvm/serial_scan.rs`) — SetupAPI, walks the
      device tree, finds CH9329-class interfaces; reports *why* nothing was
      found rather than just "0 ports".
- [x] **Key mapping** (`src/kvm/keymap.rs`) — VK → HID, US layout, media keys,
      numpad. 3 real mapping bugs found by tests and fixed
      (numpad off-by-one; `'0'` placed wrongly relative to `'1'..'9'`; a bad
      checksum in a test vector).
- [x] **Input capture** (`src/input/hook.rs`) — dedicated thread, own hidden
      window + message pump, low-level keyboard/mouse hooks + Raw Input for
      high-resolution movement. Wheel deltas normalised from 120-unit notches.
- [x] **Live KVM link + sender thread** (`src/kvm/cable.rs`) — coalesces mouse
      moves, tracks stats, checksums every frame.
- [x] **Video capture** (`src/video/mf.rs`) — MediaFoundation source reader.
      Reader negotiates **NV12** and converts NV12→RGB in-house so the colour
      path is traceable and testable. BMP snapshot writer.
- [x] **USB link measurement** (`src/video/usb_link.rs`) — *real* negotiated
      speed read from the USB hub via `IOCTL_USB_GET_NODE_CONNECTION_INFORMATION_EX`
      + `…_EX_V2`, not inferred. Reports speed, practical throughput, port, hub,
      and whether the port itself supports USB 3.x.
- [x] **Settings** (`src/config.rs`) — hand-rolled `key = value`, no serde.
- [x] **GUI** (`src/app.rs`, `src/ui/mod.rs`) — Device / Input / Macros / Options
      / About tabs, live video preview, log panel.
- [x] **GUI: verbose link & format report** — the panel tells the user which USB
      mode the link is in, what throughput that leaves, every format the device
      offers, and which of them physically fit. See
      `docs/CAPTURE_DIAGNOSIS.md`.
- [x] **App icon + version info** — `tools/make_icon.py` generates the whole set
      (stdlib only); `build.rs` embeds it via `rc.exe` with no build dependency.
      Verified from outside the code: the icon extracts back out of the exe and
      Explorer reports FileVersion 0.1.0.0 + a product name.
- [x] **Single-file artifact** — `+crt-static` in `.cargo/config.toml`. Verified
      from the PE import table: 23 imports, all Windows system DLLs, no
      `VCRUNTIME140.dll` and no `api-ms-win-crt-*` shims. Costs ~170 KB.
- [x] **CLI verification modes** — `--version`, `--list-ports`, `--list-devices`,
      `--list-formats N`, `--probe`, `--usb`, `--snapshot`, `--raw-probe`,
      `--grab-native`. These are how the capture problem was diagnosed.

## Not done

- [ ] **Capture format auto-selection is still manual.** The report *warns*
      loudly that an uncompressed format cannot fit a USB 2.0 link, but the app
      does not yet refuse to select one or auto-switch to MJPG. Planned: default
      the combo to the best MJPG mode at the highest offered rate, and treat
      uncompressed NV12/YUV as USB-3-only when building the list.
- [~] **Live picture confirmation.** Everything up to and including "the bytes
      arrive, the JPEG is valid, the buffer is the right size" is verified. A
      real moving picture has *not* been confirmed in-app, because the HDMI
      source on this machine was asleep every time it was tested (that produces
      a correct black frame). Needs one session with the source awake.
- [ ] **Mouse: relative mode.** Only absolute works today; relative HID mode for
      games is sketched in `ch9329.rs` but not wired to the UI.
- [ ] **Macros** — the tab exists, the recorder/player does not.
- [ ] **Clipboard / file transfer** over the serial link (the original app had
      nothing like it; would be a differentiator).
- [ ] **`README.md` / `docs/CODE_MAP.md`** — README done, code map done; keep
      both current as modules change.
- [ ] **Signature / packaging.** Currently a bare `.exe`; no installer by design
      (eframe links statically), but no icon/signing either.

## Known limitations (documented, not bugs)

- One-way video: this is a *capture + HID* tool. The remote machine's own
  display returns via the HDMI source, not through this program.
- Only the US keyboard layout is presentable to the remote host; `layout.rs`
  translates from the local layout so the user can type normally.
- MJPG is decoded by MediaFoundation. If a future build needs to avoid that
  dependency on the OS codec, a baseline JPEG decoder would have to be written
  in-tree (~600 lines) — deliberately not done for now.

## Project policy

Verbose logging is **permanent and first-class**. Log lines are never deleted to
"clean up"; if a quieter mode is wanted, add a level/flag that filters the view
instead. The same goes for the on-screen report: the app explains its reasoning
to the user rather than presenting a bare state.

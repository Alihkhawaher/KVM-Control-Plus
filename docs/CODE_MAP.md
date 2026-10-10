# KVM-Control Plus — code map

Where everything lives and why. Keep this in sync when modules move.

```
KVM-Control Plus/
├─ Cargo.toml        2 direct deps only (eframe + windows). Read the comment
│                    block at the top of [dependencies] before adding anything.
├─ .cargo/
│  └─ config.toml    +crt-static: drops the VCRUNTIME140.dll dependency so the
│                    .exe is genuinely standalone. Do not remove it casually.
├─ build.rs          embeds BUILD_TIMESTAMP (unix secs, shown by --version and
│                    the About tab) AND compiles the Windows resources: it
│                    generates a .rc and drives the SDK's rc.exe straight to a
│                    .res, to avoid a winres/embed-resource build dependency.
│                    Missing rc.exe warns, never fails.
├─ assets/           icon.ico (multi-size), icon.rgba (window icon, raw for
│                    eframe::IconData), icon_preview.png. All generated.
├─ tools/            make_icon.py — stdlib-only icon generator (no Pillow).
├─ docs/             this map, TODO, and the capture diagnosis.
├─ out/              scratch output from the CLI modes (BMP/JPG/PNG/logs).
└─ src/
   ├─ main.rs        CLI entry: flag parsing, the headless modes, then hands off
   │                 to the GUI. Every diagnostic the project owns is reachable
   │                 from here. In GUI mode it calls `detach_own_console()` — the
   │                 binary is a console app so the modes work in a terminal, and a
   │                 double-click therefore gets a console nobody asked for; it is
   │                 given back, but only when this process solely owns it.
   ├─ app.rs         The application: state machine, tabs, panels, and the
   │                 "link & formats" report. Largest file by design — one place
   │                 to see what the UI does. Holds the pane toggle
   │                 (`show_left_pane`) and `level_color`, which resolves log
   │                 colours against the active palette (colour is presentation,
   │                 so it does not live in `logging`).
   ├─ config.rs      Settings: hand-rolled `key = value`, no serde. Load is
   │                 once-logged (startup reads it twice).
   ├─ logging.rs     Ring-buffer logger + Level. Feeds both stdout and the GUI
   │                 log panel. Levels: trace < debug < info < warn < error.
   ├─ ui/
   │   ├─ mod.rs     Palette/theme (dark + light) and the small widget kit
   │   │             (section, kv, badge, pill, chip). `kv` truncates over-long
   │   │             values so a row can never overlap. Also installs
   │   │             `ScrollStyle::solid()`: the stock floating bar is drawn at
   │   │             zero opacity while idle, which makes a long pane look cut off.
   ├─ input/
   │   ├─ mod.rs     Re-exports.
   │   ├─ hook.rs    Dedicated input thread: own hidden window + message pump,
   │   │             WH_KEYBOARD_LL / WH_MOUSE_LL hooks, Raw Input for
   │   │             high-resolution movement. Posts InputEvent to the app.
   │   └─ leave.rs   LeaveGesture — the "get me out of here" hotkey handling.
   ├─ kvm/
   │   ├─ mod.rs     Re-exports.
   │   ├─ ch9329.rs  CH9329 wire protocol. Pure: builds and parses byte frames,
   │   │             no I/O, no OS calls — which is why it can be unit-tested
   │   │             byte-exactly.
   │   ├─ serial_port.rs  Raw Win32 serial handle (CreateFileW, DCB, PurgeComm,
   │   │                  Read/WriteFile). Manual Debug impl.
   │   ├─ serial_scan.rs  SetupAPI enumeration of CH9329-class serial devices,
   │   │                  with an explanation when the list comes back empty.
   │   ├─ keymap.rs  VK → HID translation, US layout, media keys, numpad.
   │   ├─ layout.rs  Local keyboard layout → what the remote host will see.
   │   └─ cable.rs   The live link: connection state, sender thread, mouse-move
   │                 coalescing, counters.
   └─ video/
       ├─ mod.rs     Re-exports + the BMP snapshot writer.
       ├─ mf.rs      MediaFoundation: device enumeration, native-format listing,
       │             source reader, NV12→RGB conversion, and the diagnostics
       │             (per-frame byte stats, --raw-probe, --grab-native).
       └─ usb_link.rs  Real USB link-speed measurement from the hub, plus the
                      arithmetic that decides whether a format can fit.
```

## Data flow

```
keyboard/mouse ──► input/hook.rs ──► InputEvent ──► app.rs ──► kvm/layout.rs
                                                          └──► kvm/keymap.rs
                                                          └──► kvm/cable.rs
                                                                   │
                                                        kvm/ch9329.rs (frames)
                                                                   │
                                                        kvm/serial_port.rs
                                                                   │
                                                            CH9329 ──► USB ──► remote PC

MS2130 dongle ──► video/mf.rs (source reader, NV12) ──► NV12→RGB ──► egui texture
                        │
                        └── video/usb_link.rs (how fast is this link, really?)
```

## Rules that keep this codebase what it is

1. **Two dependencies.** Adding a third is a design decision, not a convenience.
   Read the `Cargo.toml` comment first.
2. **Verbose logging is permanent.** Never delete a log line to tidy up. Add a
   level filter if the view is too noisy.
3. **Explain, don't just state.** When the app knows *why* something is wrong,
   it says so on screen (see `capture_report` in `app.rs`). A bare "error" is
   considered a bug in the UI.
4. **Pure layers stay pure.** `ch9329.rs` has no I/O; that is what makes the
   byte-level tests possible. Keep new protocol code that way.
5. **Diagnostics are features.** The `--raw-probe` / `--grab-native` / `--usb`
   modes exist because guessing was tried and did not work.

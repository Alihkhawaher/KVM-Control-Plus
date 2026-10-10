//! KVM-Control Plus — a lean rewrite of the Control3 laptop-as-KVM tool.
//!
//! Two direct dependencies (`eframe` for the GUI, `windows` for the platform)
//! and nothing else. Everything the original app pulled in from libraries is
//! hand-rolled here against the Win32 API:
//!
//!   * serial I/O      -> `CreateFileW` / `BuildCommDCBW`      (`kvm::serial_port`)
//!   * device listing  -> SetupAPI                             (`kvm::serial_scan`)
//!   * video capture   -> MediaFoundation source reader        (`video::mf`)
//!   * settings        -> a small `key = value` text file      (`config`)
//!   * logging         -> in-memory ring buffer + stdout        (`logging`)
//!
//! Module map: see `docs/CODE_MAP.md`. Progress: see `docs/TODO.md`.

mod app;
mod config;
mod input;
mod kvm;
mod logging;
mod ui;
mod video;

use config::Settings;
use logging::Level;

const VERSION: &str = env!("CARGO_PKG_VERSION");
/// Unix timestamp of the build, injected by `build.rs`.
const BUILD_TIMESTAMP: &str = env!("BUILD_TIMESTAMP");

fn main() -> eframe::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.iter().any(|a| a == "--help" || a == "-h") {
        print_help();
        return Ok(());
    }
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("kvm-control-plus {VERSION} (built {BUILD_TIMESTAMP})");
        println!("dependencies: eframe, windows (nothing else)");
        return Ok(());
    }

    // Verbose logging is a first-class, permanent feature of this project: it is
    // never removed, only ever made controllable with flags like these.
    let stdout_level = if args.iter().any(|a| a == "--quiet" || a == "-q") {
        Level::Warn
    } else if args.iter().any(|a| a == "--trace") {
        Level::Trace
    } else if args
        .iter()
        .any(|a| a == "--verbose" || a == "-v" || a == "--debug")
    {
        Level::Debug
    } else if let Some(i) = args.iter().position(|a| a == "--level") {
        args.get(i + 1)
            .and_then(|s| Level::parse(s))
            .unwrap_or(Level::Info)
    } else {
        Level::Info
    };
    logging::init(stdout_level, Level::Trace);
    logging::info(format!(
        "kvm-control-plus {VERSION} (built {BUILD_TIMESTAMP})"
    ));

    // ---- command-line modes that do not start the GUI -----------------------
    if args.iter().any(|a| a == "--list-devices") {
        match video::mf::list_devices() {
            Ok(devices) => {
                if devices.is_empty() {
                    println!("no video capture devices found");
                }
                for d in devices {
                    println!("[{}] {}\n     {}", d.index, d.name, d.symbolic_link);
                }
            }
            Err(e) => {
                eprintln!("video enumeration failed: {e}");
                std::process::exit(1);
            }
        }
        return Ok(());
    }

    if let Some(i) = args.iter().position(|a| a == "--list-formats") {
        let which = args.get(i + 1).cloned().unwrap_or_else(|| "0".to_string());
        let devices = match video::mf::list_devices() {
            Ok(d) => d,
            Err(e) => {
                eprintln!("video enumeration failed: {e}");
                std::process::exit(1);
            }
        };
        let Some(device) = devices
            .iter()
            .find(|d| d.index.to_string() == which || d.name == which)
            .or_else(|| devices.first())
        else {
            eprintln!("no capture device to inspect");
            std::process::exit(1);
        };
        println!("formats for {} — {}", device.name, device.symbolic_link);
        match video::mf::list_formats(&device.symbolic_link) {
            Ok(formats) => {
                for f in formats {
                    println!("     {}", f.label());
                }
            }
            Err(e) => {
                eprintln!("format enumeration failed: {e}");
                std::process::exit(1);
            }
        }
        return Ok(());
    }

    if args.iter().any(|a| a == "--list-ports") {
        let ports = kvm::serial_scan::list();
        if ports.is_empty() {
            println!("no serial ports found");
        }
        for p in ports {
            println!(
                "{}{}\n     {}",
                p.port,
                if p.is_kvm_cable { "  [KVM cable]" } else { "" },
                p.hardware_id
            );
        }
        return Ok(());
    }

    if let Some(i) = args.iter().position(|a| a == "--probe") {
        let port = args
            .get(i + 1)
            .cloned()
            .or_else(|| kvm::serial_scan::find_kvm_port().map(|p| p.port));
        let Some(port) = port else {
            eprintln!("no port given and no known KVM cable found");
            std::process::exit(1);
        };
        println!("probing {port} ...");
        match kvm::cable::Cable::open(&port, kvm::cable::DEFAULT_BAUD) {
            Ok(cable) => {
                match cable.chip_info() {
                    Some(info) => {
                        println!("  CH9329 handshake OK");
                        println!("  version:  {}", info.version_string());
                        println!(
                            "  usb:      {}",
                            if info.usb_connected {
                                "connected"
                            } else {
                                "not enumerated"
                            }
                        );
                        println!(
                            "  leds:     num={} caps={} scroll={}",
                            info.num_lock, info.caps_lock, info.scroll_lock
                        );
                    }
                    None => println!("  no CH9329 response (port opened, chip silent)"),
                }
                cable.close();
            }
            Err(e) => {
                eprintln!("could not open {port}: {e}");
                std::process::exit(1);
            }
        }
        return Ok(());
    }

    // ---- report the real USB link speed for the capture device ------------
    if args.iter().any(|a| a == "--usb") {
        let devices = match video::mf::list_devices() {
            Ok(d) => d,
            Err(e) => {
                eprintln!("could not enumerate capture devices: {e}");
                std::process::exit(1);
            }
        };
        let Some(device) = devices.first() else {
            eprintln!("no capture device");
            std::process::exit(1);
        };
        println!("capture device: {}", device.name);
        match video::usb_link::vid_pid_from_path(&device.symbolic_link) {
            Some((vid, pid)) => {
                println!("  VID:PID = {vid:04X}:{pid:04X}");
                match video::usb_link::measure(vid, pid) {
                    Ok(link) => {
                        println!(
                            "  link: {} ({}) on hub port {}",
                            link.speed.name(),
                            link.speed.rate(),
                            link.port
                        );
                        println!("  throughput: {}", link.throughput_note());
                        println!("  port supports USB 3.x: {}", link.port_supports_usb3);
                        println!(
                            "  operating at SuperSpeed: {}",
                            link.operating_at_superspeed
                        );
                        println!("  source: {}", link.source);
                        println!("  verdict: {}", link.verdict());

                        // Show the arithmetic for this device's own top format.
                        let (w, h) = (1920u32, 1080u32);
                        for (label, bpp) in [
                            ("MJPEG (measured ~41 kB/frame)", 0.0),
                            ("NV12", 1.5),
                            ("YUV422", 2.0),
                        ] {
                            if bpp == 0.0 {
                                println!(
                                    "    {label}: needs ~{:.1} MB/s at {w}x{h} 50 fps",
                                    41109.0 * 50.0 / 1_000_000.0
                                );
                            } else {
                                let bytes = video::usb_link::uncompressed_frame_bytes(w, h, bpp);
                                let fits = video::usb_link::fits_on_link(link.speed, bytes, 50.0);
                                println!(
                                    "    {label}: {} bytes/frame = {:.1} MB/s at 50 fps -> {}",
                                    bytes,
                                    bytes as f64 * 50.0 / 1_000_000.0,
                                    if fits { "fits" } else { "DOES NOT FIT" }
                                );
                            }
                        }
                    }
                    Err(e) => println!("  link: not measured ({e})"),
                }
            }
            None => println!("  could not parse VID/PID from {}", device.symbolic_link),
        }
        return Ok(());
    }

    // ---- dump one raw native sample straight to disk ----------------------
    if let Some(i) = args.iter().position(|a| a == "--grab-native") {
        let index: u32 = args
            .get(i + 1)
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or(1);
        let out = args
            .get(i + 2)
            .cloned()
            .unwrap_or_else(|| "native.bin".to_string());
        let devices = match video::mf::list_devices() {
            Ok(d) => d,
            Err(e) => {
                eprintln!("could not enumerate capture devices: {e}");
                std::process::exit(1);
            }
        };
        let Some(device) = devices.first() else {
            eprintln!("no capture device");
            std::process::exit(1);
        };
        match video::mf::grab_native(&device.symbolic_link, index, &out) {
            Ok(msg) => println!("{msg}"),
            Err(e) => {
                eprintln!("grab failed: {e}");
                std::process::exit(1);
            }
        }
        return Ok(());
    }

    // ---- raw probe: what is the device actually sending? -------------------
    if let Some(i) = args.iter().position(|a| a == "--raw-probe") {
        let n = args
            .get(i + 1)
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(5);
        let which = args.get(i + 2).cloned().unwrap_or_else(|| "0".to_string());
        let devices = match video::mf::list_devices() {
            Ok(d) => d,
            Err(e) => {
                eprintln!("could not enumerate capture devices: {e}");
                std::process::exit(1);
            }
        };
        let Some(device) = devices
            .iter()
            .find(|d| d.index.to_string() == which || d.name == which)
            .or_else(|| devices.first())
        else {
            eprintln!("no capture device to probe");
            std::process::exit(1);
        };
        println!("probing {} (raw, undecoded stream)", device.name);
        match video::mf::probe_raw(&device.symbolic_link, n) {
            Ok(lines) => {
                for l in lines {
                    println!("  {l}");
                }
            }
            Err(e) => {
                eprintln!("probe failed: {e}");
                std::process::exit(1);
            }
        }
        return Ok(());
    }

    // ---- grab a single frame to a BMP file (proves the whole video path) ----
    if let Some(i) = args.iter().position(|a| a == "--snapshot") {
        let out = args
            .get(i + 1)
            .cloned()
            .unwrap_or_else(|| "snapshot.bmp".to_string());
        let which = args.get(i + 2).cloned().unwrap_or_else(|| "0".to_string());
        let devices = match video::mf::list_devices() {
            Ok(d) => d,
            Err(e) => {
                eprintln!("video enumeration failed: {e}");
                std::process::exit(1);
            }
        };
        let Some(device) = devices
            .iter()
            .find(|d| d.index.to_string() == which || d.name == which)
            .or_else(|| devices.first())
        else {
            eprintln!("no capture device to grab from");
            std::process::exit(1);
        };

        println!("grabbing one frame from {}", device.name);
        let (tx, rx) = std::sync::mpsc::channel();
        let mut capture = match video::mf::CaptureThread::start(
            device.symbolic_link.clone(),
            String::new(),
            false,
            tx,
            // No UI to wake: this mode polls the channel itself.
            Box::new(|| {}),
        ) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("capture failed to start: {e}");
                std::process::exit(1);
            }
        };

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(12);
        let mut wrote = false;
        // The dongle keeps producing frames while its HDMI source is off — they
        // are just black. Hold one aside so we can still report *something*, but
        // prefer to wait for a frame with real picture in it.
        let mut blank_fallback: Option<video::Frame> = None;
        while std::time::Instant::now() < deadline {
            match rx.recv_timeout(std::time::Duration::from_millis(500)) {
                Ok(video::CaptureMsg::Frame(frame)) => {
                    if frame.looks_blank() {
                        blank_fallback = Some(frame);
                        continue;
                    }
                    match video::write_bmp(std::path::Path::new(&out), &frame) {
                        Ok(()) => {
                            println!(
                                "wrote {} ({}x{}, {:.1}% lit)",
                                out,
                                frame.width,
                                frame.height,
                                frame.lit_fraction() * 100.0
                            );
                            wrote = true;
                        }
                        Err(e) => eprintln!("could not write {out}: {e}"),
                    }
                    break;
                }
                Ok(video::CaptureMsg::Started {
                    width,
                    height,
                    label,
                    colour,
                }) => {
                    println!("  stream live at {width}x{height} {label} — colour: {colour}");
                }
                Ok(video::CaptureMsg::Warning(w)) => eprintln!("  warn: {w}"),
                Ok(video::CaptureMsg::Stopped(e)) => {
                    eprintln!("  stream stopped: {e}");
                    break;
                }
                Err(_) => {}
            }
        }
        if !wrote {
            if let Some(frame) = blank_fallback {
                eprintln!(
                    "no live picture within {}s — source looks off/standby; writing the black frame anyway",
                    12
                );
                if let Err(e) = video::write_bmp(std::path::Path::new(&out), &frame) {
                    eprintln!("could not write {out}: {e}");
                } else {
                    println!("wrote {} ({}x{}, 0% lit)", out, frame.width, frame.height);
                    wrote = true;
                }
            }
        }
        capture.stop();
        if !wrote {
            eprintln!("no frame arrived before the timeout");
            std::process::exit(1);
        }
        return Ok(());
    }

    // ---- GUI ---------------------------------------------------------------
    // Reaching here means no mode flag was given, so this run *is* the GUI: give
    // back the console Windows opened for us (see `detach_own_console`) before
    // anything is drawn. Modes above keep their console untouched.
    detach_own_console();

    let settings = Settings::load();
    logging::set_panel_level(Level::parse(&settings.log_level).unwrap_or(Level::Info));

    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([1320.0, 860.0])
            .with_min_inner_size([1000.0, 660.0])
            .with_title("KVM-Control Plus — Laptop KVM")
            .with_app_id("kvm-control-plus")
            .with_icon(window_icon()),
        ..Default::default()
    };

    eframe::run_native(
        "KVM-Control Plus",
        options,
        Box::new(|cc| Ok(Box::new(app::KvmControlPlusApp::new(cc)))),
    )
}

/// Close the console window Windows opened for us, because this run is the GUI.
///
/// The binary is deliberately built as a **console** application: the headless
/// modes (`--list-devices`, `--usb`, `--snapshot`, …) print to stdout and have to
/// behave like ordinary command-line tools — output on the terminal, the shell
/// waits for them, the exit status is real. The price is that a double-click also
/// opens a console nobody asked for, duplicating the log the GUI panel already
/// shows.
///
/// So the console is detached **only when this process is its sole owner**. If
/// cmd.exe or PowerShell shares it, `GetConsoleProcessList` reports more than one
/// entry and detaching would close the user's own terminal and swallow the output
/// they ran the command for — so it is left strictly alone. Zero entries means no
/// console was ever attached (bash/mintty, a shortcut, a service) and there is
/// nothing to give back.
///
/// Order matters: the logger stops writing to stderr *before* the handle dies,
/// because Rust's `eprintln!` panics on a failed write. Logging itself keeps
/// running into the ring buffer — that is what feeds the panel — so nothing is
/// lost or silenced; see `logging::detach_stdout`.
fn detach_own_console() {
    use windows::Win32::System::Console::{FreeConsole, GetConsoleProcessList};

    // SAFETY: the windows crate takes the buffer as a slice, so it can only write
    // within bounds; `FreeConsole` takes no arguments and no other pointer is used.
    unsafe {
        let mut pids = [0u32; 4];
        let attached = GetConsoleProcessList(&mut pids);

        if attached == 1 {
            logging::detach_stdout();
            match FreeConsole() {
                Ok(()) => logging::info(
                    "console detached — this run owns no terminal, so the log lives in the panel",
                ),
                Err(e) => logging::warn(format!("could not detach the console: {e}")),
            }
        } else {
            logging::debug(format!(
                "console kept: {attached} process(es) share it with us"
            ));
        }
    }
}

/// The window / taskbar icon.
///
/// The embedded .ico from `build.rs` covers Explorer, but a window needs its own
/// icon: eframe takes raw RGBA, so `tools/make_icon.py` also writes the 256px
/// master as a flat RGBA blob which we hand over directly. Raw bytes rather than
/// a PNG keeps the "two dependencies" rule intact — decoding a PNG would mean
/// adding an image crate for one 16 KB file.
fn window_icon() -> std::sync::Arc<eframe::egui::IconData> {
    const RGBA: &[u8] = include_bytes!("../assets/icon.rgba");
    const EDGE: usize = 256;
    debug_assert_eq!(
        RGBA.len(),
        EDGE * EDGE * 4,
        "assets/icon.rgba is not 256x256 RGBA"
    );
    std::sync::Arc::new(eframe::egui::IconData {
        rgba: RGBA.to_vec(),
        width: EDGE as u32,
        height: EDGE as u32,
    })
}

fn print_help() {
    println!(
        "kvm-control-plus {VERSION} — laptop-as-KVM: USB capture + CH9329 serial HID\n\
         \n\
         USAGE:\n\
         \x20   kvm-control-plus [OPTIONS]\n\
         \n\
         With no options, the GUI starts.\n\
         \n\
         INSPECTION (no GUI):\n\
         \x20   --list-devices              list MediaFoundation capture devices\n\
         \x20   --list-formats [DEVICE]     list a device's native formats (index or name)\n\
         \x20   --list-ports                list serial ports, flagging known KVM cables\n\
         \x20   --probe [COM]               open the cable and read CH9329 chip info\n\
         \n\
         LOGGING:\n\
         \x20   -q, --quiet                 warnings and errors only\n\
         \x20   -v, --verbose, --debug      debug detail (default: info)\n\
         \x20   --trace                     everything, including per-packet tracing\n\
         \x20   --level <trace|debug|info|warn|error>\n\
         \n\
         MISC:\n\
         \x20   -V, --version               version and build timestamp\n\
         \x20   -h, --help                  this help"
    );
}

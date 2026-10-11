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
mod picture;
mod proc_mem;
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

    // ---- picture adjustments, for every mode that captures ----------------
    // The same knobs the GUI has, from a terminal: an adjustment can then be
    // tested (and A/B'd against the untouched picture) without the window, which
    // is the only way to see what "range=limited" actually did to a real frame.
    if let Some(i) = args.iter().position(|a| a == "--picture") {
        if let Some(spec) = args.get(i + 1) {
            let parsed = parse_picture_spec(spec);
            crate::logging::info(format!("--picture {spec} => {}", parsed.describe()));
            let _ = PICTURE.set(parsed);
        } else {
            eprintln!("--picture needs a spec, e.g. --picture range=limited,gamma=1.4");
        }
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
    // ---- soak: the regression harness for the 2026-10-11 out-of-memory crash --
    if let Some(i) = args.iter().position(|a| a == "--soak") {
        let secs: u64 = args.get(i + 1).and_then(|s| s.parse().ok()).unwrap_or(30);
        let which = args.get(i + 2).cloned().unwrap_or_default();
        return soak(&which, secs);
    }

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
        // Frames arrive in the slot, control messages on the channel.
        let frames = video::mf::FrameSlot::new();
        let mut capture = match video::mf::CaptureThread::start(
            device.symbolic_link.clone(),
            String::new(),
            false,
            tx,
            // No UI to wake: this mode polls the slot itself.
            Box::new(|| {}),
            frames.clone(),
            picture_handle(),
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
            // Control messages arrive on the channel; frames sit in the slot.
            match rx.recv_timeout(std::time::Duration::from_millis(200)) {
                Ok(video::CaptureMsg::Started {
                    width,
                    height,
                    label,
                    colour,
                }) => {
                    println!(
                        "  stream live at {width}x{height} {label} — colour: {}",
                        colour.describe()
                    );
                }
                Ok(video::CaptureMsg::Warning(w)) => eprintln!("  warn: {w}"),
                Ok(video::CaptureMsg::Stopped(e)) => {
                    eprintln!("  stream stopped: {e}");
                    break;
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            }
            let Some(frame) = frames.take() else { continue };
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
    match start_run_log() {
        Some(p) => logging::info(format!("this run logs to {}", p.display())),
        None => logging::warn("could not open a log file for this run"),
    }
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

/// Give this run a durable log file, then let the console go.
///
/// Why this exists: on 2026-10-11 the app died of a failed allocation, and the one
/// line that would have said *how big* the allocation was — Rust prints that to
/// stderr itself — went to a console that had already been detached. Nothing
/// survived to read. A GUI run now logs to a file, so the next fatal message is
/// still there afterwards; see `docs/CRASH_2026-10-11.md`.
fn start_run_log() -> Option<std::path::PathBuf> {
    let path = crate::config::Settings::path()
        .parent()?
        .join("last-run.log");
    std::fs::create_dir_all(path.parent()?).ok()?;
    if !logging::redirect_stderr_to_file(&path) {
        return None;
    }
    // The file owns stderr now; our own records must not also be written to a
    // console that is about to be given back to Windows.
    logging::detach_stdout();
    Some(path)
}

/// `--picture` as parsed at startup.
///
/// A `OnceLock` rather than a parameter threaded through seven modes: it is set
/// once, before any capture begins, and never changes afterwards.
static PICTURE: std::sync::OnceLock<picture::Adjust> = std::sync::OnceLock::new();

/// The adjustments a capture mode hands to the capture thread. Each mode gets
/// its own handle; the CLI has no UI to share one with.
fn picture_handle() -> std::sync::Arc<std::sync::Mutex<picture::Adjust>> {
    std::sync::Arc::new(std::sync::Mutex::new(
        PICTURE.get().copied().unwrap_or_default(),
    ))
}

/// `--picture range=limited,contrast=1.2,gamma=1.4` — the GUI's knobs as text.
///
/// Lenient on purpose: an unrecognised key is reported and skipped rather than
/// being fatal, and an unparseable *value* keeps the default. A typo should cost
/// a line of output, not a run.
fn parse_picture_spec(spec: &str) -> picture::Adjust {
    let mut a = picture::Adjust::default();
    for part in spec.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let Some((key, value)) = part.split_once('=') else {
            eprintln!("--picture: {part:?} is not key=value; ignoring");
            continue;
        };
        let (key, value) = (key.trim().to_ascii_lowercase(), value.trim());
        match key.as_str() {
            "range" => a.range = picture::Adjust::parse_range(value, a.range),
            "matrix" => a.matrix = picture::Adjust::parse_matrix(value, a.matrix),
            "brightness" => a.brightness = value.parse().unwrap_or(a.brightness),
            "contrast" => a.contrast = value.parse().unwrap_or(a.contrast),
            "saturation" => a.saturation = value.parse().unwrap_or(a.saturation),
            "gamma" => a.gamma = value.parse().unwrap_or(a.gamma),
            other => eprintln!("--picture: unknown setting {other:?}; ignoring"),
        }
    }
    a
}

/// Resolve a capture device by index, by name, or to the first one available.
fn pick_device(which: &str) -> Option<video::mf::VideoDevice> {
    let devices = video::mf::list_devices().ok()?;
    if !which.is_empty() {
        if let Some(d) = devices
            .iter()
            .find(|d| d.index.to_string() == which || d.name == which)
        {
            return Some(d.clone());
        }
    }
    devices.first().cloned()
}

/// Run capture with **nobody collecting frames**, and measure what it costs.
///
/// This is the regression harness for the 2026-10-11 crash
/// (`docs/CRASH_2026-10-11.md`): an unbounded queue of frames filled the machine's
/// memory and took the desktop's compositor down with it. The fix is that a frame
/// now waits in a single slot (`video::mf::FrameSlot`), so this mode reproduces
/// the exact condition that used to be fatal — a producer with no consumer — and
/// prints the process's committed memory while it happens. That turns "this
/// cannot happen again" into a measurement instead of a promise.
///
/// It deliberately never takes a frame. Three things should be visible:
///   * committed memory stays flat instead of climbing by 6 MB per frame,
///   * `produced` stops climbing after a couple of seconds, because the producer
///     backs off once it sees that nothing is being collected,
///   * with `KVM_MAX_COMMIT_MB` set low, the ceiling stops the stream outright.
fn soak(which: &str, secs: u64) -> eframe::Result<()> {
    let Some(device) = pick_device(which) else {
        eprintln!("no capture device to soak (try --list-devices)");
        std::process::exit(1);
    };

    println!(
        "soaking \"{}\" for {secs}s with no consumer — ceiling {} MB (change with KVM_MAX_COMMIT_MB)",
        device.name,
        crate::proc_mem::mb(crate::proc_mem::ceiling_bytes()).round()
    );
    println!(
        "{:>5}  {:>11}  {:>10}  {:>10}",
        "t(s)", "commit(MB)", "produced", "collected"
    );

    let (tx, rx) = std::sync::mpsc::channel();
    let frames = video::mf::FrameSlot::new();
    let mut capture = match video::mf::CaptureThread::start(
        device.symbolic_link.clone(),
        String::new(),
        false,
        tx,
        // Nobody collects: that is the entire point of this mode.
        Box::new(|| {}),
        frames.clone(),
        picture_handle(),
    ) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("capture failed to start: {e}");
            std::process::exit(1);
        }
    };

    let start = std::time::Instant::now();
    let mut stopped: Option<String> = None;
    let mut peak = 0u64;
    while start.elapsed() < std::time::Duration::from_secs(secs) {
        std::thread::sleep(std::time::Duration::from_millis(500));
        // Control messages are read; frames are deliberately left in the slot.
        while let Ok(m) = rx.try_recv() {
            match m {
                video::CaptureMsg::Stopped(why) => stopped = Some(why),
                video::CaptureMsg::Warning(w) => println!("  warn: {w}"),
                video::CaptureMsg::Started { .. } => {}
            }
        }
        let (put, taken) = frames.counters();
        let commit = crate::proc_mem::commit_bytes();
        peak = peak.max(commit);
        println!(
            "{:>5}  {:>11.1}  {:>10}  {:>10}",
            start.elapsed().as_secs(),
            crate::proc_mem::mb(commit),
            put,
            taken
        );
        if let Some(why) = &stopped {
            println!("\nstream stopped during the soak: {why}");
            break;
        }
    }

    capture.stop();
    let (put, taken) = frames.counters();
    println!(
        "\nproduced {put}, collected {taken}, peak commit {:.1} MB, ceiling {:.0} MB",
        crate::proc_mem::mb(peak),
        crate::proc_mem::mb(crate::proc_mem::ceiling_bytes())
    );

    // The assertion that matters: memory must not track the frame count. A 1080p
    // frame is 6,220,800 bytes, so the old design was into gigabytes within
    // seconds; anything near this bound means the bound is not holding.
    if peak >= 256 * 1024 * 1024 {
        println!(
            "FAIL — committed {:.0} MB with no consumer: a stalled UI is still costing memory",
            crate::proc_mem::mb(peak)
        );
        std::process::exit(2);
    }
    if stopped.is_some() {
        println!(
            "       (the stream was stopped early by the memory ceiling — the watchdog working)"
        );
    }
    println!("PASS — {put} frames produced with nobody collecting, memory stayed flat");
    if taken == 0 {
        println!("       (nothing was collected, which is exactly what this mode tests)");
    }
    Ok(())
}

fn print_help() {
    // Written as a raw string, one line per line: no line-continuation escapes and
    // no padding tricks, so what you read here is exactly what is printed. The
    // previous form leaned on trailing-backslash continuations, which printed
    // literal escape text the moment one was edited carelessly.
    println!(
        r#"kvm-control-plus {VERSION} — laptop-as-KVM: USB capture + CH9329 serial HID

USAGE:
    kvm-control-plus [OPTIONS]

With no options, the GUI starts.

INSPECTION (no GUI):
    --list-devices              list MediaFoundation capture devices
    --list-formats [DEVICE]     list a device's native formats (index or name)
    --list-ports                list serial ports, flagging known KVM cables
    --probe [COM]               open the cable and read CH9329 chip info

DIAGNOSTICS:
    --usb                       measure the real negotiated USB link speed
    --snapshot <file> [DEVICE]  write one frame to a .bmp
    --raw-probe [DEVICE]        walk the native formats, report which ones stream
    --soak <seconds> [DEVICE]   run capture with nobody collecting, and measure
                                that the frame path cannot grow without bound
    --picture <spec>            picture tweaks in any capture mode, e.g.
                                range=limited,contrast=1.2,gamma=1.4 (also in
                                the GUI's Options tab, with sliders)

LOGGING:
    -q, --quiet                 warnings and errors only
    -v, --verbose, --debug      debug detail (default: info)
    --trace                     everything, including per-packet tracing
    --level <trace|debug|info|warn|error>

MISC:
    -V, --version               version and build timestamp
    -h, --help                  this help

ENVIRONMENT:
    KVM_MAX_COMMIT_MB           ceiling on this app's committed memory, in MB
                                (default 4096). A bad value cannot remove it."#
    );
}

//! The application: state machine, event plumbing, and every panel.
//!
//! Reading order for a newcomer:
//!   1. [`KvmControlPlusApp::new`]   — what the app discovers at startup
//!   2. [`KvmControlPlusApp::update`] — the per-frame loop (events → state → UI)
//!   3. the `panel_*` methods  — one per screen area

use std::sync::mpsc::{channel, Receiver, Sender, TryRecvError};
use std::sync::Arc;

use eframe::egui::{self, Color32, RichText};

use crate::config::Settings;
use crate::input::hook::{InputEvent, InputThread};
use crate::kvm::cable::{Cable, LinkStatsSnapshot, COMMON_BAUD_RATES};
use crate::kvm::layout;
use crate::kvm::serial_scan::{self, SerialPortInfo};
use crate::kvm::{ch9329, keymap};
use crate::logging::{self as log, Level};
use crate::ui::{self, Palette};
use crate::video::mf::{self, CaptureMsg, CaptureThread, FrameSlot, VideoDevice, VideoFormat};

/// Which controls the left panel is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    Devices,
    Input,
    Macros,
    Options,
    About,
}

impl Tab {
    const ALL: [Tab; 5] = [
        Tab::Devices,
        Tab::Input,
        Tab::Macros,
        Tab::Options,
        Tab::About,
    ];
    fn label(self) -> &'static str {
        match self {
            Tab::Devices => "Devices",
            Tab::Input => "Input",
            Tab::Macros => "Macros",
            Tab::Options => "Options",
            Tab::About => "About",
        }
    }
}

/// A transient message shown in the top bar.
struct Toast {
    text: String,
    color: Color32,
    at: std::time::Instant,
}

pub struct KvmControlPlusApp {
    settings: Settings,
    palette: Palette,
    /// A handle on the egui context so the capture thread can wake the UI when a
    /// frame lands, instead of the UI redrawing at the display's refresh rate
    /// hoping to notice one.
    ctx: egui::Context,

    // ---- discovered devices ----
    devices: Vec<VideoDevice>,
    formats: Vec<VideoFormat>,
    ports: Vec<SerialPortInfo>,
    device_sel: usize,
    format_sel: usize,
    port_sel: usize,

    // ---- live objects ----
    cable: Option<Arc<Cable>>,
    input: Option<InputThread>,
    capture: Option<CaptureThread>,
    /// Rare control messages from the capture thread (`Started`/`Warning`/`Stopped`).
    /// Frames do **not** travel here — see `frames`.
    frame_rx: Option<Receiver<CaptureMsg>>,
    /// The newest captured frame, exactly one of them. This is the bound that
    /// stops a stalled UI from consuming the machine: see `FrameSlot`.
    frames: Option<FrameSlot>,
    texture: Option<egui::TextureHandle>,
    last_frame: Option<crate::video::Frame>,
    video_label: String,
    video_error: Option<String>,
    /// How the arriving samples are being interpreted (range + matrix), as
    /// reported by the capture thread. Shown so a wrong-looking picture can be
    /// diagnosed from the window instead of from the source.
    video_colour: String,
    /// Real measured USB link for the selected capture device, if we could get
    /// one. `None` + a note means "could not measure", never "assumed fine".
    usb_link: Option<crate::video::usb_link::UsbLink>,
    usb_link_note: Option<String>,

    // ---- counters ----
    frames_this_second: u32,
    fps: f32,
    fps_mark: std::time::Instant,
    /// Age of the newest frame when it was drawn, in milliseconds. This is the
    /// pipeline latency — measured, because "the picture is behind" is a claim
    /// that needs a number.
    latency_ms: f32,
    latency_sum_ms: f32,
    latency_max_ms: f32,
    latency_samples: u32,

    // ---- ui state ----
    tab: Tab,
    capturing: bool,
    fullscreen_video: bool,
    toast: Option<Toast>,
    macro_text: String,
    log_level: Level,
    log_autoscroll: bool,
    show_log: bool,
    /// Hide the left controls pane for a video-only window (top-bar toggle).
    show_left_pane: bool,
    /// Last left-pane geometry we logged, so the diagnostic fires on change
    /// instead of once per frame (and does not drown the panel it describes).
    pane_geom_logged: Option<(i32, i32, i32, i32)>,
    /// When the app came up, so every log line can carry an offset instead of a
    /// wall-clock timestamp nobody can line up with anything.
    log_start: std::time::Instant,

    // ---- input thread events ----
    input_tx: Sender<InputEvent>,
    input_rx: Receiver<InputEvent>,
}

impl KvmControlPlusApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> KvmControlPlusApp {
        let settings = Settings::load();
        let palette = Palette::by_name(&settings.theme);
        ui::apply(&cc.egui_ctx, &palette);

        let (input_tx, input_rx) = channel();

        // Discover hardware up front so the UI is populated immediately.
        let ports = serial_scan::list();
        let devices = mf::list_devices().unwrap_or_else(|e| {
            log::error(format!("video device enumeration failed: {e}"));
            Vec::new()
        });
        log::info(format!(
            "found {} serial port(s) and {} video source(s)",
            ports.len(),
            devices.len()
        ));

        let mut app = KvmControlPlusApp {
            port_sel: 0,
            device_sel: 0,
            format_sel: 0,
            formats: Vec::new(),
            devices,
            ports,
            cable: None,
            input: None,
            capture: None,
            frame_rx: None,
            frames: None,
            texture: None,
            last_frame: None,
            video_label: String::new(),
            video_error: None,
            video_colour: String::new(),
            usb_link: None,
            usb_link_note: None,
            frames_this_second: 0,
            fps: 0.0,
            fps_mark: std::time::Instant::now(),
            latency_ms: 0.0,
            latency_sum_ms: 0.0,
            latency_max_ms: 0.0,
            latency_samples: 0,
            tab: Tab::Devices,
            capturing: false,
            fullscreen_video: false,
            toast: None,
            macro_text: String::new(),
            log_level: parse_level(&settings.log_level),
            log_autoscroll: true,
            show_log: true,
            show_left_pane: settings.show_left_pane,
            pane_geom_logged: None,
            log_start: std::time::Instant::now(),
            input_tx,
            input_rx,
            settings,
            palette,
            ctx: cc.egui_ctx.clone(),
        };

        app.restore_selections();
        app.start_input_thread();

        if app.settings.auto_connect {
            app.try_auto_connect();
        }
        app
    }

    // ------------------------------------------------------------------
    //  Startup helpers
    // ------------------------------------------------------------------

    /// Re-select the device/port/format the user had chosen last time.
    fn restore_selections(&mut self) {
        if !self.settings.video_source_id.is_empty() {
            if let Some(i) = self
                .devices
                .iter()
                .position(|d| d.symbolic_link == self.settings.video_source_id)
            {
                self.device_sel = i;
            }
        }
        if !self.settings.com_port.is_empty() {
            if let Some(i) = self
                .ports
                .iter()
                .position(|p| p.port == self.settings.com_port)
            {
                self.port_sel = i;
            }
        }
        self.refresh_formats();
        if !self.settings.video_format.is_empty() {
            if let Some(i) = self
                .formats
                .iter()
                .position(|f| f.label().eq_ignore_ascii_case(&self.settings.video_format))
            {
                self.format_sel = i;
            }
        }
    }

    fn start_input_thread(&mut self) {
        match InputThread::start(self.input_tx.clone(), self.settings.leave_gesture) {
            Ok(t) => {
                self.apply_input_settings(&t);
                self.input = Some(t);
            }
            Err(e) => {
                log::error(format!("input capture unavailable: {e}"));
                self.set_toast(format!("Input capture unavailable: {e}"), self.palette.bad);
            }
        }
    }

    fn apply_input_settings(&self, t: &InputThread) {
        t.set_layout_aware(self.settings.layout_aware);
        t.set_mouse_speed(self.settings.mouse_speed);
        t.set_invert_scroll(self.settings.invert_scroll);
        t.set_swap_buttons(self.settings.swap_buttons);
        t.set_invert_y(self.settings.invert_mouse_y);
        t.set_panic_hotkey(self.settings.panic_hotkey_enabled);
        t.set_leave_gesture(self.settings.leave_gesture);
    }

    /// Auto-connect to a detected KVM cable at startup.
    fn try_auto_connect(&mut self) {
        if self.ports.is_empty() {
            log::info("no serial ports present — video will be view-only");
            return;
        }
        let idx = self.ports.iter().position(|p| p.is_kvm_cable).unwrap_or(0);
        self.port_sel = idx;
        if self.ports[idx].is_kvm_cable {
            log::info(format!(
                "auto-connecting to detected KVM cable on {}",
                self.ports[idx].port
            ));
            self.connect();
        } else {
            log::info("no KVM cable detected; select a port manually to enable input");
        }
    }

    // ------------------------------------------------------------------
    //  Actions
    // ------------------------------------------------------------------

    fn refresh_devices(&mut self) {
        match mf::list_devices() {
            Ok(d) => {
                self.devices = d;
                log::info(format!("{} video source(s)", self.devices.len()));
                if self.device_sel >= self.devices.len() {
                    self.device_sel = 0;
                }
                self.refresh_formats();
            }
            Err(e) => {
                log::error(format!("video enumeration failed: {e}"));
                self.set_toast(format!("Video enumeration failed: {e}"), self.palette.bad);
            }
        }
    }

    fn refresh_ports(&mut self) {
        self.ports = serial_scan::list();
        log::info(format!("{} serial port(s)", self.ports.len()));
        if self.port_sel >= self.ports.len() {
            self.port_sel = 0;
        }
    }

    fn refresh_formats(&mut self) {
        self.formats.clear();
        self.format_sel = 0;
        if let Some(d) = self.devices.get(self.device_sel) {
            match mf::list_formats(&d.symbolic_link) {
                Ok(fs) => {
                    log::debug(format!("{} format(s) on {}", fs.len(), d.name));
                    self.formats = fs;
                }
                Err(e) => log::warn(format!("cannot list formats for {}: {e}", d.name)),
            }
        }
        self.refresh_usb_link();
    }

    /// Ask the USB hub what speed the selected capture device actually
    /// negotiated. This is a measurement, not an inference — and when it cannot
    /// be taken, say so rather than quietly showing nothing.
    fn refresh_usb_link(&mut self) {
        use crate::video::usb_link;

        self.usb_link = None;
        self.usb_link_note = None;

        let Some(d) = self.devices.get(self.device_sel) else {
            self.usb_link_note = Some("no capture device selected".into());
            return;
        };

        let Some((vid, pid)) = usb_link::vid_pid_from_path(&d.symbolic_link) else {
            self.usb_link_note = Some(format!("no USB VID/PID in {}", d.symbolic_link));
            return;
        };

        match usb_link::measure(vid, pid) {
            Ok(link) => self.usb_link = Some(link),
            Err(e) => {
                log::debug(format!("USB link not measured: {e}"));
                self.usb_link_note = Some(e);
            }
        }
    }

    /// Verbose, honest explanation of the capture link and of every format the
    /// device offers.
    ///
    /// A black picture has several unrelated causes on these dongles: the HDMI
    /// source may be asleep, the chosen format may physically not fit down the
    /// link, or the device may simply not be sending content. This panel exists
    /// so the user is *told* which one applies, with the arithmetic shown,
    /// instead of having to guess and blame the app.
    fn capture_report(&mut self, ui: &mut egui::Ui) {
        use crate::video::usb_link::{self, uncompressed_mb_per_s};

        let p = self.palette;
        ui::section(
            ui,
            &p,
            "LINK & FORMATS — WHY THE PICTURE LOOKS LIKE IT DOES",
        );

        // ---- 0. what the picture is doing right now ----
        // The colour interpretation is the single fact that decides whether the
        // picture looks right, and the latency is the only honest answer to "is
        // it behind" — both belong in the window, not in a debug session.
        if !self.video_colour.is_empty() {
            ui::kv(ui, &p, "Colour", &self.video_colour, None);
        }
        if self.capture.is_some() {
            let latency_color = if self.latency_ms > 120.0 {
                p.warn
            } else {
                p.ok
            };
            ui::kv(
                ui,
                &p,
                "Latency",
                &format!(
                    "{:.0} ms now, {:.0} ms worst — {:.0} fps",
                    self.latency_ms, self.latency_max_ms, self.fps
                ),
                Some(latency_color),
            );
        }

        // ---- 1. which device, on which link ----
        let mut device_vid_pid: Option<(u16, u16)> = None;
        if let Some(d) = self.devices.get(self.device_sel) {
            ui::kv(ui, &p, "Capture device", &d.name, None);
            if let Some((vid, pid)) = usb_link::vid_pid_from_path(&d.symbolic_link) {
                device_vid_pid = Some((vid, pid));
                ui::kv(ui, &p, "USB id", &format!("{vid:04X}:{pid:04X}"), None);
            }
        }

        let link = self.usb_link.clone();
        match &link {
            Some(l) => {
                let color = if l.speed.is_usb3() { p.ok } else { p.warn };
                ui::kv(
                    ui,
                    &p,
                    "USB link (measured)",
                    &format!("{} — {}", l.speed.name(), l.speed.rate()),
                    Some(color),
                );
                ui::kv(
                    ui,
                    &p,
                    "Usable throughput",
                    &format!(
                        "~{:.0} MB/s after USB overhead",
                        l.speed.practical_mb_per_s()
                    ),
                    None,
                );
                ui::kv(
                    ui,
                    &p,
                    "Attached at",
                    &format!("port {} of hub {}", l.port, hub_short(&l.hub)),
                    None,
                );
                if l.port_supports_usb3 && !l.speed.is_usb3() {
                    ui::kv(
                        ui,
                        &p,
                        "Port capability",
                        "this port CAN do USB 3.x but the link came up at 2.0 — \
                         check the cable before blaming the dongle",
                        Some(p.warn),
                    );
                }
                ui.add_space(2.0);
                ui.colored_label(color, l.verdict());
                ui.label(
                    RichText::new(format!("How this was determined: {}", l.source))
                        .color(p.dim)
                        .size(11.0),
                );
            }
            None => {
                ui.colored_label(
                    p.warn,
                    "USB link speed could not be measured, so the uncompressed formats \
                     cannot be trusted on this connection.",
                );
                if let Some(n) = &self.usb_link_note {
                    ui.label(RichText::new(n).color(p.dim).size(11.0));
                }
                if let Some((vid, pid)) = device_vid_pid {
                    ui.label(
                        RichText::new(format!(
                            "device {vid:04X}:{pid:04X} was not found on any USB hub port"
                        ))
                        .color(p.dim)
                        .size(11.0),
                    );
                }
            }
        }

        // ---- 2. can the CURRENT selection actually be carried? ----
        ui.add_space(6.0);
        if let Some(f) = self.formats.get(self.format_sel) {
            let needed = uncompressed_mb_per_s(f.width, f.height, f.fps(), &f.subtype);
            match (&link, needed) {
                (Some(l), Some(mb)) => {
                    let fits =
                        usb_link::format_fits(l.speed, f.width, f.height, f.fps(), &f.subtype);
                    if fits == Some(false) {
                        ui.colored_label(
                            p.bad,
                            format!(
                                "{NO_MARK} SELECTED FORMAT CANNOT FIT THIS LINK: {} is uncompressed and \
                                 needs {mb:.1} MB/s at {:.0} fps, but this link carries about \
                                 {:.0} MB/s. The device will still stream — it just sends filler \
                                 frames, which is why the picture is black. Choose an MJPG format.",
                                f.label(),
                                f.fps(),
                                l.speed.practical_mb_per_s()
                            ),
                        );
                    } else {
                        ui::badge(
                            ui,
                            &p,
                            &format!(
                                "{OK_MARK} Selected format: {} — about {mb:.1} MB/s, fits this link.",
                                f.label()
                            ),
                        );
                    }
                }
                (Some(_), None) => {
                    ui::badge(
                        ui,
                        &p,
                        &format!("{OK_MARK} Selected format: {} — compressed, so it fits easily (measured ~2 MB/s at 1080p50).", f.label()),
                    );
                }
                (None, _) => {}
            }
        }

        // ---- 3. every format the device offers, and what each one costs ----
        ui.add_space(6.0);
        ui.label(
            RichText::new(format!(
                "Formats this device offers ({} total) — what each one would cost the link:",
                self.formats.len()
            ))
            .color(p.dim)
            .size(12.0),
        );

        if self.formats.is_empty() {
            ui.label(
                RichText::new("  (none listed — device busy or not streaming)")
                    .color(p.dim)
                    .size(11.0),
            );
        }

        let mut seen: Vec<String> = Vec::new();
        let mut shown = 0usize;
        let mut hidden = 0usize;
        for f in &self.formats {
            // One row per (size, subtype); the rate list is summarised instead.
            let key = format!("{}x{} {}", f.width, f.height, f.subtype);
            if seen.contains(&key) {
                hidden += 1;
                continue;
            }
            seen.push(key);

            let row = match uncompressed_mb_per_s(f.width, f.height, f.fps(), &f.subtype) {
                Some(mb) => {
                    let fits = link
                        .as_ref()
                        .map(|l| {
                            usb_link::format_fits(l.speed, f.width, f.height, f.fps(), &f.subtype)
                                == Some(true)
                        })
                        .unwrap_or(false);
                    let (mark, color) = if fits {
                        (OK_MARK, p.ok)
                    } else {
                        (NO_MARK, p.bad)
                    };
                    let why = if fits {
                        "uncompressed, fits".to_string()
                    } else {
                        format!("uncompressed, needs {mb:.0} MB/s — more than the link carries")
                    };
                    (
                        format!("{mark} {} {}x{} — {why}", f.subtype, f.width, f.height),
                        color,
                    )
                }
                None => (
                    format!(
                        "{OK_MARK} {} {}x{} — compressed, fits (frame size depends on picture)",
                        f.subtype, f.width, f.height
                    ),
                    p.ok,
                ),
            };

            if shown < 40 {
                ui.label(
                    RichText::new(format!("  {}", row.0))
                        .color(row.1)
                        .size(11.0),
                );
                shown += 1;
            } else {
                hidden += 1;
            }
        }
        if hidden > 0 {
            ui.label(
                RichText::new(format!(
                    "  … {hidden} further size/framerate entries, same pattern"
                ))
                .color(p.dim)
                .size(11.0),
            );
        }

        ui.add_space(4.0);
        ui.label(
            RichText::new(
                "Rule of thumb: MJPEG compresses each frame and fits almost any link. NV12/YUV422 \
                 send every pixel raw, so they need a USB 3.x link at high resolution.",
            )
            .color(p.dim)
            .size(11.0),
        );
    }

    fn connect(&mut self) {
        self.disconnect(false);
        let Some(p) = self.ports.get(self.port_sel) else {
            self.set_toast("No serial port selected".into(), self.palette.warn);
            return;
        };
        match Cable::open(&p.port, self.settings.baud) {
            Ok(cable) => {
                let cable = Arc::new(cable);
                if let Some(t) = &self.input {
                    t.set_cable(Some(Arc::clone(&cable)));
                }
                self.settings.com_port = p.port.clone();
                let info = cable.chip_info();
                let msg = match info {
                    Some(i) => format!("KVM cable ready on {} ({})", p.port, i.version_string()),
                    None => format!("Opened {} — no CH9329 handshake", p.port),
                };
                let color = if info.is_some() {
                    self.palette.ok
                } else {
                    self.palette.warn
                };
                log::info(&msg);
                self.set_toast(msg, color);
                self.cable = Some(cable);
                self.save_settings();
            }
            Err(e) => {
                log::error(format!("cannot open {}: {e}", p.port));
                self.set_toast(
                    format!("{} could not be opened: {e}", p.port),
                    self.palette.bad,
                );
            }
        }
    }

    fn disconnect(&mut self, notify: bool) {
        if let Some(t) = &self.input {
            t.set_capture(false);
            t.set_cable(None);
        }
        if let Some(c) = self.cable.take() {
            c.close();
            if notify {
                log::info("KVM cable released");
                self.set_toast("KVM cable released".into(), self.palette.dim);
            }
        }
        self.capturing = false;
    }

    fn start_video(&mut self) {
        self.stop_video(false);
        let Some(d) = self.devices.get(self.device_sel) else {
            self.set_toast("No video source available".into(), self.palette.warn);
            return;
        };
        let label = self
            .formats
            .get(self.format_sel)
            .map(|f| f.label())
            .unwrap_or_default();

        let (tx, rx) = channel();
        // Frames go into a single slot rather than down the channel: a channel is
        // unbounded, and a UI that stops collecting — a minimised window is never
        // painted on Windows — would then let frames pile up until the machine's
        // memory was gone. That is what happened on 2026-10-11. The slot replaces
        // the waiting frame instead, so a stalled UI costs one frame; see
        // `FrameSlot`.
        let frames = FrameSlot::new();
        // The capture thread wakes the UI when a frame arrives. Without this the
        // only way to notice a frame was to redraw continuously — which is what
        // the old unconditional `request_repaint` did, at the display's rate,
        // forever, whether or not anything had changed.
        let wake_ctx = self.ctx.clone();
        match CaptureThread::start(
            d.symbolic_link.clone(),
            label.clone(),
            self.settings.flip_vertical,
            tx,
            Box::new(move || wake_ctx.request_repaint()),
            frames.clone(),
        ) {
            Ok(c) => {
                log::info(format!("video started: {} [{}]", d.name, label));
                self.capture = Some(c);
                self.frame_rx = Some(rx);
                self.frames = Some(frames);
                self.video_error = None;
            }
            Err(e) => {
                log::error(format!("video failed: {e}"));
                self.video_error = Some(e.clone());
                self.set_toast(format!("Video failed: {e}"), self.palette.bad);
            }
        }
    }

    fn stop_video(&mut self, notify: bool) {
        if let Some(mut c) = self.capture.take() {
            c.stop();
            if notify {
                log::info("video stopped");
            }
        }
        self.frame_rx = None;
        self.frames = None;
        self.texture = None;
        self.frames_this_second = 0;
        self.fps = 0.0;
    }

    fn set_capture(&mut self, on: bool) {
        let has_cable = self.cable.is_some();
        if on && !has_cable {
            self.set_toast(
                "No KVM cable connected — video is view-only".into(),
                self.palette.warn,
            );
            return;
        }
        if let Some(t) = &self.input {
            t.set_capture(on);
            self.capturing = on;
            let msg = if on {
                "Remote session active — input captured"
            } else {
                "Local input restored"
            };
            self.set_toast(
                msg.into(),
                if on {
                    self.palette.accent
                } else {
                    self.palette.dim
                },
            );
        }
    }

    fn set_toast(&mut self, text: String, color: Color32) {
        self.toast = Some(Toast {
            text,
            color,
            at: std::time::Instant::now(),
        });
    }

    fn save_settings(&mut self) {
        self.settings.video_source_id = self
            .devices
            .get(self.device_sel)
            .map(|d| d.symbolic_link.clone())
            .unwrap_or_default();
        self.settings.video_source_name = self
            .devices
            .get(self.device_sel)
            .map(|d| d.name.clone())
            .unwrap_or_default();
        self.settings.video_format = self
            .formats
            .get(self.format_sel)
            .map(|f| f.label())
            .unwrap_or_else(|| self.settings.video_format.clone());
        self.settings.log_level = level_name(self.log_level);
        if let Err(e) = self.settings.save() {
            log::warn(format!("could not save settings: {e}"));
        }
    }

    fn save_screenshot(&mut self) {
        let Some(frame) = &self.last_frame else {
            self.set_toast("No video frame to save yet".into(), self.palette.warn);
            return;
        };
        let path = std::env::var_os("USERPROFILE")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from("."))
            .join("Pictures");
        let _ = std::fs::create_dir_all(&path);
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let file = path.join(format!("kvm-control-plus-snapshot-{stamp}.bmp"));
        match crate::video::write_bmp(&file, frame) {
            Ok(()) => {
                log::info(format!("snapshot saved to {}", file.display()));
                self.set_toast(
                    format!("Snapshot saved: {}", file.display()),
                    self.palette.ok,
                );
            }
            Err(e) => {
                log::error(format!("snapshot failed: {e}"));
                self.set_toast(format!("Snapshot failed: {e}"), self.palette.bad);
            }
        }
    }

    // ------------------------------------------------------------------
    //  Per-frame work that is not drawing
    // ------------------------------------------------------------------

    fn pump_video(&mut self, ctx: &egui::Context) {
        // Frames come from the single slot — bounded by construction, so at most
        // one frame can be waiting however long this method goes uncalled. The
        // channel carries only the rare control messages.
        let newest = self.frames.as_ref().and_then(|f| f.take());
        let mut msgs = Vec::new();
        if let Some(rx) = &self.frame_rx {
            loop {
                match rx.try_recv() {
                    Ok(m) => msgs.push(m),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => break,
                }
            }
        }
        for m in msgs {
            match m {
                CaptureMsg::Started {
                    width,
                    height,
                    label,
                    colour,
                } => {
                    self.video_label = format!("{width}x{height} {label}");
                    self.video_colour = colour;
                    log::info(format!(
                        "stream live at {} — colour: {}",
                        self.video_label, self.video_colour
                    ));
                }
                CaptureMsg::Warning(w) => log::warn(w),
                CaptureMsg::Stopped(reason) => {
                    if !reason.is_empty() {
                        log::warn(format!("capture stopped: {reason}"));
                        self.video_error = Some(reason);
                    }
                    self.capture = None;
                    self.frame_rx = None;
                    self.frames = None;
                }
            }
        }

        if let Some(frame) = newest {
            // How long the newest frame had been waiting before it was drawn.
            // That age *is* the pipeline latency, and measuring it is the only
            // way to tell whether a change to the capture path actually helped.
            let age_ms = frame.captured_at.elapsed().as_secs_f32() * 1000.0;
            self.latency_ms = age_ms;
            self.latency_sum_ms += age_ms;
            self.latency_max_ms = self.latency_max_ms.max(age_ms);
            self.latency_samples += 1;

            let image = egui::ColorImage::from_rgb(
                [frame.width as usize, frame.height as usize],
                &frame.rgb,
            );
            match &mut self.texture {
                Some(t) => t.set(image, egui::TextureOptions::LINEAR),
                None => {
                    self.texture =
                        Some(ctx.load_texture("video", image, egui::TextureOptions::LINEAR));
                }
            }
            self.last_frame = Some(frame);
            self.frames_this_second += 1;
        }

        if self.fps_mark.elapsed().as_millis() >= 1000 {
            let elapsed_ms = self.fps_mark.elapsed().as_millis() as f32;
            self.fps = self.frames_this_second as f32 * 1000.0 / elapsed_ms;
            self.frames_this_second = 0;
            self.fps_mark = std::time::Instant::now();

            // Permanent: the latency number belongs to the user, not in a debug
            // session — it is the answer to "is the picture behind".
            if self.latency_samples > 0 {
                log::info(format!(
                    "pipeline: {:.1} fps, latency {:.0} ms average / {:.0} ms worst ({} frames)",
                    self.fps,
                    self.latency_sum_ms / self.latency_samples as f32,
                    self.latency_max_ms,
                    self.latency_samples
                ));
            }
            self.latency_sum_ms = 0.0;
            self.latency_max_ms = 0.0;
            self.latency_samples = 0;
        }

        // No unconditional `request_repaint` here on purpose: the capture thread
        // wakes the UI when a frame arrives (see `start_video`). Redrawing at
        // the display's refresh rate just to notice frames burns a core and
        // turns frame timing into jitter.
    }

    fn pump_input_events(&mut self) {
        let mut events = Vec::new();
        while let Ok(e) = self.input_rx.try_recv() {
            events.push(e);
        }
        for e in events {
            match e {
                InputEvent::LeaveRequested => {
                    log::info("leave gesture received");
                    if self.fullscreen_video {
                        self.fullscreen_video = false;
                    } else {
                        self.set_capture(false);
                    }
                }
                InputEvent::Failed(msg) => {
                    log::error(format!("input thread failed: {msg}"));
                    self.set_toast(format!("Input capture failed: {msg}"), self.palette.bad);
                }
                InputEvent::DroppedWhileInactive => {}
            }
        }
    }

    // ------------------------------------------------------------------
    //  Panels
    // ------------------------------------------------------------------

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        let p = self.palette;
        ui.horizontal(|ui| {
            ui.label(RichText::new("KVM-Control Plus").size(17.0).strong());
            ui.label(RichText::new("laptop as KVM").size(12.0).color(p.dim));
            ui.add_space(10.0);

            // Connection pills
            let cable_ok = self.cable.is_some();
            let chip = self.cable.as_ref().and_then(|c| c.chip_info());
            let (txt, col) = if cable_ok {
                match chip {
                    Some(i) => (format!("KVM cable {}", i.version_string()), p.ok),
                    None => ("KVM cable (no handshake)".to_string(), p.warn),
                }
            } else {
                ("No KVM cable".to_string(), p.dim)
            };
            ui::pill(ui, &p, &txt, col);

            let (vtxt, vcol) = if self.capture.is_some() {
                (format!("Video {:.0} fps", self.fps), p.ok)
            } else if self.video_error.is_some() {
                ("Video error".to_string(), p.bad)
            } else {
                ("Video idle".to_string(), p.dim)
            };
            ui::pill(ui, &p, &vtxt, vcol);

            let (ctxt, ccol) = if self.capturing {
                ("● Capturing input", p.accent)
            } else {
                ("Input local", p.dim)
            };
            ui::pill(ui, &p, ctxt, ccol);

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let label = if self.settings.theme.eq_ignore_ascii_case("light") {
                    "🌙 Dark"
                } else {
                    "☀ Light"
                };
                if ui.button(label).clicked() {
                    self.settings.theme = if self.settings.theme.eq_ignore_ascii_case("light") {
                        "dark".into()
                    } else {
                        "light".into()
                    };
                    self.palette = Palette::by_name(&self.settings.theme);
                    ui::apply(ui.ctx(), &self.palette);
                    self.save_settings();
                }
                if ui
                    .button(if self.show_log {
                        "Hide log"
                    } else {
                        "Show log"
                    })
                    .clicked()
                {
                    self.show_log = !self.show_log;
                }
                // The pane holds every control, so it must always be reachable:
                // this button stays in the top bar whether the pane is up or not.
                if ui
                    .button(if self.show_left_pane {
                        "Hide pane"
                    } else {
                        "Show pane"
                    })
                    .on_hover_text("Hide the left pane for a video-only view")
                    .clicked()
                {
                    self.show_left_pane = !self.show_left_pane;
                    self.save_settings();
                }
            });
        });

        // Toast line
        if let Some(t) = &self.toast {
            if t.at.elapsed().as_secs() < 6 {
                ui.label(RichText::new(&t.text).color(t.color).size(12.5));
            } else {
                self.toast = None;
            }
        }
    }

    fn left_panel(&mut self, ui: &mut egui::Ui) {
        let p = self.palette;
        ui.horizontal_wrapped(|ui| {
            for t in Tab::ALL {
                if ui.selectable_label(self.tab == t, t.label()).clicked() {
                    self.tab = t;
                }
            }
        });
        ui.add_space(6.0);

        let out = egui::ScrollArea::vertical()
            .id_salt("left-pane")
            // `auto_shrink([false, false])` is what makes this a real scrolling
            // viewport: by default the area shrinks to fit its content, so it never
            // overflows, never draws a scrollbar, and everything below the pane's
            // bottom edge is unreachable — it just gets clipped.
            .auto_shrink([false, false])
            .show(ui, |ui| match self.tab {
                Tab::Devices => self.tab_devices(ui),
                Tab::Input => self.tab_input(ui),
                Tab::Macros => self.tab_macros(ui),
                Tab::Options => self.tab_options(ui),
                Tab::About => self.tab_about(ui),
            });

        // Kept permanently: when someone reports "the pane looks cut off", this is
        // the number that says whether the viewport really was too short (content
        // taller than the viewport) or the layout handed the pane no room at all.
        // Logged only when it changes, so it stays readable next to the live log.
        let geom = (
            out.inner_rect.width() as i32,
            out.inner_rect.height() as i32,
            out.content_size.x as i32,
            out.content_size.y as i32,
        );
        if self.pane_geom_logged != Some(geom) {
            self.pane_geom_logged = Some(geom);
            log::trace(format!(
                "left pane: viewport {}x{}  content {}x{}  (content taller than viewport = scrolling)",
                geom.0, geom.1, geom.2, geom.3,
            ));
        }
        let _ = p;
    }

    fn tab_devices(&mut self, ui: &mut egui::Ui) {
        let p = self.palette;

        // ---- video ----
        ui::section(ui, &p, "VIDEO SOURCE");
        let dev_label = self
            .devices
            .get(self.device_sel)
            .map(|d| d.name.clone())
            .unwrap_or_else(|| "(no capture device found)".into());
        let mut dev_pick: Option<usize> = None;
        egui::ComboBox::from_id_salt("device")
            .selected_text(dev_label)
            .width(ui.available_width() - 8.0)
            .show_ui(ui, |ui| {
                for (i, d) in self.devices.iter().enumerate() {
                    if ui.selectable_label(self.device_sel == i, &d.name).clicked() {
                        dev_pick = Some(i);
                    }
                }
            });
        if let Some(i) = dev_pick {
            self.device_sel = i;
            self.refresh_formats();
            self.save_settings();
        }

        ui.add_space(4.0);
        let fmt_label = self
            .formats
            .get(self.format_sel)
            .map(|f| f.label())
            .unwrap_or_else(|| "(auto — MediaFoundation default)".into());
        ui.label(RichText::new("Capture format").color(p.dim).size(12.0));
        let mut fmt_pick: Option<usize> = None;
        egui::ComboBox::from_id_salt("format")
            .selected_text(fmt_label)
            .width(ui.available_width() - 8.0)
            .show_ui(ui, |ui| {
                for (i, f) in self.formats.iter().enumerate() {
                    if ui
                        .selectable_label(self.format_sel == i, f.label())
                        .clicked()
                    {
                        fmt_pick = Some(i);
                    }
                }
            });
        if let Some(i) = fmt_pick {
            self.format_sel = i;
            self.save_settings();
        }

        ui.horizontal(|ui| {
            if ui.button("⟳ Refresh devices").clicked() {
                self.refresh_devices();
            }
            let live = self.capture.is_some();
            if ui
                .button(if live {
                    "■ Stop video"
                } else {
                    "▶ Start video"
                })
                .clicked()
            {
                if live {
                    self.stop_video(true);
                } else {
                    self.start_video();
                }
            }
        });

        ui.add_space(10.0);
        self.capture_report(ui);

        ui.add_space(10.0);
        ui::section(ui, &p, "KVM CABLE (KEYBOARD / MOUSE)");
        let port_label = self
            .ports
            .get(self.port_sel)
            .map(|p| p.label())
            .unwrap_or_else(|| "(no serial port found)".into());
        egui::ComboBox::from_id_salt("port")
            .selected_text(port_label)
            .width(ui.available_width() - 8.0)
            .show_ui(ui, |ui| {
                for (i, sp) in self.ports.iter().enumerate() {
                    let text = if sp.is_kvm_cable {
                        format!("★ {}  [KVM cable]", sp.label())
                    } else {
                        sp.label()
                    };
                    if ui.selectable_label(self.port_sel == i, text).clicked() {
                        self.port_sel = i;
                    }
                }
            });

        ui.horizontal(|ui| {
            ui.label(RichText::new("Baud").color(p.dim).size(12.0));
            egui::ComboBox::from_id_salt("baud")
                .selected_text(self.settings.baud.to_string())
                .width(96.0)
                .show_ui(ui, |ui| {
                    for &b in COMMON_BAUD_RATES {
                        if ui
                            .selectable_label(self.settings.baud == b, b.to_string())
                            .clicked()
                        {
                            self.settings.baud = b;
                            self.save_settings();
                        }
                    }
                });
            ui::toggle(ui, &p, "Auto-connect", &mut self.settings.auto_connect, "");
        });

        ui.horizontal(|ui| {
            if ui.button("⟳ Refresh ports").clicked() {
                self.refresh_ports();
            }
            if self.cable.is_some() {
                if ui.button("⏏ Disconnect").clicked() {
                    self.disconnect(true);
                }
            } else if ui.button("⇄ Connect").clicked() {
                self.connect();
            }
        });

        // ---- chip status ----
        if let Some(cable) = self.cable.clone() {
            ui.add_space(8.0);
            ui::card(ui, &p, |ui| {
                ui.label(RichText::new("CH9329 status").strong().size(12.5));
                match cable.chip_info() {
                    Some(i) => {
                        ui::kv(ui, &p, "Firmware", &i.version_string(), None);
                        ui::kv(
                            ui,
                            &p,
                            "USB to target",
                            if i.usb_connected {
                                "connected"
                            } else {
                                "not enumerated"
                            },
                            Some(if i.usb_connected { p.ok } else { p.warn }),
                        );
                        ui::kv(
                            ui,
                            &p,
                            "Lock LEDs",
                            &format!(
                                "{}{}{}",
                                if i.num_lock { "NUM " } else { "" },
                                if i.caps_lock { "CAPS " } else { "" },
                                if i.scroll_lock { "SCROLL" } else { "" },
                            ),
                            None,
                        );
                    }
                    None => {
                        ui.label(
                            RichText::new("No GET_INFO reply — wrong baud or not a CH9329")
                                .color(p.warn)
                                .size(12.0),
                        );
                    }
                }
                if ui.button("Read chip config (0x08)").clicked() {
                    if let Some(bytes) = cable.read_config_raw() {
                        let hex: Vec<String> = bytes.iter().map(|b| format!("{b:02X}")).collect();
                        log::info(format!(
                            "chip config ({} bytes): {}",
                            bytes.len(),
                            hex.join(" ")
                        ));
                        self.set_toast(
                            format!("Chip config read ({} bytes) — see log", bytes.len()),
                            p.ok,
                        );
                    } else {
                        self.set_toast("No reply to GET_PARA_CFG".into(), p.warn);
                    }
                }
            });
        }
    }

    fn tab_input(&mut self, ui: &mut egui::Ui) {
        let p = self.palette;

        ui::section(ui, &p, "SESSION");
        let has_cable = self.cable.is_some();
        ui.add_enabled_ui(has_cable, |ui| {
            let label = if self.capturing {
                "■ End session (release input)"
            } else {
                "▶ Start session (capture input)"
            };
            if ui
                .add_sized([ui.available_width(), 30.0], egui::Button::new(label))
                .clicked()
            {
                self.set_capture(!self.capturing);
            }
        });
        if !has_cable {
            ui.label(
                RichText::new("Connect a KVM cable to enable input capture.")
                    .color(p.warn)
                    .size(11.5),
            );
        }
        ui::blank(ui);
        ui::toggle(
            ui,
            &p,
            "Fullscreen video",
            &mut self.fullscreen_video,
            "Hide the controls and show only the remote screen",
        );

        ui.add_space(8.0);
        ui::section(ui, &p, "LEAVE GESTURE");
        ui.label(
            RichText::new("How to release the remote and get your local input back:")
                .color(p.dim)
                .size(11.5),
        );
        let mut gesture = self.settings.leave_gesture;
        for g in crate::input::LeaveGesture::ALL {
            if ui.radio_value(&mut gesture, g, g.label()).changed() {
                self.settings.leave_gesture = g;
                if let Some(t) = &self.input {
                    t.set_leave_gesture(g);
                }
                self.save_settings();
            }
        }
        ui::toggle(
            ui,
            &p,
            "Panic hotkey (Ctrl+Shift+Alt+Backspace)",
            &mut self.settings.panic_hotkey_enabled,
            "Instantly stops capture, whatever else is happening",
        );

        ui.add_space(8.0);
        ui::section(ui, &p, "MOUSE");
        let mut speed = self.settings.mouse_speed;
        ui.horizontal(|ui| {
            ui.label(RichText::new("Pointer speed").color(p.dim).size(12.0));
            if ui
                .add(egui::Slider::new(&mut speed, 0.1..=4.0).fixed_decimals(2))
                .changed()
            {
                self.settings.mouse_speed = speed;
                if let Some(t) = &self.input {
                    t.set_mouse_speed(speed);
                }
            }
        });
        ui::toggle(
            ui,
            &p,
            "Invert scroll wheel",
            &mut self.settings.invert_scroll,
            "",
        );
        ui::toggle(
            ui,
            &p,
            "Swap left / right buttons",
            &mut self.settings.swap_buttons,
            "",
        );
        ui::toggle(
            ui,
            &p,
            "Invert vertical axis",
            &mut self.settings.invert_mouse_y,
            "",
        );

        ui.add_space(8.0);
        ui::section(ui, &p, "KEYBOARD");
        ui::toggle(
            ui,
            &p,
            "Layout-aware forwarding",
            &mut self.settings.layout_aware,
            "Send the character you typed instead of the raw key position \
             (fixes AZERTY / QWERTZ / Dvorak on a US remote)",
        );
        let info = self.layout_info();
        ui::kv(ui, &p, "Local layout", &info.display_name, None);
        ui::kv(ui, &p, "Layout id", &info.klid, None);
        if ui.button("Re-read keyboard layout").clicked() {
            if let Some(t) = &self.input {
                t.refresh_layout();
            }
            let i = self.layout_info();
            log::info(format!("layout: {} ({})", i.display_name, i.klid));
        }
        ui::toggle(
            ui,
            &p,
            "Release keys when the app loses focus",
            &mut self.settings.release_on_focus_loss,
            "",
        );
    }

    fn tab_macros(&mut self, ui: &mut egui::Ui) {
        let p = self.palette;
        let has_cable = self.cable.is_some();
        ui.add_enabled_ui(has_cable, |ui| {
            ui::section(ui, &p, "SPECIAL KEYS");
            ui.horizontal_wrapped(|ui| {
                if ui.button("Ctrl + Alt + Del").clicked() {
                    self.send_ctrl_alt_del();
                }
                if ui.button("Ctrl + Alt + Esc").clicked() {
                    self.send_combo(&[0x11, 0x12, 0x1B]);
                }
                if ui.button("Alt + Tab").clicked() {
                    self.send_combo(&[0x12, 0x09]);
                }
                if ui.button("Alt + F4").clicked() {
                    self.send_combo(&[0x12, 0x73]);
                }
                if ui.button("Win").clicked() {
                    self.send_combo(&[0x5B]);
                }
                if ui.button("Esc").clicked() {
                    self.send_combo(&[0x1B]);
                }
                if ui.button("Tab").clicked() {
                    self.send_combo(&[0x09]);
                }
                if ui.button("Enter").clicked() {
                    self.send_combo(&[0x0D]);
                }
                if ui.button("F2 (BIOS)").clicked() {
                    self.send_combo(&[0x71]);
                }
                if ui.button("Del (BIOS)").clicked() {
                    self.send_combo(&[0x2E]);
                }
            });

            ui.add_space(8.0);
            ui::section(ui, &p, "MEDIA KEYS");
            ui.horizontal_wrapped(|ui| {
                for (name, bits) in [
                    ("Vol +", ch9329::media::VOLUME_UP),
                    ("Vol −", ch9329::media::VOLUME_DOWN),
                    ("Mute", ch9329::media::MUTE),
                    ("Play/Pause", ch9329::media::PLAY_PAUSE),
                    ("Next", ch9329::media::NEXT_TRACK),
                    ("Prev", ch9329::media::PREV_TRACK),
                    ("Stop", ch9329::media::STOP),
                ] {
                    if ui.button(name).clicked() {
                        if let Some(c) = &self.cable {
                            c.media_key(bits);
                        }
                    }
                }
            });

            ui.add_space(8.0);
            ui::section(ui, &p, "TYPE TEXT");
            ui.label(
                RichText::new(
                    "Types the text on the remote using US-layout scancodes. \
                               Unsupported characters are skipped.",
                )
                .color(p.dim)
                .size(11.5),
            );
            ui.add(
                egui::TextEdit::multiline(&mut self.macro_text)
                    .desired_rows(3)
                    .hint_text("text to type on the remote machine…"),
            );
            if ui.button("⌨ Type it").clicked() {
                let text = std::mem::take(&mut self.macro_text);
                if let Some(c) = &self.cable {
                    let (sent, skipped) = c.type_text(&text);
                    log::info(format!("typed {sent} character(s), skipped {skipped}"));
                    self.set_toast(
                        format!("Typed {sent} char(s), skipped {skipped}"),
                        if skipped == 0 { p.ok } else { p.warn },
                    );
                }
            }
        });

        if !has_cable {
            ui.label(RichText::new("Connect a KVM cable to send keys.").color(p.warn));
        }

        ui.add_space(8.0);
        ui::section(ui, &p, "SAFETY");
        if ui.button("⟲ Release all keys and buttons").clicked() {
            if let Some(t) = &self.input {
                t.release_all();
            }
            log::info("released all keys/buttons on the remote");
            self.set_toast("Released everything on the remote".into(), p.ok);
        }
    }

    fn tab_options(&mut self, ui: &mut egui::Ui) {
        let p = self.palette;
        ui::section(ui, &p, "SESSION");
        ui::toggle(
            ui,
            &p,
            "Fullscreen video when a session starts",
            &mut self.settings.fullscreen_on_open,
            "",
        );

        ui::section(ui, &p, "VIDEO");
        ui::toggle(
            ui,
            &p,
            "Flip video vertically",
            &mut self.settings.flip_vertical,
            "Use if the picture is upside down (some capture cables report a bottom-up buffer)",
        );
        if ui.button("📷 Save snapshot (BMP)").clicked() {
            self.save_screenshot();
        }

        ui::section(ui, &p, "LOGGING");
        ui.label(
            RichText::new("Logging is always on — this only changes what the panel shows.")
                .color(p.dim)
                .size(11.5),
        );
        ui.horizontal(|ui| {
            for lvl in [
                Level::Trace,
                Level::Debug,
                Level::Info,
                Level::Warn,
                Level::Error,
            ] {
                if ui
                    .selectable_label(self.log_level == lvl, lvl.tag().trim())
                    .clicked()
                {
                    self.log_level = lvl;
                    log::set_panel_level(lvl);
                    self.save_settings();
                }
            }
        });
        if ui.button("Save settings now").clicked() {
            self.save_settings();
            let path = Settings::path();
            self.set_toast(format!("Saved to {}", path.display()), p.ok);
        }
        ui::kv(
            ui,
            &p,
            "Settings file",
            &Settings::path().display().to_string(),
            None,
        );
    }

    fn tab_about(&mut self, ui: &mut egui::Ui) {
        let p = self.palette;
        ui::section(ui, &p, "ABOUT");
        ui.label(RichText::new("KVM-Control Plus").strong().size(15.0));
        ui.label(
            RichText::new(
                "Use your laptop as a screen, mouse and keyboard for another machine \
                 — BIOS included, with no driver on the remote and no KVM hardware \
                 beyond a capture cable and a CH9329 serial bridge.",
            )
            .size(12.0)
            .color(p.dim),
        );
        ui.add_space(6.0);
        ui::kv(ui, &p, "Version", env!("CARGO_PKG_VERSION"), None);
        ui::kv(ui, &p, "Build", env!("BUILD_TIMESTAMP"), None);

        ui.add_space(8.0);
        ui::section(ui, &p, "DEPENDENCIES");
        ui.label(
            RichText::new(
                "Two direct dependencies by design: eframe (GUI) and the windows crate \
                 (Win32 hooks, raw input, SetupAPI, MediaFoundation, serial). \
                 No serialport, nokhwa, image, serde or anyhow.",
            )
            .size(11.5)
            .color(p.dim),
        );

        ui.add_space(8.0);
        ui::section(ui, &p, "COVERAGE");
        ui::kv(ui, &p, "HID key codes", &format!("{} mapped", 100), None);
        ui::kv(
            ui,
            &p,
            "US characters",
            &format!("{} mapped", keymap::us_char_count()),
            None,
        );
        ui::kv(ui, &p, "Media keys", "7 mapped", None);
        ui::kv(
            ui,
            &p,
            "CH9329 commands",
            "0x01 0x02 0x03 0x04 0x05 0x08",
            None,
        );
    }

    fn log_panel(&mut self, ui: &mut egui::Ui) {
        let p = self.palette;
        ui.horizontal(|ui| {
            ui.label(RichText::new("LOG").color(p.dim).size(11.5).strong());
            ui.label(
                RichText::new(format!("panel level: {}", log::panel_level().tag().trim()))
                    .color(p.dim)
                    .size(11.0),
            );
            ui.checkbox(&mut self.log_autoscroll, "follow");
            if ui.button("clear view").clicked() {
                log::clear();
            }
            let dropped = log::dropped_count();
            if dropped > 0 {
                ui.label(
                    RichText::new(format!("{dropped} older line(s) trimmed"))
                        .color(p.dim)
                        .size(11.0),
                );
            }
            let stats = self.stats_line();
            if stats != "idle" {
                // Inline rather than right-aligned: right-aligning it in this row
                // pushed the badge past the panel edge and clipped its text.
                ui.label(RichText::new(stats).color(p.dim).size(11.0));
            }
        });

        let lines = log::snapshot(self.log_level);
        egui::ScrollArea::vertical()
            .max_height(150.0)
            .auto_shrink([false, false])
            .stick_to_bottom(self.log_autoscroll)
            .show(ui, |ui| {
                for l in &lines {
                    let offset = l.at.duration_since(self.log_start).as_secs_f32();
                    ui.label(
                        RichText::new(format!(
                            "+{offset:>7.2}s [{}] {}",
                            l.level.tag().trim(),
                            l.message
                        ))
                        .monospace()
                        .size(11.0)
                        .color(level_color(&p, l.level)),
                    );
                }
            });
    }

    fn stats_line(&self) -> String {
        let mut parts = Vec::new();
        if let Some(c) = &self.cable {
            let s: LinkStatsSnapshot = c.stats();
            parts.push(format!(
                "frames {} · bytes {} · moves {}/{} · err {}",
                s.frames_sent, s.bytes_sent, s.moves_sent, s.moves_coalesced, s.errors
            ));
        }
        if !self.video_label.is_empty() {
            parts.push(self.video_label.clone());
        }
        if !self.video_colour.is_empty() {
            parts.push(self.video_colour.clone());
        }
        if parts.is_empty() {
            "idle".to_string()
        } else {
            parts.join("  |  ")
        }
    }

    fn layout_info(&self) -> layout::LayoutInfo {
        layout::current()
    }

    /// Send Ctrl+Alt+Del to the remote.
    fn send_ctrl_alt_del(&mut self) {
        if let Some(c) = &self.cable {
            c.send_ctrl_alt_del();
            log::info("sent Ctrl+Alt+Del to the remote");
            self.set_toast("Sent Ctrl+Alt+Del".into(), self.palette.ok);
        }
    }

    /// Send a chord of Windows virtual-key codes.
    fn send_combo(&mut self, vks: &[u32]) {
        let Some(cable) = self.cable.clone() else {
            return;
        };
        let mut mods = 0u8;
        let mut keys = Vec::new();
        for &vk in vks {
            if let Some(bit) = keymap::vk_to_modifier(vk) {
                mods |= bit;
            } else if let Some(h) = keymap::vk_to_hid(vk) {
                keys.push(h);
            }
        }
        cable.set_modifiers(mods);
        for &k in &keys {
            cable.key_down(k, mods);
        }
        for &k in &keys {
            cable.key_up(k, mods);
        }
        cable.set_modifiers(0);
        log::debug(format!("sent chord {vks:02X?}"));
    }

    fn video_central(&mut self, ui: &mut egui::Ui) {
        let p = self.palette;
        egui::Frame::default()
            .fill(Color32::BLACK)
            .corner_radius(egui::CornerRadius::same(9))
            .show(ui, |ui| {
                let avail = ui.available_size();
                ui.set_min_size(avail);
                match (&self.texture, &self.last_frame) {
                    (Some(tex), Some(frame)) => {
                        let aspect = frame.width as f32 / frame.height.max(1) as f32;
                        let mut w = avail.x;
                        let mut h = w / aspect;
                        if h > avail.y {
                            h = avail.y;
                            w = h * aspect;
                        }
                        let size = egui::vec2(w, h);
                        let (rect, _) = ui.allocate_exact_size(avail, egui::Sense::hover());
                        let img = egui::Rect::from_center_size(rect.center(), size);
                        ui.painter().image(
                            tex.id(),
                            img,
                            egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                            Color32::WHITE,
                        );
                        // Overlay
                        ui.painter().text(
                            img.left_top() + egui::vec2(8.0, 8.0),
                            egui::Align2::LEFT_TOP,
                            format!(
                                "{}  {:.0} fps  {:.0} ms",
                                self.video_label, self.fps, self.latency_ms
                            ),
                            egui::FontId::monospace(11.0),
                            Color32::from_rgb(200, 210, 220),
                        );
                    }
                    _ => {
                        let (rect, _) = ui.allocate_exact_size(avail, egui::Sense::hover());
                        let msg = match &self.video_error {
                            Some(e) => format!("Video error: {e}"),
                            None if self.devices.is_empty() => {
                                "No capture device detected.\n\nPlug in an MS2130 (or similar) \
                                 USB capture cable and press Refresh devices."
                                    .to_string()
                            }
                            None => "Video stopped.\n\nPress “Start video” on the Devices tab."
                                .to_string(),
                        };
                        ui.painter().text(
                            rect.center(),
                            egui::Align2::CENTER_CENTER,
                            msg,
                            egui::FontId::proportional(14.0),
                            Color32::from_rgb(140, 148, 160),
                        );
                    }
                }
            });
        let _ = p;
    }
}

impl eframe::App for KvmControlPlusApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();

        self.pump_input_events();
        self.pump_video(&ctx);

        // Focus loss safety: stop capturing if the user switched away.
        if self.capturing && self.settings.release_on_focus_loss && !ctx.input(|i| i.focused) {
            log::info("focus lost — releasing captured input");
            self.set_capture(false);
        }

        // Hoisted so the closures below never need to borrow `self` for colours.
        let (panel_fill, bg_fill) = (self.palette.panel, self.palette.bg);

        // egui 0.35 hands the app a bare root `Ui`; panels dock onto it.
        egui::Panel::top("top")
            .frame(
                egui::Frame::default()
                    .fill(panel_fill)
                    .inner_margin(egui::Margin::symmetric(10, 8)),
            )
            .show(ui, |ui| self.top_bar(ui));

        if self.show_log {
            egui::Panel::bottom("log")
                .resizable(true)
                .default_size(170.0)
                .frame(
                    egui::Frame::default()
                        .fill(panel_fill)
                        .inner_margin(egui::Margin::symmetric(10, 6)),
                )
                .show(ui, |ui| self.log_panel(ui));
        }

        if self.show_left_pane && !self.fullscreen_video {
            egui::Panel::left("controls")
                .resizable(true)
                .default_size(390.0)
                .size_range(320.0..=560.0)
                .frame(
                    egui::Frame::default()
                        .fill(panel_fill)
                        .inner_margin(egui::Margin::same(10)),
                )
                .show(ui, |ui| self.left_panel(ui));
        }

        egui::CentralPanel::default_margins()
            .frame(
                egui::Frame::default()
                    .fill(bg_fill)
                    .inner_margin(egui::Margin::same(8)),
            )
            .show(ui, |ui| self.video_central(ui));
    }
}

// ---------------------------------------------------------------------------
//  Small helpers
// ---------------------------------------------------------------------------

/// Markers instead of ✓/✗/⚠: the fonts bundled with eframe do not carry those
/// glyphs, so they render as empty boxes. ASCII reads correctly everywhere.
const OK_MARK: &str = "[ok]";
const NO_MARK: &str = "[NO]";

/// Colour of a log line, resolved against the active theme.
///
/// This used to live in `logging::Level::color()`, hard-coded for the dark panel —
/// INFO was a near-white, which is invisible on the light panel's near-white
/// background. Colour is a presentation concern and `logging` is a data layer, so
/// the mapping belongs here where the palette is in hand.
fn level_color(p: &Palette, level: Level) -> Color32 {
    match level {
        Level::Trace => p.dim,
        Level::Debug => p.dim,
        Level::Info => p.text,
        Level::Warn => p.warn,
        Level::Error => p.bad,
    }
}

/// A compact hub name for the UI: `\\?\usb#root_hub30#4&…` → `root_hub30`.
fn hub_short(path: &str) -> String {
    path.split('#').nth(1).unwrap_or(path).to_string()
}

fn parse_level(s: &str) -> Level {
    match s.trim().to_ascii_lowercase().as_str() {
        "trace" => Level::Trace,
        "debug" => Level::Debug,
        "warn" | "warning" => Level::Warn,
        "error" => Level::Error,
        _ => Level::Info,
    }
}

fn level_name(l: Level) -> String {
    match l {
        Level::Trace => "trace",
        Level::Debug => "debug",
        Level::Info => "info",
        Level::Warn => "warn",
        Level::Error => "error",
    }
    .to_string()
}

//! Settings persistence — a tiny hand-rolled `key = value` format.
//!
//! No `serde`, no registry: the file is human-readable and diff-able, which makes
//! bug reports and hand-editing trivial. Unknown keys are preserved on load so a
//! newer build's settings survive a downgrade.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::path::PathBuf;

/// Set once the first time settings are read, so the startup path (which loads
/// them twice — once before logging is up, once inside the app) does not print
/// the same line twice.
static LOAD_LOGGED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Everything the app remembers between runs.
#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    /// Video capture device id (MediaFoundation symbolic link).
    pub video_source_id: String,
    /// Human-readable name of that device, for the UI when it is absent.
    pub video_source_name: String,
    /// Preferred capture format, e.g. `"1920x1080@60 MJPG"`. Empty = auto-pick.
    pub video_format: String,

    /// KVM-cable serial port, e.g. `"COM5"`.
    pub com_port: String,
    /// Baud rate for the serial link.
    pub baud: u32,
    /// Auto-connect to a detected KVM cable at startup.
    pub auto_connect: bool,

    /// Forward the *character* the user typed rather than the raw key position
    /// (see [`crate::kvm::layout`]).
    pub layout_aware: bool,
    /// Treat the mouse wheel as natural (inverted) scrolling.
    pub invert_scroll: bool,
    /// Swap left/right mouse buttons.
    pub swap_buttons: bool,
    /// Multiplier applied to forwarded pointer movement.
    pub mouse_speed: f32,
    /// Invert the vertical axis of forwarded movement.
    pub invert_mouse_y: bool,
    /// Flip the video vertically (some capture cables report a bottom-up buffer).
    pub flip_vertical: bool,

    /// Open the video pane full-screen as soon as a session starts.
    pub fullscreen_on_open: bool,
    /// Which input gesture leaves a captured session.
    pub leave_gesture: LeaveGesture,
    /// Release all held keys/buttons the instant the app loses focus.
    pub release_on_focus_loss: bool,
    /// Hotkey (Ctrl+Shift+Alt+…) that instantly ends capture. Always available.
    pub panic_hotkey_enabled: bool,

    /// `"dark"` or `"light"`.
    pub theme: String,
    /// Minimum level shown in the log panel (`trace|debug|info|warn|error`).
    pub log_level: String,
    /// Whether the left controls pane starts visible. Hiding it leaves the video
    /// pane the full width of the window; the top-bar toggle brings it back.
    pub show_left_pane: bool,

    /// Keys we did not recognise, kept verbatim so nothing is lost on save.
    pub unknown: BTreeMap<String, String>,
}

/// How a remote session is ended from the keyboard/mouse.
///
/// Re-exported from [`crate::input::leave`] so the Win32 hook and the settings
/// layer share one definition.
pub use crate::input::leave::LeaveGesture;

impl Default for Settings {
    fn default() -> Self {
        Settings {
            video_source_id: String::new(),
            video_source_name: String::new(),
            video_format: String::new(),
            com_port: String::new(),
            baud: crate::kvm::cable::DEFAULT_BAUD,
            auto_connect: true,
            layout_aware: true,
            invert_scroll: false,
            swap_buttons: false,
            mouse_speed: 1.0,
            invert_mouse_y: false,
            flip_vertical: false,
            fullscreen_on_open: false,
            leave_gesture: LeaveGesture::Middle,
            release_on_focus_loss: true,
            panic_hotkey_enabled: true,
            theme: "dark".into(),
            log_level: "info".into(),
            show_left_pane: true,
            unknown: BTreeMap::new(),
        }
    }
}

impl Settings {
    /// `%APPDATA%\KVM-Control Plus\settings.conf`, falling back to the CWD.
    pub fn path() -> PathBuf {
        let base = std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        base.join("KVM-Control Plus").join("settings.conf")
    }

    /// Load from disk; a missing or unreadable file yields defaults (never errors).
    pub fn load() -> Settings {
        match std::fs::read_to_string(Self::path()) {
            Ok(text) => {
                let s = Self::from_text(&text);
                // Startup loads settings twice (once for logging config, once in
                // the app), so log the outcome only the first time.
                if !LOAD_LOGGED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                    crate::logging::info(format!(
                        "settings loaded from {}",
                        Self::path().display()
                    ));
                }
                s
            }
            Err(_) => {
                if !LOAD_LOGGED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                    crate::logging::info(format!(
                        "no settings file yet at {} — using defaults",
                        Self::path().display()
                    ));
                }
                Settings::default()
            }
        }
    }

    /// Persist to disk, creating the directory if needed.
    pub fn save(&self) -> std::io::Result<()> {
        let path = Self::path();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&path, self.to_text())
    }

    /// Serialise to the `key = value` format.
    pub fn to_text(&self) -> String {
        let mut out = String::new();
        out.push_str("# KVM-Control Plus settings — edit freely, or delete the file\n");
        out.push_str("# to fall back to defaults. Values are read back leniently.\n\n");
        out.push_str("[video]\n");
        kv(&mut out, "source_id", &self.video_source_id);
        kv(&mut out, "source_name", &self.video_source_name);
        kv(&mut out, "format", &self.video_format);
        out.push_str("\n[serial]\n");
        kv(&mut out, "port", &self.com_port);
        kv(&mut out, "baud", &self.baud.to_string());
        kv(&mut out, "auto_connect", &self.auto_connect.to_string());
        out.push_str("\n[input]\n");
        kv(&mut out, "layout_aware", &self.layout_aware.to_string());
        kv(&mut out, "invert_scroll", &self.invert_scroll.to_string());
        kv(&mut out, "swap_buttons", &self.swap_buttons.to_string());
        kv(&mut out, "mouse_speed", &format!("{:.2}", self.mouse_speed));
        kv(&mut out, "invert_mouse_y", &self.invert_mouse_y.to_string());
        kv(&mut out, "flip_vertical", &self.flip_vertical.to_string());
        kv(&mut out, "leave_gesture", self.leave_gesture.as_str());
        kv(
            &mut out,
            "release_on_focus_loss",
            &self.release_on_focus_loss.to_string(),
        );
        kv(
            &mut out,
            "panic_hotkey",
            &self.panic_hotkey_enabled.to_string(),
        );
        out.push_str("\n[session]\n");
        kv(
            &mut out,
            "fullscreen_on_open",
            &self.fullscreen_on_open.to_string(),
        );
        out.push_str("\n[ui]\n");
        kv(&mut out, "theme", &self.theme);
        kv(&mut out, "log_level", &self.log_level);
        kv(&mut out, "show_left_pane", &self.show_left_pane.to_string());
        for (k, v) in &self.unknown {
            kv(&mut out, k, v);
        }
        out
    }

    /// Parse the `key = value` format. Unknown keys are kept in `unknown`.
    pub fn from_text(text: &str) -> Settings {
        let mut s = Settings::default();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with('[') {
                continue;
            }
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            let (k, v) = (k.trim().to_ascii_lowercase(), v.trim().to_string());
            match k.as_str() {
                "source_id" => s.video_source_id = v,
                "source_name" => s.video_source_name = v,
                "format" => s.video_format = v,
                "port" => s.com_port = v,
                "baud" => s.baud = v.parse().unwrap_or(s.baud),
                "auto_connect" => s.auto_connect = parse_bool(&v),
                "layout_aware" => s.layout_aware = parse_bool(&v),
                "invert_scroll" => s.invert_scroll = parse_bool(&v),
                "swap_buttons" => s.swap_buttons = parse_bool(&v),
                "mouse_speed" => {
                    let parsed: f32 = v.parse().unwrap_or(1.0);
                    s.mouse_speed = parsed.clamp(0.1, 10.0)
                }
                "invert_mouse_y" => s.invert_mouse_y = parse_bool(&v),
                "flip_vertical" => s.flip_vertical = parse_bool(&v),
                "leave_gesture" => s.leave_gesture = LeaveGesture::parse(&v),
                "release_on_focus_loss" => s.release_on_focus_loss = parse_bool(&v),
                "panic_hotkey" => s.panic_hotkey_enabled = parse_bool(&v),
                "fullscreen_on_open" => s.fullscreen_on_open = parse_bool(&v),
                "theme" => s.theme = v,
                "log_level" => s.log_level = v,
                "show_left_pane" => s.show_left_pane = parse_bool(&v),
                other => {
                    s.unknown.insert(other.to_string(), v);
                }
            }
        }
        s
    }
}

fn kv(out: &mut String, key: &str, value: &str) {
    out.push_str(&format!("{key} = {value}\n"));
}

fn parse_bool(v: &str) -> bool {
    matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_text() {
        let mut s = Settings::default();
        s.com_port = "COM7".into();
        s.baud = 115200;
        s.mouse_speed = 1.75;
        s.leave_gesture = LeaveGesture::CtrlAltBackspace;
        s.invert_scroll = true;
        s.theme = "light".into();

        let restored = Settings::from_text(&s.to_text());
        // `unknown` is not compared because serialisation merges it at the end.
        assert_eq!(restored.com_port, "COM7");
        assert_eq!(restored.baud, 115200);
        assert!((restored.mouse_speed - 1.75).abs() < 1e-6);
        assert_eq!(restored.leave_gesture, LeaveGesture::CtrlAltBackspace);
        assert!(restored.invert_scroll);
        assert_eq!(restored.theme, "light");
    }

    #[test]
    fn tolerates_garbage_and_missing_keys() {
        let s = Settings::from_text("garbage\n= broken\n[section]\nbaud = notanumber\n");
        assert_eq!(s.baud, crate::kvm::cable::DEFAULT_BAUD); // fell back to default
    }

    #[test]
    fn keeps_unknown_keys_so_they_survive_a_save() {
        let s = Settings::from_text("future_option = 42\n");
        assert_eq!(s.unknown.get("future_option"), Some(&"42".to_string()));
        assert!(s.to_text().contains("future_option = 42"));
    }

    #[test]
    fn clamps_absurd_mouse_speed() {
        assert_eq!(Settings::from_text("mouse_speed = 9999").mouse_speed, 10.0);
        assert_eq!(Settings::from_text("mouse_speed = 0").mouse_speed, 0.1);
    }

    #[test]
    fn bool_parsing_accepts_common_spellings() {
        assert!(parse_bool("true"));
        assert!(parse_bool("YES"));
        assert!(parse_bool("1"));
        assert!(parse_bool("on"));
        assert!(!parse_bool("false"));
        assert!(!parse_bool("no"));
    }
}

//! Permanent, first-class logging (project policy: verbose logging is never removed).
//!
//! Every record goes into an in-memory ring buffer that the GUI log panel reads,
//! regardless of the on-screen filter. The filter (`-q`) only *hides* lines — no
//! code path ever deletes a log as a way to reduce noise.

use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use eframe::egui;

/// Severity, ordered so `Trace < Debug < Info < Warn < Error`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

impl Level {
    /// Parse a level name, as used by `--level`.
    pub fn parse(s: &str) -> Option<Level> {
        match s.trim().to_ascii_lowercase().as_str() {
            "trace" => Some(Level::Trace),
            "debug" => Some(Level::Debug),
            "info" => Some(Level::Info),
            "warn" | "warning" => Some(Level::Warn),
            "error" => Some(Level::Error),
            _ => None,
        }
    }

    pub fn tag(self) -> &'static str {
        match self {
            Level::Trace => "TRACE",
            Level::Debug => "DEBUG",
            Level::Info => "INFO ",
            Level::Warn => "WARN ",
            Level::Error => "ERROR",
        }
    }

    pub fn color(self) -> egui::Color32 {
        match self {
            Level::Trace => egui::Color32::from_rgb(120, 130, 145),
            Level::Debug => egui::Color32::from_rgb(130, 170, 200),
            Level::Info => egui::Color32::from_rgb(200, 205, 215),
            Level::Warn => egui::Color32::from_rgb(230, 190, 110),
            Level::Error => egui::Color32::from_rgb(235, 120, 110),
        }
    }
}

/// One recorded log line.
#[derive(Debug, Clone)]
pub struct LogLine {
    pub at: Instant,
    pub level: Level,
    pub message: String,
}

struct LogState {
    lines: VecDeque<LogLine>,
    stdout_level: Level,
    panel_level: Level,
    /// Count of records dropped once the ring buffer was full.
    dropped: u64,
}

const RING_CAPACITY: usize = 4000;

static LOG: OnceLock<Mutex<LogState>> = OnceLock::new();

fn state() -> &'static Mutex<LogState> {
    LOG.get_or_init(|| {
        Mutex::new(LogState {
            lines: VecDeque::with_capacity(256),
            stdout_level: Level::Warn,
            panel_level: Level::Info,
            dropped: 0,
        })
    })
}

/// Configure levels. Called once from `main` before anything logs.
pub fn init(stdout_level: Level, panel_level: Level) {
    let mut s = state().lock().unwrap();
    s.stdout_level = stdout_level;
    s.panel_level = panel_level;
}

/// Record a line at `level`.
pub fn log(level: Level, message: impl Into<String>) {
    let message = message.into();
    let mut s = state().lock().unwrap();
    if level >= s.stdout_level {
        eprintln!("[{}] {message}", level.tag());
    }
    if s.lines.len() == RING_CAPACITY {
        s.lines.pop_front();
        s.dropped += 1;
    }
    s.lines.push_back(LogLine {
        at: Instant::now(),
        level,
        message,
    });
}

/// Current minimum level shown in the GUI panel.
pub fn panel_level() -> Level {
    state().lock().unwrap().panel_level
}

/// Change the GUI panel filter (never drops history).
pub fn set_panel_level(level: Level) {
    state().lock().unwrap().panel_level = level;
}

/// Snapshot of buffered lines at or above `min`.
pub fn snapshot(min: Level) -> Vec<LogLine> {
    let s = state().lock().unwrap();
    s.lines.iter().filter(|l| l.level >= min).cloned().collect()
}

/// Number of records dropped because the ring buffer wrapped.
pub fn dropped_count() -> u64 {
    state().lock().unwrap().dropped
}

/// Clear the buffer (an explicit user action in the UI, not automatic pruning).
pub fn clear() {
    let mut s = state().lock().unwrap();
    s.lines.clear();
}

// ---- convenience macros ----------------------------------------------------

pub use level_fns::*;
mod level_fns {
    use super::{log, Level};
    pub fn trace(m: impl Into<String>) {
        log(Level::Trace, m)
    }
    pub fn debug(m: impl Into<String>) {
        log(Level::Debug, m)
    }
    pub fn info(m: impl Into<String>) {
        log(Level::Info, m)
    }
    pub fn warn(m: impl Into<String>) {
        log(Level::Warn, m)
    }
    pub fn error(m: impl Into<String>) {
        log(Level::Error, m)
    }
}

/// `dbg_log!("frame {:02X?}", bytes)` — formats only when Debug is enabled.
#[macro_export]
macro_rules! dbg_log {
    ($($arg:tt)*) => {
        if $crate::logging::panel_level() <= $crate::logging::Level::Debug {
            $crate::logging::log($crate::logging::Level::Debug, format!($($arg)*));
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captures_and_filters_by_level() {
        clear();
        init(Level::Error, Level::Info); // keep stdout quiet during tests
        log(Level::Info, "hello");
        log(Level::Error, "boom");
        let all = snapshot(Level::Trace);
        assert!(all.iter().any(|l| l.message == "hello"));
        assert!(all.iter().any(|l| l.message == "boom"));
        let errs = snapshot(Level::Error);
        assert!(errs.iter().all(|l| l.level == Level::Error));
        clear();
    }

    #[test]
    fn panel_filter_is_changeable_without_losing_history() {
        clear();
        log(Level::Debug, "detail");
        set_panel_level(Level::Error);
        assert!(snapshot(panel_level()).is_empty());
        set_panel_level(Level::Trace);
        assert!(snapshot(panel_level())
            .iter()
            .any(|l| l.message == "detail"));
        clear();
    }
}

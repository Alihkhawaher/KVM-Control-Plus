//! Permanent, first-class logging (project policy: verbose logging is never removed).
//!
//! Every record goes into an in-memory ring buffer that the GUI log panel reads,
//! regardless of the on-screen filter. The filter (`-q`) only *hides* lines — no
//! code path ever deletes a log as a way to reduce noise.

use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

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

/// Whether the logger still has a console to write to. Flipped off by
/// [`detach_stdout`] when the app gives its console back to Windows.
static STDOUT_ATTACHED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

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

/// Stop writing to stdout/stderr, because the console has been detached and the
/// stream no longer exists.
///
/// Logging itself is *not* turned off — this is the opposite of removing it. The
/// ring buffer keeps receiving every line exactly as before, so the GUI log panel
/// is unaffected; only a console nobody can see anymore stops being written to.
/// (`eprintln!` panics when the write fails, so this must be called before
/// `FreeConsole`.)
pub fn detach_stdout() {
    STDOUT_ATTACHED.store(false, std::sync::atomic::Ordering::Relaxed);
}

/// Whether the logger is still writing to a console. Exposed so `main` can tell
/// the user where the log went.
pub fn stdout_attached() -> bool {
    STDOUT_ATTACHED.load(std::sync::atomic::Ordering::Relaxed)
}

/// The on-disk log for this run, when the console has been given up.
///
/// Born from a specific failure: on 2026-10-11 this app died of a failed
/// allocation, and the one line that would have said *how big* the allocation was
/// — Rust prints that to stderr itself — went to a console that had already been
/// detached. Nothing survived. Now a GUI run writes everything to a file and aims
/// the process's stderr at it, so the runtime's own fatal messages land somewhere
/// readable too. See `docs/CRASH_2026-10-11.md`.
struct FileSink {
    file: std::fs::File,
    written: u64,
    capped: bool,
}

/// A hard cap on the log file, so a chatty run cannot fill the disk. The
/// in-memory ring buffer keeps going regardless — this bounds bytes, not logging.
const LOG_FILE_CAP: u64 = 32 * 1024 * 1024;

static LOG_FILE: OnceLock<Mutex<Option<FileSink>>> = OnceLock::new();

/// Send this run's log to `path`, and point the process's stderr at the same file.
///
/// Returns false if the file cannot be opened, in which case the caller must fall
/// back to [`detach_stdout`] before giving the console up — writing to a detached
/// console makes `eprintln!` panic.
///
/// The stderr redirect is **best effort**: Rust may have cached the console's
/// handle before this is called, in which case only our own records (written
/// directly, above) reach the file. Nothing here depends on it succeeding.
#[cfg(windows)]
pub fn redirect_stderr_to_file(path: &std::path::Path) -> bool {
    use std::os::windows::io::AsRawHandle;

    let file = match std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)
    {
        Ok(f) => f,
        Err(_) => return false,
    };

    // Aim stderr at the same file, so anything that writes to stderr — including
    // the Rust runtime's "memory allocation of N bytes failed" — is captured.
    unsafe {
        let handle = windows::Win32::Foundation::HANDLE(file.as_raw_handle() as _);
        let _ = windows::Win32::System::Console::SetStdHandle(
            windows::Win32::System::Console::STD_ERROR_HANDLE,
            handle,
        );
    }

    let _ = LOG_FILE.set(Mutex::new(Some(FileSink {
        file,
        written: 0,
        capped: false,
    })));
    true
}

/// Append one record to the on-disk log, respecting the cap.
fn write_to_file(level: Level, message: &str) {
    let Some(slot) = LOG_FILE.get() else {
        return;
    };
    let mut guard = slot.lock().unwrap();
    let Some(sink) = guard.as_mut() else {
        return;
    };
    if sink.capped {
        return;
    }
    use std::io::Write;
    // Unbuffered on purpose: a crash must not lose the lines that explain it.
    let line = format!("[{}] {message}\n", level.tag());
    if sink.file.write_all(line.as_bytes()).is_err() {
        return;
    }
    sink.written += line.len() as u64;
    if sink.written >= LOG_FILE_CAP {
        sink.capped = true;
        let _ = sink
            .file
            .write_all(b"[WARN ] log file reached its size cap; the in-memory log continues\n");
    }
}

/// Record a line at `level`.
pub fn log(level: Level, message: impl Into<String>) {
    let message = message.into();
    write_to_file(level, &message);
    let mut s = state().lock().unwrap();
    if level >= s.stdout_level && stdout_attached() {
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

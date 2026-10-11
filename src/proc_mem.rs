//! How much memory this process is holding, and the ceiling it must not cross.
//!
//! Why this exists: on 2026-10-11 this app committed **478 GB** and took the
//! desktop's compositor down with it (see `docs/CRASH_2026-10-11.md`). The cause
//! is fixed — see `FrameSlot` in `video::mf` — and this module is the backstop
//! for whatever the next cause turns out to be. A bug should be able to damage
//! *this app*, never the machine it is running on.
//!
//! `PagefileUsage` is the number to watch: it is the same figure Windows'
//! Resource-Exhaustion-Detector reports as "bytes consumed", so this ceiling is
//! directly comparable with the numbers in that crash report.

/// Default ceiling on committed memory: 4 GiB.
///
/// Legitimate use is around 100 MB and this machine dies somewhere past 250 GB,
/// so 4 GiB sits far outside both: it cannot fire in normal operation, and it
/// fires two orders of magnitude sooner than the desktop is at risk.
pub const DEFAULT_CEILING: u64 = 4 * 1024 * 1024 * 1024;

/// The ceiling in bytes, honouring `KVM_MAX_COMMIT_MB` when it parses.
///
/// The override exists so the watchdog can be *tested* without risking the
/// machine: run with `KVM_MAX_COMMIT_MB=300` and a working watchdog trips at
/// 300 MB instead of 4 GB. Anything unparseable falls back to the default, so a
/// typo can never remove the ceiling.
pub fn ceiling_from(raw: Option<&str>) -> u64 {
    match raw.map(|s| s.trim().parse::<u64>()) {
        Some(Ok(mb)) if mb > 0 => mb.saturating_mul(1024 * 1024),
        _ => DEFAULT_CEILING,
    }
}

/// The active ceiling.
pub fn ceiling_bytes() -> u64 {
    ceiling_from(std::env::var("KVM_MAX_COMMIT_MB").ok().as_deref())
}

/// Committed bytes for this process (`PagefileUsage`), or 0 if the query fails.
///
/// Failing open (returning 0) is deliberate: an unreadable counter must never be
/// mistaken for "over the ceiling" and stop a working capture.
pub fn commit_bytes() -> u64 {
    use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
    use windows::Win32::System::Threading::GetCurrentProcess;

    // SAFETY: the API writes only into the struct passed by pointer, and `cb` is
    // set to that struct's real size, which is exactly what it requires.
    unsafe {
        let mut counters = PROCESS_MEMORY_COUNTERS::default();
        counters.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
        if GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, counters.cb).is_ok() {
            counters.PagefileUsage as u64
        } else {
            0
        }
    }
}

/// The current commit, when it is over the ceiling. `None` while it is not.
pub fn over_ceiling() -> Option<u64> {
    let used = commit_bytes();
    if used > ceiling_bytes() {
        Some(used)
    } else {
        None
    }
}

/// Human-readable megabytes — used by the log, so nobody has to divide by hand.
pub fn mb(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ceiling_cannot_be_removed_by_a_bad_override() {
        // Every one of these has to fall back, or a typo would silently disable
        // the only thing standing between a bug and the desktop.
        for bad in [
            None,
            Some(""),
            Some("   "),
            Some("nonsense"),
            Some("0"),
            Some("-1"),
        ] {
            assert_eq!(
                ceiling_from(bad),
                DEFAULT_CEILING,
                "override {bad:?} was trusted"
            );
        }
        assert_eq!(ceiling_from(Some("300")), 300 * 1024 * 1024);
        assert_eq!(ceiling_from(Some(" 300 ")), 300 * 1024 * 1024);
    }

    #[test]
    fn the_default_ceiling_is_far_above_normal_use_and_far_below_the_crash() {
        const CRASH_BYTES: u64 = 478_142_341_120; // what Windows reported on 2026-10-11
        assert!(
            DEFAULT_CEILING > 512 * 1024 * 1024,
            "must not fire on normal use"
        );
        assert!(
            DEFAULT_CEILING * 50 < CRASH_BYTES,
            "must fire long before the desktop dies"
        );
    }

    #[test]
    fn commit_is_readable_and_every_reading_is_in_megabytes() {
        let used = commit_bytes();
        // A live process always reports something; 0 means the query failed, and
        // that is only acceptable as "not over the ceiling".
        assert!(
            used > 0,
            "GetProcessMemoryInfo returned nothing for a running process"
        );
        assert!(!over_ceiling().is_some_and(|u| u != used));
        assert!(mb(used) > 0.0);
    }
}

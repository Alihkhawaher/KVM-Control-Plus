//! Keyboard-layout-aware forwarding.
//!
//! # The problem this solves
//!
//! The remote machine always sees a **US-layout** keyboard (the CH9329 speaks raw
//! HID scancodes). The original Control3 mapped Windows virtual-keys straight to HID
//! codes, which is correct on a US keyboard but **breaks on every other layout**:
//! on AZERTY the key that types `m` reports `VK_OEM_1` (the US `;`), so the remote
//! received `;`. The author hand-built AZERTY/QWERTZ tables to work around it.
//!
//! # The fix
//!
//! Ask Windows what *character* the pressed key produces **in the active layout**
//! (`ToUnicodeEx`), then look that character up in the US table
//! ([`crate::kvm::keymap::us_char_to_hid`]) to get the scancode + shift the remote
//! needs. The remote then receives exactly what the user typed, on any layout,
//! with no per-layout data tables.
//!
//! Non-character keys (arrows, F-keys, modifiers) have no character, so callers
//! fall back to the direct VK→HID map.

#![allow(dead_code)]

use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyboardLayout, GetKeyboardLayoutNameW, ToUnicodeEx, HKL,
};

use crate::kvm::keymap::us_char_to_hid;

/// A key that has been resolved to a US-layout HID report entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Forward {
    /// USB HID usage code to send.
    pub hid: u8,
    /// Whether the remote must also hold Shift for that code to type the character.
    pub needs_shift: bool,
    /// Whether the character came from AltGr (so Ctrl+Alt must **not** be forwarded).
    pub from_altgr: bool,
}

/// The active keyboard layout plus a friendly display name.
#[derive(Debug, Clone)]
pub struct LayoutInfo {
    pub hkl: HKL,
    /// Windows keyboard-layout id, e.g. `"00000409"`.
    pub klid: String,
    /// Human-readable name, e.g. `"English (United States)"`.
    pub display_name: String,
}

// `HKL` is an opaque kernel handle (`*mut c_void`). It is only ever read while
// holding the owning mutex, and Windows itself hands the same value to any
// thread, so sharing it between threads is sound.
unsafe impl Send for LayoutInfo {}
unsafe impl Sync for LayoutInfo {}

impl LayoutInfo {
    /// True when the active layout is a US English one.
    pub fn is_us(&self) -> bool {
        self.klid == "00000409" || self.klid == "00010409" || self.klid == "00000409".to_string()
    }
}

/// Query the keyboard layout of the calling thread (the input thread uses this).
pub fn current() -> LayoutInfo {
    unsafe {
        let hkl = GetKeyboardLayout(0);
        let mut buf = [0u16; 9]; // KLID is 8 hex digits + NUL
        let klid = if GetKeyboardLayoutNameW(&mut buf).is_ok() {
            let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
            String::from_utf16_lossy(&buf[..n])
        } else {
            String::new()
        };
        let display_name = friendly_name(&klid);
        LayoutInfo {
            hkl,
            klid,
            display_name,
        }
    }
}

/// Friendly name for a Windows keyboard-layout id (KLID).
///
/// Only the layouts a KVM user is likely to have are named; anything else falls
/// back to the raw id, which is still enough to identify it in a bug report.
pub fn friendly_name(klid: &str) -> String {
    let name = match klid {
        "00000409" => "English (United States)",
        "00000809" => "English (United Kingdom)",
        "00000C09" => "English (Canada)",
        "00001009" => "English (Canada, US)",
        "0000040C" => "French (France) — AZERTY",
        "0000080C" => "French (Belgium) — AZERTY",
        "0000100C" => "French (Switzerland)",
        "00000407" => "German (Germany) — QWERTZ",
        "00000C07" => "German (Austria) — QWERTZ",
        "00000807" => "German (Switzerland) — QWERTZ",
        "00000413" => "Dutch (Netherlands)",
        "00000406" => "Danish (Denmark)",
        "0000041D" => "Swedish (Sweden)",
        "00000414" => "Norwegian (Bokmål)",
        "0000040B" => "Finnish (Finland)",
        "0000040F" => "Icelandic (Iceland)",
        "0000040A" => "Spanish (Spain)",
        "0000080A" => "Spanish (Latin America)",
        "00000416" => "Portuguese (Brazil)",
        "00000816" => "Portuguese (Portugal)",
        "00000410" => "Italian (Italy)",
        "00000419" => "Russian (Russia)",
        "00000408" => "Greek (Greece)",
        "0000040E" => "Hungarian (Hungary)",
        "00000405" => "Czech (Czechia)",
        "00000415" => "Polish (Poland)",
        "00000418" => "Romanian (Romania)",
        "0000041F" => "Turkish (Turkey)",
        "0000040D" => "Hebrew (Israel)",
        "00000401" => "Arabic (Saudi Arabia)",
        "00000411" => "Japanese (Japan)",
        "00000412" => "Korean (Korea)",
        "00000804" => "Chinese (Simplified)",
        "00000404" => "Chinese (Traditional)",
        _ => return format!("Keyboard layout {klid}"),
    };
    name.to_string()
}

/// Resolve a key press to the US-layout HID report entry for the character it
/// produces in the given layout.
///
/// Returns `None` when the key produces no character (function/arrow/modifier
/// keys, dead keys) or a character that has no US equivalent (`é`, `ß`, `€`…);
/// callers then fall back to the direct VK→HID map.
///
/// * `shift_down` — is either Shift held?
/// * `altgr_down` — is AltGr (Right Alt) held?
///
/// `altgr_down` is applied to `ToUnicodeEx` as RightCtrl+RightAlt, which is what
/// Windows itself synthesises for AltGr, so third-level characters (`@`, `€`, `{`
/// on many European layouts) resolve correctly.
pub fn resolve(
    hkl: HKL,
    vk: u32,
    scancode: u32,
    shift_down: bool,
    altgr_down: bool,
) -> Option<Forward> {
    let mut keystate = [0u8; 256];
    // High bit set == key down; ToUnicodeEx only reads these two/Menus.
    if shift_down {
        keystate[0x10] = 0x80; // VK_SHIFT
    }
    if altgr_down {
        keystate[0x11] = 0x80; // VK_CONTROL
        keystate[0x12] = 0x80; // VK_MENU (Alt)
    }
    // A Ctrl/Alt that is not AltGr is *not* pushed into the conversion: the user
    // wants the character, not the control code.

    let mut out = [0u16; 4];
    let n = unsafe { ToUnicodeEx(vk, scancode, &keystate, &mut out, 0, Some(hkl)) };

    match n {
        1 => {
            let ch = char::from_u32(out[0] as u32)?;
            let (hid, needs_shift) = us_char_to_hid(ch)?;
            Some(Forward {
                hid,
                needs_shift,
                from_altgr: altgr_down,
            })
        }
        // -1: dead key awaiting a second stroke. Treat as unmapped so the direct
        // map handles the key instead of swallowing the input.
        // 0: no character.
        _ => None,
    }
}

// ===========================================================================
//  Tests — pure helpers, plus a live ToUnicodeEx smoke test on the host layout.
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_common_layouts() {
        assert_eq!(friendly_name("00000409"), "English (United States)");
        assert!(friendly_name("0000040C").contains("AZERTY"));
        assert!(friendly_name("00000407").contains("QWERTZ"));
        assert!(friendly_name("00009999").starts_with("Keyboard layout"));
    }

    #[test]
    fn us_layout_detected() {
        let info = LayoutInfo {
            hkl: HKL::default(),
            klid: "00000409".into(),
            display_name: friendly_name("00000409"),
        };
        assert!(info.is_us());
    }

    #[test]
    fn current_layout_is_queryable() {
        // Must never panic and must produce a plausible KLID on a real host.
        let info = current();
        assert_eq!(
            info.klid.len(),
            8,
            "KLID should be 8 hex digits, got {:?}",
            info.klid
        );
        assert!(info.klid.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn resolves_a_letter_on_the_host_layout() {
        let info = current();
        // On every Latin layout, VK_A with no shift produces 'a'.
        if let Some(f) = resolve(info.hkl, 0x41, 0x1E, false, false) {
            assert_eq!(f.hid, crate::kvm::ch9329::hid::A);
            assert!(!f.needs_shift);
        }
    }
}

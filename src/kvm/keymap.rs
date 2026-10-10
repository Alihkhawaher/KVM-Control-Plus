//! Key translation: Windows virtual-key ↔ USB HID usage codes.
//!
//! Two independent maps are provided:
//!
//! 1. **Direct VK → HID** ([`vk_to_hid`]) — position/Latin-table based, exactly what
//!    the original Control3 used. Correct on a US keyboard, and correct for letters
//!    and digits on *any* Latin layout (Windows derives the VK from the active
//!    layout, so VK_A always means "the key that types `a`").
//!
//! 2. **US character → (HID, shift)** ([`us_char_to_hid`]) — used by [`crate::kvm::layout`]
//!    to forward the *character* the user actually typed, which fixes the punctuation
//!    mismatch that made the original US-only on AZERTY/QWERTZ/Dvorak boards.
//!
//! All values come from the USB HID Usage Tables (Keyboard page 0x07) and were
//! cross-checked against the original project's `keycodes.xlsx` mapping sheets.

#![allow(dead_code)]

use crate::kvm::ch9329::hid;

// ---------------------------------------------------------------------------
//  Windows virtual-key codes we care about
// ---------------------------------------------------------------------------

pub mod vk {
    pub const VK_BACK: u32 = 0x08;
    pub const VK_TAB: u32 = 0x09;
    pub const VK_RETURN: u32 = 0x0D;
    pub const VK_SHIFT: u32 = 0x10;
    pub const VK_CONTROL: u32 = 0x11;
    pub const VK_MENU: u32 = 0x12; // Alt
    pub const VK_PAUSE: u32 = 0x13;
    pub const VK_CAPITAL: u32 = 0x14;
    pub const VK_ESCAPE: u32 = 0x1B;
    pub const VK_SPACE: u32 = 0x20;
    pub const VK_PRIOR: u32 = 0x21;
    pub const VK_NEXT: u32 = 0x22;
    pub const VK_END: u32 = 0x23;
    pub const VK_HOME: u32 = 0x24;
    pub const VK_LEFT: u32 = 0x25;
    pub const VK_UP: u32 = 0x26;
    pub const VK_RIGHT: u32 = 0x27;
    pub const VK_DOWN: u32 = 0x28;
    pub const VK_SNAPSHOT: u32 = 0x2C; // PrintScreen
    pub const VK_INSERT: u32 = 0x2D;
    pub const VK_DELETE: u32 = 0x2E;
    pub const VK_LWIN: u32 = 0x5B;
    pub const VK_RWIN: u32 = 0x5C;
    pub const VK_APPS: u32 = 0x5D;
    pub const VK_NUMPAD0: u32 = 0x60;
    pub const VK_MULTIPLY: u32 = 0x6A;
    pub const VK_ADD: u32 = 0x6B;
    pub const VK_SUBTRACT: u32 = 0x6D;
    pub const VK_DECIMAL: u32 = 0x6E;
    pub const VK_DIVIDE: u32 = 0x6F;
    pub const VK_NUMLOCK: u32 = 0x90;
    pub const VK_SCROLL: u32 = 0x91;
    pub const VK_LSHIFT: u32 = 0xA0;
    pub const VK_RSHIFT: u32 = 0xA1;
    pub const VK_LCONTROL: u32 = 0xA2;
    pub const VK_RCONTROL: u32 = 0xA3;
    pub const VK_LMENU: u32 = 0xA4;
    pub const VK_RMENU: u32 = 0xA5;
    pub const VK_VOLUME_MUTE: u32 = 0xAD;
    pub const VK_VOLUME_DOWN: u32 = 0xAE;
    pub const VK_VOLUME_UP: u32 = 0xAF;
    pub const VK_MEDIA_NEXT_TRACK: u32 = 0xB0;
    pub const VK_MEDIA_PREV_TRACK: u32 = 0xB1;
    pub const VK_MEDIA_STOP: u32 = 0xB2;
    pub const VK_MEDIA_PLAY_PAUSE: u32 = 0xB3;

    /// Left-side modifier keys that emit a distinct 0xA0..0xA5 code.
    pub const ALL_SPECIFIC: [u32; 6] = [
        VK_LSHIFT,
        VK_RSHIFT,
        VK_LCONTROL,
        VK_RCONTROL,
        VK_LMENU,
        VK_RMENU,
    ];
}

use crate::kvm::ch9329::{media, modifier};

// ---------------------------------------------------------------------------
//  Modifiers and media keys
// ---------------------------------------------------------------------------

/// Map a virtual key to its CH9329 modifier bit, if it is a modifier key.
pub fn vk_to_modifier(v: u32) -> Option<u8> {
    let bit = match v {
        vk::VK_LCONTROL => modifier::LCTRL,
        vk::VK_LSHIFT => modifier::LSHIFT,
        vk::VK_LMENU => modifier::LALT,
        vk::VK_LWIN => modifier::LGUI,
        vk::VK_RCONTROL => modifier::RCTRL,
        vk::VK_RSHIFT => modifier::RSHIFT,
        vk::VK_RMENU => modifier::RALT,
        vk::VK_RWIN => modifier::RGUI,
        _ => return None,
    };
    Some(bit)
}

/// Map a virtual key to its media-report bit, if it is a multimedia key.
pub fn vk_to_media(v: u32) -> Option<u8> {
    let bit = match v {
        vk::VK_VOLUME_UP => media::VOLUME_UP,
        vk::VK_VOLUME_DOWN => media::VOLUME_DOWN,
        vk::VK_VOLUME_MUTE => media::MUTE,
        vk::VK_MEDIA_PLAY_PAUSE => media::PLAY_PAUSE,
        vk::VK_MEDIA_NEXT_TRACK => media::NEXT_TRACK,
        vk::VK_MEDIA_PREV_TRACK => media::PREV_TRACK,
        vk::VK_MEDIA_STOP => media::STOP,
        _ => return None,
    };
    Some(bit)
}

/// True for keys that only change modifier state and never carry a USB report
/// key code of their own.
pub fn is_modifier(v: u32) -> bool {
    vk_to_modifier(v).is_some()
}

// ---------------------------------------------------------------------------
//  Direct VK → HID (position based)
// ---------------------------------------------------------------------------

/// Virtual key → USB HID key code. `None` for modifiers (handled separately)
/// and for keys the CH9329 has no code for.
pub fn vk_to_hid(v: u32) -> Option<u8> {
    // Letters and digits: contiguous ranges.
    let code = match v {
        0x41..=0x5A => hid::A + (v as u8 - 0x41),    // A..Z
        0x31..=0x39 => hid::NUM1 + (v as u8 - 0x31), // '1'..'9'
        0x30 => hid::NUM0,                           // '0' sits after '9' in HID
        0x70..=0x7B => hid::F1 + (v as u8 - 0x70),   // F1..F12
        vk::VK_NUMPAD0 => hid::NP0,
        0x61..=0x69 => hid::NP1 + (v as u8 - 0x61), // numpad 1..9
        vk::VK_BACK => hid::BACKSPACE,
        vk::VK_TAB => hid::TAB,
        vk::VK_RETURN => hid::ENTER,
        vk::VK_PAUSE => hid::PAUSE,
        vk::VK_CAPITAL => hid::CAPSLOCK,
        vk::VK_ESCAPE => hid::ESC,
        vk::VK_SPACE => hid::SPACE,
        vk::VK_PRIOR => hid::PAGEUP,
        vk::VK_NEXT => hid::PAGEDOWN,
        vk::VK_END => hid::END,
        vk::VK_HOME => hid::HOME,
        vk::VK_LEFT => hid::LEFT,
        vk::VK_UP => hid::UP,
        vk::VK_RIGHT => hid::RIGHT,
        vk::VK_DOWN => hid::DOWN,
        vk::VK_SNAPSHOT => hid::PRINTSCREEN,
        vk::VK_INSERT => hid::INSERT,
        vk::VK_DELETE => hid::DELETE,
        vk::VK_APPS => hid::APPLICATION,
        vk::VK_MULTIPLY => hid::NP_MULTIPLY,
        vk::VK_ADD => hid::NP_ADD,
        vk::VK_SUBTRACT => hid::NP_SUBTRACT,
        vk::VK_DECIMAL => hid::NP_DECIMAL,
        vk::VK_DIVIDE => hid::NP_DIVIDE,
        vk::VK_NUMLOCK => hid::NUMLOCK,
        vk::VK_SCROLL => hid::SCROLLLOCK,
        // OEM punctuation (US positions)
        0xBA => hid::SEMICOLON,       // VK_OEM_1  ;
        0xBB => hid::EQUAL,           // VK_OEM_PLUS =
        0xBC => hid::COMMA,           // VK_OEM_COMMA ,
        0xBD => hid::MINUS,           // VK_OEM_MINUS -
        0xBE => hid::PERIOD,          // VK_OEM_PERIOD .
        0xBF => hid::SLASH,           // VK_OEM_2 /
        0xC0 => hid::GRAVE,           // VK_OEM_3 `
        0xDB => hid::LBRACKET,        // VK_OEM_4 [
        0xDC => hid::BACKSLASH,       // VK_OEM_5 \
        0xDD => hid::RBRACKET,        // VK_OEM_6 ]
        0xDE => hid::QUOTE,           // VK_OEM_7 '
        0xE2 => hid::NONUS_BACKSLASH, // VK_OEM_102 (ISO < >)
        _ => return None,
    };
    Some(code)
}

// ---------------------------------------------------------------------------
//  US character → (HID, shift)
// ---------------------------------------------------------------------------

/// US-layout character table: `(char, hid_code, needs_shift)`.
///
/// Derived from the USB HID Usage Tables and verified against the original
/// project's `keycodes.xlsx` sheet 1 (HID / Normal / Shift columns).
const US_CHARS: &[(char, u8, bool)] = &[
    // unshifted / shifted letters are derived procedurally in `us_char_to_hid`,
    // but keeping them explicit makes the table self-documenting and testable.
    (' ', hid::SPACE, false),
    ('`', hid::GRAVE, false),
    ('~', hid::GRAVE, true),
    ('-', hid::MINUS, false),
    ('_', hid::MINUS, true),
    ('=', hid::EQUAL, false),
    ('+', hid::EQUAL, true),
    ('[', hid::LBRACKET, false),
    ('{', hid::LBRACKET, true),
    (']', hid::RBRACKET, false),
    ('}', hid::RBRACKET, true),
    ('\\', hid::BACKSLASH, false),
    ('|', hid::BACKSLASH, true),
    (';', hid::SEMICOLON, false),
    (':', hid::SEMICOLON, true),
    ('\'', hid::QUOTE, false),
    ('"', hid::QUOTE, true),
    (',', hid::COMMA, false),
    ('<', hid::COMMA, true),
    ('.', hid::PERIOD, false),
    ('>', hid::PERIOD, true),
    ('/', hid::SLASH, false),
    ('?', hid::SLASH, true),
    ('1', hid::NUM1, false),
    ('!', hid::NUM1, true),
    ('2', hid::NUM2, false),
    ('@', hid::NUM2, true),
    ('3', hid::NUM3, false),
    ('#', hid::NUM3, true),
    ('4', hid::NUM4, false),
    ('$', hid::NUM4, true),
    ('5', hid::NUM5, false),
    ('%', hid::NUM5, true),
    ('6', hid::NUM6, false),
    ('^', hid::NUM6, true),
    ('7', hid::NUM7, false),
    ('&', hid::NUM7, true),
    ('8', hid::NUM8, false),
    ('*', hid::NUM8, true),
    ('9', hid::NUM9, false),
    ('(', hid::NUM9, true),
    ('0', hid::NUM0, false),
    (')', hid::NUM0, true),
];

/// Resolve a character to the US-layout `(HID code, needs_shift)`.
///
/// Letters are handled arithmetically; everything else comes from [`US_CHARS`].
/// Returns `None` for characters with no US keyboard equivalent (e.g. `ß`, `€`,
/// `é` typed on a local layout) — callers then fall back to the direct VK map.
pub fn us_char_to_hid(c: char) -> Option<(u8, bool)> {
    match c {
        'a'..='z' => Some((hid::A + (c as u8 - b'a'), false)),
        'A'..='Z' => Some((hid::A + (c as u8 - b'A'), true)),
        _ => US_CHARS
            .iter()
            .find(|(ch, _, _)| *ch == c)
            .map(|(_, hid, shift)| (*hid, *shift)),
    }
}

/// Every character usable with the forward-layout engine, for the UI's
/// "layout coverage" read-out.
pub fn us_char_count() -> usize {
    US_CHARS.len() + 52 // + a-z, A-Z
}

// ===========================================================================
//  Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letters_map_to_contiguous_hid_codes() {
        assert_eq!(vk_to_hid(0x41), Some(hid::A)); // VK_A
        assert_eq!(vk_to_hid(0x5A), Some(hid::Z)); // VK_Z
    }

    #[test]
    fn digits_wrap_correctly() {
        assert_eq!(vk_to_hid(0x31), Some(hid::NUM1)); // '1'
        assert_eq!(vk_to_hid(0x39), Some(hid::NUM9)); // '9'
        assert_eq!(vk_to_hid(0x30), Some(hid::NUM0)); // '0' is the last digit code
    }

    #[test]
    fn function_keys_map_contiguously() {
        assert_eq!(vk_to_hid(0x70), Some(hid::F1));
        assert_eq!(vk_to_hid(0x7B), Some(hid::F12));
    }

    #[test]
    fn numpad_maps_contiguously() {
        assert_eq!(vk_to_hid(0x60), Some(hid::NP0));
        assert_eq!(vk_to_hid(0x69), Some(hid::NP9));
    }

    #[test]
    fn modifiers_are_not_key_codes() {
        assert!(vk_to_hid(vk::VK_LSHIFT).is_none());
        assert!(vk_to_hid(vk::VK_RCONTROL).is_none());
        assert_eq!(vk_to_modifier(vk::VK_LSHIFT), Some(modifier::LSHIFT));
        assert_eq!(vk_to_modifier(vk::VK_RMENU), Some(modifier::RALT));
        assert_eq!(vk_to_modifier(vk::VK_RWIN), Some(modifier::RGUI));
    }

    #[test]
    fn media_keys_resolve() {
        assert_eq!(vk_to_media(vk::VK_VOLUME_UP), Some(media::VOLUME_UP));
        assert_eq!(
            vk_to_media(vk::VK_MEDIA_PLAY_PAUSE),
            Some(media::PLAY_PAUSE)
        );
        assert_eq!(vk_to_media(vk::VK_MEDIA_STOP), Some(media::STOP));
        assert_eq!(vk_to_media(0x41), None);
    }

    #[test]
    fn oem_punctuation_matches_us_positions() {
        assert_eq!(vk_to_hid(0xBA), Some(hid::SEMICOLON));
        assert_eq!(vk_to_hid(0xC0), Some(hid::GRAVE));
        assert_eq!(vk_to_hid(0xDC), Some(hid::BACKSLASH));
        assert_eq!(vk_to_hid(0xDE), Some(hid::QUOTE));
    }

    #[test]
    fn us_char_lookup_shift_pairs() {
        assert_eq!(us_char_to_hid('a'), Some((hid::A, false)));
        assert_eq!(us_char_to_hid('A'), Some((hid::A, true)));
        assert_eq!(us_char_to_hid('1'), Some((hid::NUM1, false)));
        assert_eq!(us_char_to_hid('!'), Some((hid::NUM1, true)));
        assert_eq!(us_char_to_hid(';'), Some((hid::SEMICOLON, false)));
        assert_eq!(us_char_to_hid(':'), Some((hid::SEMICOLON, true)));
        assert_eq!(us_char_to_hid(' '), Some((hid::SPACE, false)));
    }

    #[test]
    fn non_us_chars_have_no_mapping() {
        // Characters only reachable on other layouts must report "no mapping",
        // so the layout engine can fall back instead of sending something wrong.
        assert_eq!(us_char_to_hid('é'), None);
        assert_eq!(us_char_to_hid('ß'), None);
        assert_eq!(us_char_to_hid('€'), None);
        assert_eq!(us_char_to_hid('ç'), None);
    }

    #[test]
    fn us_table_entries_are_consistent_with_vk_table() {
        // Every punctuation char must land on the same HID code its VK maps to.
        for (vk_code, ch, shift) in [
            (0xBAu32, ';', false),
            (0xBB, '=', false),
            (0xBC, ',', false),
            (0xBD, '-', false),
            (0xBE, '.', false),
            (0xBF, '/', false),
            (0xC0, '`', false),
            (0xDB, '[', false),
            (0xDC, '\\', false),
            (0xDD, ']', false),
            (0xDE, '\'', false),
        ] {
            let (c_hid, c_shift) = us_char_to_hid(ch).unwrap();
            assert_eq!(Some(c_hid), vk_to_hid(vk_code), "char {ch:?}");
            assert!(!c_shift && !shift);
        }
    }
}

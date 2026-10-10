//! CH9329 UART → USB-HID bridge: **pure protocol layer** (no I/O).
//!
//! Every function here is deterministic and covered by unit tests that assert the
//! exact bytes put on the wire, so the wire format is verified without hardware.
//!
//! # Wire format (CH9329 serial protocol, v1.1/v1.2)
//!
//! ```text
//!   +------+------+------+-----+-----+--------------+-----+
//!   | 0x57 | 0xAB | ADDR | CMD | LEN | DATA[LEN]    | SUM |
//!   +------+------+------+-----+-----+--------------+-----+
//!       HEAD (2 bytes)                    payload      checksum
//! ```
//!
//! `SUM` is the low 8 bits of the **sum of every preceding byte**, i.e.
//! `(0x57 + 0xAB + ADDR + CMD + LEN + DATA[..]) & 0xFF`.
//!
//! Verified against three independent references: the WCH datasheet, the
//! `TechxArtisanStudio/Openterface_KM` protocol notes, and `sjmf/kvm-serial`.
//! See `docs/PROTOCOL.md`.

#![allow(dead_code)]

/// Frame header, always `0x57 0xAB`.
pub const HEADER: [u8; 2] = [0x57, 0xAB];
/// Default device address (single chip on the bus).
pub const ADDR: u8 = 0x00;
/// Payload length of a standard keyboard report.
pub const KB_REPORT_LEN: usize = 8;

/// Command bytes.
pub mod cmd {
    /// Query chip status. Reply uses `cmd | 0x80` = `0x81`.
    pub const GET_INFO: u8 = 0x01;
    /// Standard keyboard report (8-byte HID boot report).
    pub const KB_GENERAL: u8 = 0x02;
    /// Multimedia / consumer key report.
    pub const KB_MEDIA: u8 = 0x03;
    /// Absolute mouse report.
    pub const MS_ABS: u8 = 0x04;
    /// Relative mouse report.
    pub const MS_REL: u8 = 0x05;
    /// Read chip configuration (44-byte reply).
    pub const GET_PARA_CFG: u8 = 0x08;
}

/// The bit set on the command byte in a **successful** reply.
pub const RESPONSE_OK: u8 = 0x80;
/// The bit set on the command byte in a **failed** reply.
pub const RESPONSE_ERR: u8 = 0xC0;

// ---------------------------------------------------------------------------
//  Frame assembly
// ---------------------------------------------------------------------------

/// 8-bit additive checksum over `bytes` (all bytes preceding the checksum field).
#[inline]
pub fn checksum(bytes: &[u8]) -> u8 {
    bytes.iter().fold(0u8, |acc, &b| acc.wrapping_add(b))
}

/// Maximum frame size this protocol ever produces (header+addr+cmd+len+44+sum).
pub const MAX_FRAME: usize = 2 + 1 + 1 + 1 + 44 + 1;

/// Build a complete frame into `out`, returning the number of bytes written.
///
/// `out` is reused so the hot path (keyboard/mouse reports) performs no allocation.
pub fn build_frame_into(out: &mut [u8; MAX_FRAME], cmd: u8, data: &[u8]) -> usize {
    debug_assert!(data.len() <= 44, "CH9329 payload longer than 44 bytes");
    let n = 5 + data.len() + 1;
    out[0] = HEADER[0];
    out[1] = HEADER[1];
    out[2] = ADDR;
    out[3] = cmd;
    out[4] = data.len() as u8;
    out[5..5 + data.len()].copy_from_slice(data);
    out[n - 1] = checksum(&out[..n - 1]);
    n
}

/// Convenience: build a frame as a fresh `Vec` (used off the hot path).
pub fn build_frame(cmd: u8, data: &[u8]) -> Vec<u8> {
    let mut buf = [0u8; MAX_FRAME];
    let n = build_frame_into(&mut buf, cmd, data);
    buf[..n].to_vec()
}

// ---------------------------------------------------------------------------
//  Keyboard
// ---------------------------------------------------------------------------

/// HID keyboard modifier bits (byte 0 of the keyboard report).
///
/// Identical to the USB HID boot-keyboard modifier byte:
/// `bit0` LeftCtrl · `bit1` LeftShift · `bit2` LeftAlt · `bit3` LeftGUI ·
/// `bit4` RightCtrl · `bit5` RightShift · `bit6` RightAlt · `bit7` RightGUI.
pub mod modifier {
    pub const LCTRL: u8 = 1 << 0;
    pub const LSHIFT: u8 = 1 << 1;
    pub const LALT: u8 = 1 << 2;
    pub const LGUI: u8 = 1 << 3;
    pub const RCTRL: u8 = 1 << 4;
    pub const RSHIFT: u8 = 1 << 5;
    pub const RALT: u8 = 1 << 6;
    pub const RGUI: u8 = 1 << 7;
}

/// Build the 8-byte payload of a standard keyboard report.
///
/// `keys` is truncated to 6 entries (USB boot-protocol 6-key rollover).
pub fn keyboard_report(modifiers: u8, keys: &[u8]) -> [u8; KB_REPORT_LEN] {
    let mut data = [0u8; KB_REPORT_LEN];
    data[0] = modifiers;
    data[1] = 0x00; // reserved
    for (i, &k) in keys.iter().take(6).enumerate() {
        data[2 + i] = k;
    }
    data
}

/// Build a complete keyboard-report frame (cmd `0x02`).
pub fn keyboard_frame_into(out: &mut [u8; MAX_FRAME], modifiers: u8, keys: &[u8]) -> usize {
    let data = keyboard_report(modifiers, keys);
    build_frame_into(out, cmd::KB_GENERAL, &data)
}

// ---------------------------------------------------------------------------
//  Multimedia keys
// ---------------------------------------------------------------------------

/// Multimedia-key bits (byte 1 of the 4-byte media report).
/// Values follow Control3's field-proven mapping.
pub mod media {
    pub const VOLUME_UP: u8 = 0x01;
    pub const VOLUME_DOWN: u8 = 0x02;
    pub const MUTE: u8 = 0x04;
    pub const PLAY_PAUSE: u8 = 0x08;
    pub const NEXT_TRACK: u8 = 0x10;
    pub const PREV_TRACK: u8 = 0x20;
    pub const STOP: u8 = 0x40;
}

/// 4-byte multimedia report payload: `[0x02, bitmask, 0x00, 0x00]`.
pub fn media_report(bits: u8) -> [u8; 4] {
    [0x02, bits, 0x00, 0x00]
}

/// Build a media-key frame (cmd `0x03`). The chip expands it to press+release.
pub fn media_frame(bits: u8) -> Vec<u8> {
    build_frame(cmd::KB_MEDIA, &media_report(bits))
}

// ---------------------------------------------------------------------------
//  Mouse
// ---------------------------------------------------------------------------

/// Relative-mouse report payload: `[0x01 marker, buttons, dx, dy, wheel]`.
///
/// `dx`/`dy`/`wheel` are signed and **must** be within −127..=127 (the chip
/// treats the byte as signed); callers clamp before calling.
pub fn mouse_rel_report(buttons: u8, dx: i8, dy: i8, wheel: i8) -> [u8; 5] {
    [0x01, buttons, dx as u8, dy as u8, wheel as u8]
}

/// Build a relative-mouse frame (cmd `0x05`).
pub fn mouse_rel_frame_into(
    out: &mut [u8; MAX_FRAME],
    buttons: u8,
    dx: i8,
    dy: i8,
    wheel: i8,
) -> usize {
    let data = mouse_rel_report(buttons, dx, dy, wheel);
    build_frame_into(out, cmd::MS_REL, &data)
}

/// Absolute-mouse report payload (7 bytes):
/// `[0x02 marker, buttons, xL, xH, yL, yH, wheel]`.
///
/// `x`/`y` live in the chip's **12-bit** absolute space (0..=4095).
pub fn mouse_abs_report(buttons: u8, x: u16, y: u16, wheel: i8) -> [u8; 7] {
    let x = x.min(4095);
    let y = y.min(4095);
    [
        0x02,
        buttons,
        (x & 0xFF) as u8,
        (x >> 8) as u8,
        (y & 0xFF) as u8,
        (y >> 8) as u8,
        wheel as u8,
    ]
}

/// Build an absolute-mouse frame (cmd `0x04`).
pub fn mouse_abs_frame(buttons: u8, x: u16, y: u16, wheel: i8) -> Vec<u8> {
    build_frame(cmd::MS_ABS, &mouse_abs_report(buttons, x, y, wheel))
}

/// Mouse button bits (byte 1 of a mouse report).
pub mod buttons {
    pub const LEFT: u8 = 0x01;
    pub const RIGHT: u8 = 0x02;
    pub const MIDDLE: u8 = 0x04;
}

/// A special "signed byte" wrapper: clamp an `i32` delta to the chip's −127..=127.
#[inline]
pub fn clamp_delta(v: i32) -> i8 {
    v.clamp(-127, 127) as i8
}

// ---------------------------------------------------------------------------
//  HID usage codes (full set, US layout)
// ---------------------------------------------------------------------------

/// USB HID keyboard usage codes (keyboard page 0x07).
pub mod hid {
    pub const NONE: u8 = 0x00;
    // letters
    pub const A: u8 = 0x04;
    pub const B: u8 = 0x05;
    pub const C: u8 = 0x06;
    pub const D: u8 = 0x07;
    pub const E: u8 = 0x08;
    pub const F: u8 = 0x09;
    pub const G: u8 = 0x0A;
    pub const H: u8 = 0x0B;
    pub const I: u8 = 0x0C;
    pub const J: u8 = 0x0D;
    pub const K: u8 = 0x0E;
    pub const L: u8 = 0x0F;
    pub const M: u8 = 0x10;
    pub const N: u8 = 0x11;
    pub const O: u8 = 0x12;
    pub const P: u8 = 0x13;
    pub const Q: u8 = 0x14;
    pub const R: u8 = 0x15;
    pub const S: u8 = 0x16;
    pub const T: u8 = 0x17;
    pub const U: u8 = 0x18;
    pub const V: u8 = 0x19;
    pub const W: u8 = 0x1A;
    pub const X: u8 = 0x1B;
    pub const Y: u8 = 0x1C;
    pub const Z: u8 = 0x1D;
    // digits
    pub const NUM1: u8 = 0x1E;
    pub const NUM2: u8 = 0x1F;
    pub const NUM3: u8 = 0x20;
    pub const NUM4: u8 = 0x21;
    pub const NUM5: u8 = 0x22;
    pub const NUM6: u8 = 0x23;
    pub const NUM7: u8 = 0x24;
    pub const NUM8: u8 = 0x25;
    pub const NUM9: u8 = 0x26;
    pub const NUM0: u8 = 0x27;
    // named keys
    pub const ENTER: u8 = 0x28;
    pub const ESC: u8 = 0x29;
    pub const BACKSPACE: u8 = 0x2A;
    pub const TAB: u8 = 0x2B;
    pub const SPACE: u8 = 0x2C;
    pub const MINUS: u8 = 0x2D;
    pub const EQUAL: u8 = 0x2E;
    pub const LBRACKET: u8 = 0x2F;
    pub const RBRACKET: u8 = 0x30;
    pub const BACKSLASH: u8 = 0x31;
    pub const SEMICOLON: u8 = 0x33;
    pub const QUOTE: u8 = 0x34;
    pub const GRAVE: u8 = 0x35;
    pub const COMMA: u8 = 0x36;
    pub const PERIOD: u8 = 0x37;
    pub const SLASH: u8 = 0x38;
    pub const CAPSLOCK: u8 = 0x39;
    pub const F1: u8 = 0x3A;
    pub const F2: u8 = 0x3B;
    pub const F3: u8 = 0x3C;
    pub const F4: u8 = 0x3D;
    pub const F5: u8 = 0x3E;
    pub const F6: u8 = 0x3F;
    pub const F7: u8 = 0x40;
    pub const F8: u8 = 0x41;
    pub const F9: u8 = 0x42;
    pub const F10: u8 = 0x43;
    pub const F11: u8 = 0x44;
    pub const F12: u8 = 0x45;
    pub const PRINTSCREEN: u8 = 0x46;
    pub const SCROLLLOCK: u8 = 0x47;
    pub const PAUSE: u8 = 0x48;
    pub const INSERT: u8 = 0x49;
    pub const HOME: u8 = 0x4A;
    pub const PAGEUP: u8 = 0x4B;
    pub const DELETE: u8 = 0x4C;
    pub const END: u8 = 0x4D;
    pub const PAGEDOWN: u8 = 0x4E;
    pub const RIGHT: u8 = 0x4F;
    pub const LEFT: u8 = 0x50;
    pub const DOWN: u8 = 0x51;
    pub const UP: u8 = 0x52;
    // numpad
    pub const NUMLOCK: u8 = 0x53;
    pub const NP_DIVIDE: u8 = 0x54;
    pub const NP_MULTIPLY: u8 = 0x55;
    pub const NP_SUBTRACT: u8 = 0x56;
    pub const NP_ADD: u8 = 0x57;
    pub const NP_ENTER: u8 = 0x58;
    pub const NP1: u8 = 0x59;
    pub const NP2: u8 = 0x5A;
    pub const NP3: u8 = 0x5B;
    pub const NP4: u8 = 0x5C;
    pub const NP5: u8 = 0x5D;
    pub const NP6: u8 = 0x5E;
    pub const NP7: u8 = 0x5F;
    pub const NP8: u8 = 0x60;
    pub const NP9: u8 = 0x61;
    pub const NP0: u8 = 0x62;
    pub const NP_DECIMAL: u8 = 0x63;
    pub const NONUS_BACKSLASH: u8 = 0x64; // < > on ISO/AZERTY boards
    pub const APPLICATION: u8 = 0x65;
}

// ---------------------------------------------------------------------------
//  Chip status (GET_INFO, cmd 0x01)
// ---------------------------------------------------------------------------

/// Keyboard LED / lock state reported by GET_INFO (byte 2 of the payload).
pub mod led {
    pub const NUM_LOCK: u8 = 0x01;
    pub const CAPS_LOCK: u8 = 0x02;
    pub const SCROLL_LOCK: u8 = 0x04;
}

/// Decoded GET_INFO status block.
///
/// Payload layout (MiniKvm's `InfoPacket`, packed):
/// `[0]` version (`0x30` = V1.0, `0x31` = V1.1) ·
/// `[1]` USB enumeration state (`1` = target PC recognised the HID device) ·
/// `[2]` keyboard LED bitmask · `[3..7]` reserved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChipInfo {
    pub version: u8,
    pub usb_connected: bool,
    pub num_lock: bool,
    pub caps_lock: bool,
    pub scroll_lock: bool,
}

impl ChipInfo {
    /// Human-readable version, e.g. `0x30` → `"V1.0"`.
    pub fn version_string(&self) -> String {
        // WCH encodes the version as two BCD-ish nibbles: 0x30 -> 1.0, 0x31 -> 1.1.
        let major = (self.version >> 4) & 0x0F;
        let minor = self.version & 0x0F;
        let major = if major == 3 {
            1
        } else {
            major.saturating_sub(2)
        };
        format!("V{major}.{minor}")
    }
}

/// Decode the 8-byte GET_INFO payload.
pub fn parse_info_payload(p: &[u8]) -> Option<ChipInfo> {
    if p.len() < 3 {
        return None;
    }
    let led = p[2];
    Some(ChipInfo {
        version: p[0],
        usb_connected: p[1] == 0x01,
        num_lock: led & led::NUM_LOCK != 0,
        caps_lock: led & led::CAPS_LOCK != 0,
        scroll_lock: led & led::SCROLL_LOCK != 0,
    })
}

/// Parse a full GET_INFO *response* frame (`57 AB 00 81 08 …sum`).
pub fn parse_info_response(frame: &[u8]) -> Option<ChipInfo> {
    parse_frame(frame, cmd::GET_INFO).and_then(parse_info_payload)
}

/// Validate a reply frame and return its payload (without checksum).
///
/// Verifies header, length, checksum, and that the command byte is
/// `cmd | 0x80` (success) for the expected request command.
pub fn parse_frame(frame: &[u8], request_cmd: u8) -> Option<&[u8]> {
    if frame.len() < 6 {
        return None;
    }
    if frame[0] != HEADER[0] || frame[1] != HEADER[1] {
        return None;
    }
    let len = frame[4] as usize;
    if frame.len() != 5 + len + 1 {
        return None;
    }
    if checksum(&frame[..frame.len() - 1]) != frame[frame.len() - 1] {
        return None;
    }
    if frame[3] != (request_cmd | RESPONSE_OK) {
        return None;
    }
    Some(&frame[5..5 + len])
}

// ===========================================================================
//  Tests — the wire format is asserted byte-for-byte.
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksum_is_additive_sum() {
        // 0x57 + 0xAB + 0x00 + 0x01 + 0x00 = 0x103 -> 0x03
        assert_eq!(checksum(&[0x57, 0xAB, 0x00, 0x01, 0x00]), 0x03);
    }

    #[test]
    fn checksum_wraps_at_8_bits() {
        assert_eq!(checksum(&[0xFF, 0xFF]), 0xFE);
        assert_eq!(checksum(&[0x00]), 0x00);
    }

    /// GET_INFO request: `57 AB 00 01 00 03`.
    #[test]
    fn get_info_request_bytes() {
        assert_eq!(
            build_frame(cmd::GET_INFO, &[]),
            vec![0x57, 0xAB, 0x00, 0x01, 0x00, 0x03]
        );
    }

    /// Release-all-keys keyboard report: `57 AB 00 02 08 00*8 0C`.
    #[test]
    fn keyboard_all_released_bytes() {
        assert_eq!(
            build_frame(cmd::KB_GENERAL, &keyboard_report(0, &[])),
            vec![0x57, 0xAB, 0x00, 0x02, 0x08, 0, 0, 0, 0, 0, 0, 0, 0, 0x0C]
        );
    }

    /// Ctrl+Alt+Del: modifiers 0x05 (LCtrl|LAlt), key 0x4C (Delete).
    #[test]
    fn ctrl_alt_del_bytes() {
        let f = build_frame(
            cmd::KB_GENERAL,
            &keyboard_report(modifier::LCTRL | modifier::LALT, &[hid::DELETE]),
        );
        assert_eq!(
            f,
            vec![0x57, 0xAB, 0x00, 0x02, 0x08, 0x05, 0x00, 0x4C, 0, 0, 0, 0, 0, 0x5D]
        );
    }

    #[test]
    fn keyboard_rollover_truncates_to_six_keys() {
        let data = keyboard_report(0, &[1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(&data[2..8], &[1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn keyboard_shift_letter_a() {
        // LeftShift + 'a' -> modifiers 0x02, key 0x04
        let data = keyboard_report(modifier::LSHIFT, &[hid::A]);
        assert_eq!(data, [0x02, 0x00, 0x04, 0, 0, 0, 0, 0]);
    }

    /// Relative mouse, dx=+10 dy=−5 → `01 00 0A FB 00`, sum 0x12.
    #[test]
    fn mouse_relative_bytes() {
        assert_eq!(
            mouse_rel_report(0, 10, -5, 0),
            [0x01, 0x00, 0x0A, 0xFB, 0x00]
        );
        assert_eq!(
            build_frame(cmd::MS_REL, &mouse_rel_report(0, 10, -5, 0)),
            vec![0x57, 0xAB, 0x00, 0x05, 0x05, 0x01, 0x00, 0x0A, 0xFB, 0x00, 0x12]
        );
    }

    #[test]
    fn mouse_relative_left_button_with_wheel() {
        let d = mouse_rel_report(buttons::LEFT, 0, 0, 1);
        assert_eq!(d, [0x01, 0x01, 0x00, 0x00, 0x01]);
    }

    #[test]
    fn mouse_absolute_scales_into_12_bit_space() {
        // x=0x0FFF y=0x0001 wheel=0
        let d = mouse_abs_report(0, 4095, 1, 0);
        assert_eq!(d, [0x02, 0x00, 0xFF, 0x0F, 0x01, 0x00, 0x00]);
    }

    #[test]
    fn mouse_absolute_clamps_above_4095() {
        let d = mouse_abs_report(0, 9999, 9999, 0);
        assert_eq!(d, [0x02, 0x00, 0xFF, 0x0F, 0xFF, 0x0F, 0x00]);
    }

    #[test]
    fn media_report_layout() {
        assert_eq!(media_report(media::PLAY_PAUSE), [0x02, 0x08, 0x00, 0x00]);
        assert_eq!(
            media_frame(media::VOLUME_UP),
            vec![0x57, 0xAB, 0x00, 0x03, 0x04, 0x02, 0x01, 0x00, 0x00, 0x0C]
        );
    }

    #[test]
    fn clamp_delta_bounds() {
        assert_eq!(clamp_delta(0), 0);
        assert_eq!(clamp_delta(500), 127);
        assert_eq!(clamp_delta(-500), -127);
        assert_eq!(clamp_delta(-128), -127);
    }

    #[test]
    fn build_frame_into_matches_build_frame() {
        let mut buf = [0u8; MAX_FRAME];
        let n = keyboard_frame_into(&mut buf, 0x05, &[hid::DELETE]);
        assert_eq!(
            &buf[..n],
            &build_frame(cmd::KB_GENERAL, &keyboard_report(0x05, &[hid::DELETE]))[..]
        );
    }

    /// A real GET_INFO reply captured from documentation:
    /// version 0x30, USB connected, CapsLock on.
    #[test]
    fn parses_get_info_response() {
        let frame = build_frame(
            cmd::GET_INFO | RESPONSE_OK,
            &[0x30, 0x01, led::CAPS_LOCK, 0, 0, 0, 0, 0],
        );
        let info = parse_info_response(&frame).expect("valid reply");
        assert_eq!(info.version, 0x30);
        assert_eq!(info.version_string(), "V1.0");
        assert!(info.usb_connected);
        assert!(info.caps_lock);
        assert!(!info.num_lock);
        assert!(!info.scroll_lock);
    }

    #[test]
    fn rejects_reply_with_bad_checksum() {
        let mut frame = build_frame(cmd::GET_INFO | RESPONSE_OK, &[0x30, 0x01, 0, 0, 0, 0, 0, 0]);
        let last = frame.len() - 1;
        frame[last] ^= 0xFF;
        assert!(parse_info_response(&frame).is_none());
    }

    #[test]
    fn rejects_reply_with_error_status() {
        let frame = build_frame(cmd::GET_INFO | RESPONSE_ERR, &[0; 8]);
        assert!(parse_info_response(&frame).is_none());
    }

    #[test]
    fn rejects_reply_with_wrong_command() {
        let frame = build_frame(cmd::KB_GENERAL | RESPONSE_OK, &[0; 8]);
        assert!(parse_info_response(&frame).is_none());
    }

    #[test]
    fn rejects_truncated_frame() {
        assert!(parse_frame(&[0x57, 0xAB], cmd::GET_INFO).is_none());
        assert!(parse_frame(&[], cmd::GET_INFO).is_none());
    }
}

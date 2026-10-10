//! The **live** CH9329 link: an opened serial port plus a background sender thread.
//!
//! Responsibilities (mirrors the original `CH9329` class, but thread-safe):
//!
//! * serialise every report through **one** sender thread, so key-down/key-up
//!   frames can never overtake each other on the wire;
//! * **coalesce mouse movement** — deltas accumulate between sends and are split
//!   into ≤127-pixel steps, so a fast flick is neither dropped nor truncated;
//! * keep the HID state (held modifiers / keys / buttons) and snapshot it into
//!   each keyboard report;
//! * drain the chip's replies so its buffer never backs up.

#![allow(dead_code)]

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::kvm::ch9329::{self as p, ChipInfo, MAX_FRAME};
use crate::kvm::serial_port::SerialPort;
use crate::logging as log;

/// The CH9329's factory default baud rate.
pub const DEFAULT_BAUD: u32 = 9600;

/// Baud rates the chip supports and the UI offers.
pub const COMMON_BAUD_RATES: &[u32] = &[9600, 19200, 38400, 57600, 115200];

/// Counters shown in the Stats panel (lock-free, read from the UI thread).
#[derive(Default, Debug)]
pub struct LinkStats {
    frames_sent: AtomicU64,
    bytes_sent: AtomicU64,
    moves_sent: AtomicU64,
    moves_coalesced: AtomicU64,
    errors: AtomicU64,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct LinkStatsSnapshot {
    pub frames_sent: u64,
    pub bytes_sent: u64,
    pub moves_sent: u64,
    pub moves_coalesced: u64,
    pub errors: u64,
}

impl LinkStats {
    fn snapshot(&self) -> LinkStatsSnapshot {
        LinkStatsSnapshot {
            frames_sent: self.frames_sent.load(Ordering::Relaxed),
            bytes_sent: self.bytes_sent.load(Ordering::Relaxed),
            moves_sent: self.moves_sent.load(Ordering::Relaxed),
            moves_coalesced: self.moves_coalesced.load(Ordering::Relaxed),
            errors: self.errors.load(Ordering::Relaxed),
        }
    }
}

/// Messages handed to the sender thread.
enum Msg {
    Bytes(Vec<u8>),
    /// "Look at the movement accumulator now" — keeps mouse latency low.
    Nudge,
    Stop,
}

/// HID state owned by the link. Only the input thread mutates it in practice.
#[derive(Default)]
struct HidState {
    modifiers: u8,
    keys: Vec<u8>,
    buttons: u8,
}

/// Shared between the `Cable` and its sender thread.
struct SenderCtx {
    port: Arc<SerialPort>,
    pending: Mutex<(i32, i32)>,
    /// Held for the duration of every port write so the sender thread and a
    /// UI-initiated request/response exchange can never interleave frames.
    io: Mutex<()>,
    /// Latest mouse-button state: a relative-mouse report carries buttons as
    /// *absolute* state, so movement packets must repeat the held buttons or the
    /// chip would release them.
    buttons: std::sync::atomic::AtomicU8,
    stats: LinkStats,
    alive: AtomicBool,
}

/// An opened, streaming CH9329 link.
pub struct Cable {
    ctx: Arc<SenderCtx>,
    tx: Sender<Msg>,
    state: Mutex<HidState>,
    sender: Mutex<Option<JoinHandle<()>>>,
    info: Option<ChipInfo>,
}

impl Cable {
    /// Open the port, handshake with `GET_INFO`, then start streaming.
    pub fn open(port_name: &str, baud: u32) -> Result<Cable, String> {
        let port = Arc::new(SerialPort::open(port_name, baud)?);
        log::info(format!("serial {port_name} opened at {baud} baud"));

        // Handshake before the sender thread starts: the reply read must not race
        // with streaming writes.
        let info = query_info_on(&port);
        match &info {
            Some(i) => log::info(format!(
                "CH9329 detected on {port_name}: {} usb_connected={} leds[num={} caps={} scroll={}]",
                i.version_string(),
                i.usb_connected,
                i.num_lock,
                i.caps_lock,
                i.scroll_lock
            )),
            None => log::warn(format!(
                "{port_name} did not answer GET_INFO — not a CH9329, or the wrong baud rate"
            )),
        }

        let (tx, rx) = channel::<Msg>();
        let ctx = Arc::new(SenderCtx {
            port,
            pending: Mutex::new((0, 0)),
            io: Mutex::new(()),
            buttons: std::sync::atomic::AtomicU8::new(0),
            stats: LinkStats::default(),
            alive: AtomicBool::new(true),
        });

        let sender = {
            let ctx = Arc::clone(&ctx);
            thread::Builder::new()
                .name("ch9329-sender".into())
                .spawn(move || sender_loop(rx, ctx))
                .map_err(|e| format!("cannot start sender thread: {e}"))?
        };

        Ok(Cable {
            ctx,
            tx,
            state: Mutex::new(HidState::default()),
            sender: Mutex::new(Some(sender)),
            info,
        })
    }

    /// Status block obtained during the connect handshake.
    pub fn chip_info(&self) -> Option<ChipInfo> {
        self.info
    }

    pub fn port_name(&self) -> &str {
        self.ctx.port.name()
    }

    pub fn stats(&self) -> LinkStatsSnapshot {
        self.ctx.stats.snapshot()
    }

    pub fn is_alive(&self) -> bool {
        self.ctx.alive.load(Ordering::Relaxed)
    }

    // ---------------- keyboard ----------------

    /// Set the modifier byte and emit a keyboard report.
    pub fn set_modifiers(&self, modifiers: u8) {
        self.state.lock().unwrap().modifiers = modifiers;
        self.emit_keyboard();
    }

    /// Press a key (HID usage code) while `modifiers` are held.
    pub fn key_down(&self, hid: u8, modifiers: u8) {
        {
            let mut s = self.state.lock().unwrap();
            s.modifiers = modifiers;
            if hid != 0 && !s.keys.contains(&hid) {
                if s.keys.len() >= 6 {
                    s.keys.remove(0); // 6-key rollover
                }
                s.keys.push(hid);
            }
        }
        self.emit_keyboard();
    }

    /// Release a key.
    pub fn key_up(&self, hid: u8, modifiers: u8) {
        {
            let mut s = self.state.lock().unwrap();
            s.modifiers = modifiers;
            s.keys.retain(|&k| k != hid);
        }
        self.emit_keyboard();
    }

    /// Release every key and modifier.
    pub fn release_all_keys(&self) {
        {
            let mut s = self.state.lock().unwrap();
            s.modifiers = 0;
            s.keys.clear();
        }
        self.emit_keyboard();
    }

    fn emit_keyboard(&self) {
        let (modifiers, keys) = {
            let s = self.state.lock().unwrap();
            (s.modifiers, s.keys.clone())
        };
        let mut buf = [0u8; MAX_FRAME];
        let n = p::keyboard_frame_into(&mut buf, modifiers, &keys);
        self.send_bytes(&buf[..n]);
    }

    /// Send one multimedia report (the chip expands it to press + release).
    pub fn media_key(&self, bits: u8) {
        let frame = p::media_frame(bits);
        self.send_bytes(&frame);
    }

    /// Send **Ctrl+Alt+Del** to the *remote* machine. A local SAS cannot be
    /// injected by any user-mode app, which is exactly why the cable is used.
    pub fn send_ctrl_alt_del(&self) {
        let mods = p::modifier::LCTRL | p::modifier::LALT;
        let mut buf = [0u8; MAX_FRAME];
        let n = p::keyboard_frame_into(&mut buf, mods, &[p::hid::DELETE]);
        self.send_bytes(&buf[..n]);
        let n = p::keyboard_frame_into(&mut buf, 0, &[]);
        self.send_bytes(&buf[..n]);

        let mut s = self.state.lock().unwrap();
        s.modifiers = 0;
        s.keys.clear();
    }

    /// Type an ASCII string on the remote by pressing and releasing each key.
    ///
    /// Returns `(sent, skipped)`. Characters with no US-layout equivalent are
    /// skipped and counted, so the caller can report exactly what went out.
    pub fn type_text(&self, text: &str) -> (usize, usize) {
        use crate::kvm::keymap::us_char_to_hid;
        let (mut sent, mut skipped) = (0usize, 0usize);
        for ch in text.chars() {
            match us_char_to_hid(ch) {
                Some((hid, needs_shift)) => {
                    let mods = if needs_shift { p::modifier::LSHIFT } else { 0 };
                    self.key_down(hid, mods);
                    self.key_up(hid, mods);
                    sent += 1;
                }
                None => skipped += 1,
            }
        }
        self.release_all_keys();
        (sent, skipped)
    }

    // ---------------- mouse ----------------

    /// Accumulate a relative movement; the sender thread flushes it promptly.
    pub fn mouse_move(&self, dx: i32, dy: i32) {
        if dx == 0 && dy == 0 {
            return;
        }
        {
            let mut pend = self.ctx.pending.lock().unwrap();
            pend.0 = pend.0.saturating_add(dx);
            pend.1 = pend.1.saturating_add(dy);
            self.ctx
                .stats
                .moves_coalesced
                .fetch_add(1, Ordering::Relaxed);
        }
        let _ = self.tx.send(Msg::Nudge);
    }

    /// Set the mouse button state (bit0 left, bit1 right, bit2 middle).
    pub fn set_mouse_buttons(&self, buttons: u8) {
        self.state.lock().unwrap().buttons = buttons;
        self.ctx.buttons.store(buttons, Ordering::Relaxed);
        let mut buf = [0u8; MAX_FRAME];
        let n = p::mouse_rel_frame_into(&mut buf, buttons, 0, 0, 0);
        self.send_bytes(&buf[..n]);
    }

    /// Current mouse button state.
    pub fn mouse_buttons(&self) -> u8 {
        self.state.lock().unwrap().buttons
    }

    /// One or more wheel notches (positive = away from the user).
    pub fn mouse_wheel(&self, teeth: i32) {
        if teeth == 0 {
            return;
        }
        let mut remaining = teeth;
        let buttons = self.mouse_buttons();
        let mut buf = [0u8; MAX_FRAME];
        while remaining != 0 {
            let step = remaining.clamp(-127, 127) as i8;
            remaining -= step as i32;
            let n = p::mouse_rel_frame_into(&mut buf, buttons, 0, 0, step);
            self.send_bytes(&buf[..n]);
        }
    }

    /// Release every mouse button.
    pub fn release_all_buttons(&self) {
        self.set_mouse_buttons(0);
    }

    /// Safety net: release everything the remote could still think is held.
    pub fn release_all(&self) {
        self.release_all_keys();
        self.release_all_buttons();
    }

    // ---------------- chip queries ----------------

    /// Ask the chip for the 44-byte configuration blob (cmd `0x08`).
    pub fn read_config_raw(&self) -> Option<Vec<u8>> {
        self.round_trip(p::cmd::GET_PARA_CFG, 500)
    }

    /// Send GET_INFO and decode it (works while streaming — the write is
    /// serialised against the sender thread by the `io` lock).
    pub fn query_info(&self) -> Option<ChipInfo> {
        self.round_trip(p::cmd::GET_INFO, 500)
            .and_then(|payload| p::parse_info_payload(&payload))
    }

    fn round_trip(&self, cmd: u8, timeout_ms: u32) -> Option<Vec<u8>> {
        let frame = p::build_frame(cmd, &[]);
        let _io = self.ctx.io.lock().unwrap();
        if self.ctx.port.write_all(&frame).is_err() {
            return None;
        }
        let reply = read_frame(&self.ctx.port, timeout_ms)?;
        p::parse_frame(&reply, cmd).map(|payload| payload.to_vec())
    }

    // ---------------- plumbing ----------------

    fn send_bytes(&self, bytes: &[u8]) {
        if self.tx.send(Msg::Bytes(bytes.to_vec())).is_err() {
            log::debug("sender thread is gone; dropping report");
        }
    }

    /// Stop the sender thread and close the port.
    pub fn close(&self) {
        let _ = self.tx.send(Msg::Stop);
        self.ctx.alive.store(false, Ordering::Relaxed);
        if let Some(h) = self.sender.lock().unwrap().take() {
            let _ = h.join();
        }
    }
}

impl Drop for Cable {
    fn drop(&mut self) {
        let _ = self.tx.send(Msg::Stop);
        self.ctx.alive.store(false, Ordering::Relaxed);
        if let Some(h) = self.sender.lock().unwrap().take() {
            let _ = h.join();
        }
    }
}

/// The sender thread: writes queued frames, then drains accumulated movement.
fn sender_loop(rx: Receiver<Msg>, ctx: Arc<SenderCtx>) {
    let mut buf = [0u8; MAX_FRAME];
    loop {
        match rx.recv_timeout(Duration::from_millis(25)) {
            Ok(Msg::Stop) => break,
            Ok(Msg::Bytes(bytes)) => {
                write_all(&ctx, &bytes);
                ctx.stats.frames_sent.fetch_add(1, Ordering::Relaxed);
            }
            Ok(Msg::Nudge) => {}
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }

        // Drain whatever movement accumulated while we were blocked.
        drain_movement(&ctx, &mut buf);

        // Drop the chip's replies once nothing else is queued, so its buffer
        // cannot fill up during a long streaming session.
        if ctx.port.bytes_available() > 64 {
            ctx.port.purge_input();
        }
    }
    ctx.alive.store(false, Ordering::Relaxed);
}

/// Flush the movement accumulator in ≤127-pixel steps.
fn drain_movement(ctx: &Arc<SenderCtx>, buf: &mut [u8; MAX_FRAME]) {
    // The relative-mouse report carries buttons as absolute state, so movement
    // packets must repeat whatever is currently held.
    let buttons = ctx.buttons.load(Ordering::Relaxed);
    loop {
        let (dx, dy) = {
            let mut pend = ctx.pending.lock().unwrap();
            let dx = pend.0.clamp(-127, 127);
            let dy = pend.1.clamp(-127, 127);
            pend.0 -= dx;
            pend.1 -= dy;
            (dx, dy)
        };
        if dx == 0 && dy == 0 {
            return;
        }
        let n = p::mouse_rel_frame_into(buf, buttons, dx as i8, dy as i8, 0);
        write_all(ctx, &buf[..n]);
        ctx.stats.moves_sent.fetch_add(1, Ordering::Relaxed);
    }
}

fn write_all(ctx: &Arc<SenderCtx>, bytes: &[u8]) {
    let _io = ctx.io.lock().unwrap();
    match ctx.port.write_all(bytes) {
        Ok(n) => {
            ctx.stats.bytes_sent.fetch_add(n as u64, Ordering::Relaxed);
        }
        Err(e) => {
            // Cable unplugged or port closed mid-session: note it and stop.
            ctx.stats.errors.fetch_add(1, Ordering::Relaxed);
            log::debug(format!("serial write failed: {e}"));
            ctx.alive.store(false, Ordering::Relaxed);
        }
    }
}

/// Read one complete reply frame: 5 header bytes (to learn LEN), then LEN+1.
fn read_frame(port: &SerialPort, timeout_ms: u32) -> Option<Vec<u8>> {
    let mut head = [0u8; 5];
    if !port.read_exact_timeout(&mut head, timeout_ms).ok()? {
        return None;
    }
    if head[0] != p::HEADER[0] || head[1] != p::HEADER[1] {
        log::debug(format!("reply header mismatch: {head:02X?}"));
        return None;
    }
    let len = head[4] as usize;
    let mut rest = vec![0u8; len + 1]; // payload + checksum
    if !port.read_exact_timeout(&mut rest, timeout_ms).ok()? {
        return None;
    }
    let mut frame = head.to_vec();
    frame.extend_from_slice(&rest);
    Some(frame)
}

fn query_info_on(port: &SerialPort) -> Option<ChipInfo> {
    let frame = p::build_frame(p::cmd::GET_INFO, &[]);
    port.write_all(&frame).ok()?;
    let reply = read_frame(port, 500)?;
    p::parse_info_response(&reply)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opening_a_nonexistent_port_fails_gracefully() {
        assert!(Cable::open("COM999", DEFAULT_BAUD).is_err());
    }

    #[test]
    fn baud_rate_list_is_sane() {
        assert!(COMMON_BAUD_RATES.contains(&DEFAULT_BAUD));
        assert!(COMMON_BAUD_RATES.windows(2).all(|w| w[0] < w[1]));
    }
}

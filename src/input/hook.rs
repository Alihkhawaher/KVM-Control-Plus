//! Input capture: a **dedicated thread** with its own hidden window and message
//! pump, independent of the GUI's event loop.
//!
//! The original C# app relied on the WinUI window's message pump for its hooks and
//! its `WM_INPUT` subclass. Here the input thread owns everything, so the GUI can
//! block, repaint, or free its own window without ever stalling key delivery.
//!
//! # What it does
//!
//! | source                    | purpose                                            |
//! |---------------------------|----------------------------------------------------|
//! | `WH_KEYBOARD_LL`          | reads keys *and* swallows them (they go remote)     |
//! | `WH_MOUSE_LL`             | freezes the local cursor, detects the leave gesture |
//! | Raw Input (`WM_INPUT`)    | true relative movement, unbounded by screen edges   |
//!
//! Nothing is recorded or stored — every event is translated to a HID report and
//! handed straight to the CH9329 link.

#![allow(dead_code)]

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, OnceLock};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    VK_BACK, VK_CONTROL, VK_LMENU, VK_LSHIFT, VK_RMENU, VK_RSHIFT,
};
use windows::Win32::UI::Input::{
    GetRawInputData, RegisterRawInputDevices, HRAWINPUT, RAWINPUTDEVICE, RAWINPUTHEADER, RAWMOUSE,
    RIDEV_INPUTSINK, RID_INPUT, RIM_TYPEMOUSE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, PostMessageW,
    RegisterClassW, SetWindowsHookExW, TranslateMessage, UnhookWindowsHookEx, KBDLLHOOKSTRUCT,
    LLKHF_UP, MSG, RI_MOUSE_LEFT_BUTTON_DOWN, RI_MOUSE_LEFT_BUTTON_UP, RI_MOUSE_MIDDLE_BUTTON_DOWN,
    RI_MOUSE_MIDDLE_BUTTON_UP, RI_MOUSE_RIGHT_BUTTON_DOWN, RI_MOUSE_RIGHT_BUTTON_UP,
    RI_MOUSE_WHEEL, WH_KEYBOARD_LL, WH_MOUSE_LL, WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP, WM_INPUT,
    WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN, WM_MBUTTONDOWN, WM_MOUSEWHEEL, WM_RBUTTONDOWN,
    WM_SYSKEYDOWN, WM_SYSKEYUP, WNDCLASSW,
};

use crate::kvm::cable::Cable;
use crate::kvm::ch9329::{buttons, hid, modifier};
use crate::kvm::keymap::{is_modifier, vk_to_hid, vk_to_media, vk_to_modifier};
use crate::kvm::layout;
use crate::logging as log;

use super::leave::LeaveGesture;

/// Events the input thread reports back to the GUI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputEvent {
    /// The user performed the leave gesture / panic hotkey.
    LeaveRequested,
    /// A key was pressed while capture was off (used for the "activity" read-out).
    DroppedWhileInactive,
    /// The input thread hit a fatal error and stopped.
    Failed(String),
}

/// Everything shared between the hook callbacks and the owner.
struct Shared {
    cable: std::sync::Mutex<Option<Arc<Cable>>>,
    remote: AtomicBool,
    layout_aware: AtomicBool,
    mouse_speed_bits: AtomicU32,
    invert_scroll: AtomicBool,
    swap_buttons: AtomicBool,
    invert_y: AtomicBool,
    panic_hotkey: AtomicBool,
    leave_gesture: AtomicU8,
    /// Active keyboard layout, refreshed when the app gains focus.
    klid: std::sync::Mutex<layout::LayoutInfo>,
    /// Modifier byte currently held (kept in sync with the cable's report).
    modifiers: AtomicU8,
    /// Buttons currently held.
    buttons: AtomicU8,
    events: Sender<InputEvent>,
}

// One input thread at a time, reachable from the `extern "system"` hook procs.
static SHARED: OnceLock<Arc<Shared>> = OnceLock::new();

/// Handle to the input thread.
pub struct InputThread {
    shared: Arc<Shared>,
    hwnd: isize,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl InputThread {
    /// Start the input thread: it creates its window, installs the hooks, and
    /// runs a message pump until [`InputThread::stop`].
    pub fn start(events: Sender<InputEvent>, gesture: LeaveGesture) -> Result<InputThread, String> {
        let shared = Arc::new(Shared {
            cable: std::sync::Mutex::new(None),
            remote: AtomicBool::new(false),
            layout_aware: AtomicBool::new(true),
            mouse_speed_bits: AtomicU32::new(1.0f32.to_bits()),
            invert_scroll: AtomicBool::new(false),
            swap_buttons: AtomicBool::new(false),
            invert_y: AtomicBool::new(false),
            panic_hotkey: AtomicBool::new(true),
            leave_gesture: AtomicU8::new(gesture as u8),
            klid: std::sync::Mutex::new(layout::current()),
            modifiers: AtomicU8::new(0),
            buttons: AtomicU8::new(0),
            events,
        });

        if SHARED.get().is_some() {
            return Err("an input thread is already running in this process".into());
        }
        let _ = SHARED.set(Arc::clone(&shared));

        // The window handle is published back so `stop()` can post WM_QUIT.
        let (tx, rx) = std::sync::mpsc::channel::<Result<isize, String>>();
        let shared_thread = Arc::clone(&shared);
        let thread = std::thread::Builder::new()
            .name("input-capture".into())
            .spawn(move || match thread_main(Arc::clone(&shared_thread)) {
                Ok(hwnd) => {
                    let _ = tx.send(Ok(hwnd.0 as isize));
                    message_loop(hwnd, &shared_thread);
                }
                Err(e) => {
                    let _ = tx.send(Err(e.clone()));
                    let _ = shared_thread.events.send(InputEvent::Failed(e));
                }
            })
            .map_err(|e| format!("cannot start input thread: {e}"))?;

        let hwnd = rx
            .recv()
            .map_err(|_| "input thread died before reporting its window".to_string())??;

        Ok(InputThread {
            shared,
            hwnd,
            thread: Some(thread),
        })
    }

    /// Attach (or detach) the CH9329 link that receives the captured input.
    pub fn set_cable(&self, cable: Option<Arc<Cable>>) {
        *self.shared.cable.lock().unwrap() = cable;
    }

    /// Enable/disable capture. Disabling always releases everything on the remote.
    pub fn set_capture(&self, on: bool) {
        let was = self.shared.remote.swap(on, Ordering::SeqCst);
        if was != on {
            log::info(if on {
                "input capture ON"
            } else {
                "input capture OFF"
            });
        }
        if !on {
            self.release_all();
        }
    }

    pub fn is_capturing(&self) -> bool {
        self.shared.remote.load(Ordering::SeqCst)
    }

    pub fn set_layout_aware(&self, on: bool) {
        self.shared.layout_aware.store(on, Ordering::Relaxed);
    }

    pub fn set_mouse_speed(&self, speed: f32) {
        self.shared
            .mouse_speed_bits
            .store(speed.clamp(0.1, 10.0).to_bits(), Ordering::Relaxed);
    }

    pub fn set_invert_scroll(&self, on: bool) {
        self.shared.invert_scroll.store(on, Ordering::Relaxed);
    }

    pub fn set_swap_buttons(&self, on: bool) {
        self.shared.swap_buttons.store(on, Ordering::Relaxed);
    }

    pub fn set_invert_y(&self, on: bool) {
        self.shared.invert_y.store(on, Ordering::Relaxed);
    }

    pub fn set_panic_hotkey(&self, on: bool) {
        self.shared.panic_hotkey.store(on, Ordering::Relaxed);
    }

    pub fn set_leave_gesture(&self, g: LeaveGesture) {
        self.shared.leave_gesture.store(g as u8, Ordering::Relaxed);
    }

    /// Re-read the active keyboard layout (call when the app regains focus).
    pub fn refresh_layout(&self) {
        *self.shared.klid.lock().unwrap() = layout::current();
    }

    /// Release every key and button on the remote.
    pub fn release_all(&self) {
        self.shared.modifiers.store(0, Ordering::Relaxed);
        self.shared.buttons.store(0, Ordering::Relaxed);
        if let Some(c) = self.shared.cable.lock().unwrap().as_ref() {
            c.release_all();
        }
    }

    /// Stop the thread and uninstall the hooks.
    pub fn stop(&mut self) {
        unsafe {
            let hwnd = HWND(self.hwnd as *mut _);
            let _ = PostMessageW(Some(hwnd), WM_APP + 1, WPARAM(0), LPARAM(0));
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for InputThread {
    fn drop(&mut self) {
        self.stop();
    }
}

// ---------------------------------------------------------------------------
//  Thread body
// ---------------------------------------------------------------------------

fn thread_main(shared: Arc<Shared>) -> Result<HWND, String> {
    unsafe {
        let hinstance = GetModuleHandleW(None).map_err(|e| format!("GetModuleHandleW: {e}"))?;

        let class_name = w!("KVMControlPlusInputSink");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wnd_proc),
            hInstance: hinstance.into(),
            lpszClassName: class_name,
            ..Default::default()
        };
        // A zero return means "already registered", which is fine.
        RegisterClassW(&wc);

        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class_name,
            w!("KVM-Control-Plus input sink"),
            WINDOW_STYLE(0), // invisible: never shown, only a message target
            0,
            0,
            0,
            0,
            None,
            None,
            Some(hinstance.into()),
            None,
        )
        .map_err(|e| format!("CreateWindowExW: {e}"))?;

        // Register for raw mouse input delivered to this window.
        let devices = [RAWINPUTDEVICE {
            usUsagePage: 0x01, // generic desktop
            usUsage: 0x02,     // mouse
            dwFlags: RIDEV_INPUTSINK,
            hwndTarget: hwnd,
        }];
        RegisterRawInputDevices(&devices, std::mem::size_of::<RAWINPUTDEVICE>() as u32)
            .map_err(|e| format!("RegisterRawInputDevices: {e}"))?;

        // Global low-level hooks; the callbacks run on this thread's pump.
        let kb = SetWindowsHookExW(
            WH_KEYBOARD_LL,
            Some(keyboard_proc),
            Some(hinstance.into()),
            0,
        )
        .map_err(|e| format!("SetWindowsHookExW(WH_KEYBOARD_LL): {e}"))?;
        let ms = SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), Some(hinstance.into()), 0)
            .map_err(|e| format!("SetWindowsHookExW(WH_MOUSE_LL): {e}"))?;

        let _ = HOOKS.set(Some((kb.0 as isize, ms.0 as isize)));
        log::info("input hooks installed (keyboard + mouse + raw input)");
        let _ = &shared;
        Ok(hwnd)
    }
}

static HOOKS: OnceLock<Option<(isize, isize)>> = OnceLock::new();

fn message_loop(hwnd: HWND, shared: &Arc<Shared>) {
    unsafe {
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            if msg.message == WM_APP + 1 {
                break; // shutdown request
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        let _ = hwnd;

        // Uninstall hooks so the OS stops calling into this thread.
        if let Some(Some((kb, ms))) = HOOKS.get().copied() {
            let _ =
                UnhookWindowsHookEx(windows::Win32::UI::WindowsAndMessaging::HHOOK(kb as *mut _));
            let _ =
                UnhookWindowsHookEx(windows::Win32::UI::WindowsAndMessaging::HHOOK(ms as *mut _));
        }
        shared.remote.store(false, Ordering::SeqCst);
        if let Some(c) = shared.cable.lock().unwrap().as_ref() {
            c.release_all();
        }
        log::info("input thread stopped");
    }
}

extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if msg == WM_INPUT {
        if let Some(shared) = SHARED.get() {
            if shared.remote.load(Ordering::Relaxed) {
                handle_raw_input(HRAWINPUT(lparam.0 as *mut _), shared);
            }
        }
        return LRESULT(0);
    }
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

// ---------------------------------------------------------------------------
//  Keyboard hook
// ---------------------------------------------------------------------------

extern "system" fn keyboard_proc(ncode: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if ncode < 0 {
        return unsafe { CallNextHookEx(None, ncode, wparam, lparam) };
    }
    let Some(shared) = SHARED.get() else {
        return unsafe { CallNextHookEx(None, ncode, wparam, lparam) };
    };
    if !shared.remote.load(Ordering::Relaxed) {
        return unsafe { CallNextHookEx(None, ncode, wparam, lparam) };
    }

    let info = unsafe { *(lparam.0 as *const KBDLLHOOKSTRUCT) };
    let is_up = info.flags.contains(LLKHF_UP);
    let vk_code = info.vkCode;
    let scancode = info.scanCode;

    let mods = shared.modifiers.load(Ordering::Relaxed);

    // ---- panic hotkey: Ctrl+Shift+Alt+Backspace ends capture immediately ----
    if shared.panic_hotkey.load(Ordering::Relaxed)
        && !is_up
        && vk_code == VK_BACK.0 as u32
        && mods & modifier::LCTRL != 0
        && mods & modifier::LALT != 0
        && mods & modifier::LSHIFT != 0
    {
        let _ = shared.events.send(InputEvent::LeaveRequested);
        return LRESULT(1);
    }

    let Some(cable) = shared.cable.lock().unwrap().clone() else {
        let _ = shared.events.send(InputEvent::DroppedWhileInactive);
        return LRESULT(1);
    };

    // ---- modifiers ----
    if let Some(bit) = vk_to_modifier(vk_code) {
        let new = if is_up { mods & !bit } else { mods | bit };
        shared.modifiers.store(new, Ordering::Relaxed);
        cable.set_modifiers(new);
        return LRESULT(1);
    }

    // ---- multimedia keys ----
    if let Some(bits) = vk_to_media(vk_code) {
        if !is_up {
            cable.media_key(bits);
        }
        return LRESULT(1);
    }

    // ---- ordinary keys ----
    let shift_down = mods & (modifier::LSHIFT | modifier::RSHIFT) != 0;
    let altgr_down = mods & modifier::RALT != 0;

    let (hid_code, report_mods) =
        resolve_key(shared, vk_code, scancode, mods, shift_down, altgr_down);

    if let Some(code) = hid_code {
        if is_up {
            cable.key_up(code, report_mods);
        } else {
            cable.key_down(code, report_mods);
        }
    }
    LRESULT(1) // swallow: the keystroke goes to the remote only
}

/// Turn a virtual key into `(HID code, modifier byte for the report)`.
fn resolve_key(
    shared: &Arc<Shared>,
    vk_code: u32,
    scancode: u32,
    mods: u8,
    shift_down: bool,
    altgr_down: bool,
) -> (Option<u8>, u8) {
    // Modifiers that must accompany the key: everything held except Shift (which
    // is decided by the resolved character) and except the AltGr pair.
    let mut base = mods & !(modifier::LSHIFT | modifier::RSHIFT);
    if altgr_down {
        // AltGr is a local input method, never a modifier to forward.
        base &= !(modifier::RCTRL | modifier::RALT);
    }

    if shared.layout_aware.load(Ordering::Relaxed) {
        let klid = shared.klid.lock().unwrap().clone();
        if let Some(f) = layout::resolve(klid.hkl, vk_code, scancode, shift_down, altgr_down) {
            let mut out = base;
            if f.from_altgr {
                out &= !(modifier::LCTRL | modifier::RCTRL | modifier::LALT | modifier::RALT);
            }
            if f.needs_shift {
                out |= modifier::LSHIFT;
            }
            return (Some(f.hid), out);
        }
    }

    // Fall back to the direct position map (function keys, arrows, media, and
    // characters with no US equivalent).
    if let Some(code) = vk_to_hid(vk_code) {
        let mut out = base;
        if shift_down {
            out |= modifier::LSHIFT;
        }
        return (Some(code), out);
    }
    (None, base)
}

// ---------------------------------------------------------------------------
//  Mouse hook (freeze local cursor + leave gesture)
// ---------------------------------------------------------------------------

extern "system" fn mouse_proc(ncode: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if ncode < 0 {
        return unsafe { CallNextHookEx(None, ncode, wparam, lparam) };
    }
    let Some(shared) = SHARED.get() else {
        return unsafe { CallNextHookEx(None, ncode, wparam, lparam) };
    };
    if !shared.remote.load(Ordering::Relaxed) {
        return unsafe { CallNextHookEx(None, ncode, wparam, lparam) };
    }

    let gesture = LeaveGesture::from_u8(shared.leave_gesture.load(Ordering::Relaxed));
    if wparam.0 as u32 == WM_MBUTTONDOWN && gesture == LeaveGesture::Middle {
        let _ = shared.events.send(InputEvent::LeaveRequested);
    }
    LRESULT(1) // freeze the local cursor; movement arrives via raw input
}

// ---------------------------------------------------------------------------
//  Raw input (true relative movement + buttons + wheel)
// ---------------------------------------------------------------------------

fn handle_raw_input(hraw: HRAWINPUT, shared: &Arc<Shared>) {
    unsafe {
        let header_size = std::mem::size_of::<RAWINPUTHEADER>() as u32;
        let mut size = 0u32;
        if GetRawInputData(hraw, RID_INPUT, None, &mut size, header_size) != 0 || size == 0 {
            return;
        }
        let mut buf = vec![0u8; size as usize];
        if GetRawInputData(
            hraw,
            RID_INPUT,
            Some(buf.as_mut_ptr() as *mut _),
            &mut size,
            header_size,
        ) != size
        {
            return;
        }

        let header = &*(buf.as_ptr() as *const RAWINPUTHEADER);
        if header.dwType != RIM_TYPEMOUSE.0 {
            return;
        }
        let m = &*(buf.as_ptr().add(header_size as usize) as *const RAWMOUSE);

        let Some(cable) = shared.cable.lock().unwrap().clone() else {
            return;
        };

        // ---- movement (relative only: absolute devices report jumps) ----
        if m.lLastX != 0 || m.lLastY != 0 {
            let speed = f32::from_bits(shared.mouse_speed_bits.load(Ordering::Relaxed));
            let mut dx = (m.lLastX as f32 * speed).round() as i32;
            let mut dy = (m.lLastY as f32 * speed).round() as i32;
            if shared.invert_y.load(Ordering::Relaxed) {
                dy = -dy;
            }
            if shared.swap_buttons.load(Ordering::Relaxed) {
                // (pointer handedness is a button concern only; movement unchanged)
            }
            let _ = &mut dx;
            cable.mouse_move(dx, dy);
        }

        // ---- buttons ----
        // RAWMOUSE carries its button flags in an anonymous union.
        let bf = m.Anonymous.Anonymous.usButtonFlags as u32;
        if bf != 0 {
            let swap = shared.swap_buttons.load(Ordering::Relaxed);
            let (mut left_bit, mut right_bit) = (buttons::LEFT, buttons::RIGHT);
            if swap {
                std::mem::swap(&mut left_bit, &mut right_bit);
            }
            let mut state = shared.buttons.load(Ordering::Relaxed);
            let mut changed = false;

            if bf & RI_MOUSE_LEFT_BUTTON_DOWN != 0 {
                state |= left_bit;
                changed = true;
            }
            if bf & RI_MOUSE_LEFT_BUTTON_UP != 0 {
                state &= !left_bit;
                changed = true;
            }
            if bf & RI_MOUSE_RIGHT_BUTTON_DOWN != 0 {
                state |= right_bit;
                changed = true;
            }
            if bf & RI_MOUSE_RIGHT_BUTTON_UP != 0 {
                state &= !right_bit;
                changed = true;
            }
            // Middle is reserved for the leave gesture, but we still track it so a
            // held middle button cannot get stuck on the remote.
            if bf & RI_MOUSE_MIDDLE_BUTTON_DOWN != 0 || bf & RI_MOUSE_MIDDLE_BUTTON_UP != 0 {
                changed = true;
            }

            if changed {
                shared.buttons.store(state, Ordering::Relaxed);
                cable.set_mouse_buttons(state);
            }

            // ---- wheel ----
            if bf & RI_MOUSE_WHEEL != 0 {
                let mut notches = m.Anonymous.Anonymous.usButtonData as i16 as i32 / 120;
                if shared.invert_scroll.load(Ordering::Relaxed) {
                    notches = -notches;
                }
                cable.mouse_wheel(notches);
            }
        }
    }
}

// ---------------------------------------------------------------------------
//  Unused-but-referenced Win32 symbols (keeps the feature set honest)
// ---------------------------------------------------------------------------

#[allow(dead_code)]
fn _referenced() {
    let _ = (
        VK_CONTROL,
        VK_LMENU,
        VK_RMENU,
        VK_LSHIFT,
        VK_RSHIFT,
        PCWSTR::null(),
        WM_KEYDOWN,
        WM_KEYUP,
        WM_SYSKEYDOWN,
        WM_SYSKEYUP,
        WM_LBUTTONDOWN,
        WM_RBUTTONDOWN,
        WM_MOUSEWHEEL,
        hid::A,
        is_modifier,
    );
}

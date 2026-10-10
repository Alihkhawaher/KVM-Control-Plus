//! KVM domain: the CH9329 protocol, key translation, and the serial link.
//!
//! | module          | responsibility                                        |
//! |-----------------|-------------------------------------------------------|
//! | [`ch9329`]      | pure protocol: frames, reports, reply parsing (+tests) |
//! | [`keymap`]      | VK↔HID translation, media keys, US character table     |
//! | [`layout`]      | local keyboard-layout detection → US-HID forwarding    |
//! | [`serial_scan`] | SetupAPI enumeration (port / friendly name / hw id)    |
//! | [`serial_port`] | Win32 COM-port handle: open, read, write, purge        |
//! | [`cable`]       | live CH9329 link: sender thread, HID state, coalescing |

pub mod ch9329;
pub mod keymap;
pub mod layout;
pub mod serial_port;
pub mod serial_scan;

pub mod cable;

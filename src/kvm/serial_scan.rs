//! Serial-port enumeration via the Windows **SetupAPI** — no WMI, no
//! `serialport` crate.
//!
//! `serialport`'s own `available_ports()` only yields the bare `COMx` name on
//! Windows. Studio-quality results need the *friendly name* ("USB-SERIAL CH340
//! (COM5)") and the *hardware id* (`USB\VID_1A86&PID_7523…`) so the app can
//! recognise a CH9329 KVM cable automatically. SetupAPI gives all three,
//! exactly like the original C# project did after it dropped `System.Management`.

#![allow(dead_code)]

use windows::core::{BOOL, PCWSTR};
use windows::Win32::Devices::DeviceAndDriverInstallation::*;
use windows::Win32::System::Registry::{RegCloseKey, RegQueryValueExW, KEY_READ, REG_VALUE_TYPE};

/// One serial port as reported by the Plug-and-Play manager.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SerialPortInfo {
    /// Bare port name, e.g. `"COM5"`.
    pub port: String,
    /// Human-friendly device name, e.g. `"USB-SERIAL CH340 (COM5)"`.
    pub friendly_name: String,
    /// PnP hardware id, e.g. `"USB\VID_1A86&PID_7523&REV_0264"`.
    pub hardware_id: String,
    /// True when the hardware id matches a known CH9329 KVM cable bridge.
    pub is_kvm_cable: bool,
}

impl SerialPortInfo {
    /// Display string for the combo box.
    pub fn label(&self) -> String {
        if self.friendly_name.is_empty() {
            self.port.clone()
        } else {
            self.friendly_name.clone()
        }
    }
}

/// Hardware-id fragments of USB→serial bridges commonly used on CH9329 KVM cables.
///
/// * `VID_1A86&PID_7523` — CH340 (the classic "CH9329/CH340 KVM USB cable").
/// * `VID_1A86&PID_5523` — CH341.
/// * `VID_1A86&PID_55D3` — CH9102 (used by some NanoKVM-style cables).
pub const KVM_CABLE_VID_PIDS: &[&str] = &[
    "VID_1A86&PID_7523",
    "VID_1A86&PID_5523",
    "VID_1A86&PID_55D3",
];

/// True when a hardware id belongs to a known KVM-cable serial bridge.
pub fn is_known_kvm_hardware_id(hardware_id: &str) -> bool {
    let up = hardware_id.to_ascii_uppercase();
    KVM_CABLE_VID_PIDS.iter().any(|frag| up.contains(frag))
}

/// Enumerate every present device in the **Ports (COM & LPT)** class.
pub fn list() -> Vec<SerialPortInfo> {
    let mut out = Vec::new();

    unsafe {
        let set = match SetupDiGetClassDevsW(
            Some(&GUID_DEVCLASS_PORTS),
            PCWSTR::null(),
            None,
            DIGCF_PRESENT,
        ) {
            Ok(h) => h,
            Err(_) => return out,
        };

        let mut data = SP_DEVINFO_DATA {
            cbSize: std::mem::size_of::<SP_DEVINFO_DATA>() as u32,
            ..Default::default()
        };

        let mut i = 0u32;
        while SetupDiEnumDeviceInfo(set, i, &mut data).is_ok() {
            i += 1;

            let port = read_port_name(set, &data).unwrap_or_default();
            if port.is_empty() {
                // A device in the Ports class without a PortName is not usable.
                continue;
            }
            let friendly_name = read_property_string(set, &data, SPDRP_FRIENDLYNAME);
            let hardware_id = read_property_string(set, &data, SPDRP_HARDWAREID);
            let is_kvm_cable = is_known_kvm_hardware_id(&hardware_id);

            out.push(SerialPortInfo {
                port,
                friendly_name,
                hardware_id,
                is_kvm_cable,
            });
        }

        let _ = SetupDiDestroyDeviceInfoList(set);
    }

    // Stable order: KVM cables first, then by numeric COM index.
    out.sort_by_key(|p| {
        (
            !p.is_kvm_cable,
            p.port
                .trim_start_matches("COM")
                .parse::<u32>()
                .unwrap_or(u32::MAX),
        )
    });
    out
}

/// The first port that looks like a CH9329 KVM cable, if one is present.
pub fn find_kvm_port() -> Option<SerialPortInfo> {
    list().into_iter().find(|p| p.is_kvm_cable)
}

impl std::fmt::Display for SerialPortInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.label())
    }
}

// ---------------------------------------------------------------------------
//  SetupAPI helpers
// ---------------------------------------------------------------------------

/// Read the `PortName` value out of the device's hardware registry key.
unsafe fn read_port_name(set: HDEVINFO, data: &SP_DEVINFO_DATA) -> Option<String> {
    let key = SetupDiOpenDevRegKey(set, data, DICS_FLAG_GLOBAL, 0, DIREG_DEV, KEY_READ.0).ok()?;

    let name: Vec<u16> = "PortName\0".encode_utf16().collect();
    let mut buf = [0u16; 64];
    let mut size = (buf.len() * 2) as u32;
    let mut ty = REG_VALUE_TYPE::default();

    let status = RegQueryValueExW(
        key,
        PCWSTR(name.as_ptr()),
        None,
        Some(&mut ty),
        Some(buf.as_mut_ptr() as *mut u8),
        Some(&mut size),
    );
    let _ = RegCloseKey(key);

    if status.is_err() {
        return None;
    }
    let n = (size as usize / 2).min(buf.len());
    let s = String::from_utf16_lossy(&buf[..n]);
    Some(s.trim_end_matches('\0').to_string())
}

/// Read a `REG_SZ` / `REG_MULTI_SZ` registry property as a `String`.
unsafe fn read_property_string(
    set: HDEVINFO,
    data: &SP_DEVINFO_DATA,
    property: SETUP_DI_REGISTRY_PROPERTY,
) -> String {
    // Ask for the required size first (the call "fails" with the size filled in).
    let mut required = 0u32;
    let _ = SetupDiGetDeviceRegistryPropertyW(set, data, property, None, None, Some(&mut required));
    if required == 0 {
        return String::new();
    }

    let mut buf = vec![0u8; required as usize];
    if SetupDiGetDeviceRegistryPropertyW(set, data, property, None, Some(&mut buf), None).is_err() {
        return String::new();
    }

    // Buffer holds UTF-16LE; stop at the first NUL (drops the MULTI_SZ tail).
    let u16s: Vec<u16> = buf
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .take_while(|&c| c != 0)
        .collect();
    String::from_utf16_lossy(&u16s)
}

// ---------------------------------------------------------------------------
//  SetupAPI constants not re-exported by the `windows` crate
// ---------------------------------------------------------------------------

const DICS_FLAG_GLOBAL: u32 = 0x0000_0001;
const DIREG_DEV: u32 = 0x0000_0001;

#[allow(dead_code)]
fn _assert_bool_unused(_b: BOOL) {}

// ===========================================================================
//  Tests (pure helpers only — enumeration needs real hardware)
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_ch340_kvm_cable() {
        assert!(is_known_kvm_hardware_id("USB\\VID_1A86&PID_7523&REV_0264"));
        assert!(is_known_kvm_hardware_id("usb\\vid_1a86&pid_7523"));
    }

    #[test]
    fn rejects_unrelated_hardware() {
        assert!(!is_known_kvm_hardware_id("USB\\VID_046D&PID_C52B")); // Logitech
        assert!(!is_known_kvm_hardware_id("ACPI\\PNP0501")); // motherboard COM
        assert!(!is_known_kvm_hardware_id(""));
    }

    #[test]
    fn sorts_kvm_cables_first_then_numeric_com_index() {
        let mut v = vec![
            SerialPortInfo {
                port: "COM10".into(),
                friendly_name: String::new(),
                hardware_id: String::new(),
                is_kvm_cable: false,
            },
            SerialPortInfo {
                port: "COM3".into(),
                friendly_name: String::new(),
                hardware_id: String::new(),
                is_kvm_cable: true,
            },
            SerialPortInfo {
                port: "COM2".into(),
                friendly_name: String::new(),
                hardware_id: String::new(),
                is_kvm_cable: false,
            },
        ];
        v.sort_by_key(|p| {
            (
                !p.is_kvm_cable,
                p.port
                    .trim_start_matches("COM")
                    .parse::<u32>()
                    .unwrap_or(u32::MAX),
            )
        });
        assert_eq!(v[0].port, "COM3"); // KVM cable first despite higher number
        assert_eq!(v[1].port, "COM2"); // then numeric order, not lexicographic
        assert_eq!(v[2].port, "COM10");
    }
}

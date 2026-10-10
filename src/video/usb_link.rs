//! Measure the **real** USB link speed of the capture device.
//!
//! Why this exists: on an MS2130-class dongle, which formats are actually usable
//! depends entirely on the link. Uncompressed 1080p50 needs ~155-207 MB/s, USB
//! 2.0 practical throughput is ~40 MB/s, so on a 2.0 link the uncompressed
//! stream silently delivers filler frames and the picture is black no matter
//! what the source is doing. Guessing that from a format list is possible but
//! fragile, so this asks the USB hub itself and reports what it says.
//!
//! Method: enumerate every USB hub (SetupAPI device interfaces for
//! `GUID_DEVINTERFACE_USB_HUB`), ask each hub for its port count, then walk the
//! ports with `IOCTL_USB_GET_NODE_CONNECTION_INFORMATION_EX` until one reports a
//! device descriptor matching our VID/PID. That port's `Speed` byte is the
//! negotiated speed. `IOCTL_USB_GET_NODE_CONNECTION_INFORMATION_EX_V2` on the
//! same port additionally tells us whether the port *supports* USB 3 at all —
//! which turns "it is slow" into the actionable "this port could do better".
//!
//! Verbose logging is permanent in this project: every hub and port examined is
//! reported through `logging::trace`, not silently skipped.

#![allow(dead_code)]

use std::ffi::c_void;

use windows::core::PCWSTR;
use windows::Win32::Devices::DeviceAndDriverInstallation::*;
use windows::Win32::Devices::Usb::*;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::IO::DeviceIoControl;

/// Negotiated USB link speed, as the hub reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsbSpeed {
    /// 1.5 Mbit/s — never right for a capture device.
    Low,
    /// 12 Mbit/s — a full-speed link; unusable for 1080p in any format.
    Full,
    /// 480 Mbit/s. Enough for MJPEG at 1080p, far too slow for uncompressed.
    High,
    /// 5 Gbit/s or better: uncompressed 1080p becomes possible.
    Super,
    /// The hub did not tell us.
    Unknown,
}

impl UsbSpeed {
    /// The raw `USB_DEVICE_SPEED` value from the connection info.
    pub fn from_raw(v: u8) -> UsbSpeed {
        match v {
            0 => UsbSpeed::Low,
            1 => UsbSpeed::Full,
            2 => UsbSpeed::High,
            3 => UsbSpeed::Super,
            _ => UsbSpeed::Unknown,
        }
    }

    /// "USB 2.0"-style name, for the UI.
    pub fn name(self) -> &'static str {
        match self {
            UsbSpeed::Low => "USB 1.1 low speed",
            UsbSpeed::Full => "USB 1.1 full speed",
            UsbSpeed::High => "USB 2.0 high speed",
            UsbSpeed::Super => "USB 3.x SuperSpeed",
            UsbSpeed::Unknown => "unknown link speed",
        }
    }

    /// Nominal signalling rate, for the UI.
    pub fn rate(self) -> &'static str {
        match self {
            UsbSpeed::Low => "1.5 Mbit/s",
            UsbSpeed::Full => "12 Mbit/s",
            UsbSpeed::High => "480 Mbit/s",
            UsbSpeed::Super => "5 Gbit/s+",
            UsbSpeed::Unknown => "?",
        }
    }

    /// Realistic sustained throughput in MB/s. Used to explain, in the UI, why a
    /// given format does or does not fit down this link.
    pub fn practical_mb_per_s(self) -> f64 {
        match self {
            UsbSpeed::Low => 0.19,
            UsbSpeed::Full => 1.5,
            UsbSpeed::High => 40.0,
            UsbSpeed::Super => 400.0,
            UsbSpeed::Unknown => 0.0,
        }
    }

    pub fn is_usb3(self) -> bool {
        matches!(self, UsbSpeed::Super)
    }
}

/// What the hub told us about one device's port.
#[derive(Debug, Clone)]
pub struct UsbLink {
    pub speed: UsbSpeed,
    /// Port number on that hub (1-based, as the hub numbers them).
    pub port: u32,
    /// Hub device interface path we found the device on.
    pub hub: String,
    /// Whether the port itself advertises USB 3.x support.
    pub port_supports_usb3: bool,
    /// Whether the device is running at SuperSpeed or faster right now.
    pub operating_at_superspeed: bool,
    /// How the answer was obtained, so the UI never overstates its confidence.
    pub source: &'static str,
}

impl UsbLink {
    /// Bytes per second the link should manage, as a display string.
    pub fn throughput_note(&self) -> String {
        format!("~{:.1} MB/s practically", self.speed.practical_mb_per_s())
    }

    /// A one-line verdict naming the consequence, not just the number.
    pub fn verdict(&self) -> String {
        match self.speed {
            UsbSpeed::Super => "USB 3.x link: uncompressed formats are viable.".to_string(),
            UsbSpeed::High => {
                if self.port_supports_usb3 {
                    "USB 2.0 link on a USB 3-capable port — check the cable, or move it to \
                     a native USB 3 port. Until then use MJPEG: uncompressed 1080p needs \
                     155-207 MB/s and this link carries ~40 MB/s."
                        .to_string()
                } else {
                    "USB 2.0 link: only MJPEG can carry 1080p here. Uncompressed 1080p needs \
                     155-207 MB/s against ~40 MB/s available."
                        .to_string()
                }
            }
            UsbSpeed::Full | UsbSpeed::Low => {
                "USB 1.1 link: far too slow for 1080p video in any format.".to_string()
            }
            UsbSpeed::Unknown => "Link speed unknown — the hub did not report one.".to_string(),
        }
    }
}

/// Bytes a 1080p frame needs in each uncompressed layout, for explaining the
/// arithmetic in the UI.
pub fn uncompressed_frame_bytes(width: u32, height: u32, bytes_per_pixel: f64) -> u64 {
    (width as f64 * height as f64 * bytes_per_pixel) as u64
}

/// Does `bytes_per_frame` at `fps` fit down this link, at 80% of realistic peak?
///
/// The 80% is deliberate: USB bulk transfers never reach the nominal figure, and
/// the point of this function is to avoid telling the user a format works when
/// it demonstrably does not.
pub fn fits_on_link(speed: UsbSpeed, bytes_per_frame: u64, fps: f64) -> bool {
    let available = speed.practical_mb_per_s() * 1_000_000.0 * 0.8;
    (bytes_per_frame as f64 * fps) <= available
}

/// Bytes per pixel for an uncompressed subtype, or `None` when the format is
/// compressed (MJPEG and friends) and its frame size depends on the picture.
pub fn uncompressed_bytes_per_pixel(subtype: &str) -> Option<f64> {
    match subtype.to_ascii_uppercase().as_str() {
        "NV12" | "NV21" => Some(1.5),
        "YUY2" | "UYVY" | "NV16" => Some(2.0),
        "RGB24" => Some(3.0),
        "RGB32" | "ARGB32" => Some(4.0),
        _ => None,
    }
}

/// Can this exact format be carried by this link?
///
/// `None` means "cannot say from the header alone" — a compressed format's size
/// depends on the content, so it is judged by its measured size instead.
pub fn format_fits(
    speed: UsbSpeed,
    width: u32,
    height: u32,
    fps: f32,
    subtype: &str,
) -> Option<bool> {
    let bpp = uncompressed_bytes_per_pixel(subtype)?;
    let bytes = uncompressed_frame_bytes(width, height, bpp);
    Some(fits_on_link(speed, bytes, fps as f64))
}

/// Megabytes per second an uncompressed format would need at its own geometry.
pub fn uncompressed_mb_per_s(width: u32, height: u32, fps: f32, subtype: &str) -> Option<f64> {
    let bpp = uncompressed_bytes_per_pixel(subtype)?;
    Some(uncompressed_frame_bytes(width, height, bpp) as f64 * fps as f64 / 1_000_000.0)
}

/// Pull `(vid, pid)` out of a device interface path such as
/// `\\?\usb#vid_345f&pid_2130&mi_00#7&...#{guid}\global`.
pub fn vid_pid_from_path(path: &str) -> Option<(u16, u16)> {
    let lower = path.to_ascii_lowercase();
    let vid_at = lower.find("vid_")?;
    let pid_at = lower.find("pid_")?;
    let vid = u16::from_str_radix(lower.get(vid_at + 4..vid_at + 8)?, 16).ok()?;
    let pid = u16::from_str_radix(lower.get(pid_at + 4..pid_at + 8)?, 16).ok()?;
    Some((vid, pid))
}

/// Every USB hub device interface on the machine.
fn hub_interfaces() -> Vec<String> {
    let mut out = Vec::new();
    unsafe {
        let Ok(set) = SetupDiGetClassDevsW(
            Some(&GUID_DEVINTERFACE_USB_HUB),
            PCWSTR::null(),
            None,
            DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
        ) else {
            return out;
        };

        let mut ifdata = SP_DEVICE_INTERFACE_DATA {
            cbSize: std::mem::size_of::<SP_DEVICE_INTERFACE_DATA>() as u32,
            ..Default::default()
        };

        let mut i = 0u32;
        while SetupDiEnumDeviceInterfaces(set, None, &GUID_DEVINTERFACE_USB_HUB, i, &mut ifdata)
            .is_ok()
        {
            i += 1;

            let mut needed = 0u32;
            let _ =
                SetupDiGetDeviceInterfaceDetailW(set, &ifdata, None, 0, Some(&mut needed), None);
            if needed == 0 {
                continue;
            }

            // Allocate in u32 units so the struct's 4-byte alignment holds.
            let mut raw: Vec<u32> = vec![0; (needed as usize / 4) + 4];
            let detail = raw.as_mut_ptr() as *mut SP_DEVICE_INTERFACE_DETAIL_DATA_W;
            (*detail).cbSize = std::mem::size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32;

            if SetupDiGetDeviceInterfaceDetailW(set, &ifdata, Some(detail), needed, None, None)
                .is_ok()
            {
                let ptr = PCWSTR((*detail).DevicePath.as_ptr());
                if let Ok(path) = ptr.to_string() {
                    out.push(path);
                }
            }
        }

        let _ = SetupDiDestroyDeviceInfoList(set);
    }
    out
}

fn open_for_ioctl(path: &str) -> Result<HANDLE, String> {
    const GENERIC_READ: u32 = 0x8000_0000;
    const GENERIC_WRITE: u32 = 0x4000_0000;

    let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        CreateFileW(
            PCWSTR(wide.as_ptr()),
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            None,
        )
        .map_err(|e| format!("cannot open hub {path}: {e}"))
    }
}

/// How many ports does this hub have?
fn hub_port_count(hub: HANDLE) -> Result<u8, String> {
    let mut node = Box::new(USB_NODE_INFORMATION::default());
    let mut returned = 0u32;
    unsafe {
        DeviceIoControl(
            hub,
            IOCTL_USB_GET_NODE_INFORMATION,
            Some(&mut *node as *mut _ as *const c_void),
            std::mem::size_of::<USB_NODE_INFORMATION>() as u32,
            Some(&mut *node as *mut _ as *mut c_void),
            std::mem::size_of::<USB_NODE_INFORMATION>() as u32,
            Some(&mut returned),
            None,
        )
        .map_err(|e| format!("IOCTL_USB_GET_NODE_INFORMATION: {e}"))?;

        // Copy out of the packed struct before comparing: taking a reference to a
        // packed field would be a misaligned reference.
        let node_type = node.NodeType;
        if node_type != UsbHub {
            return Err("not a hub node".to_string());
        }
        Ok(node.u.HubInformation.HubDescriptor.bNumberOfPorts)
    }
}

/// `(vid, pid, speed)` for whatever is plugged into `port`, if anything.
fn port_device(hub: HANDLE, port: u32) -> Option<(u16, u16, u8, String)> {
    const EXTRA_PIPES: usize = 32;
    let ex_size = std::mem::size_of::<USB_NODE_CONNECTION_INFORMATION_EX>();
    let alloc = ex_size + EXTRA_PIPES * std::mem::size_of::<USB_PIPE_INFO>();

    // u32 units keep the packed struct suitably aligned.
    let mut buf: Vec<u32> = vec![0; alloc / 4 + 4];
    let ex = buf.as_mut_ptr() as *mut USB_NODE_CONNECTION_INFORMATION_EX;
    let mut returned = 0u32;

    unsafe {
        (*ex).ConnectionIndex = port;
        if DeviceIoControl(
            hub,
            IOCTL_USB_GET_NODE_CONNECTION_INFORMATION_EX,
            Some(ex as *const c_void),
            ex_size as u32,
            Some(ex as *mut c_void),
            alloc as u32,
            Some(&mut returned),
            None,
        )
        .is_err()
        {
            return None;
        }

        // 1 == DeviceConnected
        if (*ex).ConnectionStatus.0 != 1 {
            return None;
        }

        let vid = (*ex).DeviceDescriptor.idVendor;
        let pid = (*ex).DeviceDescriptor.idProduct;
        let speed = (*ex).Speed;
        let addr = (*ex).DeviceAddress;
        Some((vid, pid, speed, format!("address {addr}")))
    }
}

/// Does this port support USB 3.x, and is it running at SuperSpeed right now?
fn port_protocols(hub: HANDLE, port: u32) -> (bool, bool) {
    let mut v2 = USB_NODE_CONNECTION_INFORMATION_EX_V2::default();
    v2.ConnectionIndex = port;
    v2.Length = std::mem::size_of::<USB_NODE_CONNECTION_INFORMATION_EX_V2>() as u32;
    let mut returned = 0u32;

    unsafe {
        if DeviceIoControl(
            hub,
            IOCTL_USB_GET_NODE_CONNECTION_INFORMATION_EX_V2,
            Some(&v2 as *const _ as *const c_void),
            std::mem::size_of::<USB_NODE_CONNECTION_INFORMATION_EX_V2>() as u32,
            Some(&mut v2 as *mut _ as *mut c_void),
            std::mem::size_of::<USB_NODE_CONNECTION_INFORMATION_EX_V2>() as u32,
            Some(&mut returned),
            None,
        )
        .is_err()
        {
            return (false, false);
        }

        // USB_PROTOCOLS bitfield: bit0 Usb110, bit1 Usb200, bit2 Usb300.
        let protocols = v2.SupportedUsbProtocols.ul;
        let supports_usb3 = protocols & 0b100 != 0;
        // Flags bitfield: bit0 = DeviceIsOperatingAtSuperSpeedOrHigher.
        let at_superspeed = v2.Flags.ul & 0b1 != 0;
        (supports_usb3, at_superspeed)
    }
}

/// Find `vid:pid` on any hub and report the real negotiated link speed.
///
/// Returns `Err` when the device is not found on any hub — the caller should
/// then fall back to inference and label the result as inferred.
pub fn measure(vid: u16, pid: u16) -> Result<UsbLink, String> {
    let hubs = hub_interfaces();
    crate::logging::trace(format!(
        "usb_link: {} hub interfaces to examine",
        hubs.len()
    ));

    for path in &hubs {
        let hub = match open_for_ioctl(path) {
            Ok(h) => h,
            Err(e) => {
                crate::logging::trace(format!("usb_link: skipping hub ({e})"));
                continue;
            }
        };

        let ports = hub_port_count(hub).unwrap_or(0);
        crate::logging::trace(format!("usb_link: hub {path} reports {ports} ports"));

        for port in 1..=ports as u32 {
            let Some((dev_vid, dev_pid, speed_raw, where_)) = port_device(hub, port) else {
                continue;
            };
            crate::logging::trace(format!(
                "usb_link: port {port} has {:04X}:{:04X} ({where_}, speed raw {speed_raw})",
                dev_vid, dev_pid
            ));

            if dev_vid == vid && dev_pid == pid {
                let (supports_usb3, at_superspeed) = port_protocols(hub, port);
                let speed = UsbSpeed::from_raw(speed_raw);
                let link = UsbLink {
                    speed,
                    port,
                    hub: path.clone(),
                    port_supports_usb3: supports_usb3,
                    operating_at_superspeed: at_superspeed,
                    source: "measured from the USB hub",
                };
                crate::logging::info(format!(
                    "usb_link: {} ({}), {}; source: {}",
                    link.speed.name(),
                    link.speed.rate(),
                    link.verdict(),
                    link.source
                ));
                unsafe {
                    let _ = CloseHandle(hub);
                }
                return Ok(link);
            }
        }

        unsafe {
            let _ = CloseHandle(hub);
        }
    }

    Err(format!(
        "USB device {vid:04X}:{pid:04X} was not found on any hub"
    ))
}

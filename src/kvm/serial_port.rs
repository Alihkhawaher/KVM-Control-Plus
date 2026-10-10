//! A raw Windows COM-port handle — replaces the `serialport` crate.
//!
//! Only the six calls the CH9329 link actually needs are wrapped:
//! `CreateFileW`, `GetCommState` / `BuildCommDCBW` / `SetCommState`,
//! `SetCommTimeouts`, `WriteFile`, `ReadFile`, `PurgeComm`, `ClearCommError`.
//!
//! Configuration is expressed with `BuildCommDCBW` ("baud=… parity=N data=8
//! stop=1") rather than by poking individual `DCB` bitfields, which keeps the
//! unsafe surface tiny.

#![allow(dead_code)]

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;

use windows::core::PCWSTR;
use windows::Win32::Devices::Communication::{
    BuildCommDCBW, ClearCommError, GetCommState, PurgeComm, SetCommState, SetCommTimeouts,
    COMMTIMEOUTS, COMSTAT, DCB, PURGE_RXCLEAR, PURGE_TXCLEAR,
};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, WriteFile, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE,
    OPEN_EXISTING,
};
use windows::Win32::System::SystemServices::MAXDWORD;

const GENERIC_READ: u32 = 0x8000_0000;
const GENERIC_WRITE: u32 = 0x4000_0000;

impl std::fmt::Debug for SerialPort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SerialPort")
            .field("name", &self.name())
            .field("baud", &self.baud())
            .finish_non_exhaustive()
    }
}

/// A configured, opened serial port.
pub struct SerialPort {
    handle: HANDLE,
    name: String,
    baud: u32,
}

// The handle is only ever used from one thread at a time (the CH9329 sender
// thread owns writes; the UI only queries state), so this is sound.
unsafe impl Send for SerialPort {}
unsafe impl Sync for SerialPort {}

impl SerialPort {
    /// Open `name` (e.g. `"COM5"`) at `baud` with 8N1 and no flow control.
    pub fn open(name: &str, baud: u32) -> Result<Self, String> {
        // COM ports above COM9 must use the \\.\ prefix; always using it is legal.
        let full = if name.starts_with(r"\\.\") {
            name.to_string()
        } else {
            format!(r"\\.\{name}")
        };
        let wide: Vec<u16> = OsStr::new(&full)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();

        unsafe {
            let handle = CreateFileW(
                PCWSTR(wide.as_ptr()),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                None,
            )
            .map_err(|e| format!("cannot open {name}: {e}"))?;

            let port = SerialPort {
                handle,
                name: name.to_string(),
                baud,
            };

            if let Err(e) = port.configure(baud) {
                // Drop closes the handle.
                drop(port);
                return Err(e);
            }
            Ok(port)
        }
    }

    unsafe fn configure(&self, baud: u32) -> Result<(), String> {
        let mut dcb = DCB::default();
        GetCommState(self.handle, &mut dcb).map_err(|e| format!("GetCommState failed: {e}"))?;

        // "baud=<n> parity=N data=8 stop=1" — no handshaking, which is what the
        // CH9329 expects.
        let cfg = format!("baud={baud} parity=N data=8 stop=1");
        let cfg_w: Vec<u16> = OsStr::new(&cfg)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        BuildCommDCBW(PCWSTR(cfg_w.as_ptr()), &mut dcb)
            .map_err(|e| format!("BuildCommDCBW({cfg}) failed: {e}"))?;
        SetCommState(self.handle, &dcb).map_err(|e| format!("SetCommState failed: {e}"))?;

        // Reads return as soon as any byte is available (or after the constant
        // timeout); writes block up to one second.
        let timeouts = COMMTIMEOUTS {
            ReadIntervalTimeout: MAXDWORD,
            ReadTotalTimeoutMultiplier: 0,
            ReadTotalTimeoutConstant: 200,
            WriteTotalTimeoutMultiplier: 0,
            WriteTotalTimeoutConstant: 1000,
        };
        SetCommTimeouts(self.handle, &timeouts)
            .map_err(|e| format!("SetCommTimeouts failed: {e}"))?;

        PurgeComm(self.handle, PURGE_RXCLEAR | PURGE_TXCLEAR)
            .map_err(|e| format!("PurgeComm failed: {e}"))?;
        Ok(())
    }

    /// Write the whole buffer. Returns the number of bytes written.
    pub fn write_all(&self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        let mut written = 0u32;
        unsafe {
            WriteFile(self.handle, Some(bytes), Some(&mut written), None)
                .map_err(|e| std::io::Error::other(e.message().to_string()))?;
        }
        Ok(written as usize)
    }

    /// Read up to `buf.len()` bytes, waiting at most the configured timeout.
    /// Returns the number of bytes actually read (0 on timeout).
    pub fn read(&self, buf: &mut [u8]) -> std::io::Result<usize> {
        let mut read = 0u32;
        unsafe {
            ReadFile(self.handle, Some(buf), Some(&mut read), None)
                .map_err(|e| std::io::Error::other(e.message().to_string()))?;
        }
        Ok(read as usize)
    }

    /// Read exactly `buf.len()` bytes, retrying until the deadline (ms).
    pub fn read_exact_timeout(&self, buf: &mut [u8], total_ms: u32) -> std::io::Result<bool> {
        let start = std::time::Instant::now();
        let mut got = 0usize;
        while got < buf.len() {
            if start.elapsed().as_millis() as u32 > total_ms {
                return Ok(false);
            }
            let n = self.read(&mut buf[got..])?;
            got += n;
            if n == 0 && start.elapsed().as_millis() as u32 > total_ms {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Discard buffered input/output.
    pub fn purge(&self) {
        unsafe {
            let _ = PurgeComm(self.handle, PURGE_RXCLEAR | PURGE_TXCLEAR);
        }
    }

    /// Discard buffered **input** only. Safe to call mid-stream: unlike a full
    /// purge it cannot truncate a frame that is still being written.
    pub fn purge_input(&self) {
        unsafe {
            let _ = PurgeComm(self.handle, PURGE_RXCLEAR);
        }
    }

    /// Bytes waiting to be read.
    pub fn bytes_available(&self) -> u32 {
        let mut stat = COMSTAT::default();
        unsafe {
            let _ = ClearCommError(self.handle, None, Some(&mut stat));
        }
        stat.cbInQue
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn baud(&self) -> u32 {
        self.baud
    }
}

impl Drop for SerialPort {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.handle);
        }
    }
}

// ===========================================================================
//  Tests — only the pure helpers; opening a port needs real hardware.
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_missing_port_with_a_clear_error() {
        // A channel that cannot exist: opening must fail, not panic.
        let err = SerialPort::open("COM999", 9600).unwrap_err();
        assert!(err.contains("COM999"), "error should name the port: {err}");
    }
}

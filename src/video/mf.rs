//! Video capture through **MediaFoundation** — replaces WinRT `MediaCapture` and
//! the `nokhwa` crate, and it also decodes MJPG for us.
//!
//! The trick that keeps this small: the source reader's output type is set to
//! **RGB32**, so MediaFoundation inserts its own decoder for compressed camera
//! formats (MJPG/HEVC/H.264). We never touch a JPEG decoder, and the UI gets a
//! plain RGB buffer it can upload as a texture.
//!
//! Layout: [`list_devices`] enumerates capture devices (the original's "Video
//! source" combo box), [`list_formats`] enumerates the native formats so the user
//! can pick a resolution/framerate instead of the original's hard-coded 1080p60
//! preference, and [`CaptureThread`] streams frames.

#![allow(dead_code)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, OnceLock};
use std::thread::JoinHandle;

use windows::core::{GUID, PCWSTR};
use windows::Win32::Media::MediaFoundation::{
    IMFActivate, IMFAttributes, IMFMediaSource, IMFMediaType, IMFSample, IMFSourceReader,
    MFCreateAttributes, MFCreateMediaType, MFCreateSourceReaderFromMediaSource,
    MFEnumDeviceSources, MFMediaType_Video, MFShutdown, MFStartup, MFVideoFormat_MJPG,
    MFVideoFormat_NV12, MFVideoFormat_RGB32, MFVideoFormat_YUY2, MFSTARTUP_FULL,
    MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME, MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE,
    MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
    MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK, MF_MT_DEFAULT_STRIDE,
    MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE,
    MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, MF_SOURCE_READER_FIRST_VIDEO_STREAM, MF_VERSION,
};

/// A capture device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoDevice {
    /// Position in the MediaFoundation enumeration.
    pub index: usize,
    /// Friendly name, e.g. `"USB Video"` or the MS2130's product string.
    pub name: String,
    /// Symbolic link, the stable id the original app also stored.
    pub symbolic_link: String,
}

/// One native capture format reported by the device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoFormat {
    pub width: u32,
    pub height: u32,
    pub fps_numerator: u32,
    pub fps_denominator: u32,
    /// `"MJPG"`, `"NV12"`, `"YUY2"`, `"RGB32"`, …
    pub subtype: String,
}

impl VideoFormat {
    pub fn fps(&self) -> f32 {
        if self.fps_denominator == 0 {
            0.0
        } else {
            self.fps_numerator as f32 / self.fps_denominator as f32
        }
    }

    /// Compact label for the format drop-down, e.g. `"1920x1080 @ 60 MJPG"`.
    pub fn label(&self) -> String {
        format!(
            "{}x{} @ {:.0} {}",
            self.width,
            self.height,
            self.fps(),
            self.subtype
        )
    }

    /// Score used to pick a sensible default: prefer 1080p ≥60fps, like the
    /// original did, then 1080p ≥30, then the largest area.
    pub fn auto_score(&self) -> (u8, u8, f32, u64) {
        let hd = self.width == 1920 && self.height == 1080;
        let tier = if hd && self.fps() >= 59.0 {
            3
        } else if hd && self.fps() >= 29.0 {
            2
        } else if hd {
            1
        } else {
            0
        };
        // Prefer compressed (MJPG) for high-resolution/high-fps over USB 2.0.
        let mjpg = u8::from(self.subtype.eq_ignore_ascii_case("MJPG"));
        (
            tier,
            mjpg,
            self.fps(),
            self.width as u64 * self.height as u64,
        )
    }
}

// ---------------------------------------------------------------------------
//  MediaFoundation lifetime
// ---------------------------------------------------------------------------

static MF_INIT: OnceLock<Result<(), String>> = OnceLock::new();

/// Start MediaFoundation exactly once for the process.
pub fn ensure_started() -> Result<(), String> {
    MF_INIT
        .get_or_init(|| unsafe {
            MFStartup(MF_VERSION, MFSTARTUP_FULL).map_err(|e| format!("MFStartup failed: {e}"))
        })
        .clone()
}

// MFShutdown is deliberately never called: the process is a GUI app that may
// re-open the capture at any time, and every official sample leaves MF running
// for the lifetime of the application. (Documented here so the omission is not
// mistaken for an oversight.)
#[allow(dead_code)]
fn shutdown_on_exit() {
    unsafe {
        let _ = MFShutdown();
    }
}

// ---------------------------------------------------------------------------
//  Device enumeration
// ---------------------------------------------------------------------------

/// Enumerate the video-capture devices on this machine.
pub fn list_devices() -> Result<Vec<VideoDevice>, String> {
    ensure_started()?;
    unsafe {
        let attrs = create_activation_attributes()?;

        let mut raw: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut count: u32 = 0;
        MFEnumDeviceSources(&attrs, &mut raw, &mut count)
            .map_err(|e| format!("MFEnumDeviceSources failed: {e}"))?;
        if raw.is_null() || count == 0 {
            return Ok(Vec::new());
        }

        let mut out = Vec::with_capacity(count as usize);
        for i in 0..count as usize {
            // The array is owned by MF; index into it without taking ownership.
            let activate_ptr = raw.add(i);
            let Some(activate) = (*activate_ptr).clone() else {
                continue;
            };
            out.push(VideoDevice {
                index: i,
                name: activate_string(&activate, &MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME),
                symbolic_link: activate_string(
                    &activate,
                    &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK,
                ),
            });
        }
        // Release the array MF handed us.
        let _ = windows::Win32::System::Com::CoTaskMemFree(Some(raw as *const _));
        Ok(out)
    }
}

/// Just the friendly names — used by `--probe` and the UI.
pub fn list_device_names() -> Result<Vec<String>, String> {
    Ok(list_devices()?.into_iter().map(|d| d.name).collect())
}

fn create_activation_attributes() -> Result<IMFAttributes, String> {
    unsafe {
        let mut attrs: Option<IMFAttributes> = None;
        MFCreateAttributes(&mut attrs, 1).map_err(|e| format!("MFCreateAttributes: {e}"))?;
        let attrs = attrs.ok_or("MFCreateAttributes returned null")?;
        attrs
            .SetGUID(
                &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE,
                &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
            )
            .map_err(|e| format!("SetGUID(source type): {e}"))?;
        Ok(attrs)
    }
}

/// Read a string attribute from an activation object.
fn activate_string(activate: &IMFActivate, key: &GUID) -> String {
    unsafe {
        let mut len = match activate.GetStringLength(key) {
            Ok(n) if n > 0 => n,
            _ => return String::new(),
        };
        let mut buf = vec![0u16; (len + 1) as usize];
        if activate.GetString(key, &mut buf, Some(&mut len)).is_err() {
            return String::new();
        }
        let end = buf.iter().position(|&c| c == 0).unwrap_or(len as usize);
        String::from_utf16_lossy(&buf[..end])
    }
}

/// Open an `IMFMediaSource` for the device with the given symbolic link.
pub fn open_source(symbolic_link: &str) -> Result<IMFMediaSource, String> {
    ensure_started()?;
    let devices = list_devices()?;
    let device = devices
        .iter()
        .find(|d| d.symbolic_link == symbolic_link)
        .ok_or_else(|| format!("capture device not found: {symbolic_link}"))?;

    unsafe {
        let attrs = create_activation_attributes()?;
        let mut raw: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut count = 0u32;
        MFEnumDeviceSources(&attrs, &mut raw, &mut count)
            .map_err(|e| format!("MFEnumDeviceSources: {e}"))?;
        let activate_ptr = raw.add(device.index);
        let activate = (*activate_ptr)
            .clone()
            .ok_or("activation object vanished")?;
        let source: IMFMediaSource = activate
            .ActivateObject()
            .map_err(|e| format!("ActivateObject: {e}"))?;
        let _ = windows::Win32::System::Com::CoTaskMemFree(Some(raw as *const _));
        Ok(source)
    }
}

// ---------------------------------------------------------------------------
//  Format enumeration
// ---------------------------------------------------------------------------

/// Enumerate the formats the device reports natively.
pub fn list_formats(symbolic_link: &str) -> Result<Vec<VideoFormat>, String> {
    let source = open_source(symbolic_link)?;
    let reader = create_reader(&source)?;
    let mut formats = Vec::new();

    unsafe {
        for i in 0u32..512 {
            let mt =
                match reader.GetNativeMediaType(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32, i) {
                    Ok(mt) => mt,
                    Err(_) => break, // no more formats
                };
            if let Some(f) = media_type_to_format(&mt) {
                formats.push(f);
            }
        }
    }

    // Deduplicate and sort nicest-first for the drop-down.
    // (`auto_score` contains an f32, so compare with `partial_cmp`.)
    formats.sort_by(|a, b| {
        b.auto_score()
            .partial_cmp(&a.auto_score())
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    formats.dedup();
    Ok(formats)
}

fn create_reader(source: &IMFMediaSource) -> Result<IMFSourceReader, String> {
    unsafe {
        let mut attrs: Option<IMFAttributes> = None;
        MFCreateAttributes(&mut attrs, 2).map_err(|e| format!("MFCreateAttributes: {e}"))?;
        let attrs = attrs.ok_or("attributes null")?;
        // Let MF insert decoders/scalers so we can always ask for RGB32.
        let _ = attrs.SetUINT32(&MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, 1);

        MFCreateSourceReaderFromMediaSource(source, &attrs)
            .map_err(|e| format!("MFCreateSourceReaderFromMediaSource: {e}"))
    }
}

fn media_type_to_format(mt: &IMFMediaType) -> Option<VideoFormat> {
    unsafe {
        let major = mt.GetGUID(&MF_MT_MAJOR_TYPE).ok()?;
        if major != MFMediaType_Video {
            return None;
        }
        let subtype = mt.GetGUID(&MF_MT_SUBTYPE).ok()?;

        let (width, height) = unpack_frame_size(mt.GetUINT64(&MF_MT_FRAME_SIZE).ok()?);
        let (fps_num, fps_den) = unpack_frame_size(mt.GetUINT64(&MF_MT_FRAME_RATE).unwrap_or(0));

        Some(VideoFormat {
            width,
            height,
            fps_numerator: fps_num,
            fps_denominator: if fps_den == 0 { 1 } else { fps_den },
            subtype: format_guid(&subtype),
        })
    }
}

/// Map the well-known video subtype GUIDs to their fourcc names.
fn format_guid(g: &GUID) -> String {
    for (guid, name) in [
        (MFVideoFormat_MJPG, "MJPG"),
        (MFVideoFormat_NV12, "NV12"),
        (MFVideoFormat_YUY2, "YUY2"),
        (MFVideoFormat_RGB32, "RGB32"),
    ] {
        if *g == guid {
            return name.to_string();
        }
    }
    // Unknown: render the data1 field, which is the fourcc for video formats.
    let bytes = g.data1.to_le_bytes();
    let ascii: String = bytes
        .iter()
        .filter(|b| b.is_ascii_graphic())
        .map(|&b| b as char)
        .collect();
    if ascii.len() == 4 {
        ascii
    } else {
        format!("{:08X}", g.data1)
    }
}

/// Find the format whose size/rate/fourcc matches `wanted`, if any.
pub fn find_format(formats: &[VideoFormat], wanted: &str) -> Option<VideoFormat> {
    formats
        .iter()
        .find(|f| f.label().eq_ignore_ascii_case(wanted))
        .cloned()
}

// ---------------------------------------------------------------------------
//  Capture thread
// ---------------------------------------------------------------------------

/// A decoded RGB frame ready for the UI to upload as a texture.
#[derive(Clone)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    /// Tightly packed RGB8, `width * height * 3` bytes.
    pub rgb: Vec<u8>,
    /// Whether the source buffer was bottom-up (MediaFoundation RGB32 often is).
    pub flipped: bool,
}

impl Frame {
    /// True when the picture is effectively black — what a capture dongle
    /// reports while its HDMI source is off or in standby. Sampled, so it is
    /// cheap enough to run on every frame.
    pub fn looks_blank(&self) -> bool {
        // Walk the image in a coarse grid; a single non-dark pixel means we
        // have a real picture and we can stop immediately.
        let px = self.width as usize * self.height as usize;
        if px == 0 {
            return true;
        }
        let step = (px / 4096).max(1) * 3;
        self.rgb.iter().step_by(step).any(|&b| b > 8)
    }

    /// Fraction of sampled pixels that are not near-black, 0.0..=1.0.
    /// Shown in the UI so a dim-but-live picture is distinguishable from a
    /// dead one.
    pub fn lit_fraction(&self) -> f32 {
        let step = ((self.rgb.len() / 4096).max(1) / 3).max(1) * 3;
        let mut total = 0usize;
        let mut lit = 0usize;
        for chunk in self.rgb.chunks(step) {
            total += 1;
            if chunk.iter().any(|&b| b > 8) {
                lit += 1;
            }
        }
        if total == 0 {
            0.0
        } else {
            lit as f32 / total as f32
        }
    }
}

/// Messages from the capture thread.
pub enum CaptureMsg {
    /// The stream is live, at this size/framerate.
    Started {
        width: u32,
        height: u32,
        label: String,
    },
    Frame(Frame),
    /// Non-fatal problem (e.g. a dropped sample).
    Warning(String),
    /// The stream ended or failed.
    Stopped(String),
}

/// Streaming capture, running on its own thread.
pub struct CaptureThread {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl CaptureThread {
    /// Start streaming `symbolic_link`, preferring `format_label` (empty = auto).
    pub fn start(
        symbolic_link: String,
        format_label: String,
        flip_vertical: bool,
        tx: Sender<CaptureMsg>,
    ) -> Result<CaptureThread, String> {
        ensure_started()?;
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = Arc::clone(&stop);

        let thread = std::thread::Builder::new()
            .name("video-capture".into())
            .spawn(move || {
                if let Err(e) = capture_loop(
                    &symbolic_link,
                    &format_label,
                    flip_vertical,
                    &tx,
                    &stop_thread,
                ) {
                    let _ = tx.send(CaptureMsg::Stopped(e));
                }
                stop_thread.store(true, Ordering::Relaxed);
            })
            .map_err(|e| format!("cannot start capture thread: {e}"))?;

        Ok(CaptureThread {
            stop,
            thread: Some(thread),
        })
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for CaptureThread {
    fn drop(&mut self) {
        if self.thread.is_some() {
            self.stop();
        }
    }
}

fn capture_loop(
    symbolic_link: &str,
    format_label: &str,
    flip_vertical: bool,
    tx: &Sender<CaptureMsg>,
    stop: &AtomicBool,
) -> Result<(), String> {
    let source = open_source(symbolic_link)?;
    let reader = create_reader(&source)?;

    // Choose the format: the requested one, or MF's own best default.
    // The frame size lives on the *media type*, not on the samples, so it is
    // captured here and handed to every conversion.
    let (width, height, label) = configure_reader(&reader, symbolic_link, format_label)?;
    let _ = tx.send(CaptureMsg::Started {
        width,
        height,
        label,
    });

    let mut cb = CaptureBuffers::new();
    loop {
        // The read below blocks for at most one frame, so checking here keeps
        // `CaptureThread::stop()` responsive.
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }

        let mut sample: Option<IMFSample> = None;
        let mut flags = 0u32;
        unsafe {
            reader
                .ReadSample(
                    MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32,
                    0,
                    None,
                    Some(&mut flags),
                    None,
                    Some(&mut sample),
                )
                .map_err(|e| format!("ReadSample failed: {e}"))?;
        }

        const MF_SOURCE_READERF_STREAMTICK: u32 = 0x0000_0100;
        const MF_SOURCE_READERF_ENDOFSTREAM: u32 = 0x0000_0002;
        if flags & MF_SOURCE_READERF_ENDOFSTREAM != 0 {
            return Ok(());
        }
        if flags & MF_SOURCE_READERF_STREAMTICK != 0 {
            continue;
        }
        let Some(sample) = sample else { continue };

        match convert_sample(&sample, &mut cb, flip_vertical, width, height) {
            Ok(frame) => {
                if tx.send(CaptureMsg::Frame(frame)).is_err() {
                    return Ok(()); // UI closed
                }
            }
            Err(e) => {
                let _ = tx.send(CaptureMsg::Warning(e));
            }
        }
    }
}

/// Set the reader's output type to NV12 at the requested (or best) size.
///
/// Returns the chosen `(width, height, label)`.
///
/// Why NV12 rather than RGB32: NV12 is the uncompressed layout this class of
/// UVC dongle produces natively, so taking it directly keeps one transform out
/// of the graph and leaves the colour maths visible and testable here.
///
/// Careful — the native formats are NOT interchangeable on every device or
/// every USB link speed. `probe_raw` walks the device's whole format list and
/// reports which ones actually stream real content; trust that, not an
/// assumption.
fn configure_reader(
    reader: &IMFSourceReader,
    symbolic_link: &str,
    format_label: &str,
) -> Result<(u32, u32, String), String> {
    // Which native format should the output match?
    let native = if format_label.is_empty() {
        None
    } else {
        list_formats(symbolic_link)
            .ok()
            .and_then(|fs| find_format(&fs, format_label))
    };

    unsafe {
        let out = MFCreateMediaType().map_err(|e| format!("MFCreateMediaType: {e}"))?;
        out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).ok();
        out.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12).ok();

        if let Some(f) = &native {
            let size = ((f.width as u64) << 32) | f.height as u64;
            let _ = out.SetUINT64(&MF_MT_FRAME_SIZE, size);
            let rate = ((f.fps_numerator as u64) << 32) | f.fps_denominator.max(1) as u64;
            let _ = out.SetUINT64(&MF_MT_FRAME_RATE, rate);
            // RGB32 from MF is commonly bottom-up; record the stride so the
            // converter can flip when needed.
            let _ = out.SetUINT32(&MF_MT_DEFAULT_STRIDE, f.width * 4);
        }

        reader
            .SetCurrentMediaType(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32, None, &out)
            .map_err(|e| format!("cannot set RGB32 output (device busy?): {e}"))?;

        // Ask MF what it actually settled on.
        let actual = reader
            .GetCurrentMediaType(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32)
            .map_err(|e| format!("GetCurrentMediaType: {e}"))?;
        let f = media_type_to_format(&actual);
        let (w, h, label) = match f.or(native) {
            Some(f) => (f.width, f.height, f.label()),
            None => (0, 0, "unknown".to_string()),
        };
        Ok((w, h, label))
    }
}

/// Reusable buffers so the capture loop does not allocate per frame.
struct CaptureBuffers {
    rgb: Vec<u8>,
}

impl CaptureBuffers {
    fn new() -> Self {
        CaptureBuffers { rgb: Vec::new() }
    }
}

/// Lock the sample's buffer and convert BGRA/RGB32 → packed RGB8.
fn convert_sample(
    sample: &IMFSample,
    cb: &mut CaptureBuffers,
    flip_vertical: bool,
    width: u32,
    height: u32,
) -> Result<Frame, String> {
    unsafe {
        let buffer = sample
            .ConvertToContiguousBuffer()
            .map_err(|e| format!("ConvertToContiguousBuffer: {e}"))?;

        let mut ptr: *mut u8 = std::ptr::null_mut();
        let mut max_len = 0u32;
        buffer
            .Lock(&mut ptr, Some(&mut max_len), None)
            .map_err(|e| format!("IMFMediaBuffer::Lock: {e}"))?;

        // ---- permanent diagnostics (project policy: verbose logging stays) ----
        // `current` is the number of bytes MF actually wrote; `max_len` is how
        // much the buffer can hold. A zero `current` — or a buffer that is all
        // zeros — explains a black picture far faster than staring at a window.
        let current = buffer.GetCurrentLength().unwrap_or(0);
        {
            let probe = std::slice::from_raw_parts(ptr, max_len as usize);
            let brightest = probe.iter().copied().max().unwrap_or(0);
            let sum: u64 = probe.iter().map(|&b| b as u64).sum();
            let mean = if probe.is_empty() {
                0.0
            } else {
                sum as f64 / probe.len() as f64
            };
            // NV12 split, so a uniformly dark *picture* is distinguishable from
            // a stream that carries no picture at all.
            let y_part = (width as usize * height as usize).min(probe.len());
            let (y_slice, uv_slice) = probe.split_at(y_part);
            let y_min = y_slice.iter().copied().min().unwrap_or(0);
            let y_max = y_slice.iter().copied().max().unwrap_or(0);
            let y_mean = if y_slice.is_empty() {
                0.0
            } else {
                y_slice.iter().map(|&b| b as u64).sum::<u64>() as f64 / y_slice.len() as f64
            };
            let uv_mean = if uv_slice.is_empty() {
                0.0
            } else {
                uv_slice.iter().map(|&b| b as u64).sum::<u64>() as f64 / uv_slice.len() as f64
            };
            crate::logging::trace(format!(
                "frame {}x{} buffer current={current} max={max_len} brightest={brightest} mean={mean:.2} | Y min={y_min} max={y_max} mean={y_mean:.2} | UV mean={uv_mean:.2}",
                width, height
            ));
        }

        let result = (|| {
            let (w, h) = (width, height);
            if w == 0 || h == 0 {
                return Err("capture format has no frame size".to_string());
            }

            let packed = w as usize * 3;
            cb.rgb.resize(packed * h as usize, 0);

            // NV12: a full-size Y plane followed by a half-size interleaved
            // U,V plane — the uncompressed layout the device offers natively,
            // so no transform MFT is required and the colour maths stays here
            // where it can be traced and tested.
            let src = std::slice::from_raw_parts(ptr, max_len as usize);
            let y_plane = w as usize * h as usize;
            if src.len() < y_plane + y_plane / 2 {
                return Err(format!(
                    "NV12 buffer too small: {} bytes for {}x{}",
                    src.len(),
                    w,
                    h
                ));
            }
            let uv_plane = &src[y_plane..];
            let uv_stride = w as usize; // 2 bytes per 2x2 chroma block

            for y in 0..h as usize {
                let src_y = if flip_vertical { h as usize - 1 - y } else { y };
                let dst_row = y * packed;
                let lum_row = src_y * w as usize;
                let chroma_row = (src_y / 2) * uv_stride;
                for x in 0..w as usize {
                    let luma = src[lum_row + x] as i32;
                    let cidx = chroma_row + (x / 2) * 2;
                    let u = uv_plane[cidx] as i32 - 128;
                    let v = uv_plane[cidx + 1] as i32 - 128;

                    // BT.601 limited range, integer fixed point.
                    let c = 298 * (luma - 16);
                    let r = (c + 409 * v + 128) >> 8;
                    let g = (c - 100 * u - 208 * v + 128) >> 8;
                    let b = (c + 516 * u + 128) >> 8;

                    let d = dst_row + x * 3;
                    cb.rgb[d] = r.clamp(0, 255) as u8;
                    cb.rgb[d + 1] = g.clamp(0, 255) as u8;
                    cb.rgb[d + 2] = b.clamp(0, 255) as u8;

                    if y == 0 && x == 0 {
                        crate::logging::trace(format!(
                            "px0 luma_idx={} luma={luma} cidx={cidx} u={u} v={v} -> r={r} g={g} b={b}; src.len={} src[0..12]={:?} uv[0..12]={:?}",
                            lum_row + x,
                            src.len(),
                            &src[..12.min(src.len())],
                            &uv_plane[..12.min(uv_plane.len())]
                        ));
                    }
                }
            }
            // ---- permanent diagnostics: prove what we produced, not just read ----
            {
                let lit = cb.rgb.iter().filter(|&&b| b > 8).count();
                let head: Vec<u8> = cb.rgb.iter().copied().take(6).collect();
                crate::logging::trace(format!(
                    "converted {}x{} lit={}/{} head={:?}",
                    w,
                    h,
                    lit,
                    cb.rgb.len(),
                    head
                ));
            }

            Ok(Frame {
                width: w,
                height: h,
                rgb: std::mem::take(&mut cb.rgb),
                flipped: flip_vertical,
            })
        })();

        let _ = buffer.Unlock();
        result
    }
}

/// Diagnostic: try EVERY native format the device offers and report what each
/// one actually delivers.
///
/// A black window cannot tell you whether the device is silent, whether one
/// particular format stream is broken, or whether the conversion threw the data
/// away. This walks the device's own format list, selects each one as a
/// pass-through, and prints the byte statistics of the samples that arrive. A
/// format whose samples vary (min != max, non-trivial mean) is streaming real
/// picture; a format whose samples are uniform is not.
pub fn probe_raw(symbolic_link: &str, frames: usize) -> Result<Vec<String>, String> {
    let source = open_source(symbolic_link)?;
    let reader = create_reader(&source)?;
    let mut lines = Vec::new();
    let stream = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;

    // Every native (device-side) format, in the order the device lists them.
    let mut natives: Vec<IMFMediaType> = Vec::new();
    unsafe {
        for i in 0..512u32 {
            match reader.GetNativeMediaType(stream, i) {
                Ok(mt) => natives.push(mt),
                Err(_) => break,
            }
        }
    }
    lines.push(format!(
        "{} native media types on the device",
        natives.len()
    ));

    for (i, mt) in natives.iter().enumerate() {
        let label = media_type_to_format(mt)
            .map(|f| f.label())
            .unwrap_or_else(|| "unknown".to_string());
        unsafe {
            if let Err(e) = reader.SetCurrentMediaType(stream, None, mt) {
                lines.push(format!("[{i}] {label}: cannot select ({e})"));
                continue;
            }

            let mut stats: Vec<String> = Vec::new();
            for _ in 0..frames {
                let mut sample: Option<IMFSample> = None;
                let mut flags = 0u32;
                if reader
                    .ReadSample(stream, 0, None, Some(&mut flags), None, Some(&mut sample))
                    .is_err()
                {
                    break;
                }
                let Some(sample) = sample else { continue };
                let Ok(buffer) = sample.ConvertToContiguousBuffer() else {
                    continue;
                };
                let mut ptr: *mut u8 = std::ptr::null_mut();
                let mut max_len = 0u32;
                if buffer.Lock(&mut ptr, Some(&mut max_len), None).is_err() {
                    continue;
                }
                let current = buffer.GetCurrentLength().unwrap_or(0);
                let data = std::slice::from_raw_parts(ptr, max_len as usize);
                let mn = data.iter().copied().min().unwrap_or(0);
                let mx = data.iter().copied().max().unwrap_or(0);
                let mean = if data.is_empty() {
                    0.0
                } else {
                    data.iter().map(|&b| b as u64).sum::<u64>() as f64 / data.len() as f64
                };
                // Varying content => real picture. Uniform => a filler frame.
                stats.push(format!(
                    "bytes={current} min={mn} max={mx} mean={mean:.2}{}",
                    if mn == mx { " UNIFORM" } else { " VARYING" }
                ));
                let _ = buffer.Unlock();
                if current as usize != max_len as usize {
                    stats.push(format!("(note: length {current} != capacity {max_len})"));
                }
            }
            if stats.is_empty() {
                lines.push(format!("[{i}] {label}: no samples arrived"));
            } else {
                lines.push(format!("[{i}] {label}: {}", stats.join(" | ")));
            }
        }
    }
    Ok(lines)
}

/// Diagnostic: pull one raw sample of a chosen native format straight to disk.
///
/// For a format that is already a complete file (MJPEG frames are JPEG images)
/// this is the ground truth — open the result and you know exactly what the
/// device is sending, with no conversion of ours in the path.
pub fn grab_native(symbolic_link: &str, native_index: u32, out: &str) -> Result<String, String> {
    let source = open_source(symbolic_link)?;
    let reader = create_reader(&source)?;
    let stream = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;

    unsafe {
        let mt = reader
            .GetNativeMediaType(stream, native_index)
            .map_err(|e| format!("GetNativeMediaType({native_index}): {e}"))?;
        let label = media_type_to_format(&mt)
            .map(|f| f.label())
            .unwrap_or_else(|| "unknown".to_string());
        reader
            .SetCurrentMediaType(stream, None, &mt)
            .map_err(|e| format!("SetCurrentMediaType: {e}"))?;

        // Give the stream a moment to settle, then take the first sample.
        for _ in 0..30 {
            let mut sample: Option<IMFSample> = None;
            let mut flags = 0u32;
            reader
                .ReadSample(stream, 0, None, Some(&mut flags), None, Some(&mut sample))
                .map_err(|e| format!("ReadSample: {e}"))?;
            let Some(sample) = sample else { continue };

            let buffer = sample
                .ConvertToContiguousBuffer()
                .map_err(|e| format!("ConvertToContiguousBuffer: {e}"))?;
            let mut ptr: *mut u8 = std::ptr::null_mut();
            let mut max_len = 0u32;
            buffer
                .Lock(&mut ptr, Some(&mut max_len), None)
                .map_err(|e| format!("Lock: {e}"))?;
            let current = buffer.GetCurrentLength().unwrap_or(0) as usize;
            let data = std::slice::from_raw_parts(ptr, current);
            let res = std::fs::write(out, data)
                .map_err(|e| format!("could not write {out}: {e}"))
                .map(|_| format!("[{native_index}] {label}: wrote {current} bytes to {out}"));
            let _ = buffer.Unlock();
            return res;
        }
    }
    Err("no sample arrived".to_string())
}

/// Decode the packed `MF_MT_FRAME_SIZE` attribute value.
pub fn unpack_frame_size(v: u64) -> (u32, u32) {
    ((v >> 32) as u32, (v & 0xFFFF_FFFF) as u32)
}

#[allow(dead_code)]
fn _pcwstr_unused(_: PCWSTR) {}

// ===========================================================================
//  Tests — pure helpers; capture itself needs a camera.
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_size_packing_round_trips() {
        let packed = ((1920u64) << 32) | 1080u64;
        assert_eq!(unpack_frame_size(packed), (1920, 1080));
    }

    #[test]
    fn format_labels_and_fps() {
        let f = VideoFormat {
            width: 1920,
            height: 1080,
            fps_numerator: 60,
            fps_denominator: 1,
            subtype: "MJPG".into(),
        };
        assert_eq!(f.fps(), 60.0);
        assert_eq!(f.label(), "1920x1080 @ 60 MJPG");
    }

    #[test]
    fn auto_score_prefers_1080p60_mjpg() {
        let hd60 = VideoFormat {
            width: 1920,
            height: 1080,
            fps_numerator: 60,
            fps_denominator: 1,
            subtype: "MJPG".into(),
        };
        let hd30 = VideoFormat {
            width: 1920,
            height: 1080,
            fps_numerator: 30,
            fps_denominator: 1,
            subtype: "YUY2".into(),
        };
        let big = VideoFormat {
            width: 3840,
            height: 2160,
            fps_numerator: 30,
            fps_denominator: 1,
            subtype: "MJPG".into(),
        };
        assert!(hd60.auto_score() > hd30.auto_score());
        assert!(hd60.auto_score() > big.auto_score());
    }

    #[test]
    fn finds_a_requested_format_case_insensitively() {
        let fs = vec![VideoFormat {
            width: 1280,
            height: 720,
            fps_numerator: 30,
            fps_denominator: 1,
            subtype: "MJPG".into(),
        }];
        assert!(find_format(&fs, "1280x720 @ 30 mjpg").is_some());
        assert!(find_format(&fs, "1920x1080 @ 60 MJPG").is_none());
    }

    /// MediaFoundation must actually start on a real Windows host.
    #[test]
    fn media_foundation_starts() {
        assert!(ensure_started().is_ok(), "MFStartup should succeed");
    }

    /// Device enumeration must work (possibly returning zero devices).
    #[test]
    fn enumerates_devices_without_panicking() {
        match list_devices() {
            Ok(devs) => {
                for d in &devs {
                    assert!(!d.name.is_empty(), "device {d:?} has no friendly name");
                }
            }
            Err(e) => panic!("device enumeration failed: {e}"),
        }
    }
}

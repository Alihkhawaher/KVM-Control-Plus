//! Video capture through **MediaFoundation** — replaces WinRT `MediaCapture` and
//! the `nokhwa` crate, and it also decodes MJPG for us.
//!
//! The trick that keeps this small: the source reader's output type is set to
//! **NV12**, and MediaFoundation inserts its own decoder for compressed camera
//! formats (MJPG/HEVC/H.264) — we never touch a JPEG decoder. The NV12 → RGB
//! conversion is ours ([`ColorInfo`]), deliberately: it keeps the colour maths
//! visible and unit-testable, and it is the only way to get the *range* right,
//! because the format the user picks decides whether the samples are
//! studio-swing (16–235) or full-range (0–255).
//!
//! Layout: [`list_devices`] enumerates capture devices (the original's "Video
//! source" combo box), [`list_formats`] enumerates the native formats so the user
//! can pick a resolution/framerate instead of the original's hard-coded 1080p60
//! preference, and [`CaptureThread`] streams frames.

#![allow(dead_code)]

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows::core::{GUID, PCWSTR};
use windows::Win32::Media::MediaFoundation::{
    IMFActivate, IMFAttributes, IMFMediaSource, IMFMediaType, IMFSample, IMFSourceReader,
    MFCreateAttributes, MFCreateMediaType, MFCreateSourceReaderFromMediaSource,
    MFEnumDeviceSources, MFMediaType_Video, MFShutdown, MFStartup, MFVideoFormat_MJPG,
    MFVideoFormat_NV12, MFVideoFormat_RGB32, MFVideoFormat_YUY2, MFVideoTransferMatrix_BT601,
    MFVideoTransferMatrix_BT709, MFSTARTUP_FULL, MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME,
    MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE, MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
    MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK, MF_LOW_LATENCY, MF_MT_DEFAULT_STRIDE,
    MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE, MF_MT_VIDEO_NOMINAL_RANGE,
    MF_MT_YUV_MATRIX, MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING,
    MF_SOURCE_READER_FIRST_VIDEO_STREAM, MF_VERSION,
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
        // Let MF insert decoders/scalers so we can always ask for NV12.
        let _ = attrs.SetUINT32(&MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, 1);
        // Smallest possible delay between a sample being produced and being
        // handed to us. Without this the source reader buffers ahead, which is
        // the single biggest contributor to "the picture is a second behind".
        let _ = attrs.SetUINT32(&MF_LOW_LATENCY, 1);

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
//  Colour: which range and which matrix the arriving YUV is actually in
// ---------------------------------------------------------------------------

/// Studio-swing (16–235) or full-range (0–255) luma and chroma.
///
/// This is not a preference, it is a property of the *format*. MJPG is JPEG,
/// and JPEG is full-range by definition; the uncompressed formats a UVC device
/// offers natively (NV12/YUY2) are studio-swing. Getting it wrong is plainly
/// visible: studio-swing maths applied to full-range data multiplies contrast
/// by ~1.16 and clips everything from luma 235 up to pure white, which reads
/// as "the picture is washed out and too bright".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChromaRange {
    Limited,
    Full,
}

/// BT.601 or BT.709 colour-difference coefficients. SD content is 601, HD is
/// normally 709 — but only the media type knows for sure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorMatrix {
    Bt601,
    Bt709,
}

impl ColorMatrix {
    fn name(self) -> &'static str {
        match self {
            ColorMatrix::Bt601 => "BT.601",
            ColorMatrix::Bt709 => "BT.709",
        }
    }
}

impl ChromaRange {
    fn name(self) -> &'static str {
        match self {
            ChromaRange::Limited => "limited 16-235",
            ChromaRange::Full => "full 0-255",
        }
    }
}

/// How to read the YUV samples that are arriving, and where we learned it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ColorInfo {
    pub range: ChromaRange,
    pub matrix: ColorMatrix,
    /// Where the decision came from. Carried into the UI because "why does the
    /// picture look like that" is a question the app is supposed to answer.
    pub source: &'static str,
    /// Provenance of the matrix half of the decision.
    pub matrix_note: Option<&'static str>,
    /// Row pitch of the Y plane in bytes, as the decoder reports it. Assumed
    /// tight (`width`) only when the media type does not say.
    pub y_stride: usize,
}

// Attribute codes from `MFNominalRange` (mfobjects.h).
const NOMINAL_RANGE_0_255: u32 = 1;
const NOMINAL_RANGE_16_235: u32 = 2;

impl ColorInfo {
    /// Decode the answer from a negotiated media type.
    ///
    /// `native_subtype` is the *device-side* format that was asked for, and it
    /// is the fallback when the type carries no attributes: a decoded format
    /// (MJPG) yields full-range samples even if nothing says so, because that
    /// is what a JPEG decoder produces.
    fn from_media_type(mt: &IMFMediaType, native_subtype: &str, width: u32) -> ColorInfo {
        let height = unpack_frame_size(unsafe { mt.GetUINT64(&MF_MT_FRAME_SIZE) }.unwrap_or(0)).1;

        // ---- range ----
        let (range, range_src) = match unsafe { mt.GetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE) } {
            Ok(NOMINAL_RANGE_0_255) => (ChromaRange::Full, "media type says 0-255"),
            Ok(NOMINAL_RANGE_16_235) => (ChromaRange::Limited, "media type says 16-235"),
            _ if native_subtype.eq_ignore_ascii_case("MJPG") => {
                (ChromaRange::Full, "MJPG is JPEG: full-range by definition")
            }
            _ => (
                ChromaRange::Limited,
                "uncompressed capture: studio swing assumed",
            ),
        };

        // ---- matrix ----
        let (matrix, matrix_src) = match unsafe { mt.GetUINT32(&MF_MT_YUV_MATRIX) } {
            Ok(v) if v == MFVideoTransferMatrix_BT709.0 as u32 => {
                (ColorMatrix::Bt709, "media type says BT.709")
            }
            Ok(v) if v == MFVideoTransferMatrix_BT601.0 as u32 => {
                (ColorMatrix::Bt601, "media type says BT.601")
            }
            Ok(_) => (ColorMatrix::Bt601, "media type names another matrix"),
            // No attribute: fall back on the convention, HD is 709.
            Err(_) if height > 576 => (ColorMatrix::Bt709, "unstated: HD defaults to BT.709"),
            Err(_) => (ColorMatrix::Bt601, "unstated: SD defaults to BT.601"),
        };

        // ---- row pitch ----
        // Signed: a negative stride means a bottom-up buffer.
        let stride = unsafe { mt.GetUINT32(&MF_MT_DEFAULT_STRIDE) }
            .ok()
            .map(|v| (v as i32).unsigned_abs() as usize)
            .filter(|&s| s >= width as usize)
            .unwrap_or(width as usize);

        ColorInfo {
            range,
            matrix,
            source: range_src,
            matrix_note: None,
            y_stride: stride,
        }
        .with_matrix_note(matrix_src)
    }

    /// Fold the matrix's provenance into the one-line description. The range
    /// half is the one that decides the brightness, so it is named first.
    fn with_matrix_note(mut self, note: &'static str) -> ColorInfo {
        self.matrix_note = Some(note);
        self
    }

    pub fn describe(&self) -> String {
        let matrix = match self.matrix_note {
            Some(n) => format!("{} ({n})", self.matrix.name()),
            None => self.matrix.name().to_string(),
        };
        format!("{matrix}, {} ({})", self.range.name(), self.source)
    }
}

/// One pixel: YUV → RGB, integer fixed point, coefficients chosen by
/// [`ColorInfo`].
///
/// `u`/`v` are chroma already centred on zero (the stored byte minus 128).
/// Pure and side-effect free on purpose — the device is not always awake, so a
/// unit test over known ramps is the only way to prove what this does.
#[inline]
pub fn yuv_to_rgb(y: u8, u: i32, v: i32, c: &ColorInfo) -> (u8, u8, u8) {
    let (cr, cg_u, cg_v, cb) = coefficients(c);
    let base = luma_base(y, c);
    (
        clamp8((base + cr * v + 128) >> 8),
        clamp8((base - cg_u * u - cg_v * v + 128) >> 8),
        clamp8((base + cb * u + 128) >> 8),
    )
}

/// `(red-V, green-U, green-V, blue-U)`, ×256 fixed point, for a range/matrix pair.
///
/// Shared with [`crate::picture`], which needs the same numbers *after* the
/// user's curve has shaped the luma — one table, one place to get it right.
#[inline]
pub(crate) fn coefficients(c: &ColorInfo) -> (i32, i32, i32, i32) {
    match (c.range, c.matrix) {
        (ChromaRange::Limited, ColorMatrix::Bt601) => (409, 100, 208, 516),
        (ChromaRange::Limited, ColorMatrix::Bt709) => (459, 55, 136, 541),
        (ChromaRange::Full, ColorMatrix::Bt601) => (359, 88, 183, 454),
        (ChromaRange::Full, ColorMatrix::Bt709) => (403, 48, 120, 475),
    }
}

/// Luma in ×256 fixed point, before the chroma is added.
///
/// Studio swing expands 16–235 to 0–255 (298/256 = 1.164); full range passes
/// luma through untouched (256/256 = 1.0).
#[inline]
pub(crate) fn luma_base(y: u8, c: &ColorInfo) -> i32 {
    match c.range {
        ChromaRange::Limited => 298 * (y as i32 - 16),
        ChromaRange::Full => 256 * y as i32,
    }
}

#[inline]
pub(crate) fn clamp8(v: i32) -> u8 {
    v.clamp(0, 255) as u8
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
    /// When the sample was taken. The age of the newest frame at the moment it
    /// is drawn *is* the pipeline latency, so it is carried rather than
    /// estimated — "the picture is behind" deserves a number, not a hunch.
    pub captured_at: std::time::Instant,
    /// What the *source* luma plane looks like, measured during conversion.
    /// "It looks washed out to me" becomes "the source uses 19…231", which is a
    /// statement the app can act on.
    pub stats: crate::picture::PictureStats,
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
///
/// **Frames are deliberately not here.** They travel through [`FrameSlot`], which
/// holds exactly one frame; this channel carries only the rare, small, one-off
/// messages. Putting frames on an unbounded channel is what took this machine's
/// desktop down on 2026-10-11 — see `docs/CRASH_2026-10-11.md`.
pub enum CaptureMsg {
    /// The stream is live, at this size/framerate.
    Started {
        width: u32,
        height: u32,
        label: String,
        /// How the media type says the samples should be read, *before* any
        /// override. Carried as the struct rather than as a sentence, so the UI
        /// can show what the device says beside what the user chose.
        colour: ColorInfo,
    },
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
    ///
    /// `wake` is called after every frame is stored. The UI must redraw when a frame
    /// lands, and the alternative — redrawing as fast as the display allows, hoping
    /// to notice one — burns a core and adds jitter.
    ///
    /// `frames` is the single-slot hand-off to the UI. The caller keeps a clone;
    /// **nothing here needs to read it** for frames to arrive (the `--snapshot` and
    /// `--soak` modes poll it directly).
    ///
    /// `adjust` is shared with the UI. It is read **once per frame** (not per
    /// pixel), so moving a slider changes the picture on the next frame without
    /// restarting the stream, and a frame can never mix two settings.
    pub fn start(
        symbolic_link: String,
        format_label: String,
        flip_vertical: bool,
        tx: Sender<CaptureMsg>,
        wake: Box<dyn Fn() + Send>,
        frames: FrameSlot,
        adjust: Arc<Mutex<crate::picture::Adjust>>,
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
                    &wake,
                    &frames,
                    &adjust,
                ) {
                    let _ = tx.send(CaptureMsg::Stopped(e));
                    wake();
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

// ---------------------------------------------------------------------------
//  The frame slot: one frame, newest wins
// ---------------------------------------------------------------------------

/// How long the UI may collect nothing before the producer stops converting.
///
/// Deliberately generous: a *visible* window is woken by [`CaptureThread::start`]'s
/// `wake` callback on every frame, so it collects continuously; two seconds of
/// silence means the window is not being drawn at all (minimised, hidden — on
/// Windows those never receive `RedrawRequested`), not that it is briefly busy.
const BACKPRESSURE_QUIET: Duration = Duration::from_secs(2);

/// How long the producer sleeps per loop while nobody is collecting. It keeps
/// reading samples throughout, so the reader stays drained and the picture is
/// current the moment the window comes back.
const BACKPRESSURE_SLEEP: Duration = Duration::from_millis(50);

/// Where a captured frame waits for the UI — and the reason this app cannot grow
/// without bound.
///
/// A channel was the obvious choice and it was the wrong one:
/// `std::sync::mpsc::channel()` is **unbounded**, so a consumer that stops
/// consuming costs nothing at all until the queue has eaten the machine.
/// That is not hypothetical here. On 2026-10-11 this app reached 478 GB of
/// commit and took the desktop's compositor down with it, and the arithmetic
/// identifies the contents exactly:
///
/// ```text
/// 478,142,341,120 / 6,220,800  (one 1080p RGB frame) = 76,861.9 frames
/// 485,745,373,184 / 6,220,800                        = 78,084.1 frames
/// ```
///
/// A consumer legitimately *does* stop: on Windows a minimised or invisible
/// window never receives `RedrawRequested` — eframe documents that special case
/// itself — so the UI can stop collecting for hours while capture keeps producing
/// ~6 MB per frame.
///
/// So the slot holds exactly **one** frame. A newer frame replaces (and frees)
/// whatever was waiting, which is also what live video actually wants: draw the
/// newest frame, never a backlog. Memory is bounded by construction, not by the
/// UI being well behaved.
#[derive(Clone)]
pub struct FrameSlot(Arc<FrameSlotInner>);

struct FrameSlotInner {
    cell: Mutex<Option<Frame>>,
    /// Frames stored by the producer and frames collected by the UI; the gap
    /// between them is how the producer knows nobody is drawing.
    put: AtomicU64,
    taken: AtomicU64,
    /// Milliseconds from `born` to the last collection.
    last_take_ms: AtomicU64,
    born: Instant,
}

impl FrameSlot {
    pub fn new() -> Self {
        FrameSlot(Arc::new(FrameSlotInner {
            cell: Mutex::new(None),
            put: AtomicU64::new(0),
            taken: AtomicU64::new(0),
            last_take_ms: AtomicU64::new(0),
            born: Instant::now(),
        }))
    }

    /// Store the newest frame. Whatever was waiting is dropped right here — that
    /// *is* the bound: the slot can hold one frame, forever.
    pub fn put(&self, frame: Frame) {
        let mut cell = self.0.cell.lock().unwrap();
        *cell = Some(frame);
        self.0.put.fetch_add(1, Ordering::Relaxed);
    }

    /// Take the newest frame, if one is waiting.
    pub fn take(&self) -> Option<Frame> {
        let mut cell = self.0.cell.lock().unwrap();
        let frame = cell.take();
        if frame.is_some() {
            self.0.taken.fetch_add(1, Ordering::Relaxed);
            self.0
                .last_take_ms
                .store(self.0.born.elapsed().as_millis() as u64, Ordering::Relaxed);
        }
        frame
    }

    /// Is a frame waiting to be drawn?
    pub fn pending(&self) -> bool {
        self.0.cell.lock().unwrap().is_some()
    }

    /// Frames produced, frames collected — logged, so the numbers are visible.
    pub fn counters(&self) -> (u64, u64) {
        (
            self.0.put.load(Ordering::Relaxed),
            self.0.taken.load(Ordering::Relaxed),
        )
    }

    /// True when a frame is waiting *and* nothing has been collected for `quiet`.
    ///
    /// The bound already makes a stalled consumer harmless; this stops it being
    /// *wasteful*, by not decoding 50 frames a second into a slot nobody reads.
    pub fn quiet_for(&self, quiet: Duration) -> bool {
        if self.0.cell.lock().unwrap().is_none() {
            return false;
        }
        let since_born_ms = self.0.born.elapsed().as_millis() as u64;
        let last_ms = self.0.last_take_ms.load(Ordering::Relaxed);
        since_born_ms.saturating_sub(last_ms) >= quiet.as_millis() as u64
    }
}

impl Default for FrameSlot {
    fn default() -> Self {
        Self::new()
    }
}

fn capture_loop(
    symbolic_link: &str,
    format_label: &str,
    flip_vertical: bool,
    tx: &Sender<CaptureMsg>,
    stop: &AtomicBool,
    wake: &(dyn Fn() + Send),
    frames: &FrameSlot,
    adjust: &Mutex<crate::picture::Adjust>,
) -> Result<(), String> {
    let source = open_source(symbolic_link)?;
    let reader = create_reader(&source)?;

    // Choose the format: the requested one, or MF's own best default.
    // The frame size lives on the *media type*, not on the samples, so it is
    // captured here and handed to every conversion.
    let (width, height, label, color) = configure_reader(&reader, symbolic_link, format_label)?;
    // Say what the picture will be read as *and* what the user changed, on one
    // line: that pair is the first thing to look at when a picture is wrong.
    {
        let chosen = adjust.lock().unwrap();
        crate::logging::info(format!(
            "picture: {} | adjustments: {}",
            chosen.resolve(&color).describe(),
            chosen.describe()
        ));
    }
    let _ = tx.send(CaptureMsg::Started {
        width,
        height,
        label,
        colour: color,
    });

    let mut cb = CaptureBuffers::new();
    let mut mem_mark = Instant::now();
    let mut backing_off = false;
    loop {
        // The read below blocks for at most one frame, so checking here keeps
        // `CaptureThread::stop()` responsive.
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }

        // ---- the ceiling, checked once a second ----------------------------
        // The bound in `FrameSlot` means capture can no longer grow without
        // limit, and this is the backstop for whatever the *next* cause turns out
        // to be: if this app ever commits more than its ceiling, stop capturing
        // and say so, instead of growing until the machine's commit runs out and
        // the desktop's compositor dies with it. That is exactly what happened
        // on 2026-10-11 (docs/CRASH_2026-10-11.md).
        if mem_mark.elapsed() >= Duration::from_secs(1) {
            mem_mark = Instant::now();
            if let Some(used) = crate::proc_mem::over_ceiling() {
                let msg = format!(
                    "committed {:.0} MB, past this app's {:.0} MB ceiling — capture stopped to protect the machine",
                    crate::proc_mem::mb(used),
                    crate::proc_mem::mb(crate::proc_mem::ceiling_bytes()),
                );
                crate::logging::error(msg.clone());
                let _ = tx.send(CaptureMsg::Stopped(msg));
                wake();
                return Ok(());
            }
            // Recorded every second at debug level: enough to plot a trend from
            // the log if a leak is ever suspected again.
            let (put, taken) = frames.counters();
            crate::logging::debug(format!(
                "memory: {:.0} MB committed of a {:.0} MB ceiling; {put} frames produced, {taken} collected",
                crate::proc_mem::mb(crate::proc_mem::commit_bytes()),
                crate::proc_mem::mb(crate::proc_mem::ceiling_bytes()),
            ));
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

        // ---- backpressure --------------------------------------------------
        // The slot is bounded, so a UI that stops collecting can no longer cost
        // memory — but it can still cost a core, and a UI that stops collecting
        // is *normal* on Windows: a minimised window never receives
        // `RedrawRequested`, so `update()` stops while capture runs on. The
        // sample just read is dropped, which keeps the reader drained, so the
        // picture is current the instant the window comes back.
        if frames.quiet_for(BACKPRESSURE_QUIET) {
            if !backing_off {
                backing_off = true;
                crate::logging::info(format!(
                    "nothing collected for {} s — pausing conversion until the window draws again",
                    BACKPRESSURE_QUIET.as_secs()
                ));
            }
            std::thread::sleep(BACKPRESSURE_SLEEP);
            continue;
        }
        if backing_off {
            backing_off = false;
            crate::logging::info("window is drawing again — resuming conversion");
        }

        // Stamped as early as possible: everything after this is our latency,
        // and understating it would defeat the measurement.
        let captured_at = Instant::now();

        // The user's adjustments, resolved once for this frame: one lock per
        // frame (never per pixel), and no frame can mix two settings.
        let curve = {
            let chosen = adjust.lock().unwrap();
            crate::picture::Curve::build(&chosen, &color)
        };

        match convert_sample(
            &sample,
            &mut cb,
            flip_vertical,
            width,
            height,
            &curve,
            captured_at,
        ) {
            Ok(frame) => {
                // Newest wins. This replaces whatever was waiting, so the queue
                // is one frame long however long the UI stays away — the bound
                // is structural, not a promise the UI has to keep.
                frames.put(frame);
                // Wake the UI *because* a frame arrived, instead of letting it
                // spin at the display's refresh rate waiting to notice one.
                wake();
            }
            Err(e) => {
                let _ = tx.send(CaptureMsg::Warning(e));
                wake();
            }
        }
    }
}

/// Set the reader's output type to NV12 at the requested (or best) size.
///
/// Returns the chosen `(width, height, label)` plus the [`ColorInfo`] describing
/// how the arriving samples must be read.
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
) -> Result<(u32, u32, String, ColorInfo), String> {
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
            // NV12's row pitch is the luma width, not width×4 — the old
            // width×4 value was an RGB32 leftover and describes a buffer three
            // times wider than the one we get.
            let _ = out.SetUINT32(&MF_MT_DEFAULT_STRIDE, f.width);
        }

        reader
            .SetCurrentMediaType(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32, None, &out)
            .map_err(|e| format!("cannot set NV12 output (device busy?): {e}"))?;

        // Ask MF what it actually settled on.
        let actual = reader
            .GetCurrentMediaType(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32)
            .map_err(|e| format!("GetCurrentMediaType: {e}"))?;
        let f = media_type_to_format(&actual);
        let (w, h, label) = match &f {
            Some(f) => (f.width, f.height, f.label()),
            None => match &native {
                Some(f) => (f.width, f.height, f.label()),
                None => (0, 0, "unknown".to_string()),
            },
        };

        // Which device-side format are we decoding? It decides the fallback
        // when the media type carries no range attribute.
        let native_subtype = native
            .as_ref()
            .map(|f| f.subtype.clone())
            .or_else(|| f.as_ref().map(|f| f.subtype.clone()))
            .unwrap_or_default();
        let color = ColorInfo::from_media_type(&actual, &native_subtype, w);

        // Permanent: this line answers "why does the picture look like that",
        // and it is the first thing to check when it looks wrong.
        crate::logging::info(format!(
            "colour: {} | output type {}x{} stride {}",
            color.describe(),
            w,
            h,
            color.y_stride
        ));

        Ok((w, h, label, color))
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

/// Lock the sample's buffer and convert NV12 → packed RGB8.
///
/// The range and matrix come from the frame's [`crate::picture::Curve`], which
/// starts from what the decoder actually produced and then applies whatever the
/// user changed by hand — that is the difference between a correct picture and
/// one whose contrast is stretched and whose highlights are clipped flat.
fn convert_sample(
    sample: &IMFSample,
    cb: &mut CaptureBuffers,
    flip_vertical: bool,
    width: u32,
    height: u32,
    curve: &crate::picture::Curve,
    captured_at: std::time::Instant,
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
        // NV12 rows are `pitch` bytes apart, and the UV plane's rows match.
        let pitch = curve.color.y_stride.max(width as usize);
        let luma_bytes = pitch * height as usize;
        // Measured here and carried on the frame: the UI shows it, so a range
        // that is being read wrongly is visible as numbers, not as an opinion.
        let stats = {
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
            let y_part = luma_bytes.min(probe.len());
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
            // Where the luma actually sits, as percentages of the plane — the
            // measurement that separates "the picture is dark" from "the picture
            // never reaches black or white at all".
            let y_len = y_slice.len().max(1) as f64;
            let near_black = y_slice.iter().filter(|&&b| b <= 16).count() as f64;
            let near_white = y_slice.iter().filter(|&&b| b >= 235).count() as f64;
            let measured = crate::picture::PictureStats {
                y_min,
                y_max,
                y_mean: y_mean as f32,
                near_black_pct: (near_black / y_len * 100.0) as f32,
                near_white_pct: (near_white / y_len * 100.0) as f32,
            };
            crate::logging::trace(format!(
                "frame {width}x{height} buffer current={current} max={max_len} brightest={brightest} mean={mean:.2} | Y min={y_min} max={y_max} mean={y_mean:.2} | U/V mean={uv_mean:.2} | {} | {}",
                curve.color.describe(),
                measured.describe()
            ));
            measured
        };

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
            let uv_rows = h as usize / 2;
            let uv_bytes = pitch * uv_rows;
            if src.len() < luma_bytes + uv_bytes {
                return Err(format!(
                    "NV12 buffer too small: {} bytes for {w}x{h} (needs {} at stride {pitch})",
                    src.len(),
                    luma_bytes + uv_bytes
                ));
            }
            let uv_plane = &src[luma_bytes..];

            for y in 0..h as usize {
                let src_y = if flip_vertical { h as usize - 1 - y } else { y };
                let dst_row = y * packed;
                let lum_row = src_y * pitch;
                let chroma_row = (src_y / 2) * pitch;
                // 4:2:0 — two horizontal pixels share one chroma pair, so it is
                // read once per pair instead of recomputed for every pixel.
                let mut x = 0usize;
                while x < w as usize {
                    let cidx = chroma_row + (x / 2) * 2;
                    let u = uv_plane[cidx] as i32 - 128;
                    let v = uv_plane[cidx + 1] as i32 - 128;

                    let mut dx = 0usize;
                    while dx < 2 && x + dx < w as usize {
                        let (r, g, b) =
                            crate::picture::convert_pixel(src[lum_row + x + dx], u, v, curve);
                        let d = dst_row + (x + dx) * 3;
                        cb.rgb[d] = r;
                        cb.rgb[d + 1] = g;
                        cb.rgb[d + 2] = b;

                        if y == 0 && x + dx == 0 {
                            crate::logging::trace(format!(
                                "px0 y={} u={u} v={v} -> r={r} g={g} b={b} | {}",
                                src[lum_row + x + dx],
                                curve.color.describe()
                            ));
                        }
                        dx += 1;
                    }
                    x += 2;
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
                captured_at,
                stats,
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

    #[test]
    fn the_slot_keeps_one_frame_however_many_arrive_uncollected() {
        // The regression test for the 2026-10-11 crash: a consumer that stops
        // collecting used to cost 6 MB per frame without limit. Now the slot can
        // only ever hold one, so a thousand unconsumed frames cost one frame.
        let slot = FrameSlot::new();
        for width in 0..1000u32 {
            slot.put(dummy_frame(width));
        }
        assert_eq!(slot.counters(), (1000, 0));
        assert_eq!(slot.take().map(|f| f.width), Some(999), "newest must win");
        assert!(slot.take().is_none(), "only one frame may ever be waiting");
        assert!(!slot.pending());
    }

    #[test]
    fn the_slot_knows_when_nobody_is_collecting() {
        let slot = FrameSlot::new();
        // Nothing waiting yet: not a stall, just nothing to draw.
        assert!(!slot.quiet_for(Duration::ZERO));

        slot.put(dummy_frame(1));
        // A frame is waiting and nothing has ever collected: stalled.
        assert!(slot.quiet_for(Duration::ZERO));
        assert!(
            !slot.quiet_for(Duration::from_secs(3600)),
            "not stalled *that* long yet"
        );

        // Collecting clears it, and a fresh frame is not instantly stale.
        assert!(slot.take().is_some());
        assert!(
            !slot.quiet_for(Duration::ZERO),
            "a collected slot is not stalled"
        );
        slot.put(dummy_frame(2));
        assert!(
            !slot.quiet_for(Duration::from_secs(60)),
            "just collected, so not stalled"
        );
    }

    /// A frame of the given width with no pixels — the slot tests only care about
    /// identity and lifetime, not content.
    fn dummy_frame(width: u32) -> Frame {
        Frame {
            width,
            height: 1,
            rgb: Vec::new(),
            flipped: false,
            captured_at: Instant::now(),
            stats: Default::default(),
        }
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

    // -----------------------------------------------------------------------
    //  Colour maths. The device is not always awake, so a known ramp through a
    //  pure function is the only way to prove what the conversion does — and
    //  this is the maths that decided whether the picture looked right.
    // -----------------------------------------------------------------------

    fn color(range: ChromaRange, matrix: ColorMatrix) -> ColorInfo {
        ColorInfo {
            range,
            matrix,
            source: "test",
            matrix_note: None,
            y_stride: 0,
        }
    }

    const ALL: [(ChromaRange, ColorMatrix); 4] = [
        (ChromaRange::Limited, ColorMatrix::Bt601),
        (ChromaRange::Limited, ColorMatrix::Bt709),
        (ChromaRange::Full, ColorMatrix::Bt601),
        (ChromaRange::Full, ColorMatrix::Bt709),
    ];

    /// The shipped bug: studio-swing maths applied to full-range (MJPG) data.
    /// Luma 235 is 235 in a 0-255 source, but expanding it as if it were 16-235
    /// pushes it to 278 and clips it to flat white, stretching everything below
    /// by ~16%. That is the "washed out, too bright" picture.
    #[test]
    fn full_range_luma_is_neither_stretched_nor_clipped() {
        let full = color(ChromaRange::Full, ColorMatrix::Bt601);
        assert_eq!(yuv_to_rgb(0, 0, 0, &full).0, 0);
        assert_eq!(yuv_to_rgb(255, 0, 0, &full).0, 255);
        assert_eq!(yuv_to_rgb(235, 0, 0, &full).0, 235, "235 must stay 235");
        assert_eq!(yuv_to_rgb(128, 0, 0, &full).0, 128);

        let studio = color(ChromaRange::Limited, ColorMatrix::Bt601);
        assert_eq!(
            yuv_to_rgb(235, 0, 0, &studio).0,
            255,
            "and the old maths does clip it — this is the bug being fixed"
        );
    }

    /// Studio-swing input must still expand to the full 0-255 output: that half
    /// of the old behaviour was right and must stay right.
    #[test]
    fn studio_swing_luma_still_expands() {
        let studio = color(ChromaRange::Limited, ColorMatrix::Bt601);
        assert_eq!(yuv_to_rgb(16, 0, 0, &studio).0, 0);
        assert_eq!(yuv_to_rgb(235, 0, 0, &studio).0, 255);
        let mid = yuv_to_rgb(126, 0, 0, &studio).0;
        assert!((126..=131).contains(&mid), "mid grey ~128, got {mid}");
    }

    /// Every combination must agree on the things that define a colour space:
    /// grey stays grey, +V is red, +U is blue.
    #[test]
    fn chroma_leans_the_right_way_in_every_variant() {
        for (range, matrix) in ALL {
            let c = color(range, matrix);
            // A value in the middle of each range.
            let luma = match range {
                ChromaRange::Limited => 126,
                ChromaRange::Full => 128,
            };

            let (r, g, b) = yuv_to_rgb(luma, 0, 0, &c);
            assert!(
                r.abs_diff(g) <= 2 && g.abs_diff(b) <= 2,
                "{range:?}/{matrix:?}: zero chroma must stay grey, got {r},{g},{b}"
            );

            let (r_up, _, _) = yuv_to_rgb(luma, 0, 40, &c);
            let (r_dn, _, _) = yuv_to_rgb(luma, 0, -40, &c);
            assert!(r_up > r_dn, "{range:?}/{matrix:?}: +V must raise red");

            let (_, _, b_up) = yuv_to_rgb(luma, 40, 0, &c);
            assert!(b_up > b, "{range:?}/{matrix:?}: +U must raise blue");
        }
    }

    /// The attributes decide; the fallbacks only apply when the media type is
    /// silent. These are the codes from `mfobjects.h` — 1 is full, 2 is limited,
    /// and swapping them would be worse than having no fallback at all.
    #[test]
    fn nominal_range_codes_match_the_windows_header() {
        assert_eq!(NOMINAL_RANGE_0_255, 1);
        assert_eq!(NOMINAL_RANGE_16_235, 2);
        assert_eq!(MFVideoTransferMatrix_BT709.0, 1);
        assert_eq!(MFVideoTransferMatrix_BT601.0, 2);
    }
}

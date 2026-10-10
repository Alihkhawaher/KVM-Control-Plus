//! Video: MediaFoundation capture, the frame pipeline, and snapshot export.

pub mod mf;
pub mod usb_link;

pub use mf::{CaptureMsg, Frame};

use std::path::Path;

/// Write a frame as a 24-bit uncompressed **BMP**.
///
/// Deliberately dependency-free: the alternative would be pulling in an image
/// encoder crate, and a BMP is enough for "save a snapshot of the remote
/// screen". BMP is stored bottom-up with rows padded to 4 bytes.
pub fn write_bmp(path: &Path, frame: &Frame) -> Result<(), String> {
    use std::io::Write;

    let w = frame.width as usize;
    let h = frame.height as usize;
    if w == 0 || h == 0 {
        return Err("frame is empty".into());
    }
    let expected = w * h * 3;
    if frame.rgb.len() < expected {
        return Err(format!(
            "frame buffer too small: {} < {expected}",
            frame.rgb.len()
        ));
    }

    let row_bytes = w * 3;
    let padding = (4 - (row_bytes % 4)) % 4;
    let image_size = (row_bytes + padding) * h;
    let file_size = 14 + 40 + image_size;

    let mut out = Vec::with_capacity(file_size);
    // BITMAPFILEHEADER
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&(file_size as u32).to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&(14u32 + 40).to_le_bytes());
    // BITMAPINFOHEADER
    out.extend_from_slice(&40u32.to_le_bytes());
    out.extend_from_slice(&(w as i32).to_le_bytes());
    out.extend_from_slice(&(h as i32).to_le_bytes()); // positive => bottom-up
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&24u16.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // BI_RGB
    out.extend_from_slice(&(image_size as u32).to_le_bytes());
    out.extend_from_slice(&2835u32.to_le_bytes()); // ~72 dpi
    out.extend_from_slice(&2835u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());

    for y in (0..h).rev() {
        let row = &frame.rgb[y * row_bytes..y * row_bytes + row_bytes];
        for px in row.chunks_exact(3) {
            out.push(px[2]); // BMP is BGR
            out.push(px[1]);
            out.push(px[0]);
        }
        out.extend(std::iter::repeat(0u8).take(padding));
    }

    let mut f = std::fs::File::create(path).map_err(|e| format!("{}: {e}", path.display()))?;
    f.write_all(&out)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_a_valid_bmp_header() {
        let frame = Frame {
            width: 2,
            height: 2,
            rgb: vec![255, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 255],
            flipped: false,
        };
        let dir = std::env::temp_dir().join("kvm-control-plus-tests");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("snapshot.bmp");
        write_bmp(&path, &frame).unwrap();

        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(&bytes[0..2], b"BM");
        let declared = u32::from_le_bytes(bytes[2..6].try_into().unwrap()) as usize;
        assert_eq!(declared, bytes.len());
        // 2 rows, each 6 bytes of pixels + 2 bytes padding.
        assert_eq!(declared, 14 + 40 + 8 * 2);
        let width = i32::from_le_bytes(bytes[18..22].try_into().unwrap());
        let height = i32::from_le_bytes(bytes[22..26].try_into().unwrap());
        assert_eq!((width, height), (2, 2));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn rejects_an_empty_frame() {
        let frame = Frame {
            width: 0,
            height: 0,
            rgb: vec![],
            flipped: false,
        };
        let err = write_bmp(Path::new("x.bmp"), &frame).unwrap_err();
        assert!(err.contains("empty"));
    }
}

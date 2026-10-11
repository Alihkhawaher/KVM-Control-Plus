//! Picture adjustments — the user's knobs on top of the decode.
//!
//! Why this exists: the *range* of the video is a property of the whole chain
//! (source → dongle → decoder), and a cheap HDMI→USB dongle will happily hand
//! over limited-range (16-235) luma inside a format that claims full range
//! (0-255). Decoded as full range, that is a washed-out picture with no true
//! black and no true white — nothing in it is ever darker than ~6% grey. The
//! media type is the best guess available, but it is a guess; this is where the
//! user overrides what the device *says* with what the picture *shows*, and it is
//! also where brightness, contrast, saturation and gamma live.
//!
//! Every value is plain data behind one `Mutex`, read **once per frame** (the
//! conversion builds a [`Curve`] and uses it for all 2 million pixels), so a
//! frame can never mix two settings and the lock is never contended in a way
//! that matters. The conversion itself is a pure function, because the device is
//! not always awake and a unit test over known ramps is then the only way to
//! prove what this does.

use crate::video::mf::{self, ChromaRange, ColorInfo, ColorMatrix};

// ---------------------------------------------------------------------------
//  What the user chose
// ---------------------------------------------------------------------------

/// Override for the incoming range. `Auto` believes the media type.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RangeOverride {
    #[default]
    Auto,
    /// 0-255, no expansion.
    Full,
    /// 16-235, expanded to 0-255. This is the setting that fixes a washed-out
    /// picture from a device whose JPEG claims full range but is not.
    Limited,
}

/// Override for the colour-difference coefficients. `Auto` believes the media type.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MatrixOverride {
    #[default]
    Auto,
    Bt601,
    Bt709,
}

/// Everything the user can change about the picture, live.
///
/// Defaults are exactly neutral: a fresh install decodes the media type's own
/// answer and applies no correction, which is bit-for-bit what this app did
/// before the knobs existed.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Adjust {
    pub range: RangeOverride,
    pub matrix: MatrixOverride,
    /// −1.0 … +1.0. 0 = untouched. Displayed as a percentage.
    pub brightness: f32,
    /// 0.25 … 3.0, pivoting on mid grey. 1.0 = untouched.
    pub contrast: f32,
    /// 0.0 … 2.0 on the colour-difference signal. 1.0 = untouched, 0.0 = grey.
    pub saturation: f32,
    /// 0.4 … 2.5. 1.0 = untouched; >1 lifts the mid-tones, which is the other
    /// way to rescue a flat picture (it cannot invent black or white, though).
    pub gamma: f32,
}

impl Default for Adjust {
    fn default() -> Self {
        Adjust {
            range: RangeOverride::Auto,
            matrix: MatrixOverride::Auto,
            brightness: 0.0,
            contrast: 1.0,
            saturation: 1.0,
            gamma: 1.0,
        }
    }
}

impl Adjust {
    /// No correction at all: the fast path, and the state a new install is in.
    pub fn is_neutral(&self) -> bool {
        self.brightness == 0.0
            && self.contrast == 1.0
            && self.saturation == 1.0
            && self.gamma == 1.0
    }

    /// Is any range/matrix override in force?
    pub fn overrides_the_media_type(&self) -> bool {
        self.range != RangeOverride::Auto || self.matrix != MatrixOverride::Auto
    }

    /// Apply the overrides to what the media type said, keeping everything else.
    ///
    /// The provenance string is rewritten rather than left alone, because the UI
    /// prints it — "full 0-255 (media type says 0-255)" becoming "limited 16-235
    /// (you set it)" is how the user can tell at a glance which of the two is
    /// currently believed.
    pub fn resolve(&self, media: &ColorInfo) -> ColorInfo {
        let mut c = *media;
        match self.range {
            RangeOverride::Auto => {}
            RangeOverride::Full => {
                c.range = ChromaRange::Full;
                c.source = "you set full 0-255";
            }
            RangeOverride::Limited => {
                c.range = ChromaRange::Limited;
                c.source = "you set limited 16-235";
            }
        }
        match self.matrix {
            MatrixOverride::Auto => {}
            MatrixOverride::Bt601 => {
                c.matrix = ColorMatrix::Bt601;
                c.matrix_note = Some("you chose BT.601");
            }
            MatrixOverride::Bt709 => {
                c.matrix = ColorMatrix::Bt709;
                c.matrix_note = Some("you chose BT.709");
            }
        }
        c
    }

    /// One line for the GUI and the logs: what has been changed by hand.
    pub fn describe(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if self.range != RangeOverride::Auto {
            parts.push(format!("source range {}", self.range.label()));
        }
        if self.matrix != MatrixOverride::Auto {
            parts.push(format!("matrix {}", self.matrix.label()));
        }
        if self.brightness != 0.0 {
            parts.push(format!("brightness {:+.0}", self.brightness * 100.0));
        }
        if self.contrast != 1.0 {
            parts.push(format!("contrast ×{:.2}", self.contrast));
        }
        if self.saturation != 1.0 {
            parts.push(format!("saturation ×{:.2}", self.saturation));
        }
        if self.gamma != 1.0 {
            parts.push(format!("gamma {:.2}", self.gamma));
        }
        if parts.is_empty() {
            "none — the media type is believed".to_string()
        } else {
            parts.join(", ")
        }
    }

    // ---- persistence: short, human-editable strings in settings.conf --------

    pub fn range_key(self) -> &'static str {
        match self.range {
            RangeOverride::Auto => "auto",
            RangeOverride::Full => "full",
            RangeOverride::Limited => "limited",
        }
    }

    pub fn matrix_key(self) -> &'static str {
        match self.matrix {
            MatrixOverride::Auto => "auto",
            MatrixOverride::Bt601 => "601",
            MatrixOverride::Bt709 => "709",
        }
    }

    /// Lenient, like the rest of the settings file: anything unrecognised is
    /// `Auto`, never an error and never a panic.
    pub fn parse_range(v: &str, current: RangeOverride) -> RangeOverride {
        match v.trim().to_ascii_lowercase().as_str() {
            "auto" | "" => RangeOverride::Auto,
            "full" | "0-255" | "pc" => RangeOverride::Full,
            "limited" | "16-235" | "video" | "studio" => RangeOverride::Limited,
            _ => current,
        }
    }

    pub fn parse_matrix(v: &str, current: MatrixOverride) -> MatrixOverride {
        match v.trim().to_ascii_lowercase().as_str() {
            "auto" | "" => MatrixOverride::Auto,
            "601" | "bt601" | "bt.601" | "smpte170m" => MatrixOverride::Bt601,
            "709" | "bt709" | "bt.709" => MatrixOverride::Bt709,
            _ => current,
        }
    }
}

impl RangeOverride {
    pub fn label(self) -> &'static str {
        match self {
            RangeOverride::Auto => "auto",
            RangeOverride::Full => "full 0-255",
            RangeOverride::Limited => "limited 16-235",
        }
    }
}

impl MatrixOverride {
    pub fn label(self) -> &'static str {
        match self {
            MatrixOverride::Auto => "auto",
            MatrixOverride::Bt601 => "BT.601",
            MatrixOverride::Bt709 => "BT.709",
        }
    }
}

// ---------------------------------------------------------------------------
//  The per-frame curve
// ---------------------------------------------------------------------------

/// The adjustments resolved for one frame, in fixed point.
///
/// Built once per frame from an [`Adjust`] so the pixels never see a settings
/// change halfway down an image.
#[derive(Clone, Copy)]
pub struct Curve {
    /// The range/matrix actually in force, overrides already applied.
    pub color: ColorInfo,
    /// Contrast ×256 (256 = untouched).
    contrast: i32,
    /// Brightness in output levels, −128…+128 (0 = untouched).
    brightness: i32,
    /// Saturation ×256 (256 = untouched).
    saturation: i32,
    /// Luma response for the 0-255 that arrive after contrast and brightness.
    gamma: [u8; 256],
    /// Nothing changed: the conversion can take the original path untouched.
    neutral: bool,
}

impl Curve {
    /// Resolve `adjust` against the media type into per-frame constants.
    pub fn build(adjust: &Adjust, media: &ColorInfo) -> Curve {
        let color = adjust.resolve(media);

        // Clamped rather than trusted: the sliders cannot produce these values,
        // but a hand-edited settings file can, and a curve is not a place to
        // discover that a value was silly.
        let contrast = (adjust.contrast * 256.0).round().clamp(64.0, 768.0) as i32;
        let brightness = (adjust.brightness * 128.0).round().clamp(-128.0, 128.0) as i32;
        let saturation = (adjust.saturation * 256.0).round().clamp(0.0, 512.0) as i32;
        let gamma_exp = adjust.gamma.clamp(0.4, 2.5);

        let mut gamma = [0u8; 256];
        for (i, slot) in gamma.iter_mut().enumerate() {
            let x = i as f32 / 255.0;
            // out = in^(1/gamma), so gamma > 1 lifts the mid-tones (what a flat,
            // washed-out source wants) and gamma < 1 deepens them. Black and
            // white are fixed points at every gamma.
            *slot = (x.powf(1.0 / gamma_exp) * 255.0).round().clamp(0.0, 255.0) as u8;
        }

        Curve {
            color,
            contrast,
            brightness,
            saturation,
            gamma,
            neutral: contrast == 256 && brightness == 0 && saturation == 256 && gamma_exp == 1.0,
        }
    }
}

/// One pixel through the user's curve: YUV → RGB.
///
/// `u`/`v` are chroma already centred on zero (the stored byte minus 128).
/// At neutral settings this is [`mf::yuv_to_rgb`] and costs the same; anything
/// else runs the correction, in the order that keeps it predictable:
/// expand the range → contrast → brightness → gamma → saturation → matrix.
#[inline]
pub fn convert_pixel(y: u8, u: i32, v: i32, cur: &Curve) -> (u8, u8, u8) {
    if cur.neutral {
        return mf::yuv_to_rgb(y, u, v, &cur.color);
    }
    convert_with_curve(y, u, v, cur)
}

/// The corrected path. Separate from [`convert_pixel`] so a test can compare it
/// against the original conversion at neutral settings and prove they agree —
/// the guarantee that an untouched install sees exactly the old picture.
#[inline]
fn convert_with_curve(y: u8, u: i32, v: i32, cur: &Curve) -> (u8, u8, u8) {
    let (cr, cg_u, cg_v, cb) = mf::coefficients(&cur.color);
    // Luma in ×256 fixed point, as the original conversion computes it.
    let base = mf::luma_base(y, &cur.color);

    // Contrast pivots on mid grey (128), so it darkens the darks and lifts the
    // lights without moving the middle of the picture.
    let mut luma = (((base - 128 * 256) * cur.contrast) >> 8) + 128 * 256;
    luma += cur.brightness * 256;

    // Gamma works on the final 0-255 value. The sub-level remainder is carried
    // through unchanged, so at neutral settings this is the original maths
    // *exactly* (a value of 1192/256 must stay 1192/256, not round to 4.0), and
    // under a real gamma curve it is smooth rather than stepped. Anything outside
    // 0-255 — a limited source's super-black and super-white — passes through
    // untouched and is clamped at the end, so the curve never invents or hides a
    // signal that exists.
    let level = luma >> 8;
    let fraction = luma & 0xFF;
    let out = if (0..=255).contains(&level) {
        cur.gamma[level as usize] as i32 * 256 + fraction
    } else {
        luma
    };

    let (u, v) = ((u * cur.saturation) >> 8, (v * cur.saturation) >> 8);
    (
        mf::clamp8((out + cr * v + 128) >> 8),
        mf::clamp8((out - cg_u * u - cg_v * v + 128) >> 8),
        mf::clamp8((out + cb * u + 128) >> 8),
    )
}

// ---------------------------------------------------------------------------
//  What the source is actually doing
// ---------------------------------------------------------------------------

/// Statistics of a frame's **source** luma plane, before any adjustment.
///
/// This is the measurement behind the diagnosis: a washed-out picture is one
/// whose luma never approaches 0 or 255, and saying so in numbers is the
/// difference between "it looks flat to me" and "the source uses 18…232, so the
/// range is limited".
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PictureStats {
    pub y_min: u8,
    pub y_max: u8,
    pub y_mean: f32,
    /// Percentage of the luma plane at or below 16 (studio black).
    pub near_black_pct: f32,
    /// Percentage at or above 235 (studio white).
    pub near_white_pct: f32,
}

impl PictureStats {
    pub fn describe(&self) -> String {
        format!(
            "source luma {}…{} (mean {:.0}); {:.1}% at or below 16, {:.1}% at or above 235",
            self.y_min, self.y_max, self.y_mean, self.near_black_pct, self.near_white_pct
        )
    }

    /// True when the source stays inside the studio range: the signature of
    /// limited-range content inside a format that claims full range.
    ///
    /// The thresholds are deliberately loose (14/240 rather than 16/235), because
    /// this is a *diagnosis offered to the user*, not a decision: a picture with
    /// genuinely no black or white in it looks the same, and the cost of a wrong
    /// hint is one click.
    pub fn looks_limited_range(&self) -> bool {
        self.y_min >= 14 && self.y_max <= 240
    }

    /// The plain-language answer to "why does it look washed out?".
    ///
    /// Deliberately specific about what to do, because the settings that fix it
    /// are three clicks away in the Options tab.
    pub fn verdict(&self) -> &'static str {
        if self.looks_limited_range() {
            "The source never reaches black or white — that is what a washed-out picture looks like. \
             It is almost certainly limited-range video (16-235) arriving in a format that claims \
             full range: set Source range to \"limited 16-235\"."
        } else if self.y_min <= 3 && self.y_max >= 252 {
            "The source already uses the full range, so the media type is right and no range \
             override is needed."
        } else if self.y_max >= 252 && self.near_white_pct > 5.0 {
            "Bright detail is clipped at white — the range is being read as wider than the source \
             really is. Try Source range \"limited 16-235\"."
        } else {
            "The source uses a reasonable spread of luma; leave the range on auto unless the \
             picture still looks wrong to you."
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn media_full_709() -> ColorInfo {
        ColorInfo {
            range: ChromaRange::Full,
            matrix: ColorMatrix::Bt709,
            source: "media type says 0-255",
            matrix_note: Some("media type says BT.709"),
            y_stride: 1920,
        }
    }

    fn media_limited_601() -> ColorInfo {
        ColorInfo {
            range: ChromaRange::Limited,
            matrix: ColorMatrix::Bt601,
            source: "media type says 16-235",
            matrix_note: Some("media type says BT.601"),
            y_stride: 1920,
        }
    }

    /// The guarantee that an untouched install sees exactly the picture it saw
    /// before the knobs existed: the corrected path with neutral settings must
    /// agree, bit for bit, with the original conversion.
    #[test]
    fn the_corrected_path_agrees_with_the_original_at_neutral_settings() {
        let adjust = Adjust::default();
        for media in [media_full_709(), media_limited_601()] {
            let curve = Curve::build(&adjust, &media);
            assert!(curve.neutral, "defaults must be neutral");
            for y in 0..=255u8 {
                for &(u, v) in &[(0, 0), (-128, 0), (0, 127), (64, -64), (127, 127)] {
                    assert_eq!(
                        convert_with_curve(y, u, v, &curve),
                        mf::yuv_to_rgb(y, u, v, &media),
                        "y={y} u={u} v={v} differs from the original conversion"
                    );
                }
            }
        }
    }

    #[test]
    fn limited_range_expansion_reaches_true_black_and_white() {
        let curve = Curve::build(&Adjust::default(), &media_limited_601());
        let (r, g, b) = convert_pixel(16, 0, 0, &curve);
        assert_eq!((r, g, b), (0, 0, 0), "studio black must land on true black");
        let (r, g, b) = convert_pixel(235, 0, 0, &curve);
        assert_eq!(
            (r, g, b),
            (255, 255, 255),
            "studio white must land on true white"
        );
    }

    /// The washed-out symptom, and its cure, as numbers: with the range read as
    /// full, a limited source's "black" sits grey; switched to limited, it is a
    /// real black.
    #[test]
    fn the_range_override_is_what_removes_the_grey_black() {
        let media = media_full_709();
        let believed = Curve::build(&Adjust::default(), &media);
        let (grey, _, _) = convert_pixel(16, 0, 0, &believed);
        assert!(
            grey > 8,
            "expected a grey 'black' with the media type's answer, got {grey}"
        );

        let overridden = Adjust {
            range: RangeOverride::Limited,
            ..Default::default()
        };
        let fixed = Curve::build(&overridden, &media);
        let (black, _, _) = convert_pixel(16, 0, 0, &fixed);
        assert_eq!(black, 0, "the override must give a true black");
        assert!(
            fixed.color.source.contains("you set"),
            "the provenance must say so"
        );
    }

    #[test]
    fn contrast_pivots_on_mid_grey() {
        let media = media_full_709();
        let curve = Curve::build(
            &Adjust {
                contrast: 1.5,
                ..Default::default()
            },
            &media,
        );
        // Mid grey stays put; a dark gets darker and a light gets lighter.
        let (mid, _, _) = convert_pixel(128, 0, 0, &curve);
        assert!((mid as i32 - 128).abs() <= 1, "mid grey moved to {mid}");
        let (dark, _, _) = convert_pixel(64, 0, 0, &curve);
        assert!(dark < 64, "contrast did not darken {dark}");
        let (light, _, _) = convert_pixel(192, 0, 0, &curve);
        assert!(light > 192, "contrast did not lift {light}");
    }

    #[test]
    fn brightness_shifts_luma_by_the_expected_amount() {
        let curve = Curve::build(
            &Adjust {
                brightness: 0.25, // +32 output levels
                ..Default::default()
            },
            &media_full_709(),
        );
        let (out, _, _) = convert_pixel(100, 0, 0, &curve);
        assert!(
            (out as i32 - 132).abs() <= 2,
            "expected 100 + 32 = 132, got {out}"
        );
    }

    #[test]
    fn zero_saturation_is_grey_and_full_saturation_is_unchanged() {
        let media = media_full_709();
        let grey = Curve::build(
            &Adjust {
                saturation: 0.0,
                ..Default::default()
            },
            &media,
        );
        let (r, g, b) = convert_pixel(128, 60, -40, &grey);
        assert_eq!(
            (r, g),
            (r, b),
            "0 saturation must remove colour: {r},{g},{b}"
        );

        let full = Curve::build(&Adjust::default(), &media);
        assert_eq!(
            convert_pixel(128, 60, -40, &full),
            mf::yuv_to_rgb(128, 60, -40, &media)
        );
    }

    #[test]
    fn gamma_above_one_lifts_the_midtones_and_pins_black_and_white() {
        let curve = Curve::build(
            &Adjust {
                gamma: 2.0,
                ..Default::default()
            },
            &media_full_709(),
        );
        let (black, _, _) = convert_pixel(0, 0, 0, &curve);
        let (white, _, _) = convert_pixel(255, 0, 0, &curve);
        let (mid, _, _) = convert_pixel(64, 0, 0, &curve);
        assert_eq!(black, 0);
        assert_eq!(white, 255);
        assert!(mid > 64, "gamma 2.0 should lift 64, got {mid}");
    }

    #[test]
    fn absurd_settings_are_clamped_rather_than_believed() {
        let curve = Curve::build(
            &Adjust {
                contrast: 900.0,
                brightness: 900.0,
                saturation: -5.0,
                gamma: 0.0001,
                ..Default::default()
            },
            &media_full_709(),
        );
        // Nothing panics and every pixel still lands in range.
        for y in [0u8, 64, 128, 255] {
            let (r, g, b) = convert_pixel(y, 40, -40, &curve);
            let _ = (r, g, b); // u8 by construction; this asserts it compiles as such
        }
        assert!(!curve.neutral);
    }

    #[test]
    fn overrides_are_described_and_parsed_leniently() {
        let adjust = Adjust {
            range: RangeOverride::Limited,
            matrix: MatrixOverride::Bt601,
            brightness: 0.1,
            contrast: 1.2,
            saturation: 0.8,
            gamma: 1.4,
        };
        let text = adjust.describe();
        for needle in [
            "limited 16-235",
            "BT.601",
            "brightness",
            "contrast",
            "saturation",
            "gamma",
        ] {
            assert!(text.contains(needle), "{text:?} is missing {needle:?}");
        }
        assert_eq!(
            Adjust::default().describe(),
            "none — the media type is believed"
        );

        for spelling in ["limited", "16-235", "Video ", "STUDIO"] {
            assert_eq!(
                Adjust::parse_range(spelling, RangeOverride::Auto),
                RangeOverride::Limited,
                "{spelling} should parse as limited"
            );
        }
        for spelling in ["709", "BT.709", "bt709"] {
            assert_eq!(
                Adjust::parse_matrix(spelling, MatrixOverride::Auto),
                MatrixOverride::Bt709
            );
        }
        // Unknown text keeps what it had rather than resetting the user's choice.
        assert_eq!(
            Adjust::parse_range("banana", RangeOverride::Limited),
            RangeOverride::Limited
        );
        assert_eq!(
            Adjust::parse_matrix("", MatrixOverride::Bt601),
            MatrixOverride::Auto
        );
    }

    #[test]
    fn stats_recognise_a_washed_out_source() {
        // The signature from the live WhatsApp capture: nothing near black or white.
        let washed = PictureStats {
            y_min: 19,
            y_max: 231,
            y_mean: 141.0,
            near_black_pct: 0.2,
            near_white_pct: 0.0,
        };
        assert!(washed.looks_limited_range());
        assert!(washed.verdict().contains("limited 16-235"));
        assert!(washed.describe().contains("19…231"));
    }

    #[test]
    fn stats_leave_a_healthy_source_alone() {
        let healthy = PictureStats {
            y_min: 0,
            y_max: 255,
            y_mean: 120.0,
            near_black_pct: 12.0,
            near_white_pct: 9.0,
        };
        assert!(!healthy.looks_limited_range());
        assert!(healthy.verdict().contains("full range"));
        assert!(!healthy.verdict().contains("set Source range"));
    }
}

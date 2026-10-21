//! Night light (`COMPOSITOR.output.set_night_light`): a warmer white point on
//! every output, for the evening.
//!
//! It runs in the encode pass as a 3D LUT, like a monitor ICC profile
//! (`color::icc`), composed with the output's profile when it has one, so it
//! needs no CRTC gamma and reaches cross-GPU outputs and the hardware cursor
//! the same way the profile does.
//!
//! The white point is the Planckian locus at the asked temperature (Kang et
//! al. 2002's cubic approximation of its CIE 1931 xy) in linear sRGB, relative
//! to the locus at 6500 K so that temperature is neutral, and scaled so its
//! largest channel is 1: whites dim towards amber rather than clipping.
//! Composited values are decoded with the pure gamma the ICC source side uses,
//! scaled, and encoded again.

use super::icc::{IccLut, LUT_SIZE};

/// The temperature that leaves colours as they are.
pub const NEUTRAL_KELVIN: f32 = 6500.0;

/// The range of the locus approximation.
const MIN_KELVIN: f64 = 1667.0;
const MAX_KELVIN: f64 = 25000.0;

/// Linear sRGB of the Planckian locus at `kelvin`, unnormalised.
fn locus_rgb(kelvin: f64) -> [f64; 3] {
    let t = kelvin.clamp(MIN_KELVIN, MAX_KELVIN);
    let (t2, t3) = (t * t, t * t * t);
    let x = if t <= 4000.0 {
        -0.2661239e9 / t3 - 0.2343589e6 / t2 + 0.8776956e3 / t + 0.179910
    } else {
        -3.0258469e9 / t3 + 2.1070379e6 / t2 + 0.2226347e3 / t + 0.240390
    };
    let (x2, x3) = (x * x, x * x * x);
    let y = if t <= 2222.0 {
        -1.1063814 * x3 - 1.34811020 * x2 + 2.18555832 * x - 0.20219683
    } else if t <= 4000.0 {
        -0.9549476 * x3 - 1.37418593 * x2 + 2.09137015 * x - 0.16748867
    } else {
        3.0817580 * x3 - 5.87338670 * x2 + 3.75112997 * x - 0.37001483
    };
    // xyY with Y = 1 to XYZ, then XYZ to linear sRGB (D65).
    let (cx, cy, cz) = (x / y, 1.0, (1.0 - x - y) / y);
    [
        3.2404542 * cx - 1.5371385 * cy - 0.4985314 * cz,
        -0.9692660 * cx + 1.8760108 * cy + 0.0415560 * cz,
        0.0556434 * cx - 0.2040259 * cy + 1.0572252 * cz,
    ]
}

/// The linear-light gain per channel at `kelvin`: 1 for all three at
/// [`NEUTRAL_KELVIN`], the largest channel always 1.
pub fn white_point(kelvin: f32) -> [f32; 3] {
    let raw = locus_rgb(kelvin as f64);
    let neutral = locus_rgb(NEUTRAL_KELVIN as f64);
    let relative = [0, 1, 2].map(|channel| (raw[channel] / neutral[channel]).max(0.0));
    let peak = relative.iter().cloned().fold(f64::MIN, f64::max).max(f64::EPSILON);
    relative.map(|value| (value / peak) as f32)
}

/// The LUT that warms the composite to `kelvin`, then applies `profile` if
/// the output has one.
pub fn lut(kelvin: f32, profile: Option<&IccLut>) -> IccLut {
    let gamma = crate::backend::hdr_pipeline::sdr_reference_gamma();
    let gain = white_point(kelvin);
    // Separable: each channel's warmed value depends on that channel alone.
    let step = 1.0 / (LUT_SIZE - 1) as f32;
    let warmed = gain.map(|gain| {
        (0..LUT_SIZE)
            .map(|index| ((index as f32 * step).powf(gamma) * gain).powf(1.0 / gamma))
            .collect::<Vec<_>>()
    });
    let mut data = Vec::with_capacity(LUT_SIZE * LUT_SIZE * LUT_SIZE);
    for b in 0..LUT_SIZE {
        for g in 0..LUT_SIZE {
            for r in 0..LUT_SIZE {
                let value = [warmed[0][r], warmed[1][g], warmed[2][b]];
                data.push(profile.map_or(value, |profile| profile.sample(value)));
            }
        }
    }
    IccLut::from_data(
        profile.map_or_else(
            || format!("night light {kelvin:.0} K").into(),
            |profile| profile.path.clone(),
        ),
        data,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn neutral_leaves_colours_alone() {
        for channel in white_point(NEUTRAL_KELVIN) {
            assert!((channel - 1.0).abs() < 1e-5, "{channel}");
        }
        let lut = lut(NEUTRAL_KELVIN, None);
        for rgb in [[0.0, 0.0, 0.0], [1.0, 1.0, 1.0], [0.25, 0.5, 0.75]] {
            let out = lut.sample(rgb);
            for channel in 0..3 {
                assert!((out[channel] - rgb[channel]).abs() < 2e-3, "{rgb:?} -> {out:?}");
            }
        }
    }

    #[test]
    fn warmer_dims_blue_then_green_and_keeps_red() {
        let evening = white_point(3400.0);
        assert_eq!(evening[0], 1.0);
        assert!(evening[1] < 1.0 && evening[2] < evening[1], "{evening:?}");
        let candle = white_point(1900.0);
        assert!(candle[1] < evening[1] && candle[2] < evening[2], "{candle:?}");
        // Colder than neutral leans blue instead.
        let sky = white_point(9000.0);
        assert_eq!(sky[2], 1.0);
        assert!(sky[0] < 1.0, "{sky:?}");
    }

    #[test]
    fn white_turns_amber_through_the_lut() {
        let white = lut(3400.0, None).sample([1.0, 1.0, 1.0]);
        let gain = white_point(3400.0);
        let gamma = crate::backend::hdr_pipeline::sdr_reference_gamma();
        for channel in 0..3 {
            assert!((white[channel] - gain[channel].powf(1.0 / gamma)).abs() < 1e-3, "{white:?}");
        }
        assert_eq!(lut(3400.0, None).sample([0.0, 0.0, 0.0]), [0.0, 0.0, 0.0]);
    }

    #[test]
    fn each_lut_is_new() {
        assert_ne!(lut(3400.0, None).id, lut(3400.0, None).id);
    }
}

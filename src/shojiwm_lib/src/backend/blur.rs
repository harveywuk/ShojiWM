//! A Gaussian blur of a rasterized label or icon (the `blur` style), done
//! on the CPU when it is rasterized: three box blurs each way approximate a
//! Gaussian, over premultiplied pixels, with nothing outside the buffer.

/// The smallest blur a blurred raster makes room for, so a blur that
/// animates below it keeps one size (and one raster can stand in for
/// another while the next is made).
const ROOM_FOR: f32 = 6.0;

/// How far (logical px) a blur of standard deviation `blur` reaches: a
/// blurred label or icon is rasterized this much larger on every side, so
/// the blur is not cut at its edges.
pub fn padding(blur: f32) -> i32 {
    if blur > 0.0 { (blur.max(ROOM_FOR) * 3.0).ceil() as i32 } else { 0 }
}

/// A blur's standard deviation in steps of a quarter of a pixel, so a blur
/// that animates rasterizes a bounded number of times.
pub fn quantize(blur: f32) -> f32 {
    (blur.max(0.0) * 4.0).round() / 4.0
}

/// The widths of `passes` box blurs that together approximate a Gaussian of
/// standard deviation `sigma` (Kutskir's method).
fn box_widths(sigma: f32, passes: usize) -> Vec<usize> {
    let n = passes as f32;
    let ideal = (12.0 * sigma * sigma / n + 1.0).sqrt();
    let mut lower = ideal.floor() as i32;
    if lower % 2 == 0 {
        lower -= 1;
    }
    let lower = lower.max(1);
    let upper = lower + 2;
    let l = lower as f32;
    let small = ((12.0 * sigma * sigma - n * l * l - 4.0 * n * l - 3.0 * n) / (-4.0 * l - 4.0)).round();
    (0..passes)
        .map(|pass| if (pass as f32) < small { lower } else { upper } as usize)
        .collect()
}

/// Blur premultiplied 4-channel `pixels` (`width` × `height`) in place by
/// a Gaussian of standard deviation `sigma` physical px.
pub fn blur_premultiplied(pixels: &mut [u8], width: i32, height: i32, sigma: f32) {
    if sigma < 0.25 || width <= 0 || height <= 0 {
        return;
    }
    let (w, h) = (width as usize, height as usize);
    let mut a: Vec<f32> = pixels.iter().map(|&v| f32::from(v)).collect();
    let mut b = vec![0.0f32; a.len()];
    for size in box_widths(sigma, 3) {
        let radius = (size - 1) / 2;
        box_pass(&a, &mut b, w, h, radius, true);
        box_pass(&b, &mut a, w, h, radius, false);
    }
    for (out, value) in pixels.iter_mut().zip(a) {
        *out = value.round().clamp(0.0, 255.0) as u8;
    }
}

/// One box blur of `radius` along rows (`horizontal`) or columns.
fn box_pass(src: &[f32], dst: &mut [f32], w: usize, h: usize, radius: usize, horizontal: bool) {
    let (lines, length) = if horizontal { (h, w) } else { (w, h) };
    let index = |line: usize, at: usize| if horizontal { (line * w + at) * 4 } else { (at * w + line) * 4 };
    let scale = 1.0 / (2 * radius + 1) as f32;
    for line in 0..lines {
        for channel in 0..4 {
            let mut sum = 0.0f32;
            for at in 0..=radius.min(length - 1) {
                sum += src[index(line, at) + channel];
            }
            for at in 0..length {
                dst[index(line, at) + channel] = sum * scale;
                // Slide: take in the next sample, drop the oldest.
                if at + radius + 1 < length {
                    sum += src[index(line, at + radius + 1) + channel];
                }
                if at >= radius {
                    sum -= src[index(line, at - radius) + channel];
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn widths_approximate_the_gaussian() {
        // Three boxes of these widths have about the requested variance.
        for sigma in [1.0f32, 2.5, 6.0] {
            let variance: f32 = box_widths(sigma, 3).iter().map(|&w| ((w * w) as f32 - 1.0) / 12.0).sum();
            assert!((variance.sqrt() - sigma).abs() < 0.5, "{sigma}: {}", variance.sqrt());
        }
    }

    #[test]
    fn a_dot_spreads_and_keeps_its_weight() {
        let (w, h) = (21, 21);
        let mut pixels = vec![0u8; w * h * 4];
        let centre = (10 * w + 10) * 4;
        pixels[centre..centre + 4].copy_from_slice(&[255, 255, 255, 255]);
        blur_premultiplied(&mut pixels, w as i32, h as i32, 2.0);
        assert!(pixels[centre + 3] < 255 && pixels[centre + 3] > 0);
        // Neighbours are lit, symmetrically.
        let left = ((10 * w + 8) * 4) + 3;
        let right = ((10 * w + 12) * 4) + 3;
        assert!(pixels[left] > 0);
        assert_eq!(pixels[left], pixels[right]);
        assert!(padding(2.0) == 18 && padding(0.0) == 0 && padding(7.0) == 21);
        assert_eq!(quantize(1.13), 1.25);
    }
}

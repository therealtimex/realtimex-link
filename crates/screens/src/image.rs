//! Screenshots as pictures: packed BGRA, a small grayscale thumbnail for change detection,
//! scaling and JPEG. Shared by the recorder and Remote Control.

/// Thumbnail size every platform produces for comparison.
pub const THUMB_WIDTH: usize = 64;
pub const THUMB_HEIGHT: usize = 36;

/// Mean absolute difference of two equally sized grayscale thumbnails, 0.0 to 1.0.
pub fn thumb_diff(a: &[u8], b: &[u8]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 1.0;
    }
    let sum: u64 = a.iter().zip(b).map(|(x, y)| x.abs_diff(*y) as u64).sum();
    sum as f32 / (a.len() as f32 * 255.0)
}

/// A raw screenshot as captured: BGRA bytes, top-down rows of `stride` bytes.
pub struct Bgra<'a> {
    pub width: usize,
    pub height: usize,
    pub stride: usize,
    pub data: &'a [u8],
}

/// Area-average `img` down to `ow` x `oh`, calling `emit` with each output pixel's mean BGR.
fn box_sample(img: &Bgra, ow: usize, oh: usize, mut emit: impl FnMut([u32; 3])) {
    for oy in 0..oh {
        let (y0, y1) = (
            oy * img.height / oh,
            ((oy + 1) * img.height / oh).max(oy * img.height / oh + 1),
        );
        for ox in 0..ow {
            let (x0, x1) = (
                ox * img.width / ow,
                ((ox + 1) * img.width / ow).max(ox * img.width / ow + 1),
            );
            let mut sum = [0u32; 3];
            for y in y0..y1.min(img.height) {
                let row = &img.data[y * img.stride..];
                for x in x0..x1.min(img.width) {
                    let p = &row[x * 4..x * 4 + 3];
                    sum[0] += p[0] as u32;
                    sum[1] += p[1] as u32;
                    sum[2] += p[2] as u32;
                }
            }
            let n = ((y1.min(img.height) - y0) * (x1.min(img.width) - x0)).max(1) as u32;
            emit([sum[0] / n, sum[1] / n, sum[2] / n]);
        }
    }
}

/// THUMB_WIDTH x THUMB_HEIGHT grayscale (Rec. 601 luma) for change detection.
pub fn thumbnail_bgra(img: &Bgra) -> Vec<u8> {
    let mut out = Vec::with_capacity(THUMB_WIDTH * THUMB_HEIGHT);
    box_sample(img, THUMB_WIDTH, THUMB_HEIGHT, |[b, g, r]| {
        out.push(((299 * r + 587 * g + 114 * b) / 1000) as u8)
    });
    out
}

/// Size of `width` x `height` scaled down to at most `max_width` pixels wide.
pub fn fit_width(width: usize, height: usize, max_width: usize) -> (usize, usize) {
    if width > max_width {
        (max_width, (height * max_width / width).max(1))
    } else {
        (width, height)
    }
}

/// `img` area-averaged to `ow` x `oh`, as packed BGRA (stride `ow * 4`, alpha 255). The same
/// pixels as [`box_sample`], about 10x faster: each band of source rows is summed once into a row
/// of column totals, then each output pixel adds its columns and divides once (#2683: the live
/// view scales every changed frame, and per-pixel spans and divisions took 29 ms at 1920 -> 1280).
pub fn scale_bgra(img: &Bgra, ow: usize, oh: usize) -> Vec<u8> {
    let span = |o: usize, src: usize, out: usize| {
        let a = o * src / out;
        (a, ((o + 1) * src / out).max(a + 1).min(src))
    };
    let columns: Vec<(usize, usize)> = (0..ow).map(|ox| span(ox, img.width, ow)).collect();
    // All four bytes of a pixel, so the row loop is one flat add the compiler vectorizes.
    let mut totals = vec![0u32; img.width * 4];
    let mut packed = vec![0u8; ow * oh * 4];
    for (oy, line) in packed.chunks_exact_mut(ow * 4).enumerate() {
        let (y0, y1) = span(oy, img.height, oh);
        totals.fill(0);
        for y in y0..y1 {
            let row = &img.data[y * img.stride..y * img.stride + img.width * 4];
            for (total, &byte) in totals.iter_mut().zip(row) {
                *total += byte as u32;
            }
        }
        let rows = (y1 - y0) as u32;
        for (out, &(x0, x1)) in line.as_chunks_mut::<4>().0.iter_mut().zip(&columns) {
            let t = &totals[x0 * 4..x1 * 4];
            let [b, g, r] = match t.len() {
                4 => [t[0], t[1], t[2]],
                8 => [t[0] + t[4], t[1] + t[5], t[2] + t[6]],
                _ => t
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .fold([0, 0, 0], |s, p| [s[0] + p[0], s[1] + p[1], s[2] + p[2]]),
            };
            // sum / n as a multiply: exact while sum <= 255 * n and n stays below 2^15.
            let n = rows * (x1 - x0) as u32;
            if n < 1 << 15 {
                let inverse = (1u32 << 23).div_ceil(n);
                let div = |v: u32| ((v as u64 * inverse as u64) >> 23) as u8;
                out.copy_from_slice(&[div(b), div(g), div(r), 255]);
            } else {
                out.copy_from_slice(&[(b / n) as u8, (g / n) as u8, (r / n) as u8, 255]);
            }
        }
    }
    packed
}

/// JPEG (quality 75) of `img`, scaled down by area averaging to at most `max_width` pixels wide.
/// Returns the JPEG bytes and the encoded width and height.
pub fn encode_jpeg_bgra(img: &Bgra, max_width: usize) -> anyhow::Result<(Vec<u8>, usize, usize)> {
    let (ow, oh) = fit_width(img.width, img.height, max_width);
    let packed = scale_bgra(img, ow, oh);
    let mut jpeg = Vec::new();
    jpeg_encoder::Encoder::new(&mut jpeg, 75).encode(
        &packed,
        u16::try_from(ow)?,
        u16::try_from(oh)?,
        jpeg_encoder::ColorType::Bgra,
    )?;
    Ok((jpeg, ow, oh))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn half_image(w: usize, h: usize) -> (Vec<u8>, usize) {
        let stride = w * 4 + 8;
        let mut data = vec![0u8; stride * h];
        for y in 0..h {
            for x in 0..w {
                let px = if x < w / 2 {
                    [255, 0, 0, 255]
                } else {
                    [255, 255, 255, 255]
                };
                data[y * stride + x * 4..y * stride + x * 4 + 4].copy_from_slice(&px);
            }
        }
        (data, stride)
    }

    #[test]
    fn thumbnail_and_jpeg_from_bgra() {
        let (data, stride) = half_image(2560, 1440);
        let img = Bgra {
            width: 2560,
            height: 1440,
            stride,
            data: &data,
        };
        let thumb = thumbnail_bgra(&img);
        assert_eq!(thumb.len(), THUMB_WIDTH * THUMB_HEIGHT);
        assert!(thumb[0] < 40, "blue is dark in luma: {}", thumb[0]);
        assert_eq!(thumb[THUMB_WIDTH - 1], 255, "white");
        let (jpeg, w, h) = encode_jpeg_bgra(&img, 1920).unwrap();
        assert_eq!((w, h), (1920, 1080));
        assert_eq!(&jpeg[..2], &[0xFF, 0xD8], "JPEG start of image");
        assert!(
            jpeg.len() > 1000 && jpeg.len() < 200_000,
            "size {}",
            jpeg.len()
        );
        let (_, w, h) = encode_jpeg_bgra(&img, 4000).unwrap();
        assert_eq!((w, h), (2560, 1440), "never upscaled");
    }

    /// A thumbnail of one gray level.
    fn thumb(level: u8) -> Vec<u8> {
        vec![level; THUMB_WIDTH * THUMB_HEIGHT]
    }

    #[test]
    fn diff_bounds() {
        assert_eq!(thumb_diff(&thumb(0), &thumb(0)), 0.0);
        assert_eq!(thumb_diff(&thumb(0), &thumb(255)), 1.0);
        assert_eq!(thumb_diff(&[1, 2], &[1]), 1.0);
    }

    #[test]
    fn scaling_matches_area_averaging_pixel_for_pixel() {
        for &(w, h, ow, oh, pad) in &[
            (1920, 1080, 1280, 720, 0),
            (37, 23, 11, 7, 12),
            (5, 3, 5, 3, 4),
            (100, 1, 33, 1, 0),
        ] {
            let stride = w * 4 + pad;
            let data: Vec<u8> = (0..stride * h)
                .map(|i| ((i * 2654435761usize) >> 7) as u8)
                .collect();
            let img = Bgra {
                width: w,
                height: h,
                stride,
                data: &data,
            };
            let mut expected = Vec::new();
            box_sample(&img, ow, oh, |[b, g, r]| {
                expected.extend_from_slice(&[b as u8, g as u8, r as u8, 255])
            });
            assert_eq!(scale_bgra(&img, ow, oh), expected, "{w}x{h} -> {ow}x{oh}");
        }
    }
}

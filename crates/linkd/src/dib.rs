//! A pasted image as a Windows bitmap (#2747). Chromium,
//! Paint and Office paste an image from the clipboard's CF_DIB; the registered "PNG" format alone
//! pastes nothing there. rtx-control puts both: the PNG, and this bitmap.

use serde_json::{json, Value};

use crate::util::base64;

/// Larger pictures are pasted as PNG only (rtx-control's limit is 64 MB).
#[cfg(windows)]
const MAX_DIB_BYTES: usize = 64 * 1024 * 1024;
#[cfg(any(windows, test))]
const HEADER_BYTES: usize = 40;
#[cfg(windows)]
const MAX_DIB_PIXELS: usize = (MAX_DIB_BYTES - HEADER_BYTES) / 4;

/// RGBA pixels (top row first) as a CF_DIB: a BITMAPINFOHEADER, then 32-bit BI_RGB rows, bottom
/// row first, each pixel blue, green, red, alpha.
#[cfg(any(windows, test))]
fn dib_of_rgba(rgba: &[u8], width: usize, height: usize) -> Vec<u8> {
    let stride = width * 4;
    let mut dib = vec![0u8; HEADER_BYTES + stride * height];
    dib[0..4].copy_from_slice(&(HEADER_BYTES as u32).to_le_bytes());
    dib[4..8].copy_from_slice(&(width as i32).to_le_bytes());
    // Positive: bottom-up.
    dib[8..12].copy_from_slice(&(height as i32).to_le_bytes());
    dib[12..14].copy_from_slice(&1u16.to_le_bytes());
    dib[14..16].copy_from_slice(&32u16.to_le_bytes());
    dib[20..24].copy_from_slice(&((stride * height) as u32).to_le_bytes());
    for row in 0..height {
        let from = (height - 1 - row) * stride;
        let to = HEADER_BYTES + row * stride;
        for x in (0..stride).step_by(4) {
            dib[to + x] = rgba[from + x + 2];
            dib[to + x + 1] = rgba[from + x + 1];
            dib[to + x + 2] = rgba[from + x];
            dib[to + x + 3] = rgba[from + x + 3];
        }
    }
    dib
}

/// The PNG as a CF_DIB, or None (then the PNG is pasted alone). Its size comes from the header
/// first: a picture too large for a bitmap is never decoded.
#[cfg(windows)]
fn dib_of_png(png_bytes: &[u8]) -> Option<Vec<u8>> {
    use png::{ColorType, Decoder, Transformations};
    let mut decoder = Decoder::new(std::io::Cursor::new(png_bytes));
    decoder.set_transformations(Transformations::EXPAND | Transformations::STRIP_16);
    let mut reader = decoder.read_info().ok()?;
    let (width, height) = (reader.info().width as usize, reader.info().height as usize);
    if width == 0 || height == 0 || width.checked_mul(height)? > MAX_DIB_PIXELS {
        return None;
    }
    let mut buffer = vec![0u8; reader.output_buffer_size()];
    let frame = reader.next_frame(&mut buffer).ok()?;
    let pixels = &buffer[..frame.buffer_size()];
    let rgba: Vec<u8> = match frame.color_type {
        ColorType::Rgba => pixels.to_vec(),
        ColorType::Rgb => pixels
            .as_chunks::<3>()
            .0
            .iter()
            .flat_map(|[r, g, b]| [*r, *g, *b, 255])
            .collect(),
        ColorType::GrayscaleAlpha => pixels
            .as_chunks::<2>()
            .0
            .iter()
            .flat_map(|[gray, alpha]| [*gray, *gray, *gray, *alpha])
            .collect(),
        ColorType::Grayscale => pixels.iter().flat_map(|g| [*g, *g, *g, 255]).collect(),
        ColorType::Indexed => return None,
    };
    (rgba.len() == width * height * 4).then(|| dib_of_rgba(&rgba, width, height))
}

/// rtx-control's paste_image parameters: the PNG, and on Windows its bitmap too.
pub fn paste_image_params(png: &[u8]) -> Value {
    #[cfg_attr(not(windows), allow(unused_mut))]
    let mut params = json!({ "png": base64(png) });
    #[cfg(windows)]
    if let Some(dib) = dib_of_png(png) {
        params["dib"] = json!(base64(&dib));
    }
    params
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bottom_up_bgra() {
        // Two rows: red on top, blue below.
        let rgba = [255, 0, 0, 255, 0, 0, 255, 128];
        let dib = dib_of_rgba(&rgba, 1, 2);
        assert_eq!(&dib[0..4], &40u32.to_le_bytes());
        assert_eq!(&dib[8..12], &2i32.to_le_bytes());
        assert_eq!(&dib[40..44], &[255, 0, 0, 128]);
        assert_eq!(&dib[44..48], &[0, 0, 255, 255]);
    }
}

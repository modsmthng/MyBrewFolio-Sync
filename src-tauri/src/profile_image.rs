// SPDX-License-Identifier: GPL-3.0-or-later

use std::io::Cursor;

use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::Value;

const SIZE: usize = 300;
const PNG_PREFIX: &str = "data:image/png;base64,";
pub(crate) const DEVICE_IMAGE_BYTES: usize = 4 + SIZE * SIZE * 2;
const INVALID_IMAGE: &str = "The profile image must be a static 300x300 RGB PNG";

/// Match GaggiMate's WebUI: LVGL true-color header, then little-endian RGB565.
/// Decode before saving the profile, so a malformed image cannot cause a partial install.
pub(crate) fn device_image(image: Option<&Value>) -> Result<Option<Vec<u8>>, &'static str> {
    let Some(image) = image else { return Ok(None) };
    let image = image.as_str().ok_or(INVALID_IMAGE)?;
    if image.len() > 1024 * 1024 {
        return Err(INVALID_IMAGE);
    }
    let data = STANDARD
        .decode(image.strip_prefix(PNG_PREFIX).unwrap_or(image))
        .map_err(|_| INVALID_IMAGE)?;
    let mut decoder = png::Decoder::new(Cursor::new(data));
    decoder.set_limits(png::Limits {
        bytes: 2 * 1024 * 1024,
    });
    let mut reader = decoder.read_info().map_err(|_| INVALID_IMAGE)?;
    let info = reader.info();
    if info.width != SIZE as u32
        || info.height != SIZE as u32
        || info.color_type != png::ColorType::Rgb
        || info.bit_depth != png::BitDepth::Eight
        || info.animation_control.is_some()
    {
        return Err(INVALID_IMAGE);
    }
    let mut pixels = vec![0; SIZE * SIZE * 3];
    reader.next_frame(&mut pixels).map_err(|_| INVALID_IMAGE)?;
    reader.finish().map_err(|_| INVALID_IMAGE)?;
    let mut output = Vec::with_capacity(DEVICE_IMAGE_BYTES);
    let header = 4_u32 | ((SIZE as u32) << 10) | ((SIZE as u32) << 21);
    output.extend_from_slice(&header.to_le_bytes());
    for rgb in pixels.chunks_exact(3) {
        let pixel =
            ((rgb[0] as u16 & 0xf8) << 8) | ((rgb[1] as u16 & 0xfc) << 3) | (rgb[2] as u16 >> 3);
        output.extend_from_slice(&pixel.to_le_bytes());
    }
    Ok(Some(output))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::json;

    pub(crate) fn image_fixture() -> Value {
        let mut pixels = vec![0_u8; SIZE * SIZE * 3];
        let mut state = 173_u32;
        for pixel in &mut pixels {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            *pixel = state as u8;
        }
        pixels[..9].copy_from_slice(&[255, 0, 0, 0, 255, 0, 0, 0, 255]);
        let mut data = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut data, SIZE as u32, SIZE as u32);
            encoder.set_color(png::ColorType::Rgb);
            encoder.set_depth(png::BitDepth::Eight);
            encoder
                .write_header()
                .unwrap()
                .write_image_data(&pixels)
                .unwrap();
        }
        json!(format!("{PNG_PREFIX}{}", STANDARD.encode(data)))
    }

    #[test]
    fn converts_a_large_png_to_the_exact_gaggimate_binary_format() {
        let image = image_fixture();
        assert!(image.as_str().unwrap().len() > 64 * 1024);
        let binary = device_image(Some(&image)).unwrap().unwrap();
        assert_eq!(binary.len(), DEVICE_IMAGE_BYTES);
        assert_eq!(
            &binary[..4],
            &(4_u32 | (300 << 10) | (300 << 21)).to_le_bytes()
        );
        assert_eq!(&binary[4..10], &[0x00, 0xf8, 0xe0, 0x07, 0x1f, 0x00]);
        assert!(device_image(None).unwrap().is_none());
        let legacy = json!(image.as_str().unwrap().strip_prefix(PNG_PREFIX).unwrap());
        assert_eq!(device_image(Some(&legacy)).unwrap().unwrap(), binary);
    }

    #[test]
    fn rejects_invalid_or_oversized_images() {
        for image in [
            json!(null),
            json!({}),
            json!("data:image/png;base64,bm90IGEgcG5n"),
            json!("A".repeat(1024 * 1024 + 1)),
        ] {
            assert!(device_image(Some(&image)).is_err());
        }
        let mut data = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut data, 1, 1);
            encoder.set_color(png::ColorType::Rgb);
            encoder
                .write_header()
                .unwrap()
                .write_image_data(&[0, 0, 0])
                .unwrap();
        }
        assert!(device_image(Some(&json!(STANDARD.encode(data)))).is_err());
    }
}

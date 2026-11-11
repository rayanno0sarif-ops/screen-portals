//! Copied pictures. On the wire a picture is always a PNG, whatever the
//! operating system keeps on its clipboard, so every platform can read it.

use std::io::Cursor;

/// Pictures larger than this (in pixels) are not carried.
pub const MAX_PIXELS: u64 = 64_000_000;
const PNG_MAGIC: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

fn err<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

/// A quick check that bytes at least start like a PNG.
pub fn looks_like_png(bytes: &[u8]) -> bool {
    bytes.len() > 32 && bytes[..8] == PNG_MAGIC
}

/// Encode top-down BGRA pixels (as Windows hands them out) as an opaque PNG.
pub fn png_from_bgra(width: u32, height: u32, bgra: &[u8]) -> Result<Vec<u8>, String> {
    let pixels = width as u64 * height as u64;
    if pixels == 0 || pixels > MAX_PIXELS || bgra.len() as u64 != pixels * 4 {
        return Err("picture has an unusable size".into());
    }
    let mut rgb = Vec::with_capacity(pixels as usize * 3);
    for px in bgra.chunks_exact(4) {
        rgb.extend_from_slice(&[px[2], px[1], px[0]]);
    }
    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, width, height);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        // Speed matters more than size: this runs while the cursor crosses.
        encoder.set_compression(png::Compression::Fast);
        let mut writer = encoder.write_header().map_err(err)?;
        writer.write_image_data(&rgb).map_err(err)?;
    }
    Ok(out)
}

/// A decoded picture: top-down RGBA, eight bits per channel.
pub struct Rgba {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

/// Decode a PNG from the network. Sizes are checked before any large
/// allocation, so a hostile file cannot exhaust memory.
pub fn decode_png(bytes: &[u8]) -> Result<Rgba, String> {
    if !looks_like_png(bytes) || &bytes[12..16] != b"IHDR" {
        return Err("not a PNG picture".into());
    }
    // The size is in a fixed place at the start; read it ourselves so the
    // limit holds whatever the decoder would do with the rest.
    let claimed = |at: usize| u32::from_be_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]) as u64;
    if claimed(16) * claimed(20) > MAX_PIXELS {
        return Err("picture is too large".into());
    }
    let mut decoder = png::Decoder::new(Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().map_err(err)?;
    let (width, height) = {
        let info = reader.info();
        (info.width, info.height)
    };
    let count = width as u64 * height as u64;
    if count == 0 || count > MAX_PIXELS {
        return Err("picture is too large".into());
    }
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let frame = reader.next_frame(&mut buf).map_err(err)?;
    let channels = match frame.color_type {
        png::ColorType::Grayscale => 1,
        png::ColorType::GrayscaleAlpha => 2,
        png::ColorType::Rgb => 3,
        png::ColorType::Rgba => 4,
        png::ColorType::Indexed => return Err("unsupported picture".into()),
    };
    if frame.bit_depth != png::BitDepth::Eight || frame.line_size != width as usize * channels {
        return Err("unsupported picture".into());
    }
    let mut pixels = Vec::with_capacity(count as usize * 4);
    for px in buf[..frame.line_size * height as usize].chunks_exact(channels) {
        match channels {
            1 => pixels.extend_from_slice(&[px[0], px[0], px[0], 255]),
            2 => pixels.extend_from_slice(&[px[0], px[0], px[0], px[1]]),
            3 => pixels.extend_from_slice(&[px[0], px[1], px[2], 255]),
            _ => pixels.extend_from_slice(px),
        }
    }
    Ok(Rgba { width, height, pixels })
}

/// Build the classic Windows clipboard bitmap (a `BITMAPINFOHEADER` followed
/// by bottom-up 24-bit rows). Transparent areas are shown on white, because
/// programs disagree about transparency in this format; programs that
/// understand it read the PNG placed beside it instead.
pub fn dib_from_png(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let picture = decode_png(bytes)?;
    let (w, h) = (picture.width as usize, picture.height as usize);
    let stride = (w * 3 + 3) & !3;
    let mut dib = Vec::with_capacity(40 + stride * h);
    dib.extend_from_slice(&40u32.to_le_bytes()); // biSize
    dib.extend_from_slice(&(w as i32).to_le_bytes());
    dib.extend_from_slice(&(h as i32).to_le_bytes()); // positive: bottom-up
    dib.extend_from_slice(&1u16.to_le_bytes()); // planes
    dib.extend_from_slice(&24u16.to_le_bytes()); // bits per pixel
    dib.extend_from_slice(&0u32.to_le_bytes()); // BI_RGB
    dib.extend_from_slice(&((stride * h) as u32).to_le_bytes());
    dib.extend_from_slice(&[0u8; 16]); // resolution and palette fields
    for row in (0..h).rev() {
        let line = &picture.pixels[row * w * 4..(row + 1) * w * 4];
        for px in line.chunks_exact(4) {
            let a = px[3] as u32;
            let over_white = |c: u8| ((c as u32 * a + 255 * (255 - a) + 127) / 255) as u8;
            dib.extend_from_slice(&[over_white(px[2]), over_white(px[1]), over_white(px[0])]);
        }
        dib.resize(dib.len() + (stride - w * 3), 0);
    }
    Ok(dib)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bgra_survives_the_trip_through_png() {
        // 3x2: a red, a green and a blue pixel on top, grey below.
        let bgra = [
            0, 0, 255, 0, /**/ 0, 255, 0, 0, /**/ 255, 0, 0, 0, //
            10, 10, 10, 0, /**/ 128, 128, 128, 0, /**/ 250, 250, 250, 0,
        ];
        let png = png_from_bgra(3, 2, &bgra).unwrap();
        assert!(looks_like_png(&png));
        let back = decode_png(&png).unwrap();
        assert_eq!((back.width, back.height), (3, 2));
        assert_eq!(&back.pixels[..12], &[255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255]);
        assert_eq!(&back.pixels[12..16], &[10, 10, 10, 255]);
    }

    #[test]
    fn windows_bitmap_is_bottom_up_and_padded() {
        let bgra = [
            0, 0, 255, 0, /**/ 0, 255, 0, 0, /**/ 255, 0, 0, 0, //
            10, 10, 10, 0, /**/ 128, 128, 128, 0, /**/ 250, 250, 250, 0,
        ];
        let dib = dib_from_png(&png_from_bgra(3, 2, &bgra).unwrap()).unwrap();
        // Rows of 3 pixels are 9 bytes, padded to 12.
        assert_eq!(dib.len(), 40 + 12 * 2);
        assert_eq!(&dib[4..8], &3i32.to_le_bytes());
        assert_eq!(&dib[8..12], &2i32.to_le_bytes());
        // The first stored row is the bottom one, in blue-green-red order.
        assert_eq!(&dib[40..43], &[10, 10, 10]);
        assert_eq!(&dib[52..61], &[0, 0, 255, 0, 255, 0, 255, 0, 0]);
    }

    #[test]
    fn transparency_is_shown_on_white() {
        let mut png = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut png, 2, 1);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().unwrap();
            // One fully transparent black pixel, one opaque black pixel.
            writer.write_image_data(&[0, 0, 0, 0, 0, 0, 0, 255]).unwrap();
        }
        let dib = dib_from_png(&png).unwrap();
        assert_eq!(&dib[40..46], &[255, 255, 255, 0, 0, 0]);
    }

    #[test]
    fn junk_and_oversized_pictures_are_refused() {
        assert!(decode_png(b"definitely not a picture, just some text that is long enough").is_err());
        assert!(png_from_bgra(0, 5, &[]).is_err());
        assert!(png_from_bgra(2, 2, &[0; 15]).is_err());
        // A header that claims 60000 x 60000 pixels is refused before decoding.
        let mut huge = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut huge, 60_000, 60_000);
            encoder.set_color(png::ColorType::Grayscale);
            encoder.set_depth(png::BitDepth::Eight);
            let _ = encoder.write_header().unwrap();
        }
        huge.resize(huge.len().max(64), 0);
        assert_eq!(decode_png(&huge).err().as_deref(), Some("picture is too large"));
    }
}

//! Image thumbnails: decoding on worker threads and the RGBA image atlas.
//!
//! [`Painter::image`](crate::paint::Painter::image) asks the [`ImageAtlas`]
//! for a thumbnail of a file at an exact pixel size. A miss is queued; the
//! app decodes it on a worker ([`thumbnail`]) and hands the pixels back with
//! [`ImageAtlas::insert`]. Decoded pixels are not kept on the CPU: an image
//! evicted from the atlas is decoded again when it scrolls back into view,
//! so memory stays bounded by the atlas.
//!
//! PNG and JPEG are decoded (the formats of screenshots and photos); other
//! images keep their file chip.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Read;

use crate::atlas::Shelves;
use crate::text::{ATLAS_SIZE, Upload, pad};

/// Largest file read for a thumbnail.
const MAX_FILE: u64 = 32 << 20;
/// Largest image decoded, in pixels (about 190 MB of RGBA).
const MAX_PIXELS: usize = 48_000_000;
/// Largest thumbnail side, in pixels.
const MAX_THUMB: u32 = 1024;

/// A thumbnail: the file and the exact size it is drawn at.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ImageKey {
    /// Absolute path of the image file.
    pub path: String,
    /// Width in physical pixels.
    pub w: u32,
    /// Height in physical pixels.
    pub h: u32,
}

/// Whether a thumbnail can be made for a file of this MIME type.
#[must_use]
pub fn supported(mime: &str) -> bool {
    matches!(mime, "image/png" | "image/jpeg")
}

/// What [`ImageAtlas::lookup`] found.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Lookup {
    /// In the atlas at this texel rectangle.
    Ready([f32; 4]),
    /// Being decoded (or queued).
    Loading,
    /// The file is not a readable image.
    Failed,
}

/// CPU side of the image atlas.
pub struct ImageAtlas {
    entries: HashMap<ImageKey, ([f32; 4], u16)>,
    shelves: Shelves,
    /// Decodes in flight, and misses not yet handed to [`ImageAtlas::take_wanted`].
    pending: HashSet<ImageKey>,
    wanted: Vec<ImageKey>,
    /// Files that did not decode; not retried.
    failed: HashSet<String>,
    /// Thumbnails not yet copied to the GPU texture.
    pub uploads: Vec<Upload>,
}

impl Default for ImageAtlas {
    fn default() -> Self {
        Self {
            entries: HashMap::new(),
            shelves: Shelves::new(ATLAS_SIZE),
            pending: HashSet::new(),
            wanted: Vec::new(),
            failed: HashSet::new(),
            uploads: Vec::new(),
        }
    }
}

impl ImageAtlas {
    /// Starts a frame, for least-recently-used eviction.
    pub fn next_frame(&mut self) {
        self.shelves.next_frame();
    }

    /// Finds a thumbnail for drawing this frame, queueing a decode on a miss.
    pub fn lookup(&mut self, key: &ImageKey) -> Lookup {
        if let Some(&(uv, shelf)) = self.entries.get(key) {
            self.shelves.touch(shelf);
            return Lookup::Ready(uv);
        }
        if self.failed.contains(&key.path) {
            return Lookup::Failed;
        }
        if key.w == 0 || key.h == 0 || key.w > MAX_THUMB || key.h > MAX_THUMB {
            return Lookup::Failed;
        }
        if self.pending.insert(key.clone()) {
            self.wanted.push(key.clone());
        }
        Lookup::Loading
    }

    /// Thumbnails to decode, each handed out once.
    pub fn take_wanted(&mut self) -> Vec<ImageKey> {
        std::mem::take(&mut self.wanted)
    }

    /// Stores a decoded thumbnail (`key.w`×`key.h` straight-alpha RGBA), or
    /// remembers that the file could not be decoded.
    pub fn insert(&mut self, key: ImageKey, pixels: Result<Vec<u8>, String>) {
        self.pending.remove(&key);
        let pixels = match pixels {
            Ok(pixels) if pixels.len() == key.w as usize * key.h as usize * 4 => pixels,
            Ok(_) => return,
            Err(e) => {
                eprintln!("serechat: no thumbnail for {}: {e}", key.path);
                self.failed.insert(key.path);
                return;
            }
        };
        // Everything on screen is in use; it is decoded again when drawn.
        let Some(slot) = self.shelves.alloc(key.w + 1, key.h + 1) else { return };
        if let Some(shelf) = slot.evicted {
            self.entries.retain(|_, (_, s)| *s != shelf);
        }
        self.uploads.push(Upload { x: slot.x, y: slot.y, w: key.w + 1, h: key.h + 1, data: pad(&pixels, key.w, key.h, 4) });
        let uv = [slot.x as f32, slot.y as f32, key.w as f32, key.h as f32];
        self.entries.insert(key, (uv, slot.shelf));
    }
}

/// Decodes the image at `key.path` and scales it to cover `key.w`×`key.h`,
/// cropping the overflow evenly. Slow: run it on a worker thread.
///
/// # Errors
/// A human-readable reason: unreadable, too large, or not a PNG or JPEG.
pub fn thumbnail(key: &ImageKey) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    fs::File::open(&key.path)
        .and_then(|f| f.take(MAX_FILE + 1).read_to_end(&mut bytes))
        .map_err(|e| e.to_string())?;
    if bytes.len() as u64 > MAX_FILE {
        return Err("the file is too large".into());
    }
    let (rgba, w, h, orientation) = if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        let (rgba, w, h) = decode_png(&bytes)?;
        (rgba, w, h, 1)
    } else if bytes.starts_with(&[0xFF, 0xD8]) {
        decode_jpeg(&bytes)?
    } else {
        return Err("not a PNG or JPEG image".into());
    };
    // Orientations 5–8 swap the axes: scale for the transposed size.
    let swap = orientation >= 5;
    let (tw, th) = if swap { (key.h, key.w) } else { (key.w, key.h) };
    let small = cover(&rgba, w, h, tw, th);
    Ok(orient(&small, tw, th, orientation))
}

/// Decodes a PNG to straight-alpha RGBA.
fn decode_png(bytes: &[u8]) -> Result<(Vec<u8>, u32, u32), String> {
    let mut decoder = png::Decoder::new_with_limits(std::io::Cursor::new(bytes), png::Limits { bytes: MAX_PIXELS * 4 });
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info().map_err(|e| e.to_string())?;
    let size = reader.output_buffer_size().ok_or("the image is too large")?;
    let mut buf = vec![0; size];
    let info = reader.next_frame(&mut buf).map_err(|e| e.to_string())?;
    let (w, h) = (info.width, info.height);
    if w as usize * h as usize > MAX_PIXELS {
        return Err("the image is too large".into());
    }
    let rows = buf.chunks_exact(info.line_size).take(h as usize);
    let mut rgba = Vec::with_capacity(w as usize * h as usize * 4);
    for row in rows {
        let row = &row[..(w as usize * info.color_type.samples()).min(row.len())];
        match info.color_type {
            png::ColorType::Rgba => rgba.extend_from_slice(row),
            png::ColorType::Rgb => row.chunks_exact(3).for_each(|p| rgba.extend_from_slice(&[p[0], p[1], p[2], 255])),
            png::ColorType::GrayscaleAlpha => row.chunks_exact(2).for_each(|p| rgba.extend_from_slice(&[p[0], p[0], p[0], p[1]])),
            png::ColorType::Grayscale => row.iter().for_each(|&g| rgba.extend_from_slice(&[g, g, g, 255])),
            png::ColorType::Indexed => return Err("unexpanded palette image".into()),
        }
    }
    if rgba.len() != w as usize * h as usize * 4 {
        return Err("truncated image data".into());
    }
    Ok((rgba, w, h))
}

/// Decodes a JPEG to RGBA, with its EXIF orientation (1 when absent).
fn decode_jpeg(bytes: &[u8]) -> Result<(Vec<u8>, u32, u32, u8), String> {
    use zune_jpeg::zune_core::bytestream::ZCursor;
    use zune_jpeg::zune_core::colorspace::ColorSpace;
    use zune_jpeg::zune_core::options::DecoderOptions;

    let side = (MAX_PIXELS as f64).sqrt() as usize * 2;
    let options = DecoderOptions::default().jpeg_set_out_colorspace(ColorSpace::RGBA).set_max_width(side).set_max_height(side);
    let mut decoder = zune_jpeg::JpegDecoder::new_with_options(ZCursor::new(bytes), options);
    decoder.decode_headers().map_err(|e| e.to_string())?;
    let info = decoder.info().ok_or("missing image header")?;
    let (w, h) = (u32::from(info.width), u32::from(info.height));
    if w as usize * h as usize > MAX_PIXELS {
        return Err("the image is too large".into());
    }
    let orientation = decoder.exif().map_or(1, |exif| exif_orientation(exif));
    let len = w as usize * h as usize * 4;
    let mut rgba = vec![0; decoder.output_buffer_size().ok_or("missing image header")?.max(len)];
    decoder.decode_into(&mut rgba).map_err(|e| e.to_string())?;
    rgba.truncate(len);
    Ok((rgba, w, h, orientation))
}

/// The orientation tag (1–8) of EXIF data starting at its TIFF header; 1 if
/// absent or malformed.
fn exif_orientation(exif: &[u8]) -> u8 {
    let big = match exif.get(..4) {
        Some(b"MM\0*") => true,
        Some(b"II*\0") => false,
        _ => return 1,
    };
    let u16_at = |at: usize| exif.get(at..at + 2).map(|b| if big { u16::from_be_bytes([b[0], b[1]]) } else { u16::from_le_bytes([b[0], b[1]]) });
    let u32_at = |at: usize| {
        exif.get(at..at + 4).map(|b| if big { u32::from_be_bytes([b[0], b[1], b[2], b[3]]) } else { u32::from_le_bytes([b[0], b[1], b[2], b[3]]) })
    };
    let Some(ifd) = u32_at(4).map(|o| o as usize) else { return 1 };
    let count = u16_at(ifd).unwrap_or(0);
    for i in 0..usize::from(count) {
        let entry = ifd + 2 + i * 12;
        if u16_at(entry) == Some(0x0112) {
            return u16_at(entry + 8).filter(|o| (1..=8).contains(o)).map_or(1, |o| o as u8);
        }
    }
    1
}

/// Scales `src` (`sw`×`sh` RGBA) to cover `dw`×`dh`, cropping the overflow
/// evenly, by averaging every source pixel under each target pixel (with
/// alpha weighting, so transparent edges do not darken).
fn cover(src: &[u8], sw: u32, sh: u32, dw: u32, dh: u32) -> Vec<u8> {
    let mut out = vec![0; dw as usize * dh as usize * 4];
    if sw == 0 || sh == 0 || dw == 0 || dh == 0 {
        return out;
    }
    // Source pixels per target pixel, the same on both axes.
    let step = f64::from(sw) / f64::from(dw);
    let step = step.min(f64::from(sh) / f64::from(dh));
    let x0 = (f64::from(sw) - step * f64::from(dw)) * 0.5;
    let y0 = (f64::from(sh) - step * f64::from(dh)) * 0.5;
    let span = |start: f64, i: u32, limit: u32| {
        let a = ((start + step * f64::from(i)) as u32).min(limit - 1);
        let b = ((start + step * f64::from(i + 1)) as u32).clamp(a + 1, limit);
        a..b
    };
    for ty in 0..dh {
        let rows = span(y0, ty, sh);
        for tx in 0..dw {
            let cols = span(x0, tx, sw);
            let mut sum = [0u64; 4];
            for y in rows.clone() {
                let row = y as usize * sw as usize;
                for x in cols.clone() {
                    let p = &src[(row + x as usize) * 4..][..4];
                    let a = u64::from(p[3]);
                    sum[0] += u64::from(p[0]) * a;
                    sum[1] += u64::from(p[1]) * a;
                    sum[2] += u64::from(p[2]) * a;
                    sum[3] += a;
                }
            }
            let n = u64::from(rows.end - rows.start) * u64::from(cols.end - cols.start);
            let o = &mut out[(ty as usize * dw as usize + tx as usize) * 4..][..4];
            if sum[3] > 0 {
                for c in 0..3 {
                    o[c] = (sum[c] / sum[3]) as u8;
                }
            }
            o[3] = (sum[3] / n) as u8;
        }
    }
    out
}

/// Applies EXIF `orientation` to `src` (`w`×`h` RGBA). Orientations 5–8
/// return an `h`×`w` image.
fn orient(src: &[u8], w: u32, h: u32, orientation: u8) -> Vec<u8> {
    if !(2..=8).contains(&orientation) {
        return src.to_vec();
    }
    let (w, h) = (w as usize, h as usize);
    let swap = orientation >= 5;
    let out_w = if swap { h } else { w };
    let mut out = vec![0; src.len()];
    for y in 0..h {
        for x in 0..w {
            // Where source pixel (x, y) lands in the output.
            let (ox, oy) = match orientation {
                2 => (w - 1 - x, y),
                3 => (w - 1 - x, h - 1 - y),
                4 => (x, h - 1 - y),
                5 => (y, x),
                6 => (h - 1 - y, x),
                7 => (h - 1 - y, w - 1 - x),
                _ => (y, w - 1 - x),
            };
            let from = (y * w + x) * 4;
            let to = (oy * out_w + ox) * 4;
            out[to..to + 4].copy_from_slice(&src[from..from + 4]);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(w: u32, h: u32, pixels: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut encoder = png::Encoder::new(&mut out, w, h);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.write_header().unwrap().write_image_data(pixels).unwrap();
        out
    }

    #[test]
    fn decodes_crops_and_rejects() {
        let dir = std::env::temp_dir().join(format!("serechat-thumb-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        // 4×2: left half red, right half blue.
        let red_blue: Vec<u8> = (0..8).flat_map(|i| if i % 4 < 2 { [255, 0, 0] } else { [0, 0, 255] }).collect();
        let path = dir.join("a.png");
        fs::write(&path, png(4, 2, &red_blue)).unwrap();
        let key = |w, h| ImageKey { path: path.to_string_lossy().into_owned(), w, h };

        // Covering 2×2 keeps the middle: half red, half blue.
        let square = thumbnail(&key(2, 2)).unwrap();
        assert_eq!(&square[..4], [255, 0, 0, 255]);
        assert_eq!(&square[4..8], [0, 0, 255, 255]);
        // Upscaling repeats pixels.
        assert_eq!(thumbnail(&key(8, 4)).unwrap().len(), 8 * 4 * 4);

        fs::write(&path, b"\x89PNG\r\n\x1a\ngarbage").unwrap();
        assert!(thumbnail(&key(2, 2)).is_err());
        fs::write(&path, [0xFF, 0xD8, 0xFF, 0x00, 0x01]).unwrap();
        assert!(thumbnail(&key(2, 2)).is_err());
        fs::write(&path, b"GIF89a").unwrap();
        assert!(thumbnail(&key(2, 2)).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn transparent_pixels_do_not_darken() {
        // Top row opaque white, bottom row transparent black.
        let src = [[255u8; 8], [0; 8]].concat();
        assert_eq!(cover(&src, 2, 2, 1, 1), [255, 255, 255, 127]);
    }

    #[test]
    fn exif_orientation_is_bounded_and_applied() {
        // Little-endian TIFF header, IFD at 8 with one orientation entry = 6.
        let mut exif = b"II*\0\x08\0\0\0\x01\0".to_vec();
        exif.extend_from_slice(&[0x12, 0x01, 3, 0, 1, 0, 0, 0, 6, 0, 0, 0]);
        assert_eq!(exif_orientation(&exif), 6);
        assert_eq!(exif_orientation(&exif[..12]), 1, "truncated data is ignored");
        assert_eq!(exif_orientation(b"MM\0*\xFF\xFF\xFF\xFF"), 1);

        // A 2×1 image (A, B) rotated by 6 (90° clockwise) becomes 1×2: A over B.
        let (a, b) = ([1, 1, 1, 1], [2, 2, 2, 2]);
        let rotated = orient(&[a, b].concat(), 2, 1, 6);
        assert_eq!(rotated, [a, b].concat());
        // Orientation 8 (90° counter-clockwise): B over A.
        assert_eq!(orient(&[a, b].concat(), 2, 1, 8), [b, a].concat());
    }

    #[test]
    fn atlas_queues_each_miss_once() {
        let mut atlas = ImageAtlas::default();
        let key = ImageKey { path: "x".into(), w: 2, h: 2 };
        assert_eq!(atlas.lookup(&key), Lookup::Loading);
        assert_eq!(atlas.lookup(&key), Lookup::Loading);
        assert_eq!(atlas.take_wanted(), std::slice::from_ref(&key));
        assert!(atlas.take_wanted().is_empty());
        atlas.insert(key.clone(), Ok(vec![0; 16]));
        assert!(matches!(atlas.lookup(&key), Lookup::Ready(_)));
        assert_eq!(atlas.uploads.len(), 1);
        let broken = ImageKey { path: "y".into(), w: 2, h: 2 };
        atlas.insert(broken.clone(), Err("bad".into()));
        assert_eq!(atlas.lookup(&broken), Lookup::Failed);
    }
}

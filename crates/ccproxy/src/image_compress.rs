//! Deterministic quality compression for inline screenshot images.
//!
//! Why: dsh-style clients resend the whole conversation history on every
//! request, screenshots included, and full-resolution PNGs dominate a large
//! request (a 2560x1440 IDE screenshot is 2-3 MB of PNG). Re-encoding the same
//! pixels as JPEG (fixed quality, dimensions untouched) shrinks them 5-8x,
//! which is what keeps a long session under the request-size ceilings (the
//! proxy's own `maxBodyBytes` and the upstream's 32 MB).
//!
//! The contract this module must never break — prompt-cache prefix stability:
//!
//! * **Deterministic.** The same PNG bytes always map to the same JPEG bytes:
//!   fixed encoder settings, no metadata, no timestamps. When the client
//!   resends an image it already sent, this module reproduces byte-identical
//!   output, so the serialized prefix the upstream has cached still matches
//!   and the ~99% cache-hit rate survives.
//! * **Passthrough, never failure.** Anything disabled, undecodable,
//!   unsupported, below the size floor, or not actually smaller comes back
//!   unchanged. A compression problem must never turn into a failed request.
//! * **Memoized.** PNG decode + JPEG encode of a 3 MB screenshot costs real
//!   CPU, and the client resends the same images every turn; a bounded
//!   SHA-256-keyed memo makes the resend a hash lookup. The memo is an
//!   optimization only — it never changes the output bytes. It is
//!   deliberately not persisted across restarts: encoding is deterministic,
//!   so a cold memo reproduces the exact bytes a previous process produced
//!   and the upstream cache prefix stays stable without carrying any state
//!   over. Persisting it would only save CPU and would add a stale-output
//!   hazard (the key does not include quality, which is process-fixed).
//!
//! Not a token saving: image tokens are billed from pixel dimensions, which
//! are unchanged. Only the wire bytes shrink.
//!
//! Scope: `image/png` and `image/jpeg` data URLs. PNG is what screenshots
//! arrive in; JPEG is what pasted photos and camera captures arrive in, and a
//! 3 MB photo blows the same request ceiling a 3 MB screenshot does. Both are
//! re-encoded to JPEG at `quality`, so the output side of the memo is uniform.
//! Everything else (WebP, GIF, SVG, `http(s)` URLs) passes through untouched.
//!
//! **EXIF is the one thing JPEG input can lose that matters.** Re-encoding
//! drops every metadata segment and never rotates pixels, so an image tagged
//! "rotate 90°" would come back sideways. The orientation tag is read first;
//! anything other than 1 passes through. A sideways screenshot is worse than
//! a large one.
//!
//! Re-encoding an already-lossy JPEG loses generation quality, but never
//! accumulates: the client resends its *own* bytes every turn, so each pass
//! starts from the original and the memo makes the result byte-stable.

use base64::Engine;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, VecDeque};

/// Default JPEG quality: visually lossless for text/UI screenshots at native
/// resolution, while still cutting them ~5x versus PNG.
pub const DEFAULT_QUALITY: u8 = 80;
/// Below this many **decoded** bytes an image is not worth the CPU (small
/// icons, quick pastes) and is passed through untouched. Counted on the source
/// bytes rather than the base64 text, so the knob reads as "pictures over
/// 500 KiB". A base64 pre-gate derived from it skips the decode entirely.
pub const DEFAULT_MIN_BYTES: u64 = 500 * 1024;
/// Memo entries. Each stores one compressed data URL (~hundreds of KB), so the
/// bound keeps worst-case memory at a few tens of MB.
const MEMO_CAP: usize = 32;

/// The switch and its tuning knobs. Read once at startup (`config::load`);
/// applies to every request the process serves afterwards.
#[derive(Debug, Clone, PartialEq)]
pub struct ImageCompressConfig {
    pub enabled: bool,
    /// JPEG quality, clamped to 1..=100 by the config loader.
    pub quality: u8,
    /// Skip images whose base64 payload is below this many bytes.
    pub min_input_bytes: u64,
}

impl Default for ImageCompressConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            quality: DEFAULT_QUALITY,
            min_input_bytes: DEFAULT_MIN_BYTES,
        }
    }
}

/// Bounded SHA-256-keyed memo of `original PNG bytes -> compressed data URL`.
/// LRU by insertion order; eviction loses speed, never correctness.
#[derive(Debug, Default)]
pub struct ImageMemo {
    map: HashMap<[u8; 32], String>,
    order: VecDeque<[u8; 32]>,
}

impl ImageMemo {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn get(&self, key: &[u8; 32]) -> Option<&str> {
        self.map.get(key).map(String::as_str)
    }

    fn put(&mut self, key: [u8; 32], url: String) {
        if self.map.contains_key(&key) {
            return;
        }
        if self.map.len() >= MEMO_CAP {
            if let Some(oldest) = self.order.pop_front() {
                self.map.remove(&oldest);
            }
        }
        self.order.push_back(key);
        self.map.insert(key, url);
    }
}

/// `data:<mime>;base64,<payload>` -> `(mime, payload)` for the two formats this
/// module decodes. Anything else (other mime, other encoding, empty payload) is
/// not ours to touch.
fn split_data_url(url: &str) -> Option<(&str, &str)> {
    let rest = url.strip_prefix("data:")?;
    let (mime, payload) = rest.split_once(";base64,")?;
    if payload.is_empty() || !is_decodable_mime(mime) {
        return None;
    }
    Some((mime, payload))
}

/// Mimes the module can decode. `image/jpg` is not a real MIME type but shows
/// up in the wild often enough to be worth accepting.
fn is_decodable_mime(mime: &str) -> bool {
    matches!(mime, "image/png" | "image/jpeg" | "image/jpg")
}

/// Compress one `data:` URL image. `Some(new_url)` when it was recompressed,
/// `None` when it passed through unchanged. See the module docs for the
/// contract (deterministic, passthrough-never-failure).
pub fn compress_data_url(
    url: &str,
    cfg: &ImageCompressConfig,
    memo: &mut ImageMemo,
) -> Option<String> {
    if !cfg.enabled {
        return None;
    }
    let (mime, payload) = split_data_url(url)?;
    // Base64 inflates by 4/3, so a payload shorter than this cannot decode to
    // `min_input_bytes` and is skipped without paying for the decode or the
    // image codec. Derived from the knob so a lowered floor still works.
    let b64_floor = cfg.min_input_bytes.saturating_mul(4).saturating_div(3);
    if u64::try_from(payload.len()).unwrap_or(u64::MAX) < b64_floor {
        return None;
    }
    let src = base64::engine::general_purpose::STANDARD
        .decode(payload)
        .ok()?;
    let src_len = u64::try_from(src.len()).ok()?;
    if src_len < cfg.min_input_bytes {
        return None;
    }

    let mut hasher = Sha256::new();
    hasher.update(&src);
    let key: [u8; 32] = hasher.finalize().into();
    if let Some(cached) = memo.get(&key) {
        return Some(cached.to_owned());
    }

    // JPEG only: a rotated photo must not come back sideways.
    if mime != "image/png" && jpeg_exif_orientation(&src).is_some_and(|o| o != 1) {
        return None;
    }

    let (pixels, width, height) = decode_to_rgb(mime, &src)?;
    // The JPEG encoder addresses dimensions as u16; a legitimately larger
    // image is simply passed through rather than mis-encoded.
    let (w, h) = (u16::try_from(width).ok()?, u16::try_from(height).ok()?);

    let mut jpeg = Vec::new();
    let mut encoder = jpeg_encoder::Encoder::new(&mut jpeg, cfg.quality);
    // Optimized Huffman tables: smaller files and still fully deterministic
    // (the table is derived from the image data alone).
    encoder.set_optimized_huffman_tables(true);
    encoder
        .encode(&pixels, w, h, jpeg_encoder::ColorType::Rgb)
        .ok()?;
    if u64::try_from(jpeg.len()).ok()? >= src_len {
        // Only ever shrink: a flat, already-tiny PNG can re-encode larger
        // (measured 0.9x on clean UI screenshots), and a JPEG that is already
        // below `quality` also grows. Grow -> passthrough.
        return None;
    }

    let b64 = base64::engine::general_purpose::STANDARD.encode(&jpeg);
    let new_url = format!("data:image/jpeg;base64,{b64}");
    memo.put(key, new_url.clone());
    Some(new_url)
}

/// Decode either accepted mime into interleaved RGB8.
fn decode_to_rgb(mime: &str, bytes: &[u8]) -> Option<(Vec<u8>, u32, u32)> {
    if mime == "image/png" {
        decode_png_to_rgb(bytes)
    } else {
        decode_jpeg_to_rgb(bytes)
    }
}

/// Decode a JPEG into interleaved RGB8. CMYK/YCCK and 16-bit sources would
/// need a colour transform (or a downshift) this module does not do, so they
/// pass through instead of being mis-converted.
fn decode_jpeg_to_rgb(bytes: &[u8]) -> Option<(Vec<u8>, u32, u32)> {
    let mut decoder = jpeg_decoder::Decoder::new(std::io::Cursor::new(bytes));
    let pixels = decoder.decode().ok()?;
    let info = decoder.info()?;
    let width = u32::from(info.width);
    let height = u32::from(info.height);
    match info.pixel_format {
        jpeg_decoder::PixelFormat::RGB24 => Some((pixels, width, height)),
        jpeg_decoder::PixelFormat::L8 => Some((replicate_gray(&pixels), width, height)),
        _ => None,
    }
}

/// EXIF `Orientation` (tag 0x0112) from a JPEG's APP1 segment, or `None` when
/// the file carries no EXIF or the TIFF block is not shaped as expected.
///
/// Deliberately narrow: only IFD0 is scanned (every writer puts orientation
/// there) and only the segment walker is implemented. `None` means "nothing to
/// rotate", not "unknown": a file we cannot parse is treated as upright and
/// allowed through, which is the pre-existing behaviour for every input.
fn jpeg_exif_orientation(bytes: &[u8]) -> Option<u8> {
    match bytes.get(0..2)? {
        [0xFF, 0xD8] => {}
        _ => return None,
    }
    let mut i = 2usize;
    loop {
        // Marker (2) + length (2) must still be inside the buffer.
        if i.checked_add(4)? > bytes.len() {
            return None;
        }
        if *bytes.get(i)? != 0xFF {
            return None;
        }
        let marker = *bytes.get(i.checked_add(1)?)?;
        // Standalone markers carry no length field.
        if marker == 0xD8 || marker == 0x01 || (0xD0..=0xD7).contains(&marker) {
            i = i.checked_add(2)?;
            continue;
        }
        // Start of scan: entropy-coded data follows, no metadata past here.
        if marker == 0xDA {
            return None;
        }
        let hi = u16::from(*bytes.get(i.checked_add(2)?)?);
        let lo = u16::from(*bytes.get(i.checked_add(3)?)?);
        // The segment length covers its own two bytes.
        let len = usize::from(hi.checked_mul(256)?.checked_add(lo)?);
        if len < 2 {
            return None;
        }
        let end = i.checked_add(2)?.checked_add(len)?;
        if end > bytes.len() {
            return None;
        }
        if marker == 0xE1 {
            let seg = bytes.get(i.checked_add(4)?..end)?;
            if let Some(tiff) = seg.strip_prefix(b"Exif\0\0") {
                return tiff_orientation(tiff);
            }
        }
        i = end;
    }
}

/// IFD0 orientation out of a TIFF block (the bytes after `Exif\0\0`). Handles
/// both byte orders; a tag that is not a single SHORT is ignored rather than
/// guessed at.
fn tiff_orientation(tiff: &[u8]) -> Option<u8> {
    let little = match tiff.get(0..2)? {
        [0x49, 0x49] => true,
        [0x4D, 0x4D] => false,
        _ => return None,
    };
    let u16_at = |off: usize| -> Option<u16> {
        let b = tiff.get(off..off.checked_add(2)?)?;
        let pair = [*b.first()?, *b.get(1)?];
        Some(if little {
            u16::from_le_bytes(pair)
        } else {
            u16::from_be_bytes(pair)
        })
    };
    let u32_at = |off: usize| -> Option<u32> {
        let b = tiff.get(off..off.checked_add(4)?)?;
        let quad = [*b.first()?, *b.get(1)?, *b.get(2)?, *b.get(3)?];
        Some(if little {
            u32::from_le_bytes(quad)
        } else {
            u32::from_be_bytes(quad)
        })
    };
    // TIFF magic, then the IFD0 offset.
    if u16_at(2)? != 42 {
        return None;
    }
    let ifd0 = usize::try_from(u32_at(4)?).ok()?;
    let entries = usize::from(u16_at(ifd0)?);
    for k in 0..entries {
        // Each entry is tag(2) type(2) count(4) value(4).
        let entry = ifd0.checked_add(2)?.checked_add(k.checked_mul(12)?)?;
        if u16_at(entry)? != 0x0112 {
            continue;
        }
        return u8::try_from(u16_at(entry.checked_add(8)?)?).ok();
    }
    None
}

/// Decode a PNG into interleaved RGB8, alpha composited over white.
/// Palette/tRNS/16-bit inputs are normalized by the decoder's transforms.
fn decode_png_to_rgb(bytes: &[u8]) -> Option<(Vec<u8>, u32, u32)> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).ok()?;
    buf.truncate(info.buffer_size());
    let pixels = match info.color_type {
        png::ColorType::Rgba => flatten_rgba(&buf),
        png::ColorType::Rgb => buf,
        png::ColorType::Grayscale => replicate_gray(&buf),
        png::ColorType::GrayscaleAlpha => flatten_gray_alpha(&buf),
        _ => return None,
    };
    Some((pixels, info.width, info.height))
}

/// Straight-alpha composite of each RGBA pixel over an opaque white backdrop.
/// `as_chunks::<4>()` yields whole pixels only, so there is no indexing and
/// the arithmetic cannot overflow (every product fits u16 and the sum fits u17).
#[allow(clippy::arithmetic_side_effects)]
fn flatten_rgba(src: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(src.len() / 4 * 3);
    for &[r, g, b, a] in src.as_chunks::<4>().0 {
        if a == 255 {
            out.extend_from_slice(&[r, g, b]);
        } else {
            out.push(blend_over_white(r, a));
            out.push(blend_over_white(g, a));
            out.push(blend_over_white(b, a));
        }
    }
    out
}

/// Gray+alpha over white: one blended channel replicated to RGB.
#[allow(clippy::arithmetic_side_effects)]
fn flatten_gray_alpha(src: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(src.len() / 2 * 3);
    for &[gray, a] in src.as_chunks::<2>().0 {
        let blended = blend_over_white(gray, a);
        out.extend_from_slice(&[blended, blended, blended]);
    }
    out
}

/// Plain 8-bit grayscale, replicated to RGB.
#[allow(clippy::indexing_slicing, clippy::arithmetic_side_effects)]
fn replicate_gray(src: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(src.len() * 3);
    for g in src {
        out.extend_from_slice(&[*g, *g, *g]);
    }
    out
}

/// `c * a + 255 * (255 - a)`, all over 255 — one channel of straight alpha
/// over white, floored. Bounded: every intermediate fits in u17.
#[allow(clippy::arithmetic_side_effects)]
fn blend_over_white(c: u8, a: u8) -> u8 {
    let prod = u32::from(c) * u32::from(a);
    let rest = 255u32.saturating_sub(u32::from(a)) * 255;
    ((prod + rest) / 255) as u8
}

/// Aggregate effect of one request's compression pass. Byte counts are the
/// data-URL text lengths — what actually transits in the JSON body — so they
/// are directly comparable with the ingress (`body: N bytes`) and forwarded
/// log lines.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct CompressStats {
    pub images: usize,
    pub bytes_before: u64,
    pub bytes_after: u64,
}

impl CompressStats {
    #[must_use]
    pub fn shrink_percent(&self) -> u64 {
        if self.bytes_before == 0 {
            return 0;
        }
        // checked arithmetic keeps the crate-wide `arithmetic_side_effects`
        // lint happy without a panic path; bytes_after <= bytes_before always.
        let kept = self
            .bytes_after
            .checked_mul(100)
            .and_then(|v| v.checked_div(self.bytes_before))
            .unwrap_or(0);
        100u64.saturating_sub(kept)
    }
}

/// Walk a translated CC request body and compress every
/// `{"type": "image", "image": "data:image/<png|jpeg>;base64,..."}` part in
/// place.
/// Returns what was replaced. Only image parts are touched — text that
/// happens to contain the same URL string is left alone, so any prefix of
/// the body this module does not rewrite stays byte-identical.
pub fn compress_cc_body(
    body: &mut serde_json::Value,
    cfg: &ImageCompressConfig,
    memo: &mut ImageMemo,
) -> CompressStats {
    let mut stats = CompressStats::default();
    walk(body, cfg, memo, &mut stats);
    stats
}

fn walk(
    node: &mut serde_json::Value,
    cfg: &ImageCompressConfig,
    memo: &mut ImageMemo,
    stats: &mut CompressStats,
) {
    match node {
        serde_json::Value::Object(map) => {
            let is_image = map.get("type").and_then(serde_json::Value::as_str) == Some("image");
            if is_image {
                if let Some(serde_json::Value::String(url)) = map.get("image") {
                    let owned = url.clone();
                    if let Some(new_url) = compress_data_url(&owned, cfg, memo) {
                        stats.bytes_before = stats.bytes_before.saturating_add(owned.len() as u64);
                        stats.bytes_after = stats.bytes_after.saturating_add(new_url.len() as u64);
                        stats.images = stats.images.saturating_add(1);
                        map.insert("image".into(), serde_json::Value::String(new_url));
                    }
                }
            }
            for child in map.values_mut() {
                walk(child, cfg, memo, stats);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items.iter_mut() {
                walk(item, cfg, memo, stats);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    // Test-only helpers: unwraps and plain arithmetic are allowed under the
    // crate-level cfg(test) allowance.
    use super::*;
    use serde_json::json;

    fn lcg(seed: &mut u32) -> u32 {
        *seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
        *seed
    }

    /// Screenshot-like pixels, RGBA. Alternating UI bands; `noisy` adds
    /// per-pixel jitter, which is what makes real screenshots entropy-heavy
    /// (ClearType fringing, antialiasing) and therefore PNG-large.
    fn screenshot_rgba(w: u32, h: u32, noisy: bool) -> Vec<u8> {
        let mut raw = vec![0u8; (w * h * 4) as usize];
        let mut seed = 7u32;
        for y in 0..h {
            for x in 0..w {
                let i = ((y * w + x) * 4) as usize;
                let dark = (x / 40 + y / 12) % 2 == 0;
                for c in 0..3 {
                    let base: [u8; 3] = if dark { [28, 30, 34] } else { [240, 240, 242] };
                    let mut v = base[c];
                    if noisy {
                        let n = (lcg(&mut seed) % 7) as i32 - 3;
                        v = (i32::from(v) + n).clamp(0, 255) as u8;
                    }
                    raw[i + c] = v;
                }
                raw[i + 3] = 255;
            }
        }
        raw
    }

    /// Those pixels encoded as PNG — the format screenshots arrive in.
    fn screenshot_png(w: u32, h: u32, noisy: bool) -> Vec<u8> {
        let raw = screenshot_rgba(w, h, noisy);
        let mut out = Vec::new();
        let mut enc = png::Encoder::new(&mut out, w, h);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut writer = enc.write_header().unwrap();
        writer.write_image_data(&raw).unwrap();
        writer.finish().unwrap();
        out
    }

    /// The same pixels as a high-quality JPEG — what a pasted photo looks like,
    /// and high enough quality that re-encoding at 80 measurably shrinks it.
    fn screenshot_jpeg(w: u32, h: u32, quality: u8) -> Vec<u8> {
        let rgba = screenshot_rgba(w, h, true);
        let rgb: Vec<u8> = rgba
            .as_chunks::<4>()
            .0
            .iter()
            .flat_map(|p| [p[0], p[1], p[2]])
            .collect();
        let mut out = Vec::new();
        let enc = jpeg_encoder::Encoder::new(&mut out, quality);
        enc.encode(
            &rgb,
            u16::try_from(w).unwrap(),
            u16::try_from(h).unwrap(),
            jpeg_encoder::ColorType::Rgb,
        )
        .unwrap();
        out
    }

    /// Splice an APP1/Exif segment carrying one Orientation tag into a JPEG, so
    /// the EXIF path runs against real segment framing rather than a mock.
    fn jpeg_with_orientation(jpeg: &[u8], orientation: u16) -> Vec<u8> {
        let mut app1 = Vec::new();
        app1.extend_from_slice(b"Exif\0\0");
        // Little-endian TIFF header: magic 42, IFD0 at offset 8.
        app1.extend_from_slice(&[0x49, 0x49, 0x2A, 0x00, 0x08, 0x00, 0x00, 0x00]);
        // One IFD0 entry.
        app1.extend_from_slice(&[0x01, 0x00]);
        // tag 0x0112, type 3 (SHORT), count 1, value = orientation.
        app1.extend_from_slice(&[0x12, 0x01, 0x03, 0x00, 0x01, 0x00, 0x00, 0x00]);
        app1.extend_from_slice(&orientation.to_le_bytes());
        app1.extend_from_slice(&[0x00, 0x00]);
        // No next IFD.
        app1.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);

        let mut out = Vec::new();
        out.extend_from_slice(jpeg.get(0..2).unwrap());
        out.extend_from_slice(&[0xFF, 0xE1]);
        let len = u16::try_from(app1.len() + 2).unwrap();
        out.extend_from_slice(&len.to_be_bytes());
        out.extend_from_slice(&app1);
        out.extend_from_slice(jpeg.get(2..).unwrap());
        out
    }

    fn data_url(png: &[u8]) -> String {
        data_url_mime("image/png", png)
    }

    fn data_url_mime(mime: &str, bytes: &[u8]) -> String {
        format!(
            "data:{mime};base64,{}",
            base64::engine::general_purpose::STANDARD.encode(bytes)
        )
    }

    fn cfg_on() -> ImageCompressConfig {
        ImageCompressConfig {
            enabled: true,
            quality: 80,
            min_input_bytes: 0,
        }
    }

    #[test]
    fn same_png_yields_byte_identical_output() {
        // Two independent memos: determinism must come from the encoder, not
        // from the memo. This is the cache-prefix guarantee.
        let url = data_url(&screenshot_png(400, 260, true));
        let a = compress_data_url(&url, &cfg_on(), &mut ImageMemo::new()).unwrap();
        let b = compress_data_url(&url, &cfg_on(), &mut ImageMemo::new()).unwrap();
        assert_eq!(a, b);
        assert!(a.starts_with("data:image/jpeg;base64,"));
    }

    #[test]
    fn noisy_screenshot_shrinks_at_least_2x() {
        let png = screenshot_png(640, 400, true);
        let url = data_url(&png);
        let out = compress_data_url(&url, &cfg_on(), &mut ImageMemo::new()).unwrap();
        assert!(
            out.len() * 2 < url.len(),
            "compressed {} should be under half of {}",
            out.len(),
            url.len()
        );
    }

    #[test]
    fn below_min_bytes_passes_through() {
        let png = screenshot_png(640, 400, true);
        let url = data_url(&png);
        let cfg = ImageCompressConfig {
            min_input_bytes: u64::MAX,
            ..cfg_on()
        };
        assert!(compress_data_url(&url, &cfg, &mut ImageMemo::new()).is_none());
    }

    #[test]
    fn only_if_smaller_guard() {
        // A 2x2 flat PNG is ~70 bytes; even the smallest JPEG carries hundreds
        // of bytes of framing. Growing the request is never acceptable.
        let url = data_url(&screenshot_png(2, 2, false));
        assert!(compress_data_url(&url, &cfg_on(), &mut ImageMemo::new()).is_none());
    }

    #[test]
    fn alpha_composites_onto_white() {
        let rgba = [
            255, 0, 0, 255, // opaque red stays red
            0, 0, 255, 0, // fully transparent -> white
            0, 0, 0, 128, // half black over white -> 127 gray
        ];
        assert_eq!(
            flatten_rgba(&rgba),
            vec![255, 0, 0, 255, 255, 255, 127, 127, 127]
        );
    }

    #[test]
    fn walker_touches_only_image_parts() {
        let png = screenshot_png(640, 400, true);
        let url = data_url(&png);
        let mut body = json!({
            "messages": [
                {"role": "user", "content": [
                    {"type": "text", "text": url},
                    {"type": "image", "image": url},
                ]},
                {"role": "assistant", "content": [
                    {"type": "image", "image": "https://example.com/a.png"}
                ]},
            ]
        });
        let stats = compress_cc_body(&mut body, &cfg_on(), &mut ImageMemo::new());
        assert_eq!(stats.images, 1);
        // A text field holding the same URL string is untouched.
        assert_eq!(
            body["messages"][0]["content"][0]["text"].as_str().unwrap(),
            url
        );
        let img = body["messages"][0]["content"][1]["image"].as_str().unwrap();
        assert!(img.starts_with("data:image/jpeg;base64,"));
        assert_ne!(img, url);
    }

    #[test]
    fn unsupported_mime_and_corrupt_pass_through() {
        let mut memo = ImageMemo::new();
        // WebP/GIF/SVG are out of scope: not decoded, sent exactly as received.
        assert!(compress_data_url(
            "data:image/webp;base64,UklGRhIAAABXRUJQVlA4TAYAAAAvAAAAAAfQ//73v/+BiOh/AAA=",
            &cfg_on(),
            &mut memo
        )
        .is_none());
        // A truncated JPEG: past the mime gate, refused by the decoder.
        assert!(compress_data_url(
            "data:image/jpeg;base64,/9j/4AAQSkZJRg==",
            &cfg_on(),
            &mut memo
        )
        .is_none());
        assert!(compress_data_url(
            "data:image/png;base64,not-really-a-png",
            &cfg_on(),
            &mut memo
        )
        .is_none());
        assert!(compress_data_url("https://example.com/a.png", &cfg_on(), &mut memo).is_none());
        assert!(compress_data_url("", &cfg_on(), &mut memo).is_none());
    }

    #[test]
    fn jpeg_input_shrinks_too() {
        let jpeg = screenshot_jpeg(640, 400, 95);
        let url = data_url_mime("image/jpeg", &jpeg);
        let out = compress_data_url(&url, &cfg_on(), &mut ImageMemo::new()).unwrap();
        assert!(
            out.len() < url.len(),
            "q95 jpeg {} should shrink, got {}",
            url.len(),
            out.len()
        );
        assert!(out.starts_with("data:image/jpeg;base64,"));
    }

    #[test]
    fn exif_orientation_is_read_from_real_framing() {
        let jpeg = screenshot_jpeg(32, 32, 90);
        assert_eq!(jpeg_exif_orientation(&jpeg), None, "no EXIF at all");
        for o in [1u16, 3, 6, 8] {
            let tagged = jpeg_with_orientation(&jpeg, o);
            assert_eq!(
                jpeg_exif_orientation(&tagged),
                Some(u8::try_from(o).unwrap()),
                "orientation {o}"
            );
        }
    }

    #[test]
    fn rotated_photo_passes_through() {
        // Orientation 6 = rotate 90° CW. Re-encoding drops the tag and never
        // rotates pixels, so the guard must refuse it outright.
        let rotated = jpeg_with_orientation(&screenshot_jpeg(640, 400, 95), 6);
        let url = data_url_mime("image/jpeg", &rotated);
        assert!(compress_data_url(&url, &cfg_on(), &mut ImageMemo::new()).is_none());

        // Orientation 1 is upright: safe, and it must still shrink.
        let upright = jpeg_with_orientation(&screenshot_jpeg(640, 400, 95), 1);
        let url = data_url_mime("image/jpeg", &upright);
        assert!(compress_data_url(&url, &cfg_on(), &mut ImageMemo::new()).is_some());
    }

    #[test]
    fn min_bytes_counts_source_not_base64() {
        // 500 KiB source floor: a payload just under it in *encoded* terms is
        // still skipped, and one above it is not.
        let png = screenshot_png(640, 400, true);
        let src_len = u64::try_from(png.len()).unwrap();
        let cfg = ImageCompressConfig {
            min_input_bytes: src_len,
            ..cfg_on()
        };
        let url = data_url(&png);
        assert!(
            compress_data_url(&url, &cfg, &mut ImageMemo::new()).is_some(),
            "exactly at the floor is compressed"
        );
        let cfg_over = ImageCompressConfig {
            min_input_bytes: src_len + 1,
            ..cfg_on()
        };
        assert!(compress_data_url(&url, &cfg_over, &mut ImageMemo::new()).is_none());
    }

    #[test]
    fn memo_returns_cached_output() {
        let url = data_url(&screenshot_png(640, 400, true));
        let mut memo = ImageMemo::new();
        let a = compress_data_url(&url, &cfg_on(), &mut memo).unwrap();
        let b = compress_data_url(&url, &cfg_on(), &mut memo).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn memo_evicts_oldest_at_cap() {
        let mut memo = ImageMemo::new();
        for i in 0..=MEMO_CAP {
            memo.put([i as u8; 32], format!("url-{i}"));
        }
        assert!(memo.get(&[0u8; 32]).is_none(), "oldest evicted");
        assert_eq!(memo.get(&[MEMO_CAP as u8; 32]), Some("url-32"));
    }

    #[test]
    fn disabled_config_is_noop() {
        let url = data_url(&screenshot_png(640, 400, true));
        let cfg = ImageCompressConfig {
            enabled: false,
            ..cfg_on()
        };
        assert!(compress_data_url(&url, &cfg, &mut ImageMemo::new()).is_none());
    }
}

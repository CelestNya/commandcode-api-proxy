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
//!   optimization only — it never changes the output bytes.
//!
//! Not a token saving: image tokens are billed from pixel dimensions, which
//! are unchanged. Only the wire bytes shrink.
//!
//! Scope: `image/png` data URLs only (the format screenshots arrive in).
//! JPEG/WebP inputs are already lossy-compressed and pass through untouched.

use base64::Engine;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, VecDeque};

/// Default JPEG quality: visually lossless for text/UI screenshots at native
/// resolution, while still cutting them ~5x versus PNG.
pub const DEFAULT_QUALITY: u8 = 80;
/// Below this many base64 bytes an image is not worth the CPU (small icons,
/// quick pastes) and is passed through untouched.
pub const DEFAULT_MIN_BYTES: u64 = 300_000;
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

/// `data:image/png;base64,<payload>` -> payload. Anything else (other mime,
/// other encoding, empty payload) is not ours to touch.
fn split_data_url(url: &str) -> Option<&str> {
    let rest = url.strip_prefix("data:")?;
    let (mime, payload) = rest.split_once(";base64,")?;
    if mime != "image/png" || payload.is_empty() {
        return None;
    }
    Some(payload)
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
    let payload = split_data_url(url)?;
    if u64::try_from(payload.len()).unwrap_or(u64::MAX) < cfg.min_input_bytes {
        return None;
    }
    let png = base64::engine::general_purpose::STANDARD
        .decode(payload)
        .ok()?;

    let mut hasher = Sha256::new();
    hasher.update(&png);
    let key: [u8; 32] = hasher.finalize().into();
    if let Some(cached) = memo.get(&key) {
        return Some(cached.to_owned());
    }

    let (pixels, width, height) = decode_png_to_rgb(&png)?;
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
    if jpeg.len() >= png.len() {
        // Only ever shrink: a flat, already-tiny PNG can re-encode larger
        // (measured 0.9x on clean UI screenshots). Grow -> passthrough.
        return None;
    }

    let b64 = base64::engine::general_purpose::STANDARD.encode(&jpeg);
    let new_url = format!("data:image/jpeg;base64,{b64}");
    memo.put(key, new_url.clone());
    Some(new_url)
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

/// Walk a translated CC request body and compress every
/// `{"type": "image", "image": "data:image/png;base64,..."}` part in place.
/// Returns how many images were replaced. Only image parts are touched — text
/// that happens to contain the same URL string is left alone, so any prefix of
/// the body this module does not rewrite stays byte-identical.
pub fn compress_cc_body(
    body: &mut serde_json::Value,
    cfg: &ImageCompressConfig,
    memo: &mut ImageMemo,
) -> usize {
    let mut count = 0usize;
    walk(body, cfg, memo, &mut count);
    count
}

fn walk(
    node: &mut serde_json::Value,
    cfg: &ImageCompressConfig,
    memo: &mut ImageMemo,
    count: &mut usize,
) {
    match node {
        serde_json::Value::Object(map) => {
            let is_image = map.get("type").and_then(serde_json::Value::as_str) == Some("image");
            if is_image {
                if let Some(serde_json::Value::String(url)) = map.get("image") {
                    let owned = url.clone();
                    if let Some(new_url) = compress_data_url(&owned, cfg, memo) {
                        map.insert("image".into(), serde_json::Value::String(new_url));
                        *count = count.saturating_add(1);
                    }
                }
            }
            for child in map.values_mut() {
                walk(child, cfg, memo, count);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items.iter_mut() {
                walk(item, cfg, memo, count);
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

    /// A screenshot-like PNG: alternating UI bands; `noisy` adds per-pixel
    /// jitter, which is what makes real screenshots entropy-heavy (ClearType
    /// fringing, antialiasing) and therefore PNG-large.
    fn screenshot_png(w: u32, h: u32, noisy: bool) -> Vec<u8> {
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
        let mut out = Vec::new();
        let mut enc = png::Encoder::new(&mut out, w, h);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut writer = enc.write_header().unwrap();
        writer.write_image_data(&raw).unwrap();
        writer.finish().unwrap();
        out
    }

    fn data_url(png: &[u8]) -> String {
        format!(
            "data:image/png;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(png)
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
        let n = compress_cc_body(&mut body, &cfg_on(), &mut ImageMemo::new());
        assert_eq!(n, 1);
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
    fn non_png_and_corrupt_pass_through() {
        let mut memo = ImageMemo::new();
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

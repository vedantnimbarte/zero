//! External resources (images) the engine needs but does not fetch itself.
//!
//! The engine is network-free. To load subresources like `<img>`, the embedder
//! provides a [`ResourceLoader`]; the engine decodes whatever bytes come back.

use crate::css::Color;
use std::collections::HashMap;

/// Supplied by the embedder so the engine can request bytes for a URL without
/// knowing anything about networking or the filesystem.
pub trait ResourceLoader {
    fn load(&self, url: &str) -> Option<Vec<u8>>;
}

/// A loader that fetches nothing — used by the plain `render` path.
pub struct NullLoader;

impl ResourceLoader for NullLoader {
    fn load(&self, _url: &str) -> Option<Vec<u8>> {
        None
    }
}

/// A decoded image: RGBA pixels plus its intrinsic size.
pub struct DecodedImage {
    pub width: usize,
    pub height: usize,
    pub pixels: Vec<Color>,
}

/// Loaded images keyed by their `src` string.
pub type ImageMap = HashMap<String, DecodedImage>;

/// Decode PNG/JPEG/GIF bytes into RGBA pixels. Returns `None` on undecodable data.
pub fn decode_image(bytes: &[u8]) -> Option<DecodedImage> {
    let img = image::load_from_memory(bytes).ok()?.to_rgba8();
    let (w, h) = img.dimensions();
    let pixels = img.pixels().map(|p| Color { r: p[0], g: p[1], b: p[2], a: p[3] }).collect();
    Some(DecodedImage { width: w as usize, height: h as usize, pixels })
}

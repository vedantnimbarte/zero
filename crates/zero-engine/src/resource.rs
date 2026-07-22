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

    /// Fetch several resources at once, in the order given.
    ///
    /// The engine asks for everything a page needs in one call so an embedder
    /// can fetch them concurrently — over a real network this dominates render
    /// time. The default is sequential, so a simple loader need not implement it.
    fn load_all(&self, urls: &[String]) -> Vec<Option<Vec<u8>>> {
        urls.iter().map(|url| self.load(url)).collect()
    }
}

/// Key/value storage for scripts (`localStorage`), supplied by the embedder.
///
/// The engine never decides *where* this lives; the embedder partitions it per
/// site so one origin cannot read another's state.
pub trait KeyValueStore {
    fn get(&self, key: &str) -> Option<String>;
    fn set(&self, key: &str, value: &str);
    fn remove(&self, key: &str);
    fn clear(&self);
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
    // SVG is markup, not a raster format, so it is drawn rather than decoded.
    //
    // ponytail: rasterized once at its intrinsic size and scaled from there, so
    // an icon shown much larger than it declares goes soft. Re-rasterizing at
    // the laid-out size means decoding after layout rather than before it.
    if crate::svg::looks_like_svg(bytes) {
        let source = String::from_utf8_lossy(bytes);
        let (w, h) = crate::svg::intrinsic_size(&source);
        return crate::svg::rasterize(&source, w, h);
    }
    let img = image::load_from_memory(bytes).ok()?.to_rgba8();
    let (w, h) = img.dimensions();
    let pixels = img
        .pixels()
        .map(|p| Color {
            r: p[0],
            g: p[1],
            b: p[2],
            a: p[3],
        })
        .collect();
    Some(DecodedImage {
        width: w as usize,
        height: h as usize,
        pixels,
    })
}

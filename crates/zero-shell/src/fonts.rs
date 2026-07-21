//! Font sourcing — a platform concern, so it lives in the shell.
//! The engine only ever receives bytes.

use std::fs;
use zero_engine::Engine;

/// Every candidate that exists, in priority order, forming a fallback chain:
/// a good Latin font first, then broad-coverage Indic fonts.
fn load_system_fonts() -> Vec<Vec<u8>> {
    const CANDIDATES: &[&str] = &[
        // Windows
        "C:/Windows/Fonts/segoeui.ttf",
        "C:/Windows/Fonts/Nirmala.ttf", // Devanagari/Tamil/Telugu/Bengali/...
        "C:/Windows/Fonts/arial.ttf",
        // macOS
        "/System/Library/Fonts/Supplemental/Arial.ttf",
        "/System/Library/Fonts/Supplemental/Devanagari Sangam MN.ttc",
        "/Library/Fonts/Arial.ttf",
        // Linux
        "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
        "/usr/share/fonts/truetype/noto/NotoSansDevanagari-Regular.ttf",
        "/usr/share/fonts/TTF/DejaVuSans.ttf",
    ];
    CANDIDATES.iter().filter_map(|p| fs::read(p).ok()).collect()
}

pub fn build_engine() -> Engine {
    let fonts = load_system_fonts();
    if fonts.is_empty() {
        eprintln!("no system font found; rendering shapes only");
        return Engine::shapes_only();
    }
    Engine::with_fonts(fonts)
}

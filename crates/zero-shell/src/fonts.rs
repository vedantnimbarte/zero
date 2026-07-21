//! Font sourcing — a platform concern, so it lives in the shell.
//! The engine only ever receives bytes.

use std::fs;
use zero_engine::Engine;

/// Every candidate that exists, in priority order, forming a fallback chain:
/// a good Latin font first, then symbols, then Indic, then CJK. A script with
/// no font in the chain renders as empty boxes, so breadth here is what stops
/// the web looking broken outside Latin text — and the symbol font is what
/// gives the browser's own buttons their arrows and stars.
fn load_system_fonts() -> Vec<Vec<u8>> {
    const CANDIDATES: &[&str] = &[
        // Windows
        "C:/Windows/Fonts/segoeui.ttf",
        "C:/Windows/Fonts/seguisym.ttf", // arrows, stars and other UI symbols
        "C:/Windows/Fonts/Nirmala.ttf", // Devanagari/Tamil/Telugu/Bengali/...
        "C:/Windows/Fonts/msyh.ttc",    // Simplified Chinese
        "C:/Windows/Fonts/msjh.ttc",    // Traditional Chinese
        "C:/Windows/Fonts/YuGothR.ttc", // Japanese
        "C:/Windows/Fonts/malgun.ttf",  // Korean
        "C:/Windows/Fonts/arial.ttf",
        // macOS
        "/System/Library/Fonts/Supplemental/Arial.ttf",
        "/System/Library/Fonts/Apple Symbols.ttf",
        "/System/Library/Fonts/Supplemental/Devanagari Sangam MN.ttc",
        "/System/Library/Fonts/PingFang.ttc",         // Chinese
        "/System/Library/Fonts/Hiragino Sans GB.ttc", // Japanese
        "/System/Library/Fonts/Supplemental/AppleGothic.ttf", // Korean
        "/Library/Fonts/Arial.ttf",
        // Linux
        "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
        "/usr/share/fonts/truetype/noto/NotoSansSymbols2-Regular.ttf",
        "/usr/share/fonts/truetype/noto/NotoSansDevanagari-Regular.ttf",
        "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/truetype/wqy/wqy-zenhei.ttc",
        "/usr/share/fonts/TTF/DejaVuSans.ttf",
    ];
    // ponytail: every candidate is read into memory at startup, and CJK fonts
    // are tens of megabytes. Mapping them lazily (or on first miss) is the fix
    // if footprint starts to matter.
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

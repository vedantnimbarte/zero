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
        // A serif and a monospace face, so `font-family: serif` and the
        // `monospace` every code block on every docs site asks for resolve to
        // something that looks like what was asked for.
        "C:/Windows/Fonts/georgia.ttf",
        "C:/Windows/Fonts/consola.ttf",
        // Devanagari/Tamil/Telugu/Bengali/… Windows ships this as a collection;
        // the loose `.ttf` only exists on some installs, and looking for it
        // alone is why Hindi came out as empty boxes on the ones it does not.
        "C:/Windows/Fonts/Nirmala.ttc",
        "C:/Windows/Fonts/Nirmala.ttf",
        "C:/Windows/Fonts/msyh.ttc",    // Simplified Chinese
        "C:/Windows/Fonts/msjh.ttc",    // Traditional Chinese
        "C:/Windows/Fonts/YuGothR.ttc", // Japanese
        "C:/Windows/Fonts/malgun.ttf",  // Korean
        "C:/Windows/Fonts/arial.ttf",
        // macOS
        "/System/Library/Fonts/Supplemental/Arial.ttf",
        "/System/Library/Fonts/Apple Symbols.ttf",
        "/System/Library/Fonts/Supplemental/Georgia.ttf",
        "/System/Library/Fonts/Menlo.ttc",
        "/System/Library/Fonts/Supplemental/Devanagari Sangam MN.ttc",
        "/System/Library/Fonts/PingFang.ttc",         // Chinese
        "/System/Library/Fonts/Hiragino Sans GB.ttc", // Japanese
        "/System/Library/Fonts/Supplemental/AppleGothic.ttf", // Korean
        "/Library/Fonts/Arial.ttf",
        // Linux
        "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
        "/usr/share/fonts/truetype/noto/NotoSansSymbols2-Regular.ttf",
        "/usr/share/fonts/truetype/dejavu/DejaVuSerif.ttf",
        "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
        "/usr/share/fonts/truetype/noto/NotoSansDevanagari-Regular.ttf",
        "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/truetype/wqy/wqy-zenhei.ttc",
        "/usr/share/fonts/TTF/DejaVuSans.ttf",
    ];
    // Every candidate is read here, and the CJK ones are tens of megabytes —
    // but reading all of them costs about 30ms, while *parsing* them costs over
    // a second, so the engine parses each one only when a page first needs it.
    //
    // The reads stay eager on purpose: a renderer is meant to be able to close
    // the filesystem behind itself once it has its fonts (see `renderer::serve`),
    // which it could not do if a font might still be needed from disk later.
    //
    // ponytail: the bytes of a font that is never used stay in memory. Memory
    // mapping them is the fix if footprint starts to matter more than the jail.
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

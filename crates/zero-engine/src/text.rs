//! Text shaping: turn a string into positioned glyphs.
//!
//! One glyph per character is wrong for most of the world's scripts. In Devanagari
//! a vowel sign can render *before* the consonant it logically follows, consonants
//! merge into conjuncts, and marks need precise positioning. Shaping (via rustybuzz,
//! a HarfBuzz port) resolves all of that against the font's OpenType tables.
//!
//! ponytail: one font for everything — no per-script font fallback, so text in a
//! script the loaded font lacks renders as blank/.notdef boxes. Fallback is a later
//! phase (docs/01-ARCHITECTURE.md §3 [5]).

/// The two font views the engine needs: one to shape, one to rasterize.
/// Both must come from the same font file so glyph ids agree.
pub struct Fonts<'a> {
    pub raster: &'a fontdue::Font,
    pub shaper: &'a rustybuzz::Face<'a>,
}

/// A glyph placed relative to the start of its run (y is up-positive, like the font).
#[derive(Clone)]
pub struct PositionedGlyph {
    pub id: u16,
    pub x: f32,
    pub y: f32,
}

/// Shape `text` at `size` px, returning its glyphs and total advance width.
pub fn shape_run(fonts: &Fonts, text: &str, size: f32) -> (Vec<PositionedGlyph>, f32) {
    let mut buffer = rustybuzz::UnicodeBuffer::new();
    buffer.push_str(text);
    // Infers script, direction, and language from the text itself.
    buffer.guess_segment_properties();

    let output = rustybuzz::shape(fonts.shaper, &[], buffer);
    let scale = size / fonts.shaper.units_per_em() as f32;

    let mut glyphs = Vec::with_capacity(output.len());
    let mut pen = 0.0;
    for (info, pos) in output.glyph_infos().iter().zip(output.glyph_positions()) {
        glyphs.push(PositionedGlyph {
            id: info.glyph_id as u16,
            x: pen + pos.x_offset as f32 * scale,
            y: pos.y_offset as f32 * scale,
        });
        pen += pos.x_advance as f32 * scale;
    }
    (glyphs, pen)
}

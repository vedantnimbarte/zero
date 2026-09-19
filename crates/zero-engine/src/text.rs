//! Text shaping: turn a string into positioned glyphs, with font fallback.
//!
//! One glyph per character is wrong for most of the world's scripts. In Devanagari
//! a vowel sign can render *before* the consonant it logically follows, consonants
//! merge into conjuncts, and marks need precise positioning. Shaping (via rustybuzz,
//! a HarfBuzz port) resolves all of that against the font's OpenType tables.
//!
//! No single font covers every script, so the engine holds a prioritized [`FontSet`]
//! and picks, per run, the first font that can draw it.
//!
//! ponytail: fallback is per-word, all-or-nothing — a word mixing scripts no single
//! font covers falls back to font 0 and shows .notdef for the missing part. Per-run
//! splitting by coverage is the upgrade.

/// One font, in both the views the engine needs: shaping and rasterizing.
/// Both come from the same file so glyph ids agree.
pub struct FontEntry<'a> {
    font: &'a crate::LoadedFont,
    pub shaper: &'a rustybuzz::Face<'a>,
    /// What this font calls itself, lowercased — what a page's `font-family`
    /// is matched against. A face the page supplied through `@font-face` is
    /// known by the name the page gave it instead, which is the whole point of
    /// that at-rule: the file's own name need not be the one it is asked for.
    family: String,
    /// Whether the page supplied this face through `@font-face`. Such a font
    /// answers when it is asked for by name and at no other time: it is the
    /// page's typeface, not a fallback for text that named something else.
    page_supplied: bool,
}

impl<'a> FontEntry<'a> {
    pub(crate) fn new(
        font: &'a crate::LoadedFont,
        shaper: &'a rustybuzz::Face<'a>,
    ) -> FontEntry<'a> {
        let family = family_of(shaper);
        FontEntry {
            font,
            shaper,
            family,
            page_supplied: false,
        }
    }

    /// Same, but answering to the name a page's `@font-face` gave it.
    pub(crate) fn named(
        font: &'a crate::LoadedFont,
        shaper: &'a rustybuzz::Face<'a>,
        family: &str,
    ) -> FontEntry<'a> {
        FontEntry {
            font,
            shaper,
            family: family.to_ascii_lowercase(),
            page_supplied: true,
        }
    }

    /// The rasterizer, parsed the first time it is asked for. `None` when the
    /// file turned out not to be a font this can draw with.
    pub fn raster(&self) -> Option<&'a fontdue::Font> {
        self.font.raster()
    }
}

/// Fonts in priority order; index 0 is the primary.
pub struct FontSet<'a> {
    pub entries: Vec<FontEntry<'a>>,
}

/// A font's own family name, lowercased, from its `name` table.
fn family_of(shaper: &rustybuzz::Face) -> String {
    // A font carries the same name several times over, once per platform and
    // language, and not all of those encodings decode here. Taking the first
    // record and giving up if it did not decode left some fonts nameless —
    // Georgia among them — so this takes the first that actually reads.
    shaper
        .names()
        .into_iter()
        .filter(|name| name.name_id == rustybuzz::ttf_parser::name_id::FAMILY)
        .find_map(|name| name.to_string())
        .unwrap_or_default()
        .to_ascii_lowercase()
}

impl FontSet<'_> {
    /// Index of the best font for `text` given what the page asked for.
    ///
    /// Each family is tried in the order the page listed them, and a font only
    /// answers if it can actually draw the text — so a page asking for a Latin
    /// face and then writing Devanagari still falls through to a font that
    /// covers it rather than drawing boxes. Nothing matching means the page
    /// asked for something nobody here has, which is what the fallback chain
    /// exists for.
    pub fn pick_in(&self, families: &[String], text: &str) -> usize {
        for wanted in families {
            let matches_name = |e: &FontEntry| e.family == *wanted && covers(e, text);
            if let Some(i) = self.entries.iter().position(matches_name) {
                return i;
            }
            // A generic family names a *kind* of face rather than a face, and
            // the page means it: `monospace` in a code block is a request not
            // to render code in the body face. Nothing here knows which of the
            // embedder's fonts is which, so it is decided the way a person
            // would — by what the face calls itself.
            if let Some(i) = generic_match(self, wanted, text) {
                return i;
            }
        }
        self.pick(text)
    }

    /// Index of the first font that can draw every character of `text`,
    /// falling back to the primary font when none covers it fully.
    pub fn pick(&self, text: &str) -> usize {
        // Coverage is asked of the *shaper*, which already has the font's
        // character map open, rather than of the rasterizer, which would have to
        // parse the whole font to answer. That matters because this walks the
        // chain: a Devanagari word passes over the symbol font on its way to the
        // Devanagari one, and parsing every font merely *considered* cost more
        // than drawing the text. Now only a font that actually draws is parsed.
        self.entries
            .iter()
            .position(|e| !e.page_supplied && covers(e, text))
            .unwrap_or_else(|| {
                self.entries
                    .iter()
                    .position(|e| covers(e, text))
                    .unwrap_or(0)
            })
    }
}

/// The first loaded face that reads as the generic family `wanted`.
///
/// Matched on the face's own name — `Consolas`, `DejaVu Sans Mono` and `Menlo`
/// all say what they are — because nothing else here classifies a font, and a
/// wrong guess is no worse than the fallback it replaces. `sans-serif` is
/// deliberately absent: it is the default chain already, and matching the word
/// "sans" would pull a *serif* face named "DejaVu Sans" in front of it.
fn generic_match(set: &FontSet, wanted: &str, text: &str) -> Option<usize> {
    set.entries
        .iter()
        .position(|e| !e.page_supplied && reads_as_generic(&e.family, wanted) && covers(e, text))
}

/// Whether a face's own name says it is of the generic kind `wanted`.
///
/// Most monospace faces say "mono"; most serif faces do not say "serif", they
/// say "Georgia" or "Times" — so the serif side needs the handful of names that
/// actually ship on the three platforms.
fn reads_as_generic(family: &str, wanted: &str) -> bool {
    const SERIF_NAMES: [&str; 8] = [
        "georgia",
        "times",
        "cambria",
        "garamond",
        "palatino",
        "charter",
        "book antiqua",
        "roman",
    ];
    // Most say "mono"; macOS's two do not.
    const MONO_NAMES: [&str; 4] = ["consol", "courier", "menlo", "monaco"];
    match wanted {
        "monospace" => {
            family.contains("mono") || MONO_NAMES.iter().any(|name| family.contains(name))
        }
        "serif" => {
            (family.contains("serif") && !family.contains("sans"))
                || SERIF_NAMES.iter().any(|name| family.contains(name))
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_generic_family_is_matched_by_what_a_face_calls_itself() {
        // The faces that actually ship on the three platforms.
        for mono in [
            "consolas",
            "dejavu sans mono",
            "menlo",
            "courier new",
            "fira mono",
        ] {
            assert!(reads_as_generic(mono, "monospace"), "{mono} is monospace");
            assert!(!reads_as_generic(mono, "serif"), "{mono} is not a serif");
        }
        for serif in [
            "georgia",
            "times new roman",
            "dejavu serif",
            "source serif 4",
            "cambria",
        ] {
            assert!(reads_as_generic(serif, "serif"), "{serif} is a serif");
        }
        // The trap this is shaped around: a sans face whose name contains the
        // word "sans" must not answer to `serif`.
        assert!(!reads_as_generic("dejavu sans", "serif"));
        assert!(!reads_as_generic("segoe ui", "serif"));
        assert!(!reads_as_generic("arial", "monospace"));
        // `sans-serif` is the default chain, so nothing answers to it here —
        // matching "sans" would put a serif in front of the body face.
        assert!(!reads_as_generic("dejavu sans", "sans-serif"));
        assert!(!reads_as_generic("georgia", "cursive"));
    }
}

/// Whether this font has a glyph for every character of `text`.
fn covers(entry: &FontEntry, text: &str) -> bool {
    text.chars()
        .all(|c| c.is_whitespace() || entry.shaper.glyph_index(c).is_some())
}

/// A glyph placed relative to the start of its run (y is up-positive, like the font).
#[derive(Clone)]
pub struct PositionedGlyph {
    pub id: u16,
    pub x: f32,
    pub y: f32,
}

/// Shape `text` at `size` px with one font, returning its glyphs and total advance width.
pub fn shape_run(entry: &FontEntry, text: &str, size: f32) -> (Vec<PositionedGlyph>, f32) {
    let mut buffer = rustybuzz::UnicodeBuffer::new();
    buffer.push_str(text);
    // Infers script, direction, and language from the text itself.
    buffer.guess_segment_properties();

    let output = rustybuzz::shape(entry.shaper, &[], buffer);
    let scale = size / entry.shaper.units_per_em() as f32;

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

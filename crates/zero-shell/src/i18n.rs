//! The interface, in the user's language.
//!
//! Strings are looked up **by their English text**, not by an id. There is
//! therefore no key to keep in sync with anything, an untranslated string shows
//! in English rather than as a missing-key marker, and the source stays
//! readable: `t("New tab")` says what it draws.
//!
//! ponytail: a linear scan over a small table, called per chrome string per
//! frame. It is a few dozen comparisons; a map is the fix if the table grows.

use crate::settings;

/// `(English, Hindi)`. Everything the chrome says, and nothing else — page
/// content is the site's business, not ours.
const HINDI: &[(&str, &str)] = &[
    // Tooltips
    ("Tab rail", "टैब पट्टी"),
    ("Back", "पीछे"),
    ("Forward", "आगे"),
    ("Reload", "फिर से लोड करें"),
    ("Bookmark this page", "इस पृष्ठ को सहेजें"),
    ("Bookmarks", "सहेजे गए पृष्ठ"),
    ("Ask about this page", "इस पृष्ठ के बारे में पूछें"),
    ("More", "और"),
    ("New tab", "नया टैब"),
    ("Search your tabs", "अपने टैब खोजें"),
    ("Trackers blocked here", "यहाँ ट्रैकर रोके गए"),
    ("Page zoom", "पृष्ठ ज़ूम"),
    ("to reset", "पुनः सेट करने के लिए"),
    // Menu
    ("Reopen closed tab", "बंद टैब फिर खोलें"),
    ("Pin this tab", "इस टैब को पिन करें"),
    ("Unpin this tab", "इस टैब को अनपिन करें"),
    ("Zoom", "ज़ूम"),
    ("Find on page", "पृष्ठ में खोजें"),
    ("Save page", "पृष्ठ सहेजें"),
    ("View source", "स्रोत देखें"),
    ("History", "इतिहास"),
    ("Downloads", "डाउनलोड"),
    ("Settings", "सेटिंग्स"),
    // Built-in pages
    ("Assistant", "सहायक"),
    ("Unknown page", "अज्ञात पृष्ठ"),
    ("No built-in page at", "यहाँ कोई अंतर्निहित पृष्ठ नहीं है:"),
    ("Nothing visited yet.", "अभी तक कुछ नहीं देखा गया।"),
    ("Most recent first", "सबसे नया पहले"),
    (
        "No bookmarks yet. Press Ctrl+D on a page to add one.",
        "अभी कोई पृष्ठ सहेजा नहीं गया। किसी पृष्ठ पर Ctrl+D दबाएँ।",
    ),
    ("Saved pages, newest first.", "सहेजे गए पृष्ठ, सबसे नए पहले।"),
    // Settings
    ("APPEARANCE", "रूप"),
    ("SEARCH", "खोज"),
    ("PRIVACY", "निजता"),
    ("ABOUT", "परिचय"),
    (
        "Every preference is stored on this device, as text you can read.",
        "हर सेटिंग इसी उपकरण पर, पढ़ी जा सकने वाली सादी लिखावट में रहती है।",
    ),
    ("Tab layout", "टैब का ढाँचा"),
    ("A rail down the side, or a strip across the top", "किनारे की पट्टी, या ऊपर की पंक्ति"),
    ("Tab rail", "टैब पट्टी"),
    ("How much of the vertical rail stays open", "खड़ी पट्टी कितनी खुली रहे"),
    ("The size new tabs open at. Ctrl+= and Ctrl+- change one tab", "नए टैब किस आकार में खुलें। Ctrl+= और Ctrl+- एक टैब बदलते हैं"),
    ("Language", "भाषा"),
    ("What the browser's own screens are written in", "ब्राउज़र के अपने पृष्ठ किस भाषा में लिखे हों"),
    ("Theme", "रंग-रूप"),
    ("Light is designed but not built yet", "हल्का रूप बनाया गया है, बना नहीं"),
    ("Animation", "गति"),
    (
        "Slides the tab rail open and closed. Turn off to change it instantly",
        "टैब पट्टी सरककर खुलती-बंद होती है। बंद करने पर बदलाव तुरंत होगा",
    ),
    ("Search engine", "खोज इंजन"),
    ("Where the address bar sends anything that isn't a URL", "पता पट्टी में लिखा गैर-पता कहाँ भेजा जाए"),
    ("Block trackers", "ट्रैकर रोकें"),
    (
        "Drops requests to known tracking and ad hosts before they are made",
        "ज्ञात ट्रैकिंग और विज्ञापन पतों के अनुरोध भेजे ही नहीं जाते",
    ),
    ("Reopen tabs at launch", "शुरू करते ही टैब लौटाएँ"),
    ("Restores the last session instead of starting on a new tab", "नए टैब के बजाय पिछला सत्र खोलता है"),
    ("Engine", "इंजन"),
    ("HTML, CSS and JavaScript, written from scratch in Rust", "HTML, CSS और JavaScript, रस्ट में शुरू से लिखे गए"),
    ("Profile folder", "प्रोफ़ाइल फ़ोल्डर"),
    ("Where history, bookmarks and this file live", "इतिहास, सहेजे पृष्ठ और यह फ़ाइल यहाँ रहते हैं"),
    ("Source", "स्रोत"),
    ("Zero is open source, Apache-2.0", "ज़ीरो मुक्त स्रोत है, Apache-2.0"),
    // Option labels
    ("Vertical", "खड़ा"),
    ("Horizontal", "आड़ा"),
    ("Expanded", "खुली"),
    ("Icons", "चिह्न"),
    ("Hidden", "छिपी"),
    ("Dark", "गहरा"),
    ("Light", "हल्का"),
    ("On", "चालू"),
    ("Off", "बंद"),
];

/// The current language's table, or `None` for English.
fn table() -> Option<&'static [(&'static str, &'static str)]> {
    match settings::current().language() {
        "hi" => Some(HINDI),
        _ => None,
    }
}

/// This string, in the current language. Untranslated text passes through.
pub fn t(english: &str) -> String {
    match table().and_then(|table| table.iter().find(|(en, _)| *en == english)) {
        Some((_, translated)) => translated.to_string(),
        None => english.to_string(),
    }
}

/// A label whose two halves are separated by `·` — a phrase and the key that
/// triggers it. Only the phrase is translated; `Ctrl+T` is the same everywhere.
pub fn t_tip(tip: &str) -> String {
    match tip.split_once("  ·  ") {
        Some((phrase, key)) => format!("{}  ·  {}", t_phrase(phrase), key),
        None => t_phrase(tip),
    }
}

/// A phrase that may end in a trailing clause we translate separately
/// (`Page zoom  ·  Ctrl+0 to reset`).
fn t_phrase(phrase: &str) -> String {
    match phrase.split_once(" to reset") {
        Some((head, _)) => format!("{} {}", t(head), t("to reset")),
        None => t(phrase),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::Settings;

    #[test]
    fn strings_translate_and_fall_back() {
        settings::preview(Settings { language: 1, ..Settings::default() });
        assert_eq!(t("New tab"), "नया टैब");
        // Anything not in the table shows in English rather than blank.
        assert_eq!(t("Frobnicate"), "Frobnicate");
        // Shortcuts stay as they are; only the phrase moves.
        assert!(t_tip("New tab  ·  Ctrl+T").ends_with("  ·  Ctrl+T"));
        assert!(t_tip("New tab  ·  Ctrl+T").starts_with("नया टैब"));

        settings::preview(Settings::default());
        assert_eq!(t("New tab"), "New tab");
    }

    /// Every tooltip and menu label the chrome can draw has to survive the
    /// translation path — a `·` in the wrong place would swallow the shortcut.
    #[test]
    fn no_label_loses_its_shortcut() {
        settings::preview(Settings { language: 1, ..Settings::default() });
        for (_, tip) in crate::app::TIPS {
            let out = t_tip(tip);
            if let Some((_, key)) = tip.split_once("  ·  ") {
                assert!(out.ends_with(key), "{tip} lost {key}");
            }
        }
        settings::preview(Settings::default());
    }
}

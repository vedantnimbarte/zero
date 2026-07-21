//! A tolerant recursive-descent HTML parser.
//!
//! Handles the messy realities of real documents: doctypes, comments, void
//! elements (`<meta>`, `<br>`, ...), self-closing tags, raw-text elements
//! (`<script>`/`<style>`), unquoted/boolean attributes, and mismatched close
//! tags — recovering instead of panicking.
//!
//! ponytail: NOT full WHATWG tokenization — no implied tags (auto `<tbody>`,
//! `<p>` auto-close), no adoption-agency error recovery, no full entity table.
//! Enough to parse simple real pages without crashing (docs/01-ARCHITECTURE.md §3).

use crate::dom;
use std::collections::HashMap;

const VOID_ELEMENTS: &[&str] = &[
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source",
    "track", "wbr",
];
const RAW_TEXT_ELEMENTS: &[&str] = &["script", "style"];

pub fn parse(source: String) -> dom::Node {
    let mut parser = Parser {
        pos: 0,
        input: source,
    };
    let mut nodes = parser.parse_nodes();
    // Whitespace *between* top-level tags belongs to no element and is not
    // content — keeping it would wrap a well-formed document in a second,
    // synthetic <html>, which changes what the page's root element is.
    nodes.retain(|node| !matches!(&node.node_type, dom::NodeType::Text(t) if t.trim().is_empty()));
    if nodes.len() == 1 {
        nodes.swap_remove(0)
    } else {
        dom::elem("html".to_string(), HashMap::new(), nodes)
    }
}

struct Parser {
    pos: usize,
    input: String,
}

impl Parser {
    fn next_char(&self) -> char {
        self.input[self.pos..].chars().next().unwrap()
    }

    fn next_char_or(&self, default: char) -> char {
        if self.eof() {
            default
        } else {
            self.next_char()
        }
    }

    fn starts_with(&self, s: &str) -> bool {
        self.input[self.pos..].starts_with(s)
    }

    /// Case-insensitive, byte-safe prefix check (used for raw-text close tags).
    fn starts_with_ci(&self, s: &str) -> bool {
        let bytes = self.input.as_bytes();
        let sb = s.as_bytes();
        self.pos + sb.len() <= bytes.len()
            && bytes[self.pos..self.pos + sb.len()].eq_ignore_ascii_case(sb)
    }

    fn eof(&self) -> bool {
        self.pos >= self.input.len()
    }

    fn consume_char(&mut self) -> char {
        let mut iter = self.input[self.pos..].char_indices();
        let (_, cur_char) = iter.next().unwrap();
        let (next_pos, _) = iter.next().unwrap_or((cur_char.len_utf8(), ' '));
        self.pos += next_pos;
        cur_char
    }

    fn consume_while<F: Fn(char) -> bool>(&mut self, test: F) -> String {
        let mut result = String::new();
        while !self.eof() && test(self.next_char()) {
            result.push(self.consume_char());
        }
        result
    }

    fn consume_whitespace(&mut self) {
        self.consume_while(char::is_whitespace);
    }

    fn parse_nodes(&mut self) -> Vec<dom::Node> {
        let mut nodes = Vec::new();
        loop {
            if self.eof() || self.starts_with("</") {
                break; // end of input, or parent's close tag
            }
            if self.starts_with("<!--") {
                self.skip_comment();
            } else if self.starts_with("<!") || self.starts_with("<?") {
                self.skip_until_gt(); // doctype / processing instruction
            } else if self.starts_with("<") {
                nodes.push(self.parse_element());
            } else if let Some(text) = self.parse_text() {
                nodes.push(text);
            }
        }
        nodes
    }

    fn parse_text(&mut self) -> Option<dom::Node> {
        let raw = self.consume_while(|c| c != '<');
        let decoded = decode_entities(&raw);
        // Whitespace-only text is kept: whether it collapses is a styling
        // question (`white-space`), not a parsing one, and inside a <pre> it is
        // the indentation of the code. Text of no length at all is still
        // nothing — back-to-back tags must not sprout empty nodes.
        (!decoded.is_empty()).then(|| dom::text(decoded))
    }

    fn parse_element(&mut self) -> dom::Node {
        self.consume_char(); // '<'
        let tag = self.parse_tag_name().to_ascii_lowercase();
        let attrs = self.parse_attributes();

        self.consume_whitespace();
        let self_closing = self.starts_with("/");
        if self_closing {
            self.consume_char();
        }
        if self.starts_with(">") {
            self.consume_char();
        } else {
            self.skip_until_gt();
        }

        if self_closing || VOID_ELEMENTS.contains(&tag.as_str()) {
            return dom::elem(tag, attrs, vec![]);
        }
        if RAW_TEXT_ELEMENTS.contains(&tag.as_str()) {
            let text = self.consume_raw_text(&tag);
            let children = if text.trim().is_empty() {
                vec![]
            } else {
                vec![dom::text(text)]
            };
            self.consume_close_tag();
            return dom::elem(tag, attrs, children);
        }

        let children = self.parse_nodes();
        self.consume_close_tag();
        dom::elem(tag, attrs, children)
    }

    /// Consume one `</...>` if present — tolerant of a mismatched name.
    fn consume_close_tag(&mut self) {
        if self.starts_with("</") {
            self.pos += 2;
            self.parse_tag_name();
            self.consume_whitespace();
            if self.starts_with(">") {
                self.consume_char();
            } else {
                self.skip_until_gt();
            }
        }
    }

    fn consume_raw_text(&mut self, tag: &str) -> String {
        let close = format!("</{tag}");
        let mut result = String::new();
        while !self.eof() && !self.starts_with_ci(&close) {
            result.push(self.consume_char());
        }
        result
    }

    fn parse_attributes(&mut self) -> dom::AttrMap {
        let mut attributes = HashMap::new();
        loop {
            self.consume_whitespace();
            if self.eof() || self.starts_with(">") || self.starts_with("/") {
                break;
            }
            let (name, value) = self.parse_attr();
            if name.is_empty() {
                self.consume_char(); // stray char; keep making progress
            } else {
                attributes.insert(name, value);
            }
        }
        attributes
    }

    fn parse_attr(&mut self) -> (String, String) {
        let name = self
            .consume_while(|c| !c.is_whitespace() && c != '=' && c != '>' && c != '/')
            .to_ascii_lowercase();
        self.consume_whitespace();
        if self.starts_with("=") {
            self.consume_char();
            self.consume_whitespace();
            (name, decode_entities(&self.parse_attr_value()))
        } else {
            (name, String::new()) // boolean attribute
        }
    }

    fn parse_attr_value(&mut self) -> String {
        let c = self.next_char_or('>');
        if c == '"' || c == '\'' {
            self.consume_char();
            let value = self.consume_while(|ch| ch != c);
            if !self.eof() {
                self.consume_char();
            }
            value
        } else {
            self.consume_while(|ch| !ch.is_whitespace() && ch != '>')
        }
    }

    fn parse_tag_name(&mut self) -> String {
        self.consume_while(|c| c.is_ascii_alphanumeric() || c == '-' || c == ':')
    }

    fn skip_comment(&mut self) {
        self.pos += 4; // "<!--"
        while !self.eof() && !self.starts_with("-->") {
            self.consume_char();
        }
        if self.starts_with("-->") {
            self.pos += 3;
        }
    }

    fn skip_until_gt(&mut self) {
        self.consume_while(|c| c != '>');
        if self.starts_with(">") {
            self.consume_char();
        }
    }
}

/// The named references common enough to matter. Numeric references cover
/// everything else, including every Indic codepoint.
///
/// ponytail: HTML5 defines ~2200 names; an unknown one is left as written,
/// which reads as the source text rather than as a wrong character.
const ENTITIES: &[(&str, char)] = &[
    ("amp", '&'),
    ("lt", '<'),
    ("gt", '>'),
    ("quot", '"'),
    ("apos", '\''),
    ("nbsp", '\u{a0}'),
    ("mdash", '—'),
    ("ndash", '–'),
    ("hellip", '…'),
    ("copy", '©'),
    ("reg", '®'),
    ("trade", '™'),
    ("laquo", '«'),
    ("raquo", '»'),
    ("ldquo", '“'),
    ("rdquo", '”'),
    ("lsquo", '‘'),
    ("rsquo", '’'),
    ("times", '×'),
    ("middot", '·'),
    ("deg", '°'),
    ("bull", '•'),
    ("euro", '€'),
    ("pound", '£'),
    ("rupee", '₹'),
];

/// Decode character references in one pass.
///
/// One pass matters: replacing `&amp;` before `&lt;` would turn the *escaped*
/// text `&amp;lt;` into a real `<`.
fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find('&') {
        out.push_str(&rest[..start]);
        let tail = &rest[start + 1..];
        // A reference is short; anything longer is a stray ampersand.
        match tail.find(';').filter(|end| *end <= 10) {
            Some(end) => match decode_one(&tail[..end]) {
                Some(c) => {
                    out.push(c);
                    rest = &tail[end + 1..];
                }
                None => {
                    out.push('&'); // unknown name: leave it as the author wrote it
                    rest = tail;
                }
            },
            None => {
                out.push('&');
                rest = tail;
            }
        }
    }
    out.push_str(rest);
    out
}

/// One reference body (between `&` and `;`), named or numeric.
fn decode_one(body: &str) -> Option<char> {
    if let Some(digits) = body.strip_prefix('#') {
        let code = match digits.strip_prefix(['x', 'X']) {
            Some(hex) => u32::from_str_radix(hex, 16).ok()?,
            None => digits.parse().ok()?,
        };
        return char::from_u32(code);
    }
    ENTITIES.iter().find(|(name, _)| *name == body).map(|(_, c)| *c)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dom::NodeType;

    #[test]
    fn decodes_named_and_numeric_references() {
        assert_eq!(decode_entities("a &mdash; b"), "a — b");
        assert_eq!(decode_entities("5 &times; 3 &deg;"), "5 × 3 °");
        // Numeric, decimal and hex — this is how non-Latin text often arrives.
        assert_eq!(decode_entities("&#2325;&#x915;"), "कक");
        // Escaped markup decodes once, not twice.
        assert_eq!(decode_entities("&amp;lt;b&amp;gt;"), "&lt;b&gt;");
        // A stray ampersand, an unknown name, and a runaway `&` all survive.
        assert_eq!(decode_entities("Tom & Jerry"), "Tom & Jerry");
        assert_eq!(decode_entities("&nosuch;"), "&nosuch;");
        assert_eq!(decode_entities("a&b"), "a&b");
    }

    #[test]
    fn parses_nested_elements_and_attrs() {
        let node = parse("<div id=\"x\" class=\"a b\"><p>hi</p></div>".to_string());
        match node.node_type {
            NodeType::Element(ref e) => {
                assert_eq!(e.tag_name, "div");
                assert_eq!(e.id().map(String::as_str), Some("x"));
                assert!(e.classes().contains("a") && e.classes().contains("b"));
            }
            _ => panic!("expected element"),
        }
        assert_eq!(node.children.len(), 1);
    }

    #[test]
    fn tolerates_doctype_comments_void_and_rawtext() {
        // Should not panic, and should recover the <html> root.
        let node = parse(
            "<!DOCTYPE html><html><head><meta charset=\"utf-8\">\
             <style>body{color:red}</style></head>\
             <body><!-- hi --><p>ok</p><br></body></html>"
                .to_string(),
        );
        match node.node_type {
            NodeType::Element(ref e) => assert_eq!(e.tag_name, "html"),
            _ => panic!("expected <html> root"),
        }
    }
}

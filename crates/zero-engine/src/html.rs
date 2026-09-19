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
        depth: 0,
    };
    let mut nodes = parser.parse_nodes();
    // Whitespace *between* top-level tags belongs to no element and is not
    // content — keeping it would wrap a well-formed document in a second,
    // synthetic <html>, which changes what the page's root element is.
    nodes.retain(|node| !matches!(&node.node_type, dom::NodeType::Text(t) if t.trim().is_empty()));
    let mut root = if nodes.len() == 1 {
        nodes.swap_remove(0)
    } else {
        dom::elem("html".to_string(), HashMap::new(), nodes)
    };
    dom::stamp_positions(&mut root);
    root
}

struct Parser {
    pos: usize,
    input: String,
    /// How deep the open elements go. The parser recurses per element, so a
    /// page nested thousands deep would otherwise run the stack out — which a
    /// hostile page can arrange in a few hundred bytes of `<div>`.
    depth: usize,
}

/// Past this, further nesting is flattened rather than recursed into.
///
/// Every stage after parsing recurses too — styling, layout, paint, and the
/// tree's own drop — so the limit is set by whichever of them has the fattest
/// stack frame, not by the parser. Measured: the pipeline survives 256 levels
/// and dies by 320, so this leaves a factor of two. Real documents run tens
/// deep; the deepest thing on a normal page is a table inside a layout inside
/// a wrapper.
const MAX_DEPTH: usize = 128;

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

        // Too deep: the children are still parsed, so the document is not
        // truncated, but they become this element's siblings rather than
        // another stack frame each.
        if self.depth >= MAX_DEPTH {
            return dom::elem(tag, attrs, vec![]);
        }
        self.depth += 1;
        let children = self.parse_nodes();
        self.depth -= 1;
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
            (name, decode_attr_entities(&self.parse_attr_value()))
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

/// Decode character references in one pass.
///
/// One pass matters: replacing `&amp;` before `&lt;` would turn the *escaped*
/// text `&amp;lt;` into a real `<`.
fn decode_entities(s: &str) -> String {
    decode_refs(s, false)
}

/// The same, inside an attribute value, where a semicolon-less reference
/// followed by an alphanumeric or `=` must stay literal — otherwise a query
/// string like `?x=1&copy=2` gets mangled into `?x=1©=2`.
fn decode_attr_entities(s: &str) -> String {
    decode_refs(s, true)
}

fn decode_refs(s: &str, in_attribute: bool) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find('&') {
        out.push_str(&rest[..start]);
        let tail = &rest[start + 1..];
        match decode_one(tail, in_attribute) {
            Some((text, consumed)) => {
                out.push_str(&text);
                rest = &tail[consumed..];
            }
            // Not a reference at all: a stray ampersand is ordinary text.
            None => {
                out.push('&');
                rest = tail;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Read one reference from just after the `&`, returning what it expands to
/// and how much of `tail` it used.
fn decode_one(tail: &str, in_attribute: bool) -> Option<(String, usize)> {
    match tail.strip_prefix('#') {
        Some(digits) => decode_numeric(digits).map(|(text, used)| (text, used + 1)),
        None => decode_named(tail, in_attribute),
    }
}

/// `&#1234;` / `&#x4d2;`. The terminating semicolon is expected but not
/// required — pages have relied on that for decades.
fn decode_numeric(digits: &str) -> Option<(String, usize)> {
    let (radix, body) = match digits.strip_prefix(['x', 'X']) {
        Some(hex) => (16, hex),
        None => (10, digits),
    };
    let len = body
        .find(|c: char| !c.is_digit(radix))
        .unwrap_or(body.len());
    if len == 0 {
        return None;
    }
    let code = u32::from_str_radix(&body[..len], radix).unwrap_or(u32::MAX);
    // The prefix (`x`), the digits, and the semicolon if there is one.
    let mut used = (digits.len() - body.len()) + len;
    if body[len..].starts_with(';') {
        used += 1;
    }
    Some((replacement_char(code).to_string(), used))
}

/// The spec's replacement table for numeric references.
///
/// A code point in the C1 range means the Windows-1252 character an author
/// nearly always intended — `&#147;` is a left double quote, not a control
/// character. Nulls, surrogates and out-of-range values become U+FFFD.
fn replacement_char(code: u32) -> char {
    const WINDOWS_1252: [(u32, u32); 27] = [
        (0x80, 0x20AC),
        (0x82, 0x201A),
        (0x83, 0x0192),
        (0x84, 0x201E),
        (0x85, 0x2026),
        (0x86, 0x2020),
        (0x87, 0x2021),
        (0x88, 0x02C6),
        (0x89, 0x2030),
        (0x8A, 0x0160),
        (0x8B, 0x2039),
        (0x8C, 0x0152),
        (0x8E, 0x017D),
        (0x91, 0x2018),
        (0x92, 0x2019),
        (0x93, 0x201C),
        (0x94, 0x201D),
        (0x95, 0x2022),
        (0x96, 0x2013),
        (0x97, 0x2014),
        (0x98, 0x02DC),
        (0x99, 0x2122),
        (0x9A, 0x0161),
        (0x9B, 0x203A),
        (0x9C, 0x0153),
        (0x9E, 0x017E),
        (0x9F, 0x0178),
    ];
    if let Some((_, mapped)) = WINDOWS_1252.iter().find(|(from, _)| *from == code) {
        return char::from_u32(*mapped).unwrap_or('\u{fffd}');
    }
    match code {
        0 => '\u{fffd}',
        _ => char::from_u32(code).unwrap_or('\u{fffd}'),
    }
}

/// A named reference, matched longest-first against the spec's table.
///
/// Longest match, not scan-to-semicolon: about a hundred names are valid
/// without one, so `&notin` is `&not` followed by `in` while `&notin;` is a
/// single character.
fn decode_named(tail: &str, in_attribute: bool) -> Option<(String, usize)> {
    // Names are ASCII letters and digits with an optional trailing semicolon,
    // so the scan never has to worry about a char boundary.
    let scan = tail
        .find(|c: char| !c.is_ascii_alphanumeric() && c != ';')
        .unwrap_or(tail.len())
        .min(crate::entities::LONGEST);
    for len in (1..=scan).rev() {
        let candidate = &tail[..len];
        let Ok(found) = crate::entities::NAMED.binary_search_by(|(name, _)| (*name).cmp(candidate))
        else {
            continue;
        };
        let (name, value) = crate::entities::NAMED[found];
        if !name.ends_with(';') && in_attribute {
            // The legacy URL-compatibility rule: inside an attribute a
            // semicolon-less reference that runs straight into a name or an
            // `=` was never meant as a reference.
            let next = tail[len..].chars().next();
            if matches!(next, Some(c) if c.is_ascii_alphanumeric() || c == '=') {
                return None;
            }
        }
        return Some((value.to_string(), len));
    }
    None
}

#[cfg(test)]
mod tests {
    /// A page can nest as deep as it likes; the parser may not.
    #[test]
    fn absurd_nesting_is_flattened_rather_than_overflowing_the_stack() {
        let deep = format!("{}x{}", "<div>".repeat(20_000), "</div>".repeat(20_000));
        let dom = super::parse(deep);
        // It parsed, and the text inside is still somewhere in the tree.
        fn depth_and_text(node: &super::dom::Node) -> (usize, String) {
            let mut deepest = 0;
            let mut text = match &node.node_type {
                super::dom::NodeType::Text(t) => t.clone(),
                _ => String::new(),
            };
            for child in &node.children {
                let (d, t) = depth_and_text(child);
                deepest = deepest.max(d + 1);
                text.push_str(&t);
            }
            (deepest, text)
        }
        let (depth, text) = depth_and_text(&dom);
        assert!(depth <= super::MAX_DEPTH + 2, "nested {depth} deep");
        assert!(text.contains('x'), "the content was dropped, not flattened");
    }

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
    fn named_references_match_longest_first_and_semicolons_are_optional() {
        // The whole spec table, not a hand-picked subset.
        assert_eq!(decode_entities("&NotEqualTilde;"), "\u{2242}\u{338}");
        assert_eq!(
            decode_entities("&CounterClockwiseContourIntegral;"),
            "\u{2233}"
        );
        // About a hundred names are valid without the semicolon, and the
        // match is by longest name, not by scanning to a delimiter.
        assert_eq!(decode_entities("&copy 2026"), "\u{a9} 2026");
        assert_eq!(decode_entities("&notin;"), "\u{2209}");
        assert_eq!(decode_entities("&notit;"), "\u{ac}it;");
        // A name that is not in the table stays exactly as written.
        assert_eq!(decode_entities("&nosuch;"), "&nosuch;");
    }

    #[test]
    fn attribute_values_keep_legacy_query_strings_intact() {
        // `&copy=2` inside an attribute is a query parameter, not a copyright
        // sign — but the same text in body content is a reference.
        assert_eq!(decode_attr_entities("?x=1&copy=2"), "?x=1&copy=2");
        assert_eq!(decode_attr_entities("?x=1&copy;=2"), "?x=1\u{a9}=2");
        assert_eq!(decode_entities("?x=1&copy=2"), "?x=1\u{a9}=2");

        // And the same through a real parse, not just the helper.
        fn href(node: &super::dom::Node) -> Option<String> {
            if let NodeType::Element(e) = &node.node_type {
                if let Some(href) = e.attributes.get("href") {
                    return Some(href.clone());
                }
            }
            node.children.iter().find_map(href)
        }
        let node = parse("<a href=\"/s?q=a&amp=b&lang=hi\">x</a>".to_string());
        assert_eq!(href(&node).as_deref(), Some("/s?q=a&amp=b&lang=hi"));
    }

    #[test]
    fn numeric_references_apply_the_replacement_table() {
        // The C1 range means the Windows-1252 character the author intended.
        assert_eq!(
            decode_entities("&#147;quoted&#148;"),
            "\u{201c}quoted\u{201d}"
        );
        assert_eq!(decode_entities("&#128;"), "\u{20ac}");
        // Nulls, surrogates and out-of-range values become U+FFFD rather than
        // vanishing or aborting.
        assert_eq!(decode_entities("&#0;"), "\u{fffd}");
        assert_eq!(decode_entities("&#xD800;"), "\u{fffd}");
        assert_eq!(decode_entities("&#x110000;"), "\u{fffd}");
        // The semicolon is expected but pages have done without it forever.
        assert_eq!(decode_entities("&#65 B"), "A B");
        // `&#` with no digits is not a reference.
        assert_eq!(decode_entities("a&#b"), "a&#b");
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

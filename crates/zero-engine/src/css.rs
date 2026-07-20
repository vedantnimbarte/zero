//! A tolerant CSS parser: rules of simple selectors + declarations.
//!
//! Understands simple selectors (tag / #id / .class / *), px lengths, keywords,
//! and #rgb / #rrggbb colors. Anything else — complex selectors, at-rules,
//! functions like `rgb()`, units like `%`/`em`, multi-value shorthands — is
//! skipped rather than fatal.
//!
//! ponytail: dropping unsupported rules/values means real pages lose most of
//! their styling, but the parser never panics. Property coverage grows per Phase
//! (docs/01-ARCHITECTURE.md §10).

#[derive(Debug)]
pub struct Stylesheet {
    pub rules: Vec<Rule>,
}

#[derive(Debug)]
pub struct Rule {
    pub selectors: Vec<Selector>,
    pub declarations: Vec<Declaration>,
}

#[derive(Debug)]
pub enum Selector {
    Simple(SimpleSelector),
}

#[derive(Debug)]
pub struct SimpleSelector {
    pub tag_name: Option<String>,
    pub id: Option<String>,
    pub class: Vec<String>,
}

#[derive(Debug)]
pub struct Declaration {
    pub name: String,
    pub value: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Keyword(String),
    Length(f32, Unit),
    ColorValue(Color),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Unit {
    Px,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

/// (id count, class count, tag count) — CSS specificity, higher wins.
pub type Specificity = (usize, usize, usize);

impl Selector {
    pub fn specificity(&self) -> Specificity {
        let Selector::Simple(ref s) = *self;
        (s.id.iter().count(), s.class.len(), s.tag_name.iter().count())
    }
}

impl Value {
    /// Length in px; anything non-length (keywords, colors) resolves to 0.
    pub fn to_px(&self) -> f32 {
        match *self {
            Value::Length(f, Unit::Px) => f,
            _ => 0.0,
        }
    }
}

pub fn parse(source: String) -> Stylesheet {
    let mut parser = Parser { pos: 0, input: source };
    Stylesheet { rules: parser.parse_rules() }
}

fn is_ident(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '-' || c == '_'
}

/// Interpret a raw value string, returning `None` for anything unsupported.
fn classify_value(s: &str) -> Option<Value> {
    if s.is_empty() {
        return None;
    }
    if let Some(hex) = s.strip_prefix('#') {
        return parse_hex_color(hex);
    }
    if let Some(num) = s.strip_suffix("px") {
        return num.trim().parse::<f32>().ok().map(|f| Value::Length(f, Unit::Px));
    }
    // A single bare keyword (e.g. `block`, `auto`). A CSS identifier can't start with
    // a digit, so digit-prefixed tokens with unknown units (`60vw`, `5em`) are rejected
    // here rather than becoming a bogus keyword that silently resolves to 0.
    if s.chars().all(is_ident) && !s.starts_with(|c: char| c.is_ascii_digit()) {
        return Some(Value::Keyword(s.to_ascii_lowercase()));
    }
    None
}

fn parse_hex_color(hex: &str) -> Option<Value> {
    let byte = |s: &str| u8::from_str_radix(s, 16).ok();
    let color = match hex.len() {
        6 => Color { r: byte(&hex[0..2])?, g: byte(&hex[2..4])?, b: byte(&hex[4..6])?, a: 255 },
        3 => {
            let dup = |c: &str| byte(&format!("{c}{c}"));
            Color { r: dup(&hex[0..1])?, g: dup(&hex[1..2])?, b: dup(&hex[2..3])?, a: 255 }
        }
        _ => return None,
    };
    Some(Value::ColorValue(color))
}

struct Parser {
    pos: usize,
    input: String,
}

impl Parser {
    fn parse_rules(&mut self) -> Vec<Rule> {
        let mut rules = Vec::new();
        loop {
            self.consume_whitespace();
            if self.eof() {
                break;
            }
            if self.starts_with("@") {
                self.skip_at_rule();
            } else if self.starts_with("}") {
                self.consume_char(); // stray brace
            } else if let Some(rule) = self.parse_rule() {
                rules.push(rule);
            }
        }
        rules
    }

    fn parse_rule(&mut self) -> Option<Rule> {
        let selectors = self.parse_selectors()?;
        let declarations = self.parse_declarations();
        if selectors.is_empty() {
            None
        } else {
            Some(Rule { selectors, declarations })
        }
    }

    /// Returns `None` (and skips the whole rule) if any selector isn't a bare
    /// simple selector — descendant/child/pseudo/attribute selectors are dropped.
    fn parse_selectors(&mut self) -> Option<Vec<Selector>> {
        let mut selectors = Vec::new();
        loop {
            selectors.push(Selector::Simple(self.parse_simple_selector()));
            self.consume_whitespace();
            match self.next_char_or('\0') {
                ',' => {
                    self.consume_char();
                    self.consume_whitespace();
                }
                '{' => break,
                _ => {
                    self.skip_block(); // unsupported selector — drop the rule
                    return None;
                }
            }
        }
        selectors.sort_by(|a, b| b.specificity().cmp(&a.specificity()));
        Some(selectors)
    }

    fn parse_simple_selector(&mut self) -> SimpleSelector {
        let mut selector = SimpleSelector { tag_name: None, id: None, class: Vec::new() };
        loop {
            match self.next_char_or('\0') {
                '#' => {
                    self.consume_char();
                    selector.id = Some(self.parse_identifier());
                }
                '.' => {
                    self.consume_char();
                    selector.class.push(self.parse_identifier());
                }
                '*' => {
                    self.consume_char();
                }
                c if is_ident(c) => {
                    selector.tag_name = Some(self.parse_identifier().to_ascii_lowercase());
                }
                _ => break,
            }
        }
        selector
    }

    fn parse_declarations(&mut self) -> Vec<Declaration> {
        let mut declarations = Vec::new();
        if !self.starts_with("{") {
            return declarations;
        }
        self.consume_char(); // '{'
        loop {
            self.consume_whitespace();
            if self.eof() || self.starts_with("}") {
                break;
            }
            let name = self.parse_identifier().to_ascii_lowercase();
            self.consume_whitespace();
            if !self.starts_with(":") {
                self.skip_to_decl_end(); // malformed declaration
                continue;
            }
            self.consume_char(); // ':'
            let raw = self.consume_while(|c| c != ';' && c != '}');
            if self.starts_with(";") {
                self.consume_char();
            }
            if !name.is_empty() {
                if let Some(value) = classify_value(raw.trim()) {
                    declarations.push(Declaration { name, value });
                }
            }
        }
        if self.starts_with("}") {
            self.consume_char();
        }
        declarations
    }

    fn skip_to_decl_end(&mut self) {
        self.consume_while(|c| c != ';' && c != '}');
        if self.starts_with(";") {
            self.consume_char();
        }
    }

    /// Skip a `{ ... }` block (brace-balanced). Called after a bad selector.
    fn skip_block(&mut self) {
        self.consume_while(|c| c != '{' && c != '}');
        if self.starts_with("}") {
            self.consume_char();
            return;
        }
        self.skip_balanced_braces();
    }

    fn skip_at_rule(&mut self) {
        // `@import ...;` or `@media ... { ... }`
        self.consume_while(|c| c != '{' && c != ';');
        if self.starts_with(";") {
            self.consume_char();
        } else if self.starts_with("{") {
            self.skip_balanced_braces();
        }
    }

    fn skip_balanced_braces(&mut self) {
        let mut depth = 0;
        while !self.eof() {
            match self.consume_char() {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                _ => {}
            }
        }
    }

    fn parse_identifier(&mut self) -> String {
        self.consume_while(is_ident)
    }

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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rule_with_length_and_color() {
        let s = parse("div.box { width: 120px; background: #ff8800; }".to_string());
        assert_eq!(s.rules.len(), 1);
        let rule = &s.rules[0];
        assert_eq!(rule.declarations[0].value, Value::Length(120.0, Unit::Px));
        assert_eq!(
            rule.declarations[1].value,
            Value::ColorValue(Color { r: 0xff, g: 0x88, b: 0x00, a: 255 })
        );
    }

    #[test]
    fn skips_unsupported_without_panic() {
        // Complex selector, at-rule, rgb(), % — all dropped; the plain rule survives.
        let s = parse(
            "@media screen { body { color: #000; } } \
             a:hover { color: red; } \
             div > p { color: blue; } \
             .ok { color: #123456; width: 50%; padding: 8px; }"
                .to_string(),
        );
        assert_eq!(s.rules.len(), 1); // only `.ok`
        let ok = &s.rules[0];
        // `width: 50%` dropped (unsupported unit); color + padding kept.
        assert_eq!(ok.declarations.len(), 2);
    }
}

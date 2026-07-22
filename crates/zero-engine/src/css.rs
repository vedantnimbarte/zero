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
    /// The `@media` condition this rule came from, if any. Evaluated against the
    /// viewport at render time — see [`media_matches`].
    pub media: Option<String>,
}

/// A selector is a chain of compounds read left to right, e.g. `nav > ul li`.
/// The last part is the *subject* — the element the rule actually styles.
#[derive(Debug)]
pub struct Selector {
    pub parts: Vec<SelectorPart>,
}

#[derive(Debug)]
pub struct SelectorPart {
    pub simple: SimpleSelector,
    /// How this part relates to the one on its left. Ignored on the first part.
    pub combinator: Combinator,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Combinator {
    /// A space: any ancestor.
    Descendant,
    /// `>`: the immediate parent.
    Child,
    /// `+`: the element immediately before it.
    NextSibling,
    /// `~`: any element before it, under the same parent.
    LaterSibling,
}

#[derive(Debug, PartialEq)]
pub struct SimpleSelector {
    pub tag_name: Option<String>,
    pub id: Option<String>,
    pub class: Vec<String>,
    pub attrs: Vec<AttrTest>,
    /// `:hover`, `:nth-child(2n)`, `:not(.x)` … all of which must hold.
    pub pseudos: Vec<Pseudo>,
}

/// A pseudo-class condition. Unknown ones never reach here — the parser drops
/// the whole rule instead, so a selector we half-understand can't misapply.
#[derive(Debug, PartialEq)]
pub enum Pseudo {
    /// Matches while the cursor is over this element or something inside it.
    Hover,
    /// `:nth-child(an+b)`. `:first-child` is `(0, 1)`; `odd` is `(2, 1)`.
    NthChild(i32, i32),
    /// `:nth-last-child(an+b)` — the same, counted from the end.
    NthLastChild(i32, i32),
    NthOfType(i32, i32),
    NthLastOfType(i32, i32),
    OnlyChild,
    OnlyOfType,
    /// `:not(...)` over one compound selector.
    Not(Box<SimpleSelector>),
    /// An attribute that is present when the state is on (`checked`, `disabled`).
    AttrPresent(&'static str),
    AttrAbsent(&'static str),
    /// A state the engine does not track (`:visited`, `:focus`, `:active`).
    /// Never matches, which leaves the element at its base styling.
    Never,
}

/// An `[attr]`, `[attr=value]`, `[attr~=value]` … condition.
#[derive(Debug, PartialEq)]
pub struct AttrTest {
    pub name: String,
    pub op: AttrOp,
    pub value: String,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AttrOp {
    /// `[attr]` — present at all.
    Exists,
    Equals,
    /// `~=` — one of a space-separated list.
    Includes,
    Prefix,
    Suffix,
    Contains,
}

impl AttrTest {
    /// Does an element's value for this attribute satisfy the test?
    pub fn matches(&self, value: Option<&str>) -> bool {
        let Some(value) = value else { return false };
        match self.op {
            AttrOp::Exists => true,
            AttrOp::Equals => value == self.value,
            AttrOp::Includes => value.split_whitespace().any(|part| part == self.value),
            // An empty operand can never match, per the selectors spec.
            AttrOp::Prefix => !self.value.is_empty() && value.starts_with(&self.value),
            AttrOp::Suffix => !self.value.is_empty() && value.ends_with(&self.value),
            AttrOp::Contains => !self.value.is_empty() && value.contains(&self.value),
        }
    }
}

impl SimpleSelector {
    /// True for `*` or for a compound we failed to read anything out of.
    pub fn is_empty(&self) -> bool {
        self.tag_name.is_none()
            && self.id.is_none()
            && self.class.is_empty()
            && self.attrs.is_empty()
            && self.pseudos.is_empty()
    }
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
    /// A unitless number, e.g. `flex-grow: 2` or `opacity: 0.5`.
    Number(f32),
    ColorValue(Color),
    /// A multi-value declaration kept verbatim (e.g. a grid track list), for
    /// properties whose grammar the generic classifier can't express.
    Raw(String),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Unit {
    Px,
    /// Relative to the element's own font size.
    Em,
    /// Relative to the root font size.
    Rem,
    /// Relative to a context-dependent base, usually the containing block's width.
    Percent,
}

/// What relative lengths resolve against. Percentages normally use the containing
/// block's width; `em` uses the element's own computed font size.
#[derive(Debug, Clone, Copy)]
pub struct LengthContext {
    pub percent_base: f32,
    pub font_size: f32,
    pub root_font_size: f32,
}

pub const DEFAULT_FONT_SIZE: f32 = 16.0;

impl Default for LengthContext {
    fn default() -> Self {
        LengthContext {
            percent_base: 0.0,
            font_size: DEFAULT_FONT_SIZE,
            root_font_size: DEFAULT_FONT_SIZE,
        }
    }
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
    /// Specificity sums over the whole chain, so `nav a` beats a bare `a`.
    pub fn specificity(&self) -> Specificity {
        self.parts.iter().fold((0, 0, 0), |(ids, classes, tags), part| {
            (
                ids + part.simple.id.iter().count(),
                // A pseudo-class counts alongside classes, per the spec.
                classes
                    + part.simple.class.len()
                    + part.simple.attrs.len()
                    + part.simple.pseudos.len(),
                tags + part.simple.tag_name.iter().count(),
            )
        })
    }

    /// The element this selector styles, ignoring its ancestor conditions.
    pub fn subject(&self) -> Option<&SimpleSelector> {
        self.parts.last().map(|part| &part.simple)
    }
}

impl Value {
    /// Absolute px, resolving relative units against `ctx`.
    pub fn resolve(&self, ctx: LengthContext) -> f32 {
        match *self {
            Value::Length(v, Unit::Px) => v,
            Value::Length(v, Unit::Em) => v * ctx.font_size,
            Value::Length(v, Unit::Rem) => v * ctx.root_font_size,
            Value::Length(v, Unit::Percent) => v / 100.0 * ctx.percent_base,
            Value::Number(n) => n,
            _ => 0.0,
        }
    }

    /// Absolute px for values that need no context. Relative units resolve to 0,
    /// so prefer [`Value::resolve`] anywhere a context is available.
    pub fn to_px(&self) -> f32 {
        self.resolve(LengthContext::default())
    }

    pub fn as_number(&self) -> Option<f32> {
        match *self {
            Value::Number(n) => Some(n),
            _ => None,
        }
    }
}

pub fn parse(source: String) -> Stylesheet {
    let mut parser = Parser {
        pos: 0,
        input: source,
    };
    Stylesheet {
        rules: parser.parse_rules(),
    }
}

/// `an+b` in any of its spellings: `odd`, `even`, `3`, `2n`, `2n+1`, `-n+3`.
fn parse_nth(text: &str) -> Option<(i32, i32)> {
    let text: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    match text.to_ascii_lowercase().as_str() {
        "odd" => return Some((2, 1)),
        "even" => return Some((2, 0)),
        _ => {}
    }
    let text = text.to_ascii_lowercase();
    let Some((a, b)) = text.split_once('n') else {
        // A bare number selects exactly one child.
        return Some((0, text.parse().ok()?));
    };
    let a = match a {
        "" | "+" => 1,
        "-" => -1,
        _ => a.parse().ok()?,
    };
    let b = match b {
        "" => 0,
        _ => b.parse().ok()?, // the sign is part of the number: "+1", "-2"
    };
    Some((a, b))
}

fn is_ident(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '-' || c == '_'
}

/// Properties whose values are lists we parse later, not single tokens.
const RAW_VALUE_PROPERTIES: &[&str] = &[
    "grid-template-columns",
    "grid-template-rows",
    "grid-column",
    "grid-row",
    "grid-area",
    "grid-template-areas",
    "grid-template",
    "box-shadow",
    "background-image",
    // `overflow: hidden auto` sets the two axes at once.
    "overflow",
    // `translate(-50%, -50%)`, read at paint time.
    "transform",
];

/// The named colours worth carrying, plus `transparent`.
///
/// ponytail: CSS defines 148 names; these are the ones that actually show up.
/// An unknown name falls through to a keyword and the declaration is ignored,
/// which leaves the element at its inherited colour rather than a wrong one.
const NAMED_COLORS: &[(&str, u32)] = &[
    ("transparent", 0x00000000),
    ("black", 0x000000ff),
    ("silver", 0xc0c0c0ff),
    ("gray", 0x808080ff),
    ("grey", 0x808080ff),
    ("white", 0xffffffff),
    ("maroon", 0x800000ff),
    ("red", 0xff0000ff),
    ("purple", 0x800080ff),
    ("fuchsia", 0xff00ffff),
    ("magenta", 0xff00ffff),
    ("green", 0x008000ff),
    ("lime", 0x00ff00ff),
    ("olive", 0x808000ff),
    ("yellow", 0xffff00ff),
    ("navy", 0x000080ff),
    ("blue", 0x0000ffff),
    ("teal", 0x008080ff),
    ("aqua", 0x00ffffff),
    ("cyan", 0x00ffffff),
    ("orange", 0xffa500ff),
    ("pink", 0xffc0cbff),
    ("brown", 0xa52a2aff),
    ("gold", 0xffd700ff),
    ("beige", 0xf5f5dcff),
    ("ivory", 0xfffff0ff),
    ("khaki", 0xf0e68cff),
    ("lavender", 0xe6e6faff),
    ("salmon", 0xfa8072ff),
    ("tan", 0xd2b48cff),
    ("violet", 0xee82eeff),
    ("indigo", 0x4b0082ff),
    ("crimson", 0xdc143cff),
    ("coral", 0xff7f50ff),
    ("tomato", 0xff6347ff),
    ("turquoise", 0x40e0d0ff),
    ("plum", 0xdda0ddff),
    ("orchid", 0xda70d6ff),
    ("wheat", 0xf5deb3ff),
    ("snow", 0xfffafaff),
    ("azure", 0xf0ffffff),
    ("darkgray", 0xa9a9a9ff),
    ("darkgrey", 0xa9a9a9ff),
    ("lightgray", 0xd3d3d3ff),
    ("lightgrey", 0xd3d3d3ff),
    ("dimgray", 0x696969ff),
    ("dimgrey", 0x696969ff),
    ("lightblue", 0xadd8e6ff),
    ("darkblue", 0x00008bff),
    ("lightgreen", 0x90ee90ff),
    ("darkgreen", 0x006400ff),
    ("darkred", 0x8b0000ff),
    ("whitesmoke", 0xf5f5f5ff),
    ("gainsboro", 0xdcdcdcff),
    ("steelblue", 0x4682b4ff),
    ("skyblue", 0x87ceebff),
    ("royalblue", 0x4169e1ff),
    ("firebrick", 0xb22222ff),
    ("chocolate", 0xd2691eff),
    ("goldenrod", 0xdaa520ff),
    ("seagreen", 0x2e8b57ff),
    ("slategray", 0x708090ff),
    ("slategrey", 0x708090ff),
];

fn named_color(name: &str) -> Option<Value> {
    let name = name.to_ascii_lowercase();
    NAMED_COLORS.iter().find(|(n, _)| *n == name).map(|(_, rgba)| {
        Value::ColorValue(Color {
            r: (rgba >> 24) as u8,
            g: (rgba >> 16) as u8,
            b: (rgba >> 8) as u8,
            a: *rgba as u8,
        })
    })
}

/// `rgb()`, `rgba()`, `hsl()` and `hsla()`, in both the comma and the modern
/// space-separated form (`rgb(0 0 0 / 50%)`).
fn parse_color_function(s: &str) -> Option<Value> {
    let (name, rest) = s.split_once('(')?;
    let body = rest.strip_suffix(')')?;
    let name = name.trim().to_ascii_lowercase();
    // Both separators mean the same thing, and `/` only ever precedes alpha.
    let parts: Vec<&str> =
        body.split([',', '/', ' ']).map(str::trim).filter(|p| !p.is_empty()).collect();
    if parts.len() < 3 {
        return None;
    }
    let alpha = match parts.get(3) {
        Some(a) => (parse_alpha(a)? * 255.0).round().clamp(0.0, 255.0) as u8,
        None => 255,
    };
    let color = match name.as_str() {
        "rgb" | "rgba" => {
            let channel = |p: &str| -> Option<u8> {
                let value = match p.strip_suffix('%') {
                    Some(pct) => pct.trim().parse::<f32>().ok()? / 100.0 * 255.0,
                    None => p.parse::<f32>().ok()?,
                };
                Some(value.round().clamp(0.0, 255.0) as u8)
            };
            Color {
                r: channel(parts[0])?,
                g: channel(parts[1])?,
                b: channel(parts[2])?,
                a: alpha,
            }
        }
        "hsl" | "hsla" => {
            let hue = parts[0].trim_end_matches("deg").parse::<f32>().ok()?;
            let pct = |p: &str| p.trim_end_matches('%').parse::<f32>().ok().map(|v| v / 100.0);
            let (r, g, b) = hsl_to_rgb(hue, pct(parts[1])?, pct(parts[2])?);
            Color { r, g, b, a: alpha }
        }
        _ => return None,
    };
    Some(Value::ColorValue(color))
}

/// Alpha is a 0-1 number or a percentage.
fn parse_alpha(text: &str) -> Option<f32> {
    match text.strip_suffix('%') {
        Some(pct) => pct.trim().parse::<f32>().ok().map(|v| v / 100.0),
        None => text.parse::<f32>().ok(),
    }
}

fn hsl_to_rgb(hue: f32, saturation: f32, lightness: f32) -> (u8, u8, u8) {
    let hue = hue.rem_euclid(360.0) / 60.0;
    let saturation = saturation.clamp(0.0, 1.0);
    let lightness = lightness.clamp(0.0, 1.0);
    let chroma = (1.0 - (2.0 * lightness - 1.0).abs()) * saturation;
    let second = chroma * (1.0 - (hue % 2.0 - 1.0).abs());
    let (r, g, b) = match hue as u32 {
        0 => (chroma, second, 0.0),
        1 => (second, chroma, 0.0),
        2 => (0.0, chroma, second),
        3 => (0.0, second, chroma),
        4 => (second, 0.0, chroma),
        _ => (chroma, 0.0, second),
    };
    let base = lightness - chroma / 2.0;
    let byte = |v: f32| ((v + base) * 255.0).round().clamp(0.0, 255.0) as u8;
    (byte(r), byte(g), byte(b))
}

/// Interpret a single CSS value, for callers outside the parser (HTML
/// presentation attributes carry CSS-shaped values).
pub fn parse_value(text: &str) -> Option<Value> {
    classify_value(text.trim())
}

/// Interpret a raw value string, returning `None` for anything unsupported.
fn classify_value(s: &str) -> Option<Value> {
    if s.is_empty() {
        return None;
    }
    if let Some(hex) = s.strip_prefix('#') {
        return parse_hex_color(hex);
    }
    if s.contains('(') && !s.starts_with("linear-gradient(") {
        if let Some(color) = parse_color_function(s) {
            return Some(color);
        }
    }
    if let Some(color) = named_color(s) {
        return Some(color);
    }
    // Functions we interpret later (gradients) are kept verbatim.
    if s.starts_with("linear-gradient(") {
        return Some(Value::Raw(s.to_string()));
    }
    for (suffix, unit) in [
        ("px", Unit::Px),
        ("rem", Unit::Rem),
        ("em", Unit::Em),
        ("%", Unit::Percent),
    ] {
        // Only a *numeric* prefix makes this a length; otherwise fall through so
        // keywords that merely end in a unit name (e.g. `system`) still parse.
        if let Some(num) = s.strip_suffix(suffix) {
            if let Ok(f) = num.trim().parse::<f32>() {
                return Some(Value::Length(f, unit));
            }
        }
    }
    // A bare number (flex-grow, opacity, line-height, z-index).
    if let Ok(n) = s.parse::<f32>() {
        return Some(Value::Number(n));
    }
    // A single bare keyword (e.g. `block`, `auto`). A CSS identifier can't start with
    // a digit, so digit-prefixed tokens with unknown units (`60vw`, `5em`) are rejected
    // here rather than becoming a bogus keyword that silently resolves to 0.
    if s.chars().all(is_ident) && !s.starts_with(|c: char| c.is_ascii_digit()) {
        return Some(Value::Keyword(s.to_ascii_lowercase()));
    }
    None
}

/// Parse a bare hex colour body (no leading `#`).
pub fn parse_color_token(hex: &str) -> Option<Value> {
    parse_hex_color(hex)
}

/// Parse a single length token (`12px`, `1.5em`, `50%`) to px.
pub fn parse_length_token(token: &str, ctx: LengthContext) -> f32 {
    for (suffix, unit) in [
        ("px", Unit::Px),
        ("rem", Unit::Rem),
        ("em", Unit::Em),
        ("%", Unit::Percent),
    ] {
        if let Some(n) = token.strip_suffix(suffix) {
            if let Ok(v) = n.trim().parse::<f32>() {
                return Value::Length(v, unit).resolve(ctx);
            }
        }
    }
    token.parse::<f32>().unwrap_or(0.0)
}

fn parse_hex_color(hex: &str) -> Option<Value> {
    let byte = |s: &str| u8::from_str_radix(s, 16).ok();
    let color = match hex.len() {
        // #rrggbbaa carries alpha, which shadows and overlays rely on.
        8 => Color {
            r: byte(&hex[0..2])?,
            g: byte(&hex[2..4])?,
            b: byte(&hex[4..6])?,
            a: byte(&hex[6..8])?,
        },
        6 => Color {
            r: byte(&hex[0..2])?,
            g: byte(&hex[2..4])?,
            b: byte(&hex[4..6])?,
            a: 255,
        },
        3 => {
            let dup = |c: &str| byte(&format!("{c}{c}"));
            Color {
                r: dup(&hex[0..1])?,
                g: dup(&hex[1..2])?,
                b: dup(&hex[2..3])?,
                a: 255,
            }
        }
        _ => return None,
    };
    Some(Value::ColorValue(color))
}

/// Whether a rule applies at this viewport width. `None` (no media block)
/// always applies.
///
/// Understands media types and width features, which is what responsive layout
/// actually turns on. A feature we don't understand makes the block *not*
/// match, so an unsupported condition leaves the page at its base styling
/// rather than applying rules meant for some other context.
pub fn media_matches(condition: Option<&str>, viewport_width: f32) -> bool {
    let Some(condition) = condition else { return true };
    // Commas are "or": any branch matching is enough.
    condition.split(',').any(|branch| {
        let branch = branch.trim().to_lowercase();
        !branch.is_empty() && branch.split(" and ").all(|term| term_matches(term.trim(), viewport_width))
    })
}

fn term_matches(term: &str, viewport_width: f32) -> bool {
    match term {
        "screen" | "all" => return true,
        "print" | "speech" | "only print" => return false,
        _ => {}
    }
    if let Some(rest) = term.strip_prefix("only ") {
        return term_matches(rest.trim(), viewport_width);
    }
    let Some(inner) = term.strip_prefix('(').and_then(|t| t.strip_suffix(')')) else {
        return false; // an unknown bare term
    };
    let Some((feature, value)) = inner.split_once(':') else {
        return false; // a bare feature test like `(hover)`
    };
    let Some(px) = parse_px(value.trim()) else { return false };
    match feature.trim() {
        "min-width" => viewport_width >= px,
        "max-width" => viewport_width <= px,
        _ => false,
    }
}

/// Media queries are stated in px, em or rem; anything else we cannot judge.
fn parse_px(value: &str) -> Option<f32> {
    for (suffix, scale) in [("px", 1.0), ("rem", 16.0), ("em", 16.0)] {
        if let Some(number) = value.strip_suffix(suffix) {
            return number.trim().parse::<f32>().ok().map(|n| n * scale);
        }
    }
    value.parse().ok()
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
            if self.starts_with("@media") {
                rules.extend(self.parse_media_block());
            } else if self.starts_with("@") {
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
            Some(Rule {
                selectors,
                declarations,
                media: None,
            })
        }
    }

    /// Returns `None` (and skips the whole rule) if any selector isn't a bare
    /// simple selector — descendant/child/pseudo/attribute selectors are dropped.
    fn parse_selectors(&mut self) -> Option<Vec<Selector>> {
        let mut selectors = Vec::new();
        loop {
            let mut parts = vec![SelectorPart {
                simple: self.parse_simple_selector(),
                combinator: Combinator::Descendant, // ignored on the first part
            }];
            // Keep taking compounds until the rule body or the next selector.
            loop {
                let start = self.pos;
                self.consume_whitespace();
                let spaced = self.pos > start;
                let combinator = match self.next_char_or('\0') {
                    c @ ('>' | '+' | '~') => {
                        self.consume_char();
                        self.consume_whitespace();
                        match c {
                            '>' => Combinator::Child,
                            '+' => Combinator::NextSibling,
                            _ => Combinator::LaterSibling,
                        }
                    }
                    ',' | '{' | '\0' => break,
                    // A space then another compound is a descendant selector.
                    // Anything else (`:hover`, `[attr]`) we do not support, and
                    // must drop rather than silently treat as a match.
                    _ if spaced => Combinator::Descendant,
                    _ => {
                        self.skip_block();
                        return None;
                    }
                };
                parts.push(SelectorPart { simple: self.parse_simple_selector(), combinator });
            }
            if parts.iter().any(|part| part.simple.is_empty()) {
                self.skip_block(); // an empty compound means we mis-read something
                return None;
            }
            selectors.push(Selector { parts });
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
        let mut selector = SimpleSelector {
            tag_name: None,
            id: None,
            class: Vec::new(),
            attrs: Vec::new(),
            pseudos: Vec::new(),
        };
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
                '[' => match self.parse_attr_test() {
                    Some(test) => selector.attrs.push(test),
                    None => break, // malformed: let the caller drop the rule
                },
                ':' => match self.parse_pseudo(&mut selector) {
                    true => continue,
                    // An unrecognised pseudo-class leaves the ':' unconsumed, so
                    // the caller drops the rule rather than over-matching.
                    false => break,
                },
                c if is_ident(c) => {
                    selector.tag_name = Some(self.parse_identifier().to_ascii_lowercase());
                }
                _ => break,
            }
        }
        selector
    }

    /// One `:pseudo` or `:pseudo(args)`. Returns false, having consumed nothing,
    /// for anything we cannot honour exactly.
    fn parse_pseudo(&mut self, selector: &mut SimpleSelector) -> bool {
        let start = self.pos;
        self.consume_char(); // ':'
        // `::before` and friends are elements, not classes, and we generate no
        // content — the rule has to go.
        if self.next_char_or('\0') == ':' {
            self.pos = start;
            return false;
        }
        let name = self.parse_identifier().to_ascii_lowercase();
        let args = match self.next_char_or('\0') {
            '(' => {
                let Some(end) = self.input[self.pos..].find(')') else {
                    self.pos = start;
                    return false;
                };
                let args = self.input[self.pos + 1..self.pos + end].trim().to_string();
                self.pos += end + 1;
                Some(args)
            }
            _ => None,
        };

        let pseudo = match (name.as_str(), args.as_deref()) {
            // Sheets define their custom properties on :root, and dropping the
            // rule for the pseudo-class would lose all of them.
            ("root", None) => {
                selector.tag_name = Some("html".to_string());
                return true;
            }
            ("hover", None) => Pseudo::Hover,
            ("first-child", None) => Pseudo::NthChild(0, 1),
            ("last-child", None) => Pseudo::NthLastChild(0, 1),
            ("only-child", None) => Pseudo::OnlyChild,
            ("first-of-type", None) => Pseudo::NthOfType(0, 1),
            ("last-of-type", None) => Pseudo::NthLastOfType(0, 1),
            ("only-of-type", None) => Pseudo::OnlyOfType,
            ("nth-child", Some(a)) => match parse_nth(a) {
                Some((a, b)) => Pseudo::NthChild(a, b),
                None => Pseudo::Never,
            },
            ("nth-last-child", Some(a)) => match parse_nth(a) {
                Some((a, b)) => Pseudo::NthLastChild(a, b),
                None => Pseudo::Never,
            },
            ("nth-of-type", Some(a)) => match parse_nth(a) {
                Some((a, b)) => Pseudo::NthOfType(a, b),
                None => Pseudo::Never,
            },
            ("nth-last-of-type", Some(a)) => match parse_nth(a) {
                Some((a, b)) => Pseudo::NthLastOfType(a, b),
                None => Pseudo::Never,
            },
            ("not", Some(inner)) => {
                let mut sub = Parser { pos: 0, input: inner.to_string() };
                let inner = sub.parse_simple_selector();
                // `:not(a, b)` and `:not(div p)` need more than one compound.
                if inner.is_empty() || sub.pos < sub.input.len() {
                    self.pos = start;
                    return false;
                }
                Pseudo::Not(Box::new(inner))
            }
            ("checked", None) => Pseudo::AttrPresent("checked"),
            ("disabled", None) => Pseudo::AttrPresent("disabled"),
            ("enabled", None) => Pseudo::AttrAbsent("disabled"),
            ("required", None) => Pseudo::AttrPresent("required"),
            ("link", None) => Pseudo::AttrPresent("href"),
            // States nothing here tracks. Matching nothing keeps the base rule
            // in force, which is what an unstyled-but-visited link should look
            // like; dropping the rule would lose the base declaration too.
            ("visited" | "active" | "focus" | "focus-within" | "focus-visible" | "target", None) => {
                Pseudo::Never
            }
            _ => {
                self.pos = start;
                return false;
            }
        };
        selector.pseudos.push(pseudo);
        true
    }

    /// `[name]`, `[name=value]`, `[name~="value"]` and friends.
    fn parse_attr_test(&mut self) -> Option<AttrTest> {
        self.consume_char(); // '['
        self.consume_whitespace();
        let name = self.parse_identifier().to_ascii_lowercase();
        self.consume_whitespace();
        if name.is_empty() {
            return None;
        }
        if self.starts_with("]") {
            self.consume_char();
            return Some(AttrTest { name, op: AttrOp::Exists, value: String::new() });
        }
        let op = match self.next_char_or('\0') {
            '=' => AttrOp::Equals,
            '~' => AttrOp::Includes,
            '^' => AttrOp::Prefix,
            '$' => AttrOp::Suffix,
            '*' => AttrOp::Contains,
            _ => return None,
        };
        self.consume_char();
        if op != AttrOp::Equals {
            if self.next_char_or('\0') != '=' {
                return None;
            }
            self.consume_char();
        }
        self.consume_whitespace();
        // The value may be quoted, and either quote is allowed.
        let value = match self.next_char_or('\0') {
            quote @ ('"' | '\'') => {
                self.consume_char();
                let value = self.consume_while(|c| c != quote);
                self.consume_char(); // closing quote
                value
            }
            _ => self.consume_while(|c| c != ']' && !c.is_whitespace()),
        };
        self.consume_whitespace();
        // A case-insensitivity flag (`i`) is accepted but not honoured.
        self.consume_while(|c| c != ']');
        if !self.starts_with("]") {
            return None;
        }
        self.consume_char();
        Some(AttrTest { name, op, value })
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
                let raw = raw.trim();
                // A custom property is whatever text it was given, and a value
                // that mentions one cannot be understood until styling resolves
                // it against the element's inherited variables.
                let value = if name.starts_with("--")
                    || raw.contains("var(")
                    || RAW_VALUE_PROPERTIES.contains(&name.as_str())
                {
                    Some(Value::Raw(raw.to_string()))
                } else {
                    classify_value(raw)
                };
                if let Some(value) = value {
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

    /// Parse `@media <condition> { ... }`, tagging the rules inside with the
    /// condition rather than dropping them: real sites keep most of their CSS
    /// in media blocks, so skipping them loses nearly all styling.
    fn parse_media_block(&mut self) -> Vec<Rule> {
        self.pos += "@media".len();
        let start = self.pos;
        self.consume_while(|c| c != '{' && c != ';');
        let condition = self.input[start..self.pos].trim().to_string();
        if !self.starts_with("{") {
            if self.starts_with(";") {
                self.consume_char();
            }
            return Vec::new();
        }
        self.consume_char(); // the opening brace
        let mut rules = Vec::new();
        loop {
            self.consume_whitespace();
            if self.eof() || self.starts_with("}") {
                if !self.eof() {
                    self.consume_char();
                }
                break;
            }
            // ponytail: a nested at-rule inside @media is skipped, not merged.
            if self.starts_with("@") {
                self.skip_at_rule();
            } else if let Some(mut rule) = self.parse_rule() {
                rule.media = Some(condition.clone());
                rules.push(rule);
            }
        }
        rules
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
            Value::ColorValue(Color {
                r: 0xff,
                g: 0x88,
                b: 0x00,
                a: 255
            })
        );
    }

    #[test]
    fn parses_relative_units_and_numbers() {
        let s = parse(
            ".a { width: 50%; padding: 1.5em; margin: 2rem; flex-grow: 2; line-height: 1.4; }"
                .to_string(),
        );
        let d = &s.rules[0].declarations;
        assert_eq!(d[0].value, Value::Length(50.0, Unit::Percent));
        assert_eq!(d[1].value, Value::Length(1.5, Unit::Em));
        assert_eq!(d[2].value, Value::Length(2.0, Unit::Rem));
        assert_eq!(d[3].value, Value::Number(2.0));
        assert_eq!(d[4].value, Value::Number(1.4));
    }

    #[test]
    fn resolves_relative_units() {
        let ctx = LengthContext {
            percent_base: 800.0,
            font_size: 20.0,
            root_font_size: 16.0,
        };
        assert_eq!(Value::Length(50.0, Unit::Percent).resolve(ctx), 400.0);
        assert_eq!(Value::Length(1.5, Unit::Em).resolve(ctx), 30.0);
        assert_eq!(Value::Length(2.0, Unit::Rem).resolve(ctx), 32.0);
    }

    #[test]
    fn media_blocks_apply_at_matching_widths() {
        let sheet = parse(
            "body { color: #000000; }              @media screen and (min-width: 700px) { .wide { color: #ff0000; } }              @media print { .paper { color: #00ff00; } }"
                .to_string(),
        );
        assert_eq!(sheet.rules.len(), 3, "media rules are kept, not dropped");

        let applies = |width: f32| -> Vec<Option<String>> {
            sheet
                .rules
                .iter()
                .filter(|r| media_matches(r.media.as_deref(), width))
                .map(|r| r.media.clone())
                .collect()
        };
        // Wide: the base rule and the min-width block, never the print one.
        assert_eq!(applies(800.0).len(), 2);
        // Narrow: only the base rule.
        assert_eq!(applies(500.0), vec![None]);
    }

    #[test]
    fn nth_arguments_parse_in_every_spelling() {
        assert_eq!(parse_nth("odd"), Some((2, 1)));
        assert_eq!(parse_nth("EVEN"), Some((2, 0)));
        assert_eq!(parse_nth("3"), Some((0, 3)));
        assert_eq!(parse_nth("2n"), Some((2, 0)));
        assert_eq!(parse_nth("2n + 1"), Some((2, 1)));
        assert_eq!(parse_nth("-n+3"), Some((-1, 3)));
        assert_eq!(parse_nth("n"), Some((1, 0)));
        assert_eq!(parse_nth("junk"), None);
    }

    #[test]
    fn pseudo_classes_parse_or_take_the_rule_with_them() {
        let s = parse(
            "li:nth-child(2n+1) { color: red; } \
             p:not(.skip) { color: red; } \
             input:checked { color: red; } \
             a:visited { color: red; } \
             p::before { content: 'x'; } \
             div:has(> p) { color: red; }"
                .to_string(),
        );
        // The four we can honour exactly; `::before` and `:has()` are dropped
        // rather than applied to everything.
        assert_eq!(s.rules.len(), 4);
        let subject = |i: usize| s.rules[i].selectors[0].subject().unwrap();
        assert_eq!(subject(0).pseudos, vec![Pseudo::NthChild(2, 1)]);
        assert_eq!(subject(2).pseudos, vec![Pseudo::AttrPresent("checked")]);
        assert_eq!(subject(3).pseudos, vec![Pseudo::Never]);
        match &subject(1).pseudos[..] {
            [Pseudo::Not(inner)] => assert_eq!(inner.class, vec!["skip".to_string()]),
            other => panic!("expected :not(.skip), got {other:?}"),
        }
    }

    #[test]
    fn media_conditions_are_evaluated() {
        assert!(media_matches(None, 400.0)); // no block: always on
        assert!(media_matches(Some("screen"), 400.0));
        assert!(!media_matches(Some("print"), 400.0));
        assert!(media_matches(Some("(max-width: 600px)"), 400.0));
        assert!(!media_matches(Some("(max-width: 600px)"), 900.0));
        // `and` requires both; a comma is `or`.
        assert!(!media_matches(Some("screen and (min-width: 900px)"), 400.0));
        assert!(media_matches(Some("print, screen"), 400.0));
        // em/rem conditions resolve against the initial font size.
        assert!(media_matches(Some("(min-width: 20em)"), 400.0));
        // A feature we cannot judge must not switch styles on.
        assert!(!media_matches(Some("(prefers-color-scheme: dark)"), 400.0));
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
        // Kept: the media rule, `a:hover`, `div > p`, and `.ok`.
        assert_eq!(s.rules.len(), 4);
        let ok = s
            .rules
            .iter()
            .find(|r| r.selectors.iter().any(|sel| sel.specificity() == (0, 1, 0)))
            .expect("the .ok rule");
        // color + width(%) + padding all understood now.
        assert_eq!(ok.declarations.len(), 3);
    }

    #[test]
    fn parses_named_colors_and_color_functions() {
        let color = |text: &str| match classify_value(text) {
            Some(Value::ColorValue(c)) => Some((c.r, c.g, c.b, c.a)),
            _ => None,
        };
        assert_eq!(color("red"), Some((255, 0, 0, 255)));
        assert_eq!(color("WhiteSmoke"), Some((245, 245, 245, 255)));
        // `transparent` is a colour with zero alpha, not a missing value.
        assert_eq!(color("transparent"), Some((0, 0, 0, 0)));

        assert_eq!(color("rgb(18, 52, 86)"), Some((18, 52, 86, 255)));
        assert_eq!(color("rgba(0,0,0,0.5)"), Some((0, 0, 0, 128)));
        // The modern space-separated form, with a percentage alpha.
        assert_eq!(color("rgb(255 0 0 / 50%)"), Some((255, 0, 0, 128)));
        assert_eq!(color("rgb(100%, 0%, 0%)"), Some((255, 0, 0, 255)));

        assert_eq!(color("hsl(0, 100%, 50%)"), Some((255, 0, 0, 255)));
        assert_eq!(color("hsl(120, 100%, 50%)"), Some((0, 255, 0, 255)));
        assert_eq!(color("hsl(0, 0%, 100%)"), Some((255, 255, 255, 255)));
        assert_eq!(color("hsla(240, 100%, 50%, 1)"), Some((0, 0, 255, 255)));

        // Nonsense stays unsupported rather than becoming a wrong colour.
        assert_eq!(color("rgb(1, 2)"), None);
        assert_eq!(color("notacolor"), None);
        // A keyword that is not a colour still parses as a keyword.
        assert_eq!(classify_value("block"), Some(Value::Keyword("block".into())));
    }

    #[test]
    fn parses_descendant_and_child_chains() {
        let s = parse("nav ul > li a { color: #ff0000; } .x{color:#000000;}".to_string());
        assert_eq!(s.rules.len(), 2);
        let chain = &s.rules[0].selectors[0];
        assert_eq!(chain.parts.len(), 4);
        assert_eq!(chain.parts[0].simple.tag_name.as_deref(), Some("nav"));
        assert_eq!(chain.parts[2].combinator, Combinator::Child); // ul > li
        assert_eq!(chain.parts[3].combinator, Combinator::Descendant); // li a
        assert_eq!(chain.parts[3].simple.tag_name.as_deref(), Some("a"));
        // Four tag compounds, so it outranks any single-tag rule.
        assert_eq!(chain.specificity(), (0, 0, 4));
    }
}

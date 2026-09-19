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
    /// Every `@font-face` the sheet declared, in source order.
    pub font_faces: Vec<FontFace>,
    /// Every `@keyframes` rule, by name. A later definition of the same name
    /// replaces an earlier one, as the cascade says it should.
    pub keyframes: Vec<Keyframes>,
}

/// One `@keyframes` rule: a name an `animation` can call for, and the stops it
/// passes through.
#[derive(Debug, Clone)]
pub struct Keyframes {
    pub name: String,
    /// Each stop's position (0.0 to 1.0) and what it sets, sorted by position.
    /// `from` and `to` are 0% and 100%.
    pub stops: Vec<(f32, Vec<Declaration>)>,
}

/// One `@font-face`: a name the page's `font-family` can ask for, and where to
/// fetch the file that answers to it.
#[derive(Debug)]
pub struct FontFace {
    pub family: String,
    /// Every `url(...)` in `src`, in the order the page listed them — the
    /// first one that downloads and parses is the one used.
    pub srcs: Vec<String>,
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
    /// `::before` or `::after`, when the selector is aiming at generated
    /// content rather than at the element itself.
    pub pseudo_element: Option<PseudoElement>,
    pub tag_name: Option<String>,
    pub id: Option<String>,
    pub class: Vec<String>,
    pub attrs: Vec<AttrTest>,
    /// `:hover`, `:nth-child(2n)`, `:not(.x)` … all of which must hold.
    pub pseudos: Vec<Pseudo>,
}

/// A box a rule asks the engine to make, which no element in the document
/// stands for.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PseudoElement {
    Before,
    After,
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

#[derive(Clone, Debug)]
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
    /// `calc(...)`, kept as an expression tree rather than reduced to a
    /// single `Length` — a mixed-unit expression (`calc(100% - 20px)`) has no
    /// single `(magnitude, unit)` it could be, since resolving the `%` needs
    /// a containing block that isn't known until layout. Each leaf resolves
    /// through the same [`Value::resolve`] every other length already goes
    /// through, so the units only ever mix at the very end, in px.
    Calc(Box<CalcExpr>),
}

#[derive(Debug, Clone, PartialEq)]
pub enum CalcOp {
    Add,
    Sub,
    Mul,
    Div,
}

#[derive(Debug, Clone, PartialEq)]
pub enum CalcExpr {
    Value(Box<Value>),
    Op(Box<CalcExpr>, CalcOp, Box<CalcExpr>),
}

impl CalcExpr {
    /// Whether any term is a percentage, which decides whether the whole
    /// expression can be resolved against a containing block that has not
    /// sized itself yet.
    pub fn mentions_percentage(&self) -> bool {
        match self {
            CalcExpr::Value(value) => matches!(**value, Value::Length(_, Unit::Percent)),
            CalcExpr::Op(left, _, right) => {
                left.mentions_percentage() || right.mentions_percentage()
            }
        }
    }
}

fn resolve_calc(expr: &CalcExpr, ctx: LengthContext) -> f32 {
    match expr {
        CalcExpr::Value(v) => v.resolve(ctx),
        CalcExpr::Op(l, op, r) => {
            let (l, r) = (resolve_calc(l, ctx), resolve_calc(r, ctx));
            match op {
                CalcOp::Add => l + r,
                CalcOp::Sub => l - r,
                CalcOp::Mul => l * r,
                CalcOp::Div if r != 0.0 => l / r,
                CalcOp::Div => 0.0,
            }
        }
    }
}

/// A `calc(...)` expression parser: `<sum> = <product> ([+|-] <product>)*`,
/// `<product> = <value> ([*|/] <value>)*`, `<value>` a number/length/
/// percentage, a parenthesized `<sum>`, or a nested `calc(<sum>)`.
struct CalcParser {
    chars: Vec<char>,
    pos: usize,
}

impl CalcParser {
    fn new(s: &str) -> CalcParser {
        CalcParser {
            chars: s.chars().collect(),
            pos: 0,
        }
    }

    fn skip_ws(&mut self) {
        while matches!(self.chars.get(self.pos), Some(c) if c.is_whitespace()) {
            self.pos += 1;
        }
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn parse_sum(&mut self) -> Option<CalcExpr> {
        let mut left = self.parse_product()?;
        loop {
            self.skip_ws();
            let op = match self.peek() {
                Some('+') => CalcOp::Add,
                Some('-') => CalcOp::Sub,
                _ => break,
            };
            self.pos += 1;
            self.skip_ws();
            let right = self.parse_product()?;
            left = CalcExpr::Op(Box::new(left), op, Box::new(right));
        }
        Some(left)
    }

    fn parse_product(&mut self) -> Option<CalcExpr> {
        let mut left = self.parse_value()?;
        loop {
            self.skip_ws();
            let op = match self.peek() {
                Some('*') => CalcOp::Mul,
                Some('/') => CalcOp::Div,
                _ => break,
            };
            self.pos += 1;
            self.skip_ws();
            let right = self.parse_value()?;
            left = CalcExpr::Op(Box::new(left), op, Box::new(right));
        }
        Some(left)
    }

    fn parse_value(&mut self) -> Option<CalcExpr> {
        self.skip_ws();
        if self.peek() == Some('(') {
            self.pos += 1;
            let inner = self.parse_sum()?;
            self.skip_ws();
            if self.peek() == Some(')') {
                self.pos += 1;
            }
            return Some(inner);
        }
        if self.chars[self.pos..].starts_with(&['c', 'a', 'l', 'c', '(']) {
            self.pos += 5;
            let inner = self.parse_sum()?;
            self.skip_ws();
            if self.peek() == Some(')') {
                self.pos += 1;
            }
            return Some(inner);
        }
        // A leading `-` here is a sign, not the binary operator (`parse_sum`
        // already consumed that one and the whitespace after it).
        let negate = self.peek() == Some('-');
        if negate {
            self.pos += 1;
        }
        let start = self.pos;
        while let Some(c) = self.peek() {
            if matches!(c, '+' | '-' | '*' | '/' | '(' | ')') || c.is_whitespace() {
                break;
            }
            self.pos += 1;
        }
        let token: String = self.chars[start..self.pos].iter().collect();
        if token.is_empty() {
            return None;
        }
        let value = match classify_value(&token)? {
            Value::Length(n, u) if negate => Value::Length(-n, u),
            Value::Number(n) if negate => Value::Number(-n),
            other => other,
        };
        Some(CalcExpr::Value(Box::new(value)))
    }
}

/// `calc(...)`'s inner text (already stripped of the outer `calc(`/`)`) into
/// an expression tree, or `None` if it doesn't parse as one.
fn parse_calc(inner: &str) -> Option<CalcExpr> {
    let mut parser = CalcParser::new(inner);
    let expr = parser.parse_sum()?;
    parser.skip_ws();
    (parser.pos == parser.chars.len()).then_some(expr)
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
        self.parts
            .iter()
            .fold((0, 0, 0), |(ids, classes, tags), part| {
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
            Value::Calc(ref expr) => resolve_calc(expr, ctx),
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

/// A declaration's text, whichever shape the parser gave it.
///
/// `@font-face`'s descriptors are not real properties, so they arrive
/// classified as whatever they happened to look like — a bare family name as a
/// keyword, a quoted one as raw text.
fn value_text(value: &Value) -> String {
    match value {
        Value::Raw(text) => text.clone(),
        Value::Keyword(word) => word.clone(),
        other => format!("{other:?}"),
    }
}

/// Strip one layer of matching quotes.
fn unquote(text: &str) -> &str {
    let text = text.trim();
    for quote in ['"', '\''] {
        if let Some(inner) = text.strip_prefix(quote).and_then(|t| t.strip_suffix(quote)) {
            return inner;
        }
    }
    text
}

/// Every `url(...)` in a value, in source order.
fn url_tokens(text: &str) -> Vec<String> {
    let mut urls = Vec::new();
    let mut rest = text;
    while let Some(open) = rest.find("url(") {
        rest = &rest[open + 4..];
        let Some(close) = rest.find(')') else { break };
        let url = unquote(&rest[..close]).trim().to_string();
        if !url.is_empty() {
            urls.push(url);
        }
        rest = &rest[close + 1..];
    }
    urls
}

/// Split a `font-family` list into the names it asks for, best first.
///
/// Generic families (`sans-serif`, `monospace`, ...) are kept as written: the
/// engine has no mapping from them to a file, so they simply match nothing and
/// the fallback chain answers — which is what they mean anyway.
pub fn family_list(text: &str) -> Vec<String> {
    text.split(',')
        .map(|name| unquote(name).trim().to_ascii_lowercase())
        .filter(|name| !name.is_empty())
        .collect()
}

/// Parse the contents of a `style` attribute: a declaration list with no
/// selector or braces around it.
///
/// Wrapped rather than given a parser of its own, because a `style` attribute
/// is exactly a rule's body — and the two have to agree about shorthands,
/// `var()`, comments and malformed input, or the same text would mean something
/// subtly different inline than it does in a stylesheet.
pub fn parse_style_attribute(text: &str) -> Vec<Declaration> {
    if text.trim().is_empty() {
        return Vec::new();
    }
    let mut parser = Parser {
        pos: 0,
        input: format!("{{{text}}}"),
    };
    parser.parse_declarations()
}

pub fn parse(source: String) -> Stylesheet {
    let mut parser = Parser {
        pos: 0,
        input: source,
    };
    let (rules, font_faces, keyframes) = parser.parse_rules();
    Stylesheet {
        rules,
        font_faces,
        keyframes,
    }
}

/// Remove `/* ... */` from a value. Comments are whitespace between tokens, and
/// a value read as one span of text would otherwise carry them into the parse.
fn strip_comments(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("/*") {
        out.push_str(&rest[..start]);
        match rest[start..].find("*/") {
            Some(end) => rest = &rest[start + end + 2..],
            None => return out, // unterminated: the rest is comment
        }
    }
    out.push_str(rest);
    out
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
    // A comma-separated list, the colour of each entry possibly a function.
    "text-shadow",
    "background-image",
    // Both can be one or two space-separated tokens (`center bottom`, `50% 50%`).
    "background-position",
    "background-size",
    // `overflow: hidden auto` sets the two axes at once.
    "overflow",
    // `url(pointer.png), pointer` — a list whose last entry is the keyword.
    "cursor",
    // Function lists read at paint time.
    "filter",
    "backdrop-filter",
    // `translate(-50%, -50%)`, read at paint time.
    "transform",
    // Up to two tokens (`left top`, `50% 50%`), read with the transform.
    "transform-origin",
    // `spin 1s linear infinite` and `color 300ms, opacity 1s` — lists, read
    // when styling.
    "animation",
    "animation-name",
    "animation-duration",
    "animation-timing-function",
    "animation-delay",
    "animation-iteration-count",
    "animation-direction",
    "animation-fill-mode",
    "animation-play-state",
    "transition",
    "transition-property",
    "transition-duration",
    "transition-timing-function",
    // A comma-separated list of names, often quoted: `"Helvetica Neue", Arial,
    // sans-serif`. Classifying it would keep only the first token.
    "font-family",
    // `@font-face`'s file list: `url(a.woff2) format("woff2"), url(a.ttf)`.
    // Nothing classifies as a value, so it would be dropped outright.
    "src",
    // Generated content: a component list, kept exactly as written so an
    // escape, a leading space or a `counter()` survives to be read at use.
    "content",
    "counter-increment",
    "counter-reset",
    "counter-set",
    "quotes",
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

/// Parse one colour token — hex, `rgb()`/`rgba()`/`hsl()`/`hsla()`, or a named
/// colour — outside the context of a whole declaration. Used by gradient
/// stops, which sit inside a `background-image` spec `classify_value` never
/// gets to run on directly.
pub(crate) fn parse_color_str(token: &str) -> Option<Color> {
    let value = if let Some(hex) = token.strip_prefix('#') {
        parse_hex_color(hex)
    } else if token.contains('(') {
        parse_color_function(token)
    } else {
        named_color(token)
    }?;
    match value {
        Value::ColorValue(c) => Some(c),
        _ => None,
    }
}

fn named_color(name: &str) -> Option<Value> {
    let name = name.to_ascii_lowercase();
    NAMED_COLORS
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, rgba)| {
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
    let parts: Vec<&str> = body
        .split([',', '/', ' '])
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .collect();
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
            let pct = |p: &str| {
                p.trim_end_matches('%')
                    .parse::<f32>()
                    .ok()
                    .map(|v| v / 100.0)
            };
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
    if let Some(inner) = s
        .strip_prefix("calc(")
        .and_then(|rest| rest.strip_suffix(')'))
    {
        return parse_calc(inner).map(|expr| Value::Calc(Box::new(expr)));
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

/// The `<width> <style> <color>` grammar `border` and `outline` share, picked
/// out of `tokens` by which [`Value`] variant each one classifies as rather
/// than by position — CSS allows any order.
fn border_like_longhands(
    tokens: &[&str],
    width_name: &str,
    color_name: &str,
    style_name: &str,
) -> Option<Vec<Declaration>> {
    let mut width = None;
    let mut color = None;
    let mut style = None;
    for token in tokens {
        match classify_value(token) {
            Some(v @ (Value::Length(..) | Value::Number(_) | Value::Calc(..)))
                if width.is_none() =>
            {
                width = Some(v)
            }
            Some(v @ Value::ColorValue(_)) if color.is_none() => color = Some(v),
            Some(v @ Value::Keyword(_)) if style.is_none() => style = Some(v),
            _ => {}
        }
    }
    let mut out = Vec::new();
    if let Some(v) = width {
        out.push(Declaration {
            name: width_name.to_string(),
            value: v,
        });
    }
    if let Some(v) = color {
        out.push(Declaration {
            name: color_name.to_string(),
            value: v,
        });
    }
    if let Some(v) = style {
        out.push(Declaration {
            name: style_name.to_string(),
            value: v,
        });
    }
    (!out.is_empty()).then_some(out)
}

/// Turn a property and its value text into the declarations the rest of the
/// engine reads: raw text for the list-shaped properties, a classified value
/// where one parses, or a shorthand's longhands.
///
/// The declaration parser and `var()` substitution both need this, and they
/// need to agree — a value that arrives through a variable is a declaration
/// like any other, shorthand expansion included.
pub(crate) fn declarations_for(name: &str, raw: &str) -> Vec<Declaration> {
    let raw = raw.trim();
    if RAW_VALUE_PROPERTIES.contains(&name) {
        return vec![Declaration {
            name: name.to_string(),
            value: Value::Raw(raw.to_string()),
        }];
    }
    if let Some(value) = classify_value(raw) {
        return vec![Declaration {
            name: name.to_string(),
            value,
        }];
    }
    expand_shorthand(name, raw).unwrap_or_default()
}

/// Split a multi-token shorthand into the longhands the rest of the engine
/// already reads. Only reached once [`classify_value`] has failed on the whole
/// string, so the common single-token case (`padding: 10px`, `flex: 1`) is
/// untouched and keeps working exactly as it did.
fn expand_shorthand(name: &str, raw: &str) -> Option<Vec<Declaration>> {
    // `font`, like `background`, cannot be split on whitespace: the family runs
    // to the end of the value and may carry commas and quoted names.
    if name == "font" {
        return expand_font_shorthand(raw);
    }
    // `background` gets its own tokenizer: a naive whitespace split (used by
    // every shorthand below) tears a gradient or a space-separated colour
    // function apart (`linear-gradient(to right, red, blue)`,
    // `rgb(0 0 0 / 50%)`), and a background can legitimately be *one* token
    // (`background: radial-gradient(red, blue)`, no space after the commas) —
    // which the `tokens.len() < 2` guard below would otherwise drop entirely.
    if name == "background" {
        return expand_background_shorthand(raw);
    }
    let tokens: Vec<&str> = raw.split_whitespace().collect();
    if tokens.len() < 2 {
        return None;
    }
    match name {
        "padding" | "margin" => {
            let values: Vec<Value> = tokens
                .iter()
                .map(|t| classify_value(t))
                .collect::<Option<_>>()?;
            let (top, right, bottom, left) = match values.as_slice() {
                [all] => (all.clone(), all.clone(), all.clone(), all.clone()),
                [v, h] => (v.clone(), h.clone(), v.clone(), h.clone()),
                [t, h, b] => (t.clone(), h.clone(), b.clone(), h.clone()),
                [t, r, b, l] => (t.clone(), r.clone(), b.clone(), l.clone()),
                _ => return None,
            };
            Some(vec![
                Declaration {
                    name: format!("{name}-top"),
                    value: top,
                },
                Declaration {
                    name: format!("{name}-right"),
                    value: right,
                },
                Declaration {
                    name: format!("{name}-bottom"),
                    value: bottom,
                },
                Declaration {
                    name: format!("{name}-left"),
                    value: left,
                },
            ])
        }
        // Uniform on every side, which is the overwhelming common case
        // (`border: 1px solid #ccc`) — the per-side longhands already fall
        // back to `border-width`, so setting it once covers all four.
        "border" => border_like_longhands(&tokens, "border-width", "border-color", "border-style"),
        // `outline` has the same three-part grammar as `border` and no sides
        // to distribute over (it is drawn outside the box, not part of it).
        "outline" => {
            border_like_longhands(&tokens, "outline-width", "outline-color", "outline-style")
        }
        "flex" => {
            let values: Vec<Value> = tokens
                .iter()
                .map(|t| classify_value(t))
                .collect::<Option<_>>()?;
            let names = ["flex-grow", "flex-shrink", "flex-basis"];
            Some(
                values
                    .into_iter()
                    .zip(names)
                    .map(|(value, name)| Declaration {
                        name: name.to_string(),
                        value,
                    })
                    .collect(),
            )
        }
        _ => None,
    }
}

/// ponytail: the `<position> / <size>` slash syntax inside the shorthand
/// (`center / cover`) is not split out — write `background-size` as its own
/// declaration instead. Every other token order this grammar allows is
/// understood.
/// `font: [ <style> || <variant> || <weight> || <stretch> ]? <size>[/<line-height>]? <family>`
///
/// The size is the pivot. Everything before it is the optional leading
/// keywords in any order; everything after it is the family list, which runs
/// to the end of the value. The system keywords (`caption`, `menu`, ...) are
/// one token and so never reach here — they fall out as an unexpandable
/// value, which leaves the element on the default face, which is what they
/// mean anyway.
///
/// Per spec the shorthand resets every sub-property it does not mention, so
/// `body { font: 1rem/1.5 Arial }` has to clear an inherited bold rather than
/// quietly keeping it.
fn expand_font_shorthand(raw: &str) -> Option<Vec<Declaration>> {
    let mut leading: Vec<Declaration> = Vec::new();
    let mut rest = raw.trim();
    loop {
        // The family is the only part that may contain whitespace, and it
        // always follows the size, so a leading keyword is always one token.
        let (token, tail) = rest.split_once(char::is_whitespace)?;
        let lower = token.to_ascii_lowercase();
        let name = match lower.as_str() {
            "italic" | "oblique" => "font-style",
            "small-caps" => "font-variant",
            "bold" | "bolder" | "lighter" => "font-weight",
            "condensed" | "expanded" | "semi-condensed" | "semi-expanded" | "extra-condensed"
            | "extra-expanded" | "ultra-condensed" | "ultra-expanded" => "font-stretch",
            // `normal` stands for whichever of the four has not been given.
            // They all reset to it below, so there is nothing to record.
            "normal" => {
                rest = tail.trim_start();
                continue;
            }
            // A bare number is a weight: a font size always carries a unit or
            // is one of the named sizes.
            _ if lower.parse::<f32>().is_ok() => "font-weight",
            _ => break,
        };
        leading.push(Declaration {
            name: name.to_string(),
            value: classify_value(&lower)?,
        });
        rest = tail.trim_start();
    }
    let (size_token, family) = rest.split_once(char::is_whitespace)?;
    let family = family.trim();
    if family.is_empty() {
        return None;
    }
    let (size, line_height) = match size_token.split_once('/') {
        Some((size, line_height)) => (size, Some(line_height)),
        None => (size_token, None),
    };

    // Reset the sub-properties no leading keyword gave a value to.
    let mut out = leading;
    for name in ["font-style", "font-weight"] {
        if !out.iter().any(|d| d.name == name) {
            out.push(Declaration {
                name: name.to_string(),
                value: Value::Keyword("normal".to_string()),
            });
        }
    }
    out.push(Declaration {
        name: "font-size".to_string(),
        value: font_size_value(size)?,
    });
    out.push(Declaration {
        name: "line-height".to_string(),
        value: match line_height {
            Some(text) => classify_value(text)?,
            None => Value::Keyword("normal".to_string()),
        },
    });
    out.push(Declaration {
        name: "font-family".to_string(),
        value: Value::Raw(family.to_string()),
    });
    Some(out)
}

/// A font size, which unlike other lengths may also be one of the named
/// absolute sizes. `medium` is the initial value and the scale hangs off it.
fn font_size_value(text: &str) -> Option<Value> {
    const ABSOLUTE: &[(&str, f32)] = &[
        ("xx-small", 9.0),
        ("x-small", 10.0),
        ("small", 13.0),
        ("medium", 16.0),
        ("large", 18.0),
        ("x-large", 24.0),
        ("xx-large", 32.0),
    ];
    let lower = text.trim().to_ascii_lowercase();
    if let Some((_, px)) = ABSOLUTE.iter().find(|(name, _)| *name == lower) {
        return Some(Value::Length(*px, Unit::Px));
    }
    // `smaller`/`larger` are relative to the parent's size, which is not
    // knowable here; they resolve during the style walk like `em` does.
    match classify_value(text)? {
        value @ (Value::Length(..) | Value::Calc(..)) => Some(value),
        _ => None,
    }
}

fn expand_background_shorthand(raw: &str) -> Option<Vec<Declaration>> {
    const REPEAT_KEYWORDS: [&str; 6] = [
        "repeat",
        "no-repeat",
        "repeat-x",
        "repeat-y",
        "space",
        "round",
    ];
    let tokens = split_top_level_whitespace(raw);
    let mut out = Vec::new();
    let mut position_tokens: Vec<&str> = Vec::new();
    for token in &tokens {
        if token.starts_with("url(")
            || token.starts_with("linear-gradient(")
            || token.starts_with("radial-gradient(")
        {
            out.push(Declaration {
                name: "background-image".to_string(),
                value: Value::Raw(token.to_string()),
            });
        } else if REPEAT_KEYWORDS.contains(token) {
            out.push(Declaration {
                name: "background-repeat".to_string(),
                value: Value::Keyword(token.to_string()),
            });
        } else if let Some(v @ Value::ColorValue(_)) = classify_value(token) {
            out.push(Declaration {
                name: "background-color".to_string(),
                value: v,
            });
        } else if matches!(
            token.to_ascii_lowercase().as_str(),
            "left" | "right" | "top" | "bottom" | "center"
        ) || matches!(classify_value(token), Some(Value::Length(..)))
        {
            position_tokens.push(token);
        }
    }
    if !position_tokens.is_empty() {
        out.push(Declaration {
            name: "background-position".to_string(),
            value: Value::Raw(position_tokens.join(" ")),
        });
    }
    (!out.is_empty()).then_some(out)
}

/// Like `str::split_whitespace`, but text inside a balanced `(...)` counts as
/// one token even if it has spaces of its own — `rgb(0 0 0 / 50%)`,
/// `linear-gradient(to right, red, blue)`.
pub(crate) fn split_top_level_whitespace(s: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0;
    let mut start: Option<usize> = None;
    for (i, c) in s.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            c if c.is_whitespace() && depth == 0 => {
                if let Some(st) = start.take() {
                    parts.push(&s[st..i]);
                }
                continue;
            }
            _ => {}
        }
        if start.is_none() {
            start = Some(i);
        }
    }
    if let Some(st) = start {
        parts.push(&s[st..]);
    }
    parts
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

/// Offset of the `)` closing the paren this text is already inside of.
pub(crate) fn matching_paren(text: &str) -> Option<usize> {
    scan_top_level(text, &[')']).map(|(offset, _)| offset)
}

/// Split at the first comma that is not inside parens or a string.
pub(crate) fn split_top_level_comma(text: &str) -> Option<(&str, &str)> {
    scan_top_level(text, &[',']).map(|(offset, _)| (&text[..offset], &text[offset + 1..]))
}

/// First occurrence of any `stops` character that is not nested inside parens
/// or a quoted string.
///
/// A custom property's value is an arbitrary token stream, so neither the
/// closing paren nor the fallback comma can be found by scanning for the
/// character itself: `var(--c, rgb(1, 2, 3))` has three commas and two parens
/// before the ones that matter.
fn scan_top_level(text: &str, stops: &[char]) -> Option<(usize, char)> {
    let mut depth = 0usize;
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for (offset, c) in text.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match quote {
            Some(q) => match c {
                '\\' => escaped = true,
                _ if c == q => quote = None,
                _ => {}
            },
            None => match c {
                '"' | '\'' => quote = Some(c),
                '(' => depth += 1,
                _ if depth == 0 && stops.contains(&c) => return Some((offset, c)),
                ')' => depth = depth.saturating_sub(1),
                _ => {}
            },
        }
    }
    None
}

/// A keyframe selector as a fraction: `from` is 0, `to` is 1, `35%` is 0.35.
fn keyframe_position(selector: &str) -> Option<f32> {
    match selector.to_ascii_lowercase().as_str() {
        "from" => Some(0.0),
        "to" => Some(1.0),
        other => other
            .strip_suffix('%')?
            .trim()
            .parse::<f32>()
            .ok()
            .map(|n| n / 100.0),
    }
}

/// A 2D affine transform, in CSS's own `matrix(a, b, c, d, e, f)` order:
/// `x' = a*x + c*y + e`, `y' = b*x + d*y + f`.
///
/// Rotation and skew are why this exists: translate and scale keep a rectangle
/// axis-aligned and so fold into a scale-and-offset, but nothing else does.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Mat {
    pub a: f32,
    pub b: f32,
    pub c: f32,
    pub d: f32,
    pub e: f32,
    pub f: f32,
}

impl Mat {
    pub const IDENTITY: Mat = Mat {
        a: 1.0,
        b: 0.0,
        c: 0.0,
        d: 1.0,
        e: 0.0,
        f: 0.0,
    };

    pub fn translate(dx: f32, dy: f32) -> Mat {
        Mat {
            e: dx,
            f: dy,
            ..Mat::IDENTITY
        }
    }

    pub fn scale(sx: f32, sy: f32) -> Mat {
        Mat {
            a: sx,
            d: sy,
            ..Mat::IDENTITY
        }
    }

    pub fn rotate(radians: f32) -> Mat {
        let (sin, cos) = radians.sin_cos();
        Mat {
            a: cos,
            b: sin,
            c: -sin,
            d: cos,
            ..Mat::IDENTITY
        }
    }

    pub fn skew(x_radians: f32, y_radians: f32) -> Mat {
        Mat {
            c: x_radians.tan(),
            b: y_radians.tan(),
            ..Mat::IDENTITY
        }
    }

    /// `inner` applied first, then `self` — the order a CSS function list
    /// composes in, read left to right.
    pub fn then(self, inner: Mat) -> Mat {
        Mat {
            a: self.a * inner.a + self.c * inner.b,
            b: self.b * inner.a + self.d * inner.b,
            c: self.a * inner.c + self.c * inner.d,
            d: self.b * inner.c + self.d * inner.d,
            e: self.a * inner.e + self.c * inner.f + self.e,
            f: self.b * inner.e + self.d * inner.f + self.f,
        }
    }

    pub fn apply(self, x: f32, y: f32) -> (f32, f32) {
        (
            self.a * x + self.c * y + self.e,
            self.b * x + self.d * y + self.f,
        )
    }

    pub fn is_identity(self) -> bool {
        self == Mat::IDENTITY
    }

    /// Whether this maps rectangles to rectangles *and* scales both axes
    /// alike — the shape the painter can apply without an offscreen buffer.
    pub fn is_upright(self) -> bool {
        const EPSILON: f32 = 1.0e-4;
        self.b.abs() < EPSILON
            && self.c.abs() < EPSILON
            && (self.a - self.d).abs() < EPSILON
            && self.a > 0.0
    }

    pub fn invert(self) -> Option<Mat> {
        let det = self.a * self.d - self.b * self.c;
        if det.abs() < 1.0e-9 {
            return None; // a degenerate transform paints nothing
        }
        Some(Mat {
            a: self.d / det,
            b: -self.b / det,
            c: -self.c / det,
            d: self.a / det,
            e: (self.c * self.f - self.d * self.e) / det,
            f: (self.b * self.e - self.a * self.f) / det,
        })
    }
}

/// An angle in radians. CSS allows four units and a bare `0`.
pub fn parse_angle(token: &str) -> Option<f32> {
    let token = token.trim();
    for (suffix, per_unit) in [
        ("deg", std::f32::consts::PI / 180.0),
        ("grad", std::f32::consts::PI / 200.0),
        ("turn", std::f32::consts::TAU),
        ("rad", 1.0),
    ] {
        if let Some(number) = token.strip_suffix(suffix) {
            return number.trim().parse::<f32>().ok().map(|n| n * per_unit);
        }
    }
    // A bare number is only an angle when it is zero, which CSS lets a page
    // write without a unit.
    match token.parse::<f32>() {
        Ok(0.0) => Some(0.0),
        _ => None,
    }
}

/// Parse a `transform` list into one matrix, composed in the order written.
///
/// `size` is the box's own border box, which is what a percentage in
/// `translate()` resolves against — `translate(-50%, -50%)` is how the web
/// centres things.
///
/// ponytail: the 3D functions (`translate3d`, `rotateX`, `perspective`, ...)
/// are ignored rather than flattened. Applying their 2D shadow would move
/// things to places the page did not ask for; they need a real 3D compositor,
/// which is its own piece of work.
pub fn parse_transform(spec: &str, ctx: LengthContext, size: (f32, f32)) -> Mat {
    let mut matrix = Mat::IDENTITY;
    let mut rest = spec.trim();
    while let Some(open) = rest.find('(') {
        let name = rest[..open]
            .trim()
            .trim_start_matches(',')
            .trim()
            .to_ascii_lowercase();
        let Some(close) = matching_paren(&rest[open + 1..]) else {
            break;
        };
        let args: Vec<&str> = rest[open + 1..open + 1 + close]
            .split(',')
            .map(str::trim)
            .collect();
        rest = &rest[open + 1 + close + 1..];

        // A percentage is of this box's own size, so each axis has its own base.
        let length = |token: &str, base: f32| match token.strip_suffix('%') {
            Some(pct) => pct.trim().parse::<f32>().unwrap_or(0.0) / 100.0 * base,
            None => parse_length_token(token, ctx),
        };
        let number = |token: Option<&&str>| token.and_then(|t| t.parse::<f32>().ok());
        let angle = |token: Option<&&str>| token.and_then(|t| parse_angle(t)).unwrap_or(0.0);
        let step = match name.as_str() {
            "translate" => Mat::translate(
                length(args[0], size.0),
                args.get(1).map_or(0.0, |y| length(y, size.1)),
            ),
            "translatex" => Mat::translate(length(args[0], size.0), 0.0),
            "translatey" => Mat::translate(0.0, length(args[0], size.1)),
            "scale" => {
                let x = number(args.first()).unwrap_or(1.0);
                Mat::scale(x, number(args.get(1)).unwrap_or(x))
            }
            "scalex" => Mat::scale(number(args.first()).unwrap_or(1.0), 1.0),
            "scaley" => Mat::scale(1.0, number(args.first()).unwrap_or(1.0)),
            // `rotateZ` is a rotation about the screen normal, which is the 2D
            // one; `rotateX`/`rotateY` are not and fall through to be ignored.
            "rotate" | "rotatez" => Mat::rotate(angle(args.first())),
            "skew" => Mat::skew(angle(args.first()), angle(args.get(1))),
            "skewx" => Mat::skew(angle(args.first()), 0.0),
            "skewy" => Mat::skew(0.0, angle(args.first())),
            "matrix" if args.len() == 6 => {
                let n: Vec<f32> = args.iter().map(|a| a.parse().unwrap_or(0.0)).collect();
                Mat {
                    a: n[0],
                    b: n[1],
                    c: n[2],
                    d: n[3],
                    e: n[4],
                    f: n[5],
                }
            }
            _ => continue,
        };
        matrix = matrix.then(step);
    }
    matrix
}

/// `transform-origin`, as an offset inside the box. Defaults to its centre,
/// which is what every transform is measured about unless a page says otherwise.
pub fn parse_transform_origin(
    spec: Option<&str>,
    ctx: LengthContext,
    size: (f32, f32),
) -> (f32, f32) {
    let mut origin = (size.0 / 2.0, size.1 / 2.0);
    let Some(spec) = spec else {
        return origin;
    };
    let mut axis = 0;
    for token in spec.split_whitespace() {
        // The keywords may come in either order, so each names its own axis.
        let (value, of) = match token.to_ascii_lowercase().as_str() {
            "left" => (Some(0.0), 0),
            "right" => (Some(size.0), 0),
            "top" => (Some(0.0), 1),
            "bottom" => (Some(size.1), 1),
            "center" => (None, axis),
            _ => {
                let base = if axis == 0 { size.0 } else { size.1 };
                let value = match token.strip_suffix('%') {
                    Some(pct) => pct.trim().parse::<f32>().ok().map(|n| n / 100.0 * base),
                    None => Some(parse_length_token(token, ctx)),
                };
                (value, axis)
            }
        };
        if let Some(value) = value {
            if of == 0 {
                origin.0 = value;
            } else {
                origin.1 = value;
            }
        }
        // A keyword names its axis explicitly; anything else takes the next one.
        axis = if of == 0 { 1 } else { 0 };
    }
    origin
}

/// Whether the engine supports what an `@supports` condition asks about.
///
/// The answers have to be *honest*. `@supports` exists so a site can hand a
/// partial engine a working fallback instead of a broken layout, and an engine
/// that claims a property it only half-implements makes the site take the
/// modern path and render worse than if the rule had been ignored. So support
/// is answered from an explicit registry of what the engine *applies*, never
/// inferred from the value parsing.
pub fn supports_matches(condition: &str) -> bool {
    let mut parser = SupportsParser {
        text: condition,
        pos: 0,
    };
    let value = parser.or_expr().unwrap_or(false);
    parser.skip_whitespace();
    // Trailing junk means the condition used something we did not understand,
    // and per spec an unparsable condition is false rather than half-true.
    value && parser.pos == parser.text.len()
}

/// Properties the engine reads and acts on, as opposed to merely parses.
///
/// Gathered from what `layout.rs` and `paint.rs` actually look up. Anything
/// absent answers `false`, which is the safe direction: a site then takes its
/// fallback path, which is what an engine without the property wants anyway.
/// As the rest of the backlog lands, its entry is added here with it.
const SUPPORTED_PROPERTIES: &[&str] = &[
    "align-items",
    "align-self",
    "animation",
    "animation-delay",
    "animation-direction",
    "animation-duration",
    "animation-fill-mode",
    "animation-iteration-count",
    "animation-name",
    "animation-play-state",
    "animation-timing-function",
    "backdrop-filter",
    "background",
    "background-color",
    "background-image",
    "background-position",
    "background-repeat",
    "background-size",
    "border-bottom-width",
    "border-left-width",
    "border-radius",
    "border-right-width",
    "border-spacing",
    "border-style",
    "border-top-width",
    "bottom",
    "box-shadow",
    "box-sizing",
    "clear",
    "color",
    "content",
    // `chapter 1 section` — pairs of a name and a number.
    "counter-increment",
    "counter-reset",
    "counter-set",
    "cursor",
    // `"\201C" "\201D"` — pairs of quotation marks.
    "quotes",
    "display",
    "flex",
    "flex-basis",
    "flex-direction",
    "flex-grow",
    "flex-shrink",
    "flex-wrap",
    "float",
    "font-family",
    "font-size",
    "font-style",
    "filter",
    "font-weight",
    "gap",
    "grid-area",
    "grid-template",
    "grid-template-areas",
    "grid-template-columns",
    "grid-template-rows",
    "height",
    "justify-content",
    "left",
    "letter-spacing",
    "line-height",
    "margin",
    "margin-bottom",
    "margin-left",
    "margin-right",
    "margin-top",
    "max-height",
    "max-width",
    "min-height",
    "min-width",
    "object-fit",
    "opacity",
    "outline-color",
    "outline-style",
    "outline-width",
    "overflow",
    "padding",
    "padding-bottom",
    "padding-left",
    "padding-right",
    "padding-top",
    "position",
    "right",
    "text-align",
    "text-decoration",
    "text-overflow",
    "text-shadow",
    "text-transform",
    "top",
    "transform",
    "transform-origin",
    "transition",
    "transition-timing-function",
    "visibility",
    "white-space",
    "width",
    "z-index",
];

/// Selector syntax the engine matches. `selector()` asks about the selector,
/// not about any property, so it gets its own list.
fn supports_selector(selector: &str) -> bool {
    let selector = selector.trim();
    if selector.is_empty() {
        return false;
    }
    // Anything the engine does not match is a lie waiting to happen, so only
    // the constructs `style.rs` implements answer true.
    const KNOWN_PSEUDO: &[&str] = &[
        ":hover",
        ":first-child",
        ":last-child",
        ":only-child",
        ":not",
        ":nth-child",
        "::before",
        "::after",
    ];
    if let Some(colon) = selector.find(':') {
        let pseudo = &selector[colon..];
        let name = pseudo.split(['(', ' ', ',', '>']).next().unwrap_or(pseudo);
        if !KNOWN_PSEUDO.contains(&name) {
            return false;
        }
    }
    // Attribute selectors and the column combinator are not implemented.
    !selector.contains('[') && !selector.contains("||")
}

/// A declaration condition: does the engine both parse *and* apply this?
fn supports_declaration(declaration: &str) -> bool {
    let Some((property, value)) = declaration.split_once(':') else {
        return false;
    };
    let property = property.trim().to_ascii_lowercase();
    let value = value.trim();
    if value.is_empty() {
        return false;
    }
    // A custom property is storage, and the engine stores any of them.
    if property.starts_with("--") {
        return true;
    }
    if !SUPPORTED_PROPERTIES.contains(&property.as_str()) {
        return false;
    }
    // The property is implemented, so the question becomes whether this
    // particular value is — `display: grid` and `display: ruby` are not the
    // same question.
    match property.as_str() {
        "display" => matches!(
            value,
            "block"
                | "inline"
                | "inline-block"
                | "flex"
                | "inline-flex"
                | "grid"
                | "inline-grid"
                | "none"
                | "contents"
                | "table"
                | "table-row"
                | "table-cell"
                | "list-item"
        ),
        "position" => matches!(
            value,
            "static" | "relative" | "absolute" | "fixed" | "sticky"
        ),
        // Every 2D function is implemented; the 3D ones are deliberately not,
        // and claiming them would be exactly the lie this function exists to
        // avoid.
        "transform" => {
            !value.contains("3d(")
                && !value.contains("perspective(")
                && !value.contains("rotateX")
                && !value.contains("rotateY")
        }
        _ => !declarations_for(&property, value).is_empty(),
    }
}

struct SupportsParser<'a> {
    text: &'a str,
    pos: usize,
}

impl SupportsParser<'_> {
    fn or_expr(&mut self) -> Option<bool> {
        let mut value = self.and_expr()?;
        // `and` and `or` may not be mixed without parens, per spec, so a flat
        // left-to-right fold is the whole of the precedence rules.
        while self.eat_keyword("or") {
            value = self.and_expr()? || value;
        }
        Some(value)
    }

    fn and_expr(&mut self) -> Option<bool> {
        let mut value = self.unary()?;
        while self.eat_keyword("and") {
            value = self.unary()? && value;
        }
        Some(value)
    }

    fn unary(&mut self) -> Option<bool> {
        self.skip_whitespace();
        if self.eat_keyword("not") {
            return Some(!self.unary()?);
        }
        if let Some(inner) = self.eat_function("selector") {
            return Some(supports_selector(&inner));
        }
        // An unrecognised function — `font-tech()`, `font-format()` — is not
        // something to guess at.
        if let Some(rest) = self.text.get(self.pos..) {
            if !rest.starts_with('(') {
                return None;
            }
        }
        let inner = self.eat_parens()?;
        let trimmed = inner.trim();
        // `( <supports-condition> )` or `( <declaration> )`. A nested condition
        // starts with a paren or `not`, or joins with `and`/`or`.
        let nested = trimmed.starts_with('(')
            || trimmed.len() >= 4 && trimmed[..4].eq_ignore_ascii_case("not ");
        if nested {
            return supports_matches(trimmed).then_some(true).or(Some(false));
        }
        Some(supports_declaration(trimmed))
    }

    /// The contents of `name( ... )` if that is what comes next.
    fn eat_function(&mut self, name: &str) -> Option<String> {
        self.skip_whitespace();
        let rest = self.text.get(self.pos..)?;
        if rest.len() < name.len() + 1 || !rest[..name.len()].eq_ignore_ascii_case(name) {
            return None;
        }
        if !rest[name.len()..].starts_with('(') {
            return None;
        }
        self.pos += name.len();
        self.eat_parens()
    }

    /// The contents of the parenthesised group starting here.
    fn eat_parens(&mut self) -> Option<String> {
        self.skip_whitespace();
        let rest = self.text.get(self.pos..)?;
        if !rest.starts_with('(') {
            return None;
        }
        let end = matching_paren(&rest[1..])?;
        self.pos += 1 + end + 1;
        Some(rest[1..1 + end].to_string())
    }

    fn eat_keyword(&mut self, keyword: &str) -> bool {
        self.skip_whitespace();
        let Some(rest) = self.text.get(self.pos..) else {
            return false;
        };
        if rest.len() < keyword.len() || !rest[..keyword.len()].eq_ignore_ascii_case(keyword) {
            return false;
        }
        // `notin` is not `not`, and `ands` is not `and`.
        let after = rest[keyword.len()..].chars().next();
        if matches!(after, Some(c) if c.is_ascii_alphanumeric() || c == '-') {
            return false;
        }
        self.pos += keyword.len();
        true
    }

    fn skip_whitespace(&mut self) {
        let Some(rest) = self.text.get(self.pos..) else {
            return;
        };
        self.pos += rest.len() - rest.trim_start().len();
    }
}

/// Whether a rule applies at this viewport size. `None` (no media block)
/// always applies.
///
/// Understands media types, width/height features, `orientation`, `hover`/
/// `pointer`, and `prefers-color-scheme`. A feature we don't understand makes
/// the block *not* match, so an unsupported condition leaves the page at its
/// base styling rather than applying rules meant for some other context.
pub fn media_matches(condition: Option<&str>, viewport_width: f32, viewport_height: f32) -> bool {
    let Some(condition) = condition else {
        return true;
    };
    // Commas are "or": any branch matching is enough.
    condition.split(',').any(|branch| {
        let branch = branch.trim().to_lowercase();
        !branch.is_empty()
            && branch
                .split(" and ")
                .all(|term| term_matches(term.trim(), viewport_width, viewport_height))
    })
}

fn term_matches(term: &str, width: f32, height: f32) -> bool {
    match term {
        "screen" | "all" => return true,
        "print" | "speech" | "only print" => return false,
        _ => {}
    }
    if let Some(rest) = term.strip_prefix("only ") {
        return term_matches(rest.trim(), width, height);
    }
    let Some(inner) = term.strip_prefix('(').and_then(|t| t.strip_suffix(')')) else {
        return false; // an unknown bare term
    };
    let Some((feature, value)) = inner.split_once(':') else {
        return false; // a bare feature test like `(hover)`
    };
    let value = value.trim();
    match feature.trim() {
        "min-width" | "max-width" | "min-height" | "max-height" => {
            let Some(px) = parse_px(value) else {
                return false;
            };
            match feature.trim() {
                "min-width" => width >= px,
                "max-width" => width <= px,
                "min-height" => height >= px,
                "max-height" => height <= px,
                _ => unreachable!(),
            }
        }
        "orientation" => match value {
            "landscape" => width >= height,
            "portrait" => width < height,
            _ => false,
        },
        // No touch input path exists anywhere in this engine — mouse and
        // keyboard only — so a fine pointer that can hover is simply always
        // true here, not a cut corner.
        "hover" | "any-hover" => value == "hover",
        "pointer" | "any-pointer" => value == "fine",
        // ponytail: no OS/shell theme signal is threaded in yet — the shell
        // has no dark-mode setting of its own (see Track F/G) — so this is
        // always "light" until one exists to report.
        "prefers-color-scheme" => value == "light",
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
    fn parse_rules(&mut self) -> (Vec<Rule>, Vec<FontFace>, Vec<Keyframes>) {
        self.parse_rule_list(None, false)
    }

    /// The body of a stylesheet, or of any conditional group rule.
    ///
    /// One function for both is what makes a nested at-rule work: `@media
    /// print { @page { ... } }` and `@media ... { @supports ... { ... } }` are
    /// the same shape as the top level, so recursing handles every at-rule at
    /// once rather than special-casing the pair that happen to be common.
    ///
    /// `media` is the condition inherited from enclosing `@media` blocks;
    /// `nested` says a closing brace ends this list rather than being a stray.
    fn parse_rule_list(
        &mut self,
        media: Option<&str>,
        nested: bool,
    ) -> (Vec<Rule>, Vec<FontFace>, Vec<Keyframes>) {
        let mut rules = Vec::new();
        let mut font_faces = Vec::new();
        let mut keyframes = Vec::new();
        loop {
            self.consume_whitespace();
            if self.eof() {
                break;
            }
            if self.starts_with("}") {
                self.consume_char(); // this list's closing brace, or a stray one
                if nested {
                    break;
                }
                continue;
            }
            if self.starts_with("@media") {
                self.pos += "@media".len();
                let Some(condition) = self.at_rule_prelude() else {
                    continue;
                };
                // A nested `@media` narrows the outer one; both have to hold.
                let combined = match media {
                    Some(outer) => format!("{outer} and {condition}"),
                    None => condition,
                };
                let (inner, faces, frames) = self.parse_rule_list(Some(&combined), true);
                rules.extend(inner);
                font_faces.extend(faces);
                keyframes.extend(frames);
            } else if self.starts_with("@supports") {
                self.pos += "@supports".len();
                let Some(condition) = self.at_rule_prelude() else {
                    continue;
                };
                if supports_matches(&condition) {
                    let (inner, faces, frames) = self.parse_rule_list(media, true);
                    rules.extend(inner);
                    font_faces.extend(faces);
                    keyframes.extend(frames);
                } else {
                    self.skip_balanced_braces();
                }
            } else if self.starts_with("@font-face") {
                if let Some(face) = self.parse_font_face() {
                    font_faces.push(face);
                }
            } else if self.starts_with("@keyframes") {
                self.pos += "@keyframes".len();
                if let Some(rule) = self.parse_keyframes() {
                    // Redefinition: the last one wins, so an earlier rule of the
                    // same name goes rather than sitting there to be found first.
                    keyframes.retain(|k: &Keyframes| k.name != rule.name);
                    keyframes.push(rule);
                }
            } else if self.starts_with("@") {
                self.skip_at_rule();
            } else if let Some(mut rule) = self.parse_rule() {
                rule.media = media.map(str::to_string);
                rules.push(rule);
            }
        }
        (rules, font_faces, keyframes)
    }

    /// An at-rule's prelude, leaving the parser just past its opening brace.
    ///
    /// `None` means the rule had no block — `@media screen;` is nonsense a page
    /// can still contain, and the semicolon has been consumed.
    fn at_rule_prelude(&mut self) -> Option<String> {
        let start = self.pos;
        self.consume_while(|c| c != '{' && c != ';');
        let prelude = self.input[start..self.pos].trim().to_string();
        if !self.starts_with("{") {
            if self.starts_with(";") {
                self.consume_char();
            }
            return None;
        }
        self.consume_char(); // the opening brace
        Some(prelude)
    }

    /// `@font-face { font-family: "Outfit"; src: url(a.woff2) format("woff2"),
    /// url(a.woff) }` — the name a page's `font-family` can ask for, and where
    /// the file is.
    ///
    /// ponytail: `font-weight`/`font-style`/`unicode-range` descriptors are
    /// read past. A family with a separate file per weight loads them all under
    /// one name, and the first that parses wins — so a page gets its typeface
    /// but not its bold. Selecting within a family needs weight-aware matching,
    /// which is the same work synthesized bold is standing in for today.
    fn parse_font_face(&mut self) -> Option<FontFace> {
        self.consume_while(|c| c != '{');
        let declarations = self.parse_declarations();
        let mut family = String::new();
        let mut srcs = Vec::new();
        for declaration in &declarations {
            match declaration.name.as_str() {
                "font-family" => family = unquote(&value_text(&declaration.value)).to_string(),
                "src" => srcs = url_tokens(&value_text(&declaration.value)),
                _ => {}
            }
        }
        (!family.is_empty() && !srcs.is_empty()).then_some(FontFace { family, srcs })
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
                parts.push(SelectorPart {
                    simple: self.parse_simple_selector(),
                    combinator,
                });
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
        selectors.sort_by_key(|s| std::cmp::Reverse(s.specificity()));
        Some(selectors)
    }

    fn parse_simple_selector(&mut self) -> SimpleSelector {
        let mut selector = SimpleSelector {
            pseudo_element: None,
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
                             // A pseudo-*element* is a box the page is asking the engine to make,
                             // not a condition on this one. `::before`/`::after` carry real content
                             // on real sites — bullets, quote marks, disclosure arrows — so they are
                             // recorded and generated. Anything else is still a selector we would
                             // only half understand, and the rule goes.
        if self.next_char_or('\0') == ':' {
            self.consume_char();
            let name = self.parse_identifier().to_ascii_lowercase();
            selector.pseudo_element = match name.as_str() {
                "before" => Some(PseudoElement::Before),
                "after" => Some(PseudoElement::After),
                _ => {
                    self.pos = start;
                    return false;
                }
            };
            return true;
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
                let mut sub = Parser {
                    pos: 0,
                    input: inner.to_string(),
                };
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
            (
                "visited" | "active" | "focus" | "focus-within" | "focus-visible" | "target",
                None,
            ) => Pseudo::Never,
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
            return Some(AttrTest {
                name,
                op: AttrOp::Exists,
                value: String::new(),
            });
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
            let raw = strip_comments(&self.consume_while(|c| c != ';' && c != '}'));
            if self.starts_with(";") {
                self.consume_char();
            }
            if !name.is_empty() {
                let raw = raw.trim();
                // A custom property is whatever text it was given, and a value
                // that mentions one cannot be understood until styling resolves
                // it against the element's inherited variables.
                if name.starts_with("--") || raw.contains("var(") {
                    declarations.push(Declaration {
                        name,
                        value: Value::Raw(raw.to_string()),
                    });
                } else {
                    declarations.extend(declarations_for(&name, raw));
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

    /// `@keyframes spin { from { ... } 50% { ... } to { ... } }`.
    ///
    /// The selector of each stop is a percentage, or `from`/`to`, and one stop
    /// may list several (`0%, 100% { opacity: 1 }`).
    fn parse_keyframes(&mut self) -> Option<Keyframes> {
        let name = self.at_rule_prelude()?.trim().to_string();
        if name.is_empty() {
            self.skip_balanced_braces();
            return None;
        }
        let mut stops: Vec<(f32, Vec<Declaration>)> = Vec::new();
        loop {
            self.consume_whitespace();
            if self.eof() {
                break;
            }
            if self.starts_with("}") {
                self.consume_char();
                break;
            }
            let start = self.pos;
            self.consume_while(|c| c != '{' && c != '}');
            let selector = self.input[start..self.pos].trim().to_string();
            if !self.starts_with("{") {
                break; // malformed: stop rather than spin
            }
            let declarations = self.parse_declarations();
            for position in selector.split(',') {
                let Some(position) = keyframe_position(position.trim()) else {
                    continue;
                };
                stops.push((position, declarations.clone()));
            }
        }
        stops.sort_by(|(a, _), (b, _)| a.total_cmp(b));
        Some(Keyframes { name, stops })
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

    /// Whitespace, and comments — which are whitespace as far as the grammar is
    /// concerned, and appear between any two tokens a real stylesheet has.
    fn consume_whitespace(&mut self) {
        loop {
            self.consume_while(char::is_whitespace);
            if !self.starts_with("/*") {
                return;
            }
            self.pos += 2;
            match self.input[self.pos..].find("*/") {
                Some(end) => self.pos += end + 2,
                // Unterminated: the rest of the sheet is inside the comment.
                None => {
                    self.pos = self.input.len();
                    return;
                }
            }
        }
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
    fn multi_value_shorthands_expand_into_the_longhands_layout_reads() {
        let find = |d: &[Declaration], name: &str| {
            d.iter()
                .find(|decl| decl.name == name)
                .map(|decl| decl.value.clone())
        };

        let s = parse(".a { padding: 10px 20px; }".to_string());
        let d = &s.rules[0].declarations;
        assert_eq!(find(d, "padding-top"), Some(Value::Length(10.0, Unit::Px)));
        assert_eq!(
            find(d, "padding-right"),
            Some(Value::Length(20.0, Unit::Px))
        );
        assert_eq!(
            find(d, "padding-bottom"),
            Some(Value::Length(10.0, Unit::Px))
        );
        assert_eq!(find(d, "padding-left"), Some(Value::Length(20.0, Unit::Px)));

        let s = parse(".a { margin: 0 auto; }".to_string());
        let d = &s.rules[0].declarations;
        assert_eq!(find(d, "margin-top"), Some(Value::Number(0.0)));
        assert_eq!(
            find(d, "margin-right"),
            Some(Value::Keyword("auto".to_string()))
        );
        assert_eq!(
            find(d, "margin-left"),
            Some(Value::Keyword("auto".to_string()))
        );

        let s = parse(".a { margin: 1px 2px 3px 4px; }".to_string());
        let d = &s.rules[0].declarations;
        assert_eq!(find(d, "margin-top"), Some(Value::Length(1.0, Unit::Px)));
        assert_eq!(find(d, "margin-right"), Some(Value::Length(2.0, Unit::Px)));
        assert_eq!(find(d, "margin-bottom"), Some(Value::Length(3.0, Unit::Px)));
        assert_eq!(find(d, "margin-left"), Some(Value::Length(4.0, Unit::Px)));

        let s = parse(".a { border: 1px solid #cccccc; }".to_string());
        let d = &s.rules[0].declarations;
        assert_eq!(find(d, "border-width"), Some(Value::Length(1.0, Unit::Px)));
        assert_eq!(
            find(d, "border-style"),
            Some(Value::Keyword("solid".to_string()))
        );
        assert_eq!(
            find(d, "border-color"),
            Some(Value::ColorValue(Color {
                r: 0xcc,
                g: 0xcc,
                b: 0xcc,
                a: 255
            }))
        );

        let s = parse(".a { flex: 1 1 0; }".to_string());
        let d = &s.rules[0].declarations;
        assert_eq!(find(d, "flex-grow"), Some(Value::Number(1.0)));
        assert_eq!(find(d, "flex-shrink"), Some(Value::Number(1.0)));
        assert_eq!(find(d, "flex-basis"), Some(Value::Number(0.0)));

        let s = parse(".a { background: #ffffff url(bg.png) no-repeat; }".to_string());
        let d = &s.rules[0].declarations;
        assert_eq!(
            find(d, "background-color"),
            Some(Value::ColorValue(Color {
                r: 0xff,
                g: 0xff,
                b: 0xff,
                a: 255
            }))
        );
        assert_eq!(
            find(d, "background-image"),
            Some(Value::Raw("url(bg.png)".to_string()))
        );

        // The `font` shorthand: the size is the pivot, the family runs to the
        // end of the value, and everything unmentioned resets.
        let s = parse(".a { font: bold 14px/1.5 \"Fira Sans\", Arial; }".to_string());
        let d = &s.rules[0].declarations;
        assert_eq!(
            find(d, "font-weight"),
            Some(Value::Keyword("bold".to_string()))
        );
        assert_eq!(find(d, "font-size"), Some(Value::Length(14.0, Unit::Px)));
        assert_eq!(find(d, "line-height"), Some(Value::Number(1.5)));
        assert_eq!(
            find(d, "font-family"),
            Some(Value::Raw("\"Fira Sans\", Arial".to_string()))
        );
        // No weight given, so the shorthand resets it rather than inheriting.
        let s = parse(".a { font: italic 1rem Georgia, serif; }".to_string());
        let d = &s.rules[0].declarations;
        assert_eq!(
            find(d, "font-style"),
            Some(Value::Keyword("italic".to_string()))
        );
        assert_eq!(
            find(d, "font-weight"),
            Some(Value::Keyword("normal".to_string()))
        );
        assert_eq!(find(d, "font-size"), Some(Value::Length(1.0, Unit::Rem)));
        // A named absolute size is a size, not a family.
        let s = parse(".a { font: small Verdana; }".to_string());
        let d = &s.rules[0].declarations;
        assert_eq!(find(d, "font-size"), Some(Value::Length(13.0, Unit::Px)));
        assert_eq!(
            find(d, "font-family"),
            Some(Value::Raw("Verdana".to_string()))
        );

        let s = parse(".a { outline: 3px dashed #00ff00; }".to_string());
        let d = &s.rules[0].declarations;
        assert_eq!(find(d, "outline-width"), Some(Value::Length(3.0, Unit::Px)));
        assert_eq!(
            find(d, "outline-style"),
            Some(Value::Keyword("dashed".to_string()))
        );
        assert_eq!(
            find(d, "outline-color"),
            Some(Value::ColorValue(Color {
                r: 0x00,
                g: 0xff,
                b: 0x00,
                a: 255
            }))
        );

        // A single-token shorthand is untouched by expansion.
        let s = parse(".a { padding: 10px; }".to_string());
        assert_eq!(
            s.rules[0].declarations[0].value,
            Value::Length(10.0, Unit::Px)
        );

        // An unknown multi-token property still drops, as before.
        let s = parse(".a { unknown-thing: 1px 2px; }".to_string());
        assert!(s.rules[0].declarations.is_empty());
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
    fn calc_mixes_units_by_resolving_each_side_in_context() {
        let ctx = LengthContext {
            percent_base: 800.0,
            font_size: 20.0,
            root_font_size: 16.0,
        };
        // The canonical use: a sidebar-adjacent width. Neither side alone can
        // be one Value::Length, since resolving the % needs a context that
        // isn't known until each side is resolved separately, here.
        assert_eq!(
            classify_value("calc(100% - 250px)").unwrap().resolve(ctx),
            800.0 - 250.0
        );
        // Precedence: * binds tighter than -.
        assert_eq!(
            classify_value("calc(10px + 2 * 5px)").unwrap().resolve(ctx),
            20.0
        );
        // Parens override precedence, and nesting works.
        assert_eq!(
            classify_value("calc((10px + 2px) * 3)")
                .unwrap()
                .resolve(ctx),
            36.0
        );
        // A unary minus on a term, not just a binary subtraction.
        assert_eq!(
            classify_value("calc(100px + -20px)").unwrap().resolve(ctx),
            80.0
        );
        // Division.
        assert_eq!(
            classify_value("calc(100px / 4)").unwrap().resolve(ctx),
            25.0
        );
        // Garbage inside calc() must not silently become some other value.
        assert!(classify_value("calc(100px +)").is_none());
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
                .filter(|r| media_matches(r.media.as_deref(), width, 600.0))
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
        // The four pseudo-classes we can honour exactly, plus `::before`, which
        // now names a box to generate rather than being thrown away. `:has()`
        // is still dropped rather than applied to everything.
        assert_eq!(s.rules.len(), 5);
        let subject = |i: usize| s.rules[i].selectors[0].subject().unwrap();
        assert_eq!(subject(4).pseudo_element, Some(PseudoElement::Before));
        assert_eq!(subject(4).tag_name.as_deref(), Some("p"));
        assert_eq!(subject(0).pseudos, vec![Pseudo::NthChild(2, 1)]);
        assert_eq!(subject(2).pseudos, vec![Pseudo::AttrPresent("checked")]);
        assert_eq!(subject(3).pseudos, vec![Pseudo::Never]);
        match &subject(1).pseudos[..] {
            [Pseudo::Not(inner)] => assert_eq!(inner.class, vec!["skip".to_string()]),
            other => panic!("expected :not(.skip), got {other:?}"),
        }
    }

    #[test]
    fn comments_are_whitespace_wherever_they_appear() {
        let sheet = parse(
            "/* leading */ a { color: /* mid */ #ff0000; }              .b /* between */ .c { width: 4px; }              /* a rule commented out: p { color: #00ff00 } */              d { color: #0000ff } /* trailing"
                .to_string(),
        );
        // Three rules survive; the commented-out one does not exist, and an
        // unterminated comment swallows the rest rather than derailing it.
        assert_eq!(sheet.rules.len(), 3);
        assert_eq!(
            sheet.rules[0].declarations[0].value,
            Value::ColorValue(Color {
                r: 255,
                g: 0,
                b: 0,
                a: 255
            })
        );
        // `.b /* x */ .c` is a descendant selector, not three compounds.
        assert_eq!(sheet.rules[1].selectors[0].parts.len(), 2);
        assert_eq!(sheet.rules[2].declarations[0].name, "color");
    }

    #[test]
    fn supports_answers_what_the_engine_applies() {
        // A property the engine acts on, and a value of it that it acts on.
        assert!(supports_matches("(display: grid)"));
        assert!(supports_matches("(color: red)"));
        // Parsing is not applying. `transform` parses any function list, but
        // only translate and scale reach the screen, so claiming `rotate` would
        // send a site down a path it renders worse on.
        assert!(supports_matches("(transform: translateX(4px))"));
        assert!(supports_matches("(transform: rotate(45deg))"));
        // The 3D functions are deliberately not implemented, so they answer no.
        assert!(!supports_matches("(transform: rotateX(45deg))"));
        assert!(!supports_matches("(transform: translate3d(1px, 2px, 3px))"));
        // Properties the engine has no implementation of at all.
        assert!(!supports_matches("(mix-blend-mode: multiply)"));
        assert!(!supports_matches("(clip-path: circle(40%))"));
        assert!(!supports_matches("(display: ruby-text)"));
        // Boolean combinations, negation and grouping.
        assert!(supports_matches("(display: flex) and (color: red)"));
        assert!(!supports_matches(
            "(display: flex) and (mix-blend-mode: multiply)"
        ));
        assert!(supports_matches(
            "(mix-blend-mode: multiply) or (display: flex)"
        ));
        assert!(supports_matches("not (mix-blend-mode: multiply)"));
        assert!(supports_matches("((display: flex) or (display: grid))"));
        // A condition using syntax we do not understand is false, not true.
        assert!(!supports_matches("font-tech(color-COLRv1)"));
        assert!(!supports_matches("(display: flex) garbage"));
        // `selector()` asks about the selector, and answers honestly too.
        assert!(supports_matches("selector(a:hover)"));
        assert!(!supports_matches("selector(a[href])"));
        // A custom property is storage, and the engine stores any of them.
        assert!(supports_matches("(--anything: 1px)"));
    }

    #[test]
    fn nested_at_rules_are_no_longer_dropped() {
        let sheet = parse(
            "@media screen { @supports (display: grid) { .g { color: #ff0000; } } \
             @supports (mix-blend-mode: multiply) { .f { color: #00ff00; } } \
             .plain { color: #0000ff; } } \
             @media print { @page { margin: 1cm; } .paper { color: #010101; } }"
                .to_string(),
        );
        let selectors: Vec<String> = sheet
            .rules
            .iter()
            .flat_map(|r| r.selectors.iter().map(|s| format!("{s:?}")))
            .collect();
        let has = |name: &str| selectors.iter().any(|s| s.contains(name));
        // Inside @media, inside a @supports that holds.
        assert!(
            has("\"g\""),
            "nested @supports content was dropped: {selectors:?}"
        );
        // The @supports that does not hold takes its block with it.
        assert!(!has("\"f\""), "an unsupported @supports block was applied");
        // A rule after a nested at-rule is still there — the skip did not eat
        // the rest of the enclosing block.
        assert!(has("\"plain\""), "the rest of the @media block was lost");
        // An unknown at-rule inside @media is skipped without corrupting what
        // follows it.
        assert!(has("\"paper\""), "@page swallowed the rest of @media print");

        // Both conditions are recorded, so the inner one still has to match.
        let grid = sheet
            .rules
            .iter()
            .find(|r| {
                r.selectors
                    .iter()
                    .any(|s| format!("{s:?}").contains("\"g\""))
            })
            .expect("the nested rule");
        assert_eq!(grid.media.as_deref(), Some("screen"));

        let nested_media = parse(
            "@media screen { @media (min-width: 700px) { .w { color: #ff0000; } } }".to_string(),
        );
        let rule = &nested_media.rules[0];
        assert!(media_matches(rule.media.as_deref(), 900.0, 600.0));
        assert!(!media_matches(rule.media.as_deref(), 500.0, 600.0));
    }

    fn ctx() -> LengthContext {
        LengthContext {
            percent_base: 0.0,
            font_size: 16.0,
            root_font_size: 16.0,
        }
    }

    /// Where a point lands, rounded, so floating point noise does not fail a
    /// geometric assertion.
    fn at(m: Mat, x: f32, y: f32) -> (i32, i32) {
        let (x, y) = m.apply(x, y);
        (x.round() as i32, y.round() as i32)
    }

    #[test]
    fn every_angle_unit_parses() {
        let quarter = std::f32::consts::FRAC_PI_2;
        for token in ["90deg", "100grad", "0.25turn"] {
            let radians = parse_angle(token).unwrap_or_else(|| panic!("{token} did not parse"));
            assert!(
                (radians - quarter).abs() < 1.0e-4,
                "{token} gave {radians}, wanted {quarter}"
            );
        }
        assert!((parse_angle("1.5708rad").unwrap() - quarter).abs() < 1.0e-3);
        assert_eq!(parse_angle("0"), Some(0.0));
        assert_eq!(parse_angle("banana"), None);
    }

    #[test]
    fn transform_functions_compose_in_the_order_written() {
        let size = (100.0, 40.0);
        // Rotating a quarter turn takes +x to +y.
        let m = parse_transform("rotate(90deg)", ctx(), size);
        assert_eq!(at(m, 10.0, 0.0), (0, 10));
        // Order matters: scale-then-translate moves by the unscaled amount,
        // because the translation is applied in the scaled space.
        let scale_first = parse_transform("scale(2) translate(10px, 0)", ctx(), size);
        let translate_first = parse_transform("translate(10px, 0) scale(2)", ctx(), size);
        assert_eq!(at(scale_first, 0.0, 0.0), (20, 0));
        assert_eq!(at(translate_first, 0.0, 0.0), (10, 0));
        // A percentage in translate() is of the box's own size.
        let m = parse_transform("translate(-50%, -50%)", ctx(), size);
        assert_eq!(at(m, 0.0, 0.0), (-50, -20));
        // skewX slides x by y's tangent; matrix() is taken as written.
        let m = parse_transform("skewX(45deg)", ctx(), size);
        assert_eq!(at(m, 0.0, 10.0), (10, 10));
        let m = parse_transform("matrix(1, 0, 0, 1, 5, 6)", ctx(), size);
        assert_eq!(at(m, 0.0, 0.0), (5, 6));
        // A 3D function is ignored rather than partly applied.
        let m = parse_transform("rotateX(45deg)", ctx(), size);
        assert!(m.is_identity(), "a 3D function was applied: {m:?}");
        assert!(parse_transform("perspective(400px)", ctx(), size).is_identity());
        // Rotation is not a scale-and-offset; a plain translate is.
        assert!(!parse_transform("rotate(10deg)", ctx(), size).is_upright());
        assert!(parse_transform("translate(4px, 2px)", ctx(), size).is_upright());
        // A per-axis scale is not either, so it goes through the layer path
        // rather than being approximated by one factor.
        assert!(!parse_transform("scale(2, 3)", ctx(), size).is_upright());
    }

    #[test]
    fn transform_origin_places_the_pivot() {
        let size = (100.0, 40.0);
        // The default is the centre.
        assert_eq!(parse_transform_origin(None, ctx(), size), (50.0, 20.0));
        assert_eq!(
            parse_transform_origin(Some("left top"), ctx(), size),
            (0.0, 0.0)
        );
        // The keywords may come in either order, each naming its own axis.
        assert_eq!(
            parse_transform_origin(Some("top left"), ctx(), size),
            (0.0, 0.0)
        );
        assert_eq!(
            parse_transform_origin(Some("right bottom"), ctx(), size),
            (100.0, 40.0)
        );
        assert_eq!(
            parse_transform_origin(Some("25% 50%"), ctx(), size),
            (25.0, 20.0)
        );
        assert_eq!(
            parse_transform_origin(Some("10px 4px"), ctx(), size),
            (10.0, 4.0)
        );
    }

    #[test]
    fn a_matrix_and_its_inverse_cancel() {
        let m = parse_transform("rotate(30deg) scale(1.5) skewY(10deg)", ctx(), (10.0, 10.0));
        let inverse = m.invert().expect("invertible");
        let (x, y) = inverse.apply(m.apply(7.0, -3.0).0, m.apply(7.0, -3.0).1);
        assert!((x - 7.0).abs() < 1.0e-3, "x came back as {x}");
        assert!((y + 3.0).abs() < 1.0e-3, "y came back as {y}");
        // A collapsed transform has no inverse and paints nothing.
        assert!(Mat::scale(0.0, 1.0).invert().is_none());
    }

    #[test]
    fn media_conditions_are_evaluated() {
        assert!(media_matches(None, 400.0, 800.0)); // no block: always on
        assert!(media_matches(Some("screen"), 400.0, 800.0));
        assert!(!media_matches(Some("print"), 400.0, 800.0));
        assert!(media_matches(Some("(max-width: 600px)"), 400.0, 800.0));
        assert!(!media_matches(Some("(max-width: 600px)"), 900.0, 800.0));
        // `and` requires both; a comma is `or`.
        assert!(!media_matches(
            Some("screen and (min-width: 900px)"),
            400.0,
            800.0
        ));
        assert!(media_matches(Some("print, screen"), 400.0, 800.0));
        // em/rem conditions resolve against the initial font size.
        assert!(media_matches(Some("(min-width: 20em)"), 400.0, 800.0));
        // A feature we cannot judge must not switch styles on.
        assert!(!media_matches(
            Some("(prefers-color-scheme: dark)"),
            400.0,
            800.0
        ));
    }

    #[test]
    fn height_features_read_the_viewport_height_not_the_width() {
        assert!(media_matches(Some("(min-height: 500px)"), 400.0, 600.0));
        assert!(!media_matches(Some("(min-height: 700px)"), 400.0, 600.0));
        assert!(media_matches(Some("(max-height: 700px)"), 400.0, 600.0));
    }

    #[test]
    fn orientation_compares_width_against_height() {
        assert!(media_matches(
            Some("(orientation: landscape)"),
            800.0,
            600.0
        ));
        assert!(!media_matches(
            Some("(orientation: portrait)"),
            800.0,
            600.0
        ));
        assert!(media_matches(Some("(orientation: portrait)"), 400.0, 800.0));
        // Square counts as landscape (width >= height), matching browser behavior.
        assert!(media_matches(
            Some("(orientation: landscape)"),
            500.0,
            500.0
        ));
    }

    #[test]
    fn hover_and_pointer_report_a_mouse_and_keyboard_browser() {
        // No touch input path exists anywhere in this engine — always a fine
        // pointer that can hover, never a coarse touch pointer.
        assert!(media_matches(Some("(hover: hover)"), 400.0, 800.0));
        assert!(!media_matches(Some("(hover: none)"), 400.0, 800.0));
        assert!(media_matches(Some("(pointer: fine)"), 400.0, 800.0));
        assert!(!media_matches(Some("(pointer: coarse)"), 400.0, 800.0));
        assert!(media_matches(Some("(any-hover: hover)"), 400.0, 800.0));
        assert!(media_matches(Some("(any-pointer: fine)"), 400.0, 800.0));
    }

    #[test]
    fn prefers_color_scheme_reports_light_until_a_real_theme_signal_exists() {
        assert!(media_matches(
            Some("(prefers-color-scheme: light)"),
            400.0,
            800.0
        ));
        assert!(!media_matches(
            Some("(prefers-color-scheme: dark)"),
            400.0,
            800.0
        ));
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
        assert_eq!(
            classify_value("block"),
            Some(Value::Keyword("block".into()))
        );
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

#[cfg(test)]
mod font_face_tests {
    use super::*;

    #[test]
    fn a_font_face_names_a_typeface_and_where_to_fetch_it() {
        let sheet = parse(
            "@font-face { font-family: 'Outfit';                src: url(outfit.woff2) format(\"woff2\"), url('outfit.ttf'); }              p { font-family: \"Outfit\", Helvetica, sans-serif; }"
                .to_string(),
        );
        assert_eq!(sheet.font_faces.len(), 1);
        assert_eq!(sheet.font_faces[0].family, "Outfit");
        // Both files, in the order the page preferred them.
        assert_eq!(sheet.font_faces[0].srcs, ["outfit.woff2", "outfit.ttf"]);
        // The rule beside it survives, and keeps its whole family list.
        assert_eq!(
            sheet.rules[0].declarations[0].value,
            Value::Raw("\"Outfit\", Helvetica, sans-serif".to_string())
        );
    }

    #[test]
    fn a_family_list_is_split_and_folded_but_not_otherwise_touched() {
        assert_eq!(
            family_list("\"Helvetica Neue\", Arial , sans-serif"),
            ["helvetica neue", "arial", "sans-serif"]
        );
        // A face declaring neither a name nor a file is not a face.
        assert!(parse("@font-face { font-weight: 700; }".to_string())
            .font_faces
            .is_empty());
        assert!(parse("@font-face { font-family: X; }".to_string())
            .font_faces
            .is_empty());
        // And an unknown at-rule is still skipped whole, not parsed as one.
        assert!(
            parse("@supports (x:y) { p { color: #ff0000; } }".to_string())
                .rules
                .is_empty()
        );
    }
}

//! Style: match CSS rules to DOM nodes and produce a styled tree (the cascade).

use crate::css::{
    Combinator, LengthContext, Rule, Selector, SelectorPart, SimpleSelector, Specificity,
    Stylesheet, Unit, Value, DEFAULT_FONT_SIZE,
};
use crate::dom::{ElementData, Node, NodeType};
use std::collections::HashMap;

pub type PropertyMap = HashMap<String, Value>;

/// A DOM node paired with its computed CSS property values.
pub struct StyledNode<'a> {
    pub node: &'a Node,
    pub specified_values: PropertyMap,
    pub children: Vec<StyledNode<'a>>,
}

#[derive(Clone, Copy, PartialEq)]
pub enum Display {
    Inline,
    Block,
    Flex,
    Grid,
    Table,
    /// Sits in a line like text, but sizes itself like a block.
    InlineBlock,
    None,
}

impl<'a> StyledNode<'a> {
    pub fn value(&self, name: &str) -> Option<Value> {
        self.specified_values.get(name).cloned()
    }

    /// The element's computed font size in px. Resolved during styling, so `em`
    /// elsewhere can be resolved without walking back up the tree.
    pub fn font_size(&self) -> f32 {
        match self.specified_values.get("font-size") {
            Some(Value::Length(v, Unit::Px)) => *v,
            _ => DEFAULT_FONT_SIZE,
        }
    }

    /// Length context for this element: percentages against `percent_base`,
    /// `em` against this element's own font size.
    pub fn length_context(&self, percent_base: f32) -> LengthContext {
        LengthContext {
            percent_base,
            font_size: self.font_size(),
            root_font_size: DEFAULT_FONT_SIZE,
        }
    }

    /// Resolve a property to px in this element's context.
    pub fn px(&self, name: &str, percent_base: f32) -> Option<f32> {
        self.value(name)
            .map(|v| v.resolve(self.length_context(percent_base)))
    }

    /// Return `name`, else `fallback_name` (for shorthand like `margin`), else `default`.
    pub fn lookup(&self, name: &str, fallback_name: &str, default: &Value) -> Value {
        self.value(name)
            .or_else(|| self.value(fallback_name))
            .unwrap_or_else(|| default.clone())
    }

    pub fn display(&self) -> Display {
        match self.value("display") {
            Some(Value::Keyword(s)) => match &*s {
                "block" => Display::Block,
                "inline-block" => Display::InlineBlock,
                // inline-flex is treated as a block-level flex container for now.
                "flex" | "inline-flex" => Display::Flex,
                "grid" | "inline-grid" => Display::Grid,
                "table" | "inline-table" => Display::Table,
                "none" => Display::None,
                _ => Display::Inline,
            },
            _ => Display::Inline,
        }
    }
}

/// What a selector needs to know about an element.
///
/// The style tree matches against parsed DOM nodes and `querySelectorAll`
/// matches against the snapshot scripts see; both use the same matcher through
/// this, so the two can never disagree about what a selector means.
pub trait Matchable {
    /// Stable identity, so `:hover` can name an element.
    fn node_id(&self) -> usize;
    fn tag(&self) -> &str;
    fn elem_id(&self) -> Option<&str>;
    fn has_class(&self, class: &str) -> bool;
    fn attr(&self, name: &str) -> Option<&str>;
}

impl Matchable for ElementData {
    fn node_id(&self) -> usize {
        self.node_id
    }

    fn tag(&self) -> &str {
        &self.tag_name
    }

    fn elem_id(&self) -> Option<&str> {
        self.id().map(String::as_str)
    }

    fn has_class(&self, class: &str) -> bool {
        self.classes().contains(class)
    }

    fn attr(&self, name: &str) -> Option<&str> {
        self.attributes.get(name).map(String::as_str)
    }
}

/// Match a selector chain against an element, given its ancestors (root first).
///
/// Matching runs right to left: check the subject, then walk up looking for the
/// ancestors the chain demands. That is the cheap direction — most elements fail
/// on the subject and never look at the chain at all.
///
/// ponytail: a descendant combinator backtracks over every ancestor, so a
/// pathological selector is quadratic in depth. Real sheets are two or three
/// compounds deep; an ancestor bloom filter is the fix if it ever bites.
pub fn matches<E: Matchable>(
    elem: &E,
    ancestors: &[&E],
    selector: &Selector,
    hovered: &HoverChain,
) -> bool {
    matches_chain(elem, ancestors, &selector.parts, hovered)
}

/// The element under the cursor and its ancestors: hovering a word inside a
/// link hovers the link too, so `a:hover` has to match the whole chain.
pub type HoverChain = std::collections::HashSet<usize>;

fn matches_chain<E: Matchable>(
    elem: &E,
    ancestors: &[&E],
    parts: &[SelectorPart],
    hovered: &HoverChain,
) -> bool {
    let Some((subject, rest)) = parts.split_last() else { return true };
    if !matches_simple_selector(elem, &subject.simple) {
        return false;
    }
    if subject.simple.hover && !hovered.contains(&elem.node_id()) {
        return false;
    }
    if rest.is_empty() {
        return true;
    }
    match subject.combinator {
        Combinator::Child => match ancestors.split_last() {
            Some((parent, above)) => matches_chain(*parent, above, rest, hovered),
            None => false,
        },
        // Any ancestor may satisfy the rest of the chain; try the nearest first.
        Combinator::Descendant => (0..ancestors.len())
            .rev()
            .any(|i| matches_chain(ancestors[i], &ancestors[..i], rest, hovered)),
    }
}

fn matches_simple_selector<E: Matchable>(elem: &E, selector: &SimpleSelector) -> bool {
    if selector.tag_name.iter().any(|name| elem.tag() != name) {
        return false;
    }
    if selector.id.iter().any(|id| elem.elem_id() != Some(id.as_str())) {
        return false;
    }
    if selector.class.iter().any(|class| !elem.has_class(class)) {
        return false;
    }
    selector.attrs.iter().all(|test| test.matches(elem.attr(&test.name)))
}

/// Rules bucketed by what their subject requires, so an element only tests the
/// handful that could possibly match instead of the whole sheet.
///
/// A real page has hundreds of rules and thousands of elements; matching every
/// pair was costing more than layout and paint together.
pub struct RuleIndex {
    by_id: HashMap<String, Vec<usize>>,
    by_class: HashMap<String, Vec<usize>>,
    by_tag: HashMap<String, Vec<usize>>,
    /// Rules whose subject names nothing indexable (`*`, `[attr]`, `:hover`).
    universal: Vec<usize>,
}

impl RuleIndex {
    pub fn build(stylesheet: &Stylesheet) -> RuleIndex {
        let mut index = RuleIndex {
            by_id: HashMap::new(),
            by_class: HashMap::new(),
            by_tag: HashMap::new(),
            universal: Vec::new(),
        };
        for (i, rule) in stylesheet.rules.iter().enumerate() {
            for selector in &rule.selectors {
                // The subject decides: ancestor conditions are checked later.
                let Some(subject) = selector.subject() else {
                    index.universal.push(i);
                    continue;
                };
                // Most selective key first, so buckets stay small.
                if let Some(id) = &subject.id {
                    index.by_id.entry(id.clone()).or_default().push(i);
                } else if let Some(class) = subject.class.first() {
                    index.by_class.entry(class.clone()).or_default().push(i);
                } else if let Some(tag) = &subject.tag_name {
                    index.by_tag.entry(tag.clone()).or_default().push(i);
                } else {
                    index.universal.push(i);
                }
            }
        }
        index
    }

    /// Rule indices worth testing against this element, in document order.
    fn candidates<E: Matchable>(&self, elem: &E, classes: &[&str]) -> Vec<usize> {
        let mut out = self.universal.clone();
        if let Some(id) = elem.elem_id() {
            out.extend(self.by_id.get(id).into_iter().flatten());
        }
        for class in classes {
            out.extend(self.by_class.get(*class).into_iter().flatten());
        }
        out.extend(self.by_tag.get(elem.tag()).into_iter().flatten());
        // A rule can arrive by more than one route (two selectors, two classes).
        out.sort_unstable();
        out.dedup();
        out
    }
}

/// (specificity, document order, rule) — order breaks specificity ties, so a
/// bucketed sweep cascades exactly like a linear one.
type MatchedRule<'a> = (Specificity, usize, &'a Rule);

fn match_rule<'a>(
    elem: &ElementData,
    ancestors: &[&ElementData],
    rule: &'a Rule,
    order: usize,
    hovered: &HoverChain,
) -> Option<MatchedRule<'a>> {
    rule.selectors
        .iter()
        .find(|selector| matches(elem, ancestors, selector, hovered))
        .map(|selector| (selector.specificity(), order, rule))
}

fn matching_rules<'a>(
    elem: &ElementData,
    ancestors: &[&ElementData],
    stylesheet: &'a Stylesheet,
    index: &RuleIndex,
    hovered: &HoverChain,
) -> Vec<MatchedRule<'a>> {
    let classes: Vec<&str> = elem.classes().into_iter().collect();
    index
        .candidates(elem, &classes)
        .into_iter()
        .filter_map(|i| match_rule(elem, ancestors, &stylesheet.rules[i], i, hovered))
        .collect()
}

fn specified_values(
    elem: &ElementData,
    ancestors: &[&ElementData],
    stylesheet: &Stylesheet,
    index: &RuleIndex,
    hovered: &HoverChain,
) -> PropertyMap {
    let mut values = presentation_hints(elem);
    let mut rules = matching_rules(elem, ancestors, stylesheet, index, hovered);
    // Apply low specificity first so high specificity overrides it, and let
    // document order settle ties.
    rules.sort_by(|&(a, ai, _), &(b, bi, _)| a.cmp(&b).then(ai.cmp(&bi)));
    for (_, _, rule) in rules {
        for declaration in &rule.declarations {
            values.insert(declaration.name.clone(), declaration.value.clone());
        }
    }
    values
}

/// Styling that comes from HTML attributes rather than CSS.
///
/// Older pages carry their whole design this way — Hacker News's orange header
/// is a `bgcolor` attribute, not a stylesheet. These are the lowest priority
/// input to the cascade, so any CSS rule still overrides them.
fn presentation_hints(elem: &ElementData) -> PropertyMap {
    const NAMES: [&str; 6] = ["bgcolor", "text", "color", "align", "width", "height"];
    // Most elements have none of these, and the map itself costs more than the
    // check does.
    if !NAMES.iter().any(|name| elem.attributes.contains_key(*name)) {
        return PropertyMap::new();
    }
    let mut hints = PropertyMap::new();
    let attr = |name: &str| elem.attributes.get(name).map(|v| v.trim().to_string());

    if let Some(color) = attr("bgcolor").and_then(|v| parse_attr_color(&v)) {
        hints.insert("background-color".to_string(), color);
    }
    // `text` colours a whole document; `color` belongs to <font>.
    for name in ["text", "color"] {
        if let Some(color) = attr(name).and_then(|v| parse_attr_color(&v)) {
            hints.insert("color".to_string(), color);
        }
    }
    // `align="center"` on a cell or paragraph is the old spelling of text-align.
    if let Some(align) = attr("align") {
        if matches!(align.as_str(), "left" | "center" | "right") {
            hints.insert("text-align".to_string(), Value::Keyword(align));
        }
    }
    for name in ["width", "height"] {
        if let Some(length) = attr(name).and_then(|v| parse_attr_length(&v)) {
            hints.insert(name.to_string(), length);
        }
    }
    hints
}

/// An attribute colour may be a bare hex value (`bgcolor="ff6600"`) as well as
/// the CSS forms.
fn parse_attr_color(value: &str) -> Option<Value> {
    // Only a real colour counts: `bgcolor="ff6600"` parses as a keyword
    // otherwise, and a keyword here would silently mean "no background".
    match crate::css::parse_value(value) {
        Some(Value::ColorValue(color)) => Some(Value::ColorValue(color)),
        _ => crate::css::parse_color_token(value),
    }
}

/// `width="120"` means pixels; `width="85%"` is a percentage.
fn parse_attr_length(value: &str) -> Option<Value> {
    match value.strip_suffix('%') {
        Some(number) => number.trim().parse().ok().map(|n| Value::Length(n, Unit::Percent)),
        None => value.parse().ok().map(|n| Value::Length(n, Unit::Px)),
    }
}

/// Custom properties (`--brand`) inherit like text colour does, so a value
/// defined on `:root` reaches every element that mentions it.
fn is_custom_property(name: &str) -> bool {
    name.starts_with("--")
}

/// Replace every `var(--name)` in `text` with the variable's value, or the
/// fallback after the comma when it has none.
///
/// ponytail: no cycle detection and one level of substitution, so a variable
/// defined in terms of another resolves only if the sheet already did the work.
fn substitute_vars(text: &str, vars: &PropertyMap) -> Option<String> {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("var(") {
        out.push_str(&rest[..start]);
        let body_start = start + "var(".len();
        let end = rest[body_start..].find(')')? + body_start;
        let (name, fallback) = match rest[body_start..end].split_once(',') {
            Some((name, fallback)) => (name.trim(), Some(fallback.trim())),
            None => (rest[body_start..end].trim(), None),
        };
        let value = match vars.get(name) {
            Some(Value::Raw(raw)) => Some(raw.clone()),
            Some(other) => Some(format!("{other:?}")), // never happens: customs stay raw
            None => fallback.map(str::to_string),
        };
        // An unresolvable var makes the whole declaration invalid, per CSS.
        out.push_str(&value?);
        rest = &rest[end + 1..];
    }
    out.push_str(rest);
    Some(out)
}

/// Turn values that mention variables into real values, using the custom
/// properties visible at this element.
fn resolve_vars(values: &mut PropertyMap, vars: &PropertyMap) {
    // Most elements mention no variable at all. Finding that out first avoids
    // copying the inherited variable table onto every element on the page.
    let pending: Vec<(String, String)> = values
        .iter()
        .filter_map(|(name, value)| match value {
            Value::Raw(text) if !is_custom_property(name) && text.contains("var(") => {
                Some((name.clone(), text.clone()))
            }
            _ => None,
        })
        .collect();
    if pending.is_empty() {
        return;
    }
    for (name, text) in pending {
        match substitute_vars(&text, vars).and_then(|text| crate::css::parse_value(&text)) {
            Some(value) => {
                values.insert(name, value);
            }
            // Leave nothing behind rather than a value we could not read.
            None => {
                values.remove(&name);
            }
        }
    }
}

/// Properties that flow from parent to child when the child doesn't set them.
/// Text nodes have no rules of their own, so this is how they get color/size.
const INHERITED_PROPERTIES: [&str; 4] = ["color", "font-size", "text-align", "white-space"];

pub fn style_tree<'a>(root: &'a Node, stylesheet: &'a Stylesheet) -> StyledNode<'a> {
    style_tree_with_hover(root, stylesheet, &HoverChain::new())
}

/// Style a tree with a cursor somewhere in it, so `:hover` rules apply.
pub fn style_tree_with_hover<'a>(
    root: &'a Node,
    stylesheet: &'a Stylesheet,
    hovered: &HoverChain,
) -> StyledNode<'a> {
    let index = RuleIndex::build(stylesheet);
    style_tree_indexed(root, stylesheet, &index, hovered)
}

/// Style a tree with an index that was built once and kept, which is what a
/// repeated render should do rather than rebuilding it every frame.
pub fn style_tree_indexed<'a>(
    root: &'a Node,
    stylesheet: &'a Stylesheet,
    index: &RuleIndex,
    hovered: &HoverChain,
) -> StyledNode<'a> {
    style_tree_inner(
        root,
        stylesheet,
        index,
        &HashMap::new(),
        &std::rc::Rc::new(PropertyMap::new()),
        &mut Vec::new(),
        hovered,
    )
}

fn style_tree_inner<'a>(
    root: &'a Node,
    stylesheet: &'a Stylesheet,
    index: &RuleIndex,
    inherited: &PropertyMap,
    inherited_vars: &std::rc::Rc<PropertyMap>,
    ancestors: &mut Vec<&'a ElementData>,
    hovered: &HoverChain,
) -> StyledNode<'a> {
    let mut specified = match root.node_type {
        NodeType::Element(ref elem) => {
            specified_values(elem, ancestors, stylesheet, index, hovered)
        }
        NodeType::Text(_) => HashMap::new(),
    };
    for prop in INHERITED_PROPERTIES {
        if !specified.contains_key(prop) {
            if let Some(value) = inherited.get(prop) {
                specified.insert(prop.to_string(), value.clone());
            }
        }
    }
    // Custom properties inherit, but copying them onto every element costs more
    // than everything else in the cascade on a page that defines many. The table
    // is shared instead, and only rebuilt where an element adds to it.
    let vars = match specified.keys().any(|name| is_custom_property(name)) {
        false => std::rc::Rc::clone(inherited_vars),
        true => {
            let mut own = (**inherited_vars).clone();
            own.extend(
                specified
                    .iter()
                    .filter(|(name, _)| is_custom_property(name))
                    .map(|(name, value)| (name.clone(), value.clone())),
            );
            std::rc::Rc::new(own)
        }
    };
    resolve_vars(&mut specified, &vars);

    // Collapse font-size to absolute px now: `em` is relative to the *parent's*
    // font size, which is only knowable here during the top-down walk.
    let parent_font = match inherited.get("font-size") {
        Some(Value::Length(v, Unit::Px)) => *v,
        _ => DEFAULT_FONT_SIZE,
    };
    let font_px = match specified.get("font-size") {
        Some(Value::Length(v, Unit::Px)) => *v,
        Some(Value::Length(v, Unit::Em)) => v * parent_font,
        Some(Value::Length(v, Unit::Rem)) => v * DEFAULT_FONT_SIZE,
        Some(Value::Length(v, Unit::Percent)) => v / 100.0 * parent_font,
        _ => parent_font,
    };
    specified.insert("font-size".to_string(), Value::Length(font_px, Unit::Px));
    // Children see this element as their nearest ancestor.
    if let NodeType::Element(ref elem) = root.node_type {
        ancestors.push(elem);
    }
    let children: Vec<StyledNode> = root
        .children
        .iter()
        .map(|child| {
            style_tree_inner(child, stylesheet, index, &specified, &vars, ancestors, hovered)
        })
        .collect();
    if matches!(root.node_type, NodeType::Element(_)) {
        ancestors.pop();
    }
    StyledNode {
        node: root,
        specified_values: specified,
        children,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The element children of a node, skipping the text nodes between them.
    ///
    /// Whitespace between tags is part of the tree — it has to be, for
    /// white-space: pre — so tests address elements, not raw child positions.
    fn elements<'a>(node: &'a StyledNode<'a>) -> Vec<&'a StyledNode<'a>> {
        node.children
            .iter()
            .filter(|c| matches!(c.node.node_type, NodeType::Element(_)))
            .collect()
    }

    /// Colour of the element found at `path` (element indices from the root).
    fn color_at(html: &str, css: &str, path: &[usize]) -> Option<Value> {
        let dom = crate::html::parse(html.to_string());
        let sheet = crate::css::parse(css.to_string());
        let styled = style_tree(&dom, &sheet);
        let mut node = &styled;
        for i in path {
            node = elements(node)[*i];
        }
        node.value("color")
    }

    const RED: Value = Value::ColorValue(crate::css::Color { r: 255, g: 0, b: 0, a: 255 });

    /// Elements come out of the parser unnumbered; the Document assigns ids
    /// normally, so a bare tree has to be numbered before ids mean anything.
    fn number_elements(node: &mut Node, next: &mut usize) {
        if let NodeType::Element(ref mut e) = node.node_type {
            *next += 1;
            e.node_id = *next;
        }
        for child in &mut node.children {
            number_elements(child, next);
        }
    }

    #[test]
    fn hover_applies_to_the_element_and_what_it_sits_inside() {
        let html = "<body><a><span>inner</span></a><a>other</a></body>";
        let css = "a { color: #000000; } a:hover { color: #ff0000; }";
        let mut dom = crate::html::parse(html.to_string());
        number_elements(&mut dom, &mut 0);
        let sheet = crate::css::parse(css.to_string());
        let black = Value::ColorValue(crate::css::Color { r: 0, g: 0, b: 0, a: 255 });
        let red = Value::ColorValue(crate::css::Color { r: 255, g: 0, b: 0, a: 255 });

        let id_of = |node: &Node| match &node.node_type {
            NodeType::Element(e) => e.node_id,
            NodeType::Text(_) => unreachable!("expected an element"),
        };
        let link = id_of(&dom.children[0]);
        let span = id_of(&dom.children[0].children[0]);

        // With no cursor, hover rules never apply.
        let cold = style_tree(&dom, &sheet);
        assert_eq!(elements(&cold)[0].value("color"), Some(black.clone()));

        // Hovering the span also hovers the link that contains it...
        let chain: HoverChain = [span, link].into_iter().collect();
        let hot = style_tree_with_hover(&dom, &sheet, &chain);
        let links = elements(&hot);
        assert_eq!(links[0].value("color"), Some(red));
        // ...but not the link beside it.
        assert_eq!(links[1].value("color"), Some(black));
    }

    #[test]
    fn attribute_selectors_match_on_value() {
        let html = "<body><input type=\"text\" name=\"q\">                    <input type=\"submit\">                    <a href=\"https://a.com/x.pdf\" rel=\"nofollow noopener\">link</a></body>";
        let css = "[type=text] { color: #ff0000; }                    input[type=\"submit\"] { color: #00ff00; }                    a[href$=\".pdf\"] { color: #0000ff; }                    a[rel~=noopener] { background-color: #111111; }                    [name] { padding: 4px; }                    a[href^=\"ftp\"] { color: #ffffff; }";
        let dom = crate::html::parse(html.to_string());
        let sheet = crate::css::parse(css.to_string());
        let styled = style_tree(&dom, &sheet);
        let fields = elements(&styled);
        let color = |i: usize| fields[i].value("color");
        let rgb = |r, g, b| {
            Some(Value::ColorValue(crate::css::Color { r, g, b, a: 255 }))
        };

        assert_eq!(color(0), rgb(255, 0, 0)); // [type=text]
        assert_eq!(color(1), rgb(0, 255, 0)); // input[type="submit"]
        assert_eq!(color(2), rgb(0, 0, 255)); // suffix match on href
        // `~=` matches one word of a space-separated list.
        assert_eq!(
            fields[2].value("background-color"),
            Some(Value::ColorValue(crate::css::Color { r: 17, g: 17, b: 17, a: 255 }))
        );
        // Presence alone, and a prefix that does not match.
        assert_eq!(fields[0].value("padding"), Some(Value::Length(4.0, Unit::Px)));
        assert_eq!(fields[1].value("padding"), None);
    }

    #[test]
    fn html_attributes_style_elements_but_css_still_wins() {
        let html = "<body><td bgcolor=\"#ff6600\" width=\"85%\">head</td>                    <td bgcolor=\"ff6600\">bare hex</td>                    <td bgcolor=\"#ff6600\" class=\"over\">overridden</td></body>";
        let orange = Value::ColorValue(crate::css::Color { r: 255, g: 102, b: 0, a: 255 });

        let dom = crate::html::parse(html.to_string());
        let sheet = crate::css::parse(".over { background-color: #000000; }".to_string());
        let styled = style_tree(&dom, &sheet);

        let cells = elements(&styled);
        assert_eq!(cells[0].value("background-color"), Some(orange.clone()));
        assert_eq!(
            cells[0].value("width"),
            Some(Value::Length(85.0, Unit::Percent))
        );
        // Attributes may omit the `#`, which CSS never allows.
        assert_eq!(cells[1].value("background-color"), Some(orange));
        // A stylesheet beats a presentation hint.
        assert_eq!(
            cells[2].value("background-color"),
            Some(Value::ColorValue(crate::css::Color { r: 0, g: 0, b: 0, a: 255 }))
        );
    }

    #[test]
    fn custom_properties_inherit_and_resolve() {
        let css = ":root { --brand: #ff0000; --pad: 12px; }                    .card { color: var(--brand); padding: var(--pad); }                    .fallback { color: var(--missing, #00ff00); }                    .broken { color: var(--nothing); }";
        let html = "<html><body><div class=\"card\">a</div>                    <div class=\"fallback\">b</div><div class=\"broken\">c</div></body></html>";
        let dom = crate::html::parse(html.to_string());
        let sheet = crate::css::parse(css.to_string());
        let styled = style_tree(&dom, &sheet);
        let body = elements(&styled)[0];

        let red = Value::ColorValue(crate::css::Color { r: 255, g: 0, b: 0, a: 255 });
        let green = Value::ColorValue(crate::css::Color { r: 0, g: 255, b: 0, a: 255 });
        // Defined on :root, used several levels down.
        let cards = elements(body);
        assert_eq!(cards[0].value("color"), Some(red));
        assert_eq!(cards[0].value("padding"), Some(Value::Length(12.0, Unit::Px)));
        // A missing variable falls back to the value after the comma.
        assert_eq!(cards[1].value("color"), Some(green));
        // With no fallback the declaration is dropped, not left as raw text.
        assert_eq!(cards[2].value("color"), None);
    }

    #[test]
    fn descendant_selectors_match_at_any_depth() {
        let html = "<body><nav><div><a>deep</a></div></nav><a>outside</a></body>";
        let css = "nav a { color: #ff0000; }";
        // Nested inside <nav>, however deep.
        assert_eq!(color_at(html, css, &[0, 0, 0]), Some(RED));
        // The same tag outside <nav> is untouched.
        assert_eq!(color_at(html, css, &[1]), None);
    }

    #[test]
    fn child_selectors_require_the_immediate_parent() {
        let html = "<body><nav><a>direct</a><div><a>grandchild</a></div></nav></body>";
        let css = "nav > a { color: #ff0000; }";
        assert_eq!(color_at(html, css, &[0, 0]), Some(RED));
        assert_eq!(color_at(html, css, &[0, 1, 0]), None);
    }

    #[test]
    fn a_longer_chain_outranks_a_shorter_one() {
        // Both match; `main p` is more specific than `p`, whatever the order.
        let html = "<body><main><p>text</p></main></body>";
        let css = "main p { color: #ff0000; } p { color: #0000ff; }";
        assert_eq!(color_at(html, css, &[0, 0]), Some(RED)); // body > main > p
    }

    #[test]
    fn a_chain_that_runs_out_of_ancestors_does_not_match() {
        let html = "<body><a>lonely</a></body>";
        assert_eq!(color_at(html, "nav a { color: #ff0000; }", &[0]), None);
        assert_eq!(color_at(html, "nav > a { color: #ff0000; }", &[0]), None);
    }
}

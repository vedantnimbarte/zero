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
    fn tag(&self) -> &str;
    fn elem_id(&self) -> Option<&str>;
    fn has_class(&self, class: &str) -> bool;
}

impl Matchable for ElementData {
    fn tag(&self) -> &str {
        &self.tag_name
    }

    fn elem_id(&self) -> Option<&str> {
        self.id().map(String::as_str)
    }

    fn has_class(&self, class: &str) -> bool {
        self.classes().contains(class)
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
pub fn matches<E: Matchable>(elem: &E, ancestors: &[&E], selector: &Selector) -> bool {
    matches_chain(elem, ancestors, &selector.parts)
}

fn matches_chain<E: Matchable>(elem: &E, ancestors: &[&E], parts: &[SelectorPart]) -> bool {
    let Some((subject, rest)) = parts.split_last() else { return true };
    if !matches_simple_selector(elem, &subject.simple) {
        return false;
    }
    if rest.is_empty() {
        return true;
    }
    match subject.combinator {
        Combinator::Child => match ancestors.split_last() {
            Some((parent, above)) => matches_chain(*parent, above, rest),
            None => false,
        },
        // Any ancestor may satisfy the rest of the chain; try the nearest first.
        Combinator::Descendant => (0..ancestors.len())
            .rev()
            .any(|i| matches_chain(ancestors[i], &ancestors[..i], rest)),
    }
}

fn matches_simple_selector<E: Matchable>(elem: &E, selector: &SimpleSelector) -> bool {
    if selector.tag_name.iter().any(|name| elem.tag() != name) {
        return false;
    }
    if selector.id.iter().any(|id| elem.elem_id() != Some(id.as_str())) {
        return false;
    }
    !selector.class.iter().any(|class| !elem.has_class(class))
}

type MatchedRule<'a> = (Specificity, &'a Rule);

fn match_rule<'a>(
    elem: &ElementData,
    ancestors: &[&ElementData],
    rule: &'a Rule,
) -> Option<MatchedRule<'a>> {
    rule.selectors
        .iter()
        .find(|selector| matches(elem, ancestors, selector))
        .map(|selector| (selector.specificity(), rule))
}

fn matching_rules<'a>(
    elem: &ElementData,
    ancestors: &[&ElementData],
    stylesheet: &'a Stylesheet,
) -> Vec<MatchedRule<'a>> {
    stylesheet
        .rules
        .iter()
        .filter_map(|rule| match_rule(elem, ancestors, rule))
        .collect()
}

fn specified_values(
    elem: &ElementData,
    ancestors: &[&ElementData],
    stylesheet: &Stylesheet,
) -> PropertyMap {
    let mut values = presentation_hints(elem);
    let mut rules = matching_rules(elem, ancestors, stylesheet);
    // Apply low specificity first so high specificity overrides it.
    rules.sort_by(|&(a, _), &(b, _)| a.cmp(&b));
    for (_, rule) in rules {
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

/// Properties that flow from parent to child when the child doesn't set them.
/// Text nodes have no rules of their own, so this is how they get color/size.
const INHERITED_PROPERTIES: [&str; 2] = ["color", "font-size"];

pub fn style_tree<'a>(root: &'a Node, stylesheet: &'a Stylesheet) -> StyledNode<'a> {
    style_tree_inner(root, stylesheet, &HashMap::new(), &mut Vec::new())
}

fn style_tree_inner<'a>(
    root: &'a Node,
    stylesheet: &'a Stylesheet,
    inherited: &PropertyMap,
    ancestors: &mut Vec<&'a ElementData>,
) -> StyledNode<'a> {
    let mut specified = match root.node_type {
        NodeType::Element(ref elem) => specified_values(elem, ancestors, stylesheet),
        NodeType::Text(_) => HashMap::new(),
    };
    for prop in INHERITED_PROPERTIES {
        if !specified.contains_key(prop) {
            if let Some(value) = inherited.get(prop) {
                specified.insert(prop.to_string(), value.clone());
            }
        }
    }

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
        .map(|child| style_tree_inner(child, stylesheet, &specified, ancestors))
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

    /// Colour of the element found at `path` (child indices from the root).
    fn color_at(html: &str, css: &str, path: &[usize]) -> Option<Value> {
        let dom = crate::html::parse(html.to_string());
        let sheet = crate::css::parse(css.to_string());
        let styled = style_tree(&dom, &sheet);
        let mut node = &styled;
        for i in path {
            node = &node.children[*i];
        }
        node.value("color")
    }

    const RED: Value = Value::ColorValue(crate::css::Color { r: 255, g: 0, b: 0, a: 255 });

    #[test]
    fn html_attributes_style_elements_but_css_still_wins() {
        let html = "<body><td bgcolor=\"#ff6600\" width=\"85%\">head</td>                    <td bgcolor=\"ff6600\">bare hex</td>                    <td bgcolor=\"#ff6600\" class=\"over\">overridden</td></body>";
        let orange = Value::ColorValue(crate::css::Color { r: 255, g: 102, b: 0, a: 255 });

        let dom = crate::html::parse(html.to_string());
        let sheet = crate::css::parse(".over { background-color: #000000; }".to_string());
        let styled = style_tree(&dom, &sheet);

        assert_eq!(styled.children[0].value("background-color"), Some(orange.clone()));
        assert_eq!(
            styled.children[0].value("width"),
            Some(Value::Length(85.0, Unit::Percent))
        );
        // Attributes may omit the `#`, which CSS never allows.
        assert_eq!(styled.children[1].value("background-color"), Some(orange));
        // A stylesheet beats a presentation hint.
        assert_eq!(
            styled.children[2].value("background-color"),
            Some(Value::ColorValue(crate::css::Color { r: 0, g: 0, b: 0, a: 255 }))
        );
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
        assert_eq!(color_at(html, css, &[0, 0, 0]), Some(RED));
    }

    #[test]
    fn a_chain_that_runs_out_of_ancestors_does_not_match() {
        let html = "<body><a>lonely</a></body>";
        assert_eq!(color_at(html, "nav a { color: #ff0000; }", &[0]), None);
        assert_eq!(color_at(html, "nav > a { color: #ff0000; }", &[0]), None);
    }
}

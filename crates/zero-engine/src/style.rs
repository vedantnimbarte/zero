//! Style: match CSS rules to DOM nodes and produce a styled tree (the cascade).

use crate::css::{
    LengthContext, Rule, Selector, SimpleSelector, Specificity, Stylesheet, Unit, Value,
    DEFAULT_FONT_SIZE,
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

fn matches(elem: &ElementData, selector: &Selector) -> bool {
    match *selector {
        Selector::Simple(ref simple) => matches_simple_selector(elem, simple),
    }
}

fn matches_simple_selector(elem: &ElementData, selector: &SimpleSelector) -> bool {
    if selector.tag_name.iter().any(|name| elem.tag_name != *name) {
        return false;
    }
    if selector.id.iter().any(|id| elem.id() != Some(id)) {
        return false;
    }
    let elem_classes = elem.classes();
    if selector
        .class
        .iter()
        .any(|class| !elem_classes.contains(&class.as_str()))
    {
        return false;
    }
    true
}

type MatchedRule<'a> = (Specificity, &'a Rule);

fn match_rule<'a>(elem: &ElementData, rule: &'a Rule) -> Option<MatchedRule<'a>> {
    rule.selectors
        .iter()
        .find(|selector| matches(elem, selector))
        .map(|selector| (selector.specificity(), rule))
}

fn matching_rules<'a>(elem: &ElementData, stylesheet: &'a Stylesheet) -> Vec<MatchedRule<'a>> {
    stylesheet
        .rules
        .iter()
        .filter_map(|rule| match_rule(elem, rule))
        .collect()
}

fn specified_values(elem: &ElementData, stylesheet: &Stylesheet) -> PropertyMap {
    let mut values = HashMap::new();
    let mut rules = matching_rules(elem, stylesheet);
    // Apply low specificity first so high specificity overrides it.
    rules.sort_by(|&(a, _), &(b, _)| a.cmp(&b));
    for (_, rule) in rules {
        for declaration in &rule.declarations {
            values.insert(declaration.name.clone(), declaration.value.clone());
        }
    }
    values
}

/// Properties that flow from parent to child when the child doesn't set them.
/// Text nodes have no rules of their own, so this is how they get color/size.
const INHERITED_PROPERTIES: [&str; 2] = ["color", "font-size"];

pub fn style_tree<'a>(root: &'a Node, stylesheet: &'a Stylesheet) -> StyledNode<'a> {
    style_tree_inner(root, stylesheet, &HashMap::new())
}

fn style_tree_inner<'a>(
    root: &'a Node,
    stylesheet: &'a Stylesheet,
    inherited: &PropertyMap,
) -> StyledNode<'a> {
    let mut specified = match root.node_type {
        NodeType::Element(ref elem) => specified_values(elem, stylesheet),
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
    let children = root
        .children
        .iter()
        .map(|child| style_tree_inner(child, stylesheet, &specified))
        .collect();
    StyledNode {
        node: root,
        specified_values: specified,
        children,
    }
}

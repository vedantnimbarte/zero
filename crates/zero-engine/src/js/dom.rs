//! The bridge between JavaScript and the document.
//!
//! Scripts get *handles* (indices) into a read-only snapshot of the element tree,
//! and any writes are recorded as [`Mutation`]s that the engine applies afterwards.
//! This keeps the DOM a plain tree instead of forcing `Rc<RefCell<..>>` everywhere.
//!
//! Writes are mirrored back into the snapshot as they are recorded, so a script
//! can set a class and then query for it within the same run.
//!
//! ponytail: only the modelled fields (text, class) are mirrored — a script that
//! writes `innerHTML` and then queries for elements *inside* it still won't find
//! them, because the new nodes don't exist until the run ends. A live DOM with
//! interior mutability is the real fix.

/// One element, addressable by its child-index path from the document root.
#[derive(Clone, Default)]
pub struct ElementInfo {
    pub path: Vec<usize>,
    /// Stable element identity (see `dom::ElementData::node_id`).
    pub node_id: usize,
    pub id: String,
    pub class: String,
    pub tag: String,
    pub text: String,
}

/// A snapshot of every element in the document, in tree order.
#[derive(Clone, Default)]
pub struct DomView {
    pub elements: Vec<ElementInfo>,
}

impl DomView {
    pub fn find_by_id(&self, id: &str) -> Option<usize> {
        self.elements.iter().position(|e| e.id == id)
    }

    /// Element handles matching a CSS selector, in document order.
    ///
    /// Reuses the stylesheet parser, so whatever the cascade understands the
    /// query API understands too.
    ///
    /// ponytail: simple selectors only (`tag`, `#id`, `.class`, and compounds).
    /// A combinator like `div p` is dropped by the parser and matches nothing.
    pub fn query(&self, selector: &str) -> Vec<usize> {
        let sheet = crate::css::parse(format!("{selector} {{}}"));
        let Some(rule) = sheet.rules.first() else { return Vec::new() };
        self.elements
            .iter()
            .enumerate()
            .filter(|(_, element)| rule.selectors.iter().any(|s| matches(element, s)))
            .map(|(i, _)| i)
            .collect()
    }

    pub fn find_by_tag(&self, tag: &str) -> Vec<usize> {
        let tag = tag.to_ascii_lowercase();
        self.elements
            .iter()
            .enumerate()
            .filter(|(_, e)| e.tag == tag)
            .map(|(i, _)| i)
            .collect()
    }
}

/// Does this element satisfy a parsed selector?
fn matches(element: &ElementInfo, selector: &crate::css::Selector) -> bool {
    let crate::css::Selector::Simple(simple) = selector;
    if simple.tag_name.iter().any(|tag| &element.tag != tag) {
        return false;
    }
    if simple.id.iter().any(|id| &element.id != id) {
        return false;
    }
    let classes: Vec<&str> = element.class.split_whitespace().collect();
    !simple.class.iter().any(|c| !classes.contains(&c.as_str()))
}

/// A pending change to an element, applied by the engine after the script runs.
#[derive(Debug, Clone)]
pub enum Mutation {
    /// Replace children with a single text node.
    SetText(usize, String),
    /// Replace children with parsed HTML.
    SetHtml(usize, String),
    /// Replace the element's `class` attribute, restyling it.
    SetClass(usize, String),
}

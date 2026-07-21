//! The bridge between JavaScript and the document.
//!
//! Scripts get *handles* (indices) into a read-only snapshot of the element tree,
//! and any writes are recorded as [`Mutation`]s that the engine applies afterwards.
//! This keeps the DOM a plain tree instead of forcing `Rc<RefCell<..>>` everywhere.
//!
//! ponytail: a snapshot means scripts can't observe their own writes, and mutating
//! an ancestor invalidates handles to its descendants. A live DOM with interior
//! mutability is the upgrade, needed before event handlers make sense.

/// One element, addressable by its child-index path from the document root.
#[derive(Clone, Default)]
pub struct ElementInfo {
    pub path: Vec<usize>,
    pub id: String,
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

/// A pending change to an element, applied by the engine after the script runs.
#[derive(Debug, Clone)]
pub enum Mutation {
    /// Replace children with a single text node.
    SetText(usize, String),
    /// Replace children with parsed HTML.
    SetHtml(usize, String),
}

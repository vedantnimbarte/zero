//! The DOM: a tree of nodes. Slice 1 supports elements and text only.

use std::collections::{HashMap, HashSet};

pub type AttrMap = HashMap<String, String>;

#[derive(Debug)]
pub struct Node {
    pub children: Vec<Node>,
    pub node_type: NodeType,
}

#[derive(Debug)]
pub enum NodeType {
    Element(ElementData),
    Text(String),
}

#[derive(Debug)]
pub struct ElementData {
    pub tag_name: String,
    pub attributes: AttrMap,
    /// Stable identity used by scripts and event dispatch. 0 means "not yet
    /// assigned"; the document numbers new nodes without renumbering old ones,
    /// so handlers registered earlier keep pointing at the same element.
    pub node_id: usize,
    /// Where this element sits among its siblings, for `:nth-child` and friends.
    pub pos: SiblingPos,
}

/// An element's 1-based position among its element siblings, counted both over
/// all of them and over the ones sharing its tag.
///
/// Kept on the node rather than passed down the matcher: a selector chain tests
/// ancestors too, and threading each one's position through would mean carrying
/// a parallel tree everywhere the matcher goes.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct SiblingPos {
    pub index: u32,
    pub count: u32,
    pub type_index: u32,
    pub type_count: u32,
}

/// Stamp every element with its position among its siblings.
///
/// Runs after parsing and after any DOM mutation — a script that appends a row
/// changes what `:last-child` means for the row before it.
pub fn stamp_positions(node: &mut Node) {
    let tags: Vec<String> = node
        .children
        .iter()
        .filter_map(|c| match &c.node_type {
            NodeType::Element(e) => Some(e.tag_name.clone()),
            NodeType::Text(_) => None,
        })
        .collect();
    let mut index = 0;
    let mut seen: HashMap<String, u32> = HashMap::new();
    for child in &mut node.children {
        if let NodeType::Element(ref mut e) = child.node_type {
            index += 1;
            let type_index = seen.entry(e.tag_name.clone()).or_default();
            *type_index += 1;
            e.pos = SiblingPos {
                index,
                count: tags.len() as u32,
                type_index: *type_index,
                type_count: tags.iter().filter(|t| **t == e.tag_name).count() as u32,
            };
        }
        stamp_positions(child);
    }
}

pub fn text(data: String) -> Node {
    Node {
        children: vec![],
        node_type: NodeType::Text(data),
    }
}

pub fn elem(name: String, attributes: AttrMap, children: Vec<Node>) -> Node {
    Node {
        children,
        node_type: NodeType::Element(ElementData {
            tag_name: name,
            attributes,
            node_id: 0,
            pos: SiblingPos::default(),
        }),
    }
}

impl ElementData {
    pub fn id(&self) -> Option<&String> {
        self.attributes.get("id")
    }

    pub fn classes(&self) -> HashSet<&str> {
        match self.attributes.get("class") {
            Some(classlist) => classlist.split(' ').filter(|c| !c.is_empty()).collect(),
            None => HashSet::new(),
        }
    }
}

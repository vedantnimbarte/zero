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
}

pub fn text(data: String) -> Node {
    Node { children: vec![], node_type: NodeType::Text(data) }
}

pub fn elem(name: String, attributes: AttrMap, children: Vec<Node>) -> Node {
    Node { children, node_type: NodeType::Element(ElementData { tag_name: name, attributes }) }
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

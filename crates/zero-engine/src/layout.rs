//! Layout: turn the styled tree into positioned boxes (the box model).
//!
//! Block layout + basic inline text layout (word wrapping). Text nodes become
//! positioned [`TextFragment`]s the painter draws.
//!
//! Words are shaped (see [`crate::text`]) so complex scripts render correctly.
//!
//! ponytail: inline layout is word-granular with a naive `size * 1.25` line height;
//! no bidi, justification, or mixed-baseline runs. Proper line-breaking is a later
//! phase (docs/01-ARCHITECTURE.md §10).

use crate::css::{Color, Unit, Value};
use crate::dom::NodeType;
use crate::resource::ImageMap;
use crate::style::{Display, StyledNode};
use crate::text::{shape_run, Fonts, PositionedGlyph};

/// A run of text (one word) placed at an absolute position, ready to paint.
/// Holds shaped glyphs, not characters — see [`crate::text`].
#[derive(Clone)]
pub struct TextFragment {
    pub glyphs: Vec<PositionedGlyph>,
    pub x: f32,
    /// Top of the line box (baseline is derived at paint time from font ascent).
    pub y: f32,
    pub size: f32,
    pub color: Color,
}

/// A clickable region for an `<a href>`, in absolute page coordinates.
#[derive(Clone)]
pub struct LinkArea {
    pub href: String,
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

#[derive(Clone, Copy, Default)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

#[derive(Clone, Copy, Default)]
pub struct EdgeSizes {
    pub left: f32,
    pub right: f32,
    pub top: f32,
    pub bottom: f32,
}

#[derive(Clone, Copy, Default)]
pub struct Dimensions {
    pub content: Rect,
    pub padding: EdgeSizes,
    pub border: EdgeSizes,
    pub margin: EdgeSizes,
}

impl Rect {
    fn expanded_by(self, edge: EdgeSizes) -> Rect {
        Rect {
            x: self.x - edge.left,
            y: self.y - edge.top,
            width: self.width + edge.left + edge.right,
            height: self.height + edge.top + edge.bottom,
        }
    }
}

impl Dimensions {
    pub fn padding_box(self) -> Rect {
        self.content.expanded_by(self.padding)
    }
    pub fn border_box(self) -> Rect {
        self.padding_box().expanded_by(self.border)
    }
    pub fn margin_box(self) -> Rect {
        self.border_box().expanded_by(self.margin)
    }
}

pub struct LayoutBox<'a> {
    pub dimensions: Dimensions,
    pub box_type: BoxType<'a>,
    pub children: Vec<LayoutBox<'a>>,
    /// Text placed by inline layout (only non-empty on anonymous/inline boxes).
    pub text_fragments: Vec<TextFragment>,
    /// Clickable link regions produced by inline layout.
    pub link_areas: Vec<LinkArea>,
}

pub enum BoxType<'a> {
    BlockNode(&'a StyledNode<'a>),
    InlineNode(&'a StyledNode<'a>),
    AnonymousBlock,
}

impl<'a> LayoutBox<'a> {
    fn new(box_type: BoxType<'a>) -> LayoutBox<'a> {
        LayoutBox {
            box_type,
            dimensions: Default::default(),
            children: Vec::new(),
            text_fragments: Vec::new(),
            link_areas: Vec::new(),
        }
    }

    fn get_style_node(&self) -> &'a StyledNode<'a> {
        match self.box_type {
            BoxType::BlockNode(node) | BoxType::InlineNode(node) => node,
            BoxType::AnonymousBlock => panic!("Anonymous block box has no style node"),
        }
    }

    fn layout(&mut self, containing_block: Dimensions, fonts: Option<&Fonts>, images: &ImageMap) {
        match self.box_type {
            BoxType::BlockNode(_) => self.layout_block(containing_block, fonts, images),
            BoxType::AnonymousBlock => self.layout_inline(containing_block, fonts),
            // A bare inline node is laid out by its anonymous-block parent, not here.
            BoxType::InlineNode(_) => {}
        }
    }

    fn layout_block(&mut self, containing_block: Dimensions, fonts: Option<&Fonts>, images: &ImageMap) {
        // Width depends on the parent, so it's computed top-down first.
        self.calculate_block_width(containing_block);
        self.calculate_block_position(containing_block);
        // Then children are laid out to discover this box's height.
        self.layout_block_children(fonts, images);
        self.calculate_block_height();
        // A replaced element (<img>) overrides content size with its resolved dimensions.
        if let Some((w, h)) = self.resolved_image_size(images) {
            self.dimensions.content.width = w;
            self.dimensions.content.height = h;
        }
    }

    /// If this box is an `<img>`, resolve its display size from CSS width/height,
    /// else the `width`/`height` attributes, else the image's intrinsic size.
    fn resolved_image_size(&self, images: &ImageMap) -> Option<(f32, f32)> {
        let styled = match self.box_type {
            BoxType::BlockNode(n) | BoxType::InlineNode(n) => n,
            BoxType::AnonymousBlock => return None,
        };
        let elem = match styled.node.node_type {
            NodeType::Element(ref e) => e,
            _ => return None,
        };
        if elem.tag_name != "img" {
            return None;
        }
        let src = elem.attributes.get("src")?;
        let img = images.get(src);
        let css_px = |name: &str| styled.value(name).map(|v| v.to_px()).filter(|v| *v > 0.0);
        let attr_px = |name: &str| elem.attributes.get(name).and_then(|s| s.trim().parse::<f32>().ok());

        let w = css_px("width").or_else(|| attr_px("width")).or_else(|| img.map(|i| i.width as f32))?;
        let h = css_px("height")
            .or_else(|| attr_px("height"))
            .or_else(|| img.map(|i| i.height as f32))
            .unwrap_or(w);
        Some((w, h))
    }

    /// Lay out inline children (text) as wrapped lines, producing text fragments
    /// and this box's height. ponytail: word-level wrapping, no per-glyph breaking.
    fn layout_inline(&mut self, containing_block: Dimensions, fonts: Option<&Fonts>) {
        let start_x = containing_block.content.x;
        let max_width = containing_block.content.width;
        let top = containing_block.content.height + containing_block.content.y;

        self.dimensions.content.x = start_x;
        self.dimensions.content.y = top;
        self.dimensions.content.width = max_width;

        let fonts = match fonts {
            Some(f) => f,
            None => {
                self.dimensions.content.height = 0.0;
                return;
            }
        };

        let default_size = 16.0_f32;
        let mut cursor_x = start_x;
        let mut cursor_y = top;
        let mut line_height = 0.0_f32; // tallest word on the current line
        let mut fragments: Vec<TextFragment> = Vec::new();
        let mut link_areas: Vec<LinkArea> = Vec::new();

        // Flatten the inline subtree (text, plus <a href>/<span>/... wrappers) into pieces,
        // carrying the nearest ancestor link's href with each piece.
        let mut pieces: Vec<TextPiece> = Vec::new();
        for child in &self.children {
            collect_inline_text(child, default_size, None, &mut pieces);
        }

        for piece in &pieces {
            let word_height = piece.size * 1.25;
            let (_, space_w) = shape_run(fonts, " ", piece.size);

            for word in piece.text.split_whitespace() {
                // Shape the whole word: this is where Indic reordering/conjuncts happen.
                let (glyphs, word_w) = shape_run(fonts, word, piece.size);
                // Wrap if this word overflows and we're not at line start.
                if cursor_x > start_x && cursor_x + word_w > start_x + max_width {
                    cursor_y += line_height;
                    cursor_x = start_x;
                    line_height = 0.0;
                }
                line_height = line_height.max(word_height);
                fragments.push(TextFragment {
                    glyphs,
                    x: cursor_x,
                    y: cursor_y,
                    size: piece.size,
                    color: piece.color,
                });
                if let Some(href) = &piece.href {
                    link_areas.push(LinkArea {
                        href: href.clone(),
                        x: cursor_x,
                        y: cursor_y,
                        width: word_w,
                        height: word_height,
                    });
                }
                cursor_x += word_w + space_w;
            }
        }

        self.dimensions.content.height =
            if fragments.is_empty() { 0.0 } else { (cursor_y - top) + line_height };
        self.text_fragments = fragments;
        self.link_areas = link_areas;
    }

    fn calculate_block_width(&mut self, containing_block: Dimensions) {
        let style = self.get_style_node();
        let auto = Value::Keyword("auto".to_string());
        let zero = Value::Length(0.0, Unit::Px);

        let mut width = style.value("width").unwrap_or_else(|| auto.clone());
        let mut margin_left = style.lookup("margin-left", "margin", &zero);
        let mut margin_right = style.lookup("margin-right", "margin", &zero);
        let border_left = style.lookup("border-left-width", "border-width", &zero);
        let border_right = style.lookup("border-right-width", "border-width", &zero);
        let padding_left = style.lookup("padding-left", "padding", &zero);
        let padding_right = style.lookup("padding-right", "padding", &zero);

        let total: f32 = [
            &margin_left, &margin_right, &border_left, &border_right, &padding_left,
            &padding_right, &width,
        ]
        .iter()
        .map(|v| v.to_px())
        .sum();

        // If the box is too wide, auto margins collapse to zero.
        if width != auto && total > containing_block.content.width {
            if margin_left == auto {
                margin_left = zero.clone();
            }
            if margin_right == auto {
                margin_right = zero.clone();
            }
        }

        let underflow = containing_block.content.width - total;
        match (width == auto, margin_left == auto, margin_right == auto) {
            // Over-constrained: adjust the right margin.
            (false, false, false) => {
                margin_right = Value::Length(margin_right.to_px() + underflow, Unit::Px);
            }
            (false, false, true) => margin_right = Value::Length(underflow, Unit::Px),
            (false, true, false) => margin_left = Value::Length(underflow, Unit::Px),
            // width is auto: it absorbs the slack.
            (true, _, _) => {
                if margin_left == auto {
                    margin_left = zero.clone();
                }
                if margin_right == auto {
                    margin_right = zero.clone();
                }
                if underflow >= 0.0 {
                    width = Value::Length(underflow, Unit::Px);
                } else {
                    width = zero.clone();
                    margin_right = Value::Length(margin_right.to_px() + underflow, Unit::Px);
                }
            }
            // Both margins auto: center the box.
            (false, true, true) => {
                margin_left = Value::Length(underflow / 2.0, Unit::Px);
                margin_right = Value::Length(underflow / 2.0, Unit::Px);
            }
        }

        let d = &mut self.dimensions;
        d.content.width = width.to_px();
        d.padding.left = padding_left.to_px();
        d.padding.right = padding_right.to_px();
        d.border.left = border_left.to_px();
        d.border.right = border_right.to_px();
        d.margin.left = margin_left.to_px();
        d.margin.right = margin_right.to_px();
    }

    fn calculate_block_position(&mut self, containing_block: Dimensions) {
        let style = self.get_style_node();
        let zero = Value::Length(0.0, Unit::Px);

        let d = &mut self.dimensions;
        d.margin.top = style.lookup("margin-top", "margin", &zero).to_px();
        d.margin.bottom = style.lookup("margin-bottom", "margin", &zero).to_px();
        d.border.top = style.lookup("border-top-width", "border-width", &zero).to_px();
        d.border.bottom = style.lookup("border-bottom-width", "border-width", &zero).to_px();
        d.padding.top = style.lookup("padding-top", "padding", &zero).to_px();
        d.padding.bottom = style.lookup("padding-bottom", "padding", &zero).to_px();

        d.content.x = containing_block.content.x + d.margin.left + d.border.left + d.padding.left;
        // Stack below the content already placed in the containing block.
        d.content.y = containing_block.content.height
            + containing_block.content.y
            + d.margin.top
            + d.border.top
            + d.padding.top;
    }

    fn layout_block_children(&mut self, fonts: Option<&Fonts>, images: &ImageMap) {
        let d = &mut self.dimensions;
        for child in &mut self.children {
            child.layout(*d, fonts, images);
            // Grow this box to contain each child's margin box.
            d.content.height += child.dimensions.margin_box().height;
        }
    }

    fn calculate_block_height(&mut self) {
        // An explicit height overrides the content-derived height.
        if let Some(Value::Length(h, Unit::Px)) = self.get_style_node().value("height") {
            self.dimensions.content.height = h;
        }
    }

    fn get_inline_container(&mut self) -> &mut LayoutBox<'a> {
        match self.box_type {
            BoxType::InlineNode(_) | BoxType::AnonymousBlock => self,
            BoxType::BlockNode(_) => {
                // Inline children of a block box go in an anonymous block wrapper.
                let needs_new = !matches!(
                    self.children.last().map(|c| &c.box_type),
                    Some(BoxType::AnonymousBlock)
                );
                if needs_new {
                    self.children.push(LayoutBox::new(BoxType::AnonymousBlock));
                }
                self.children.last_mut().unwrap()
            }
        }
    }
}

pub fn layout_tree<'a>(
    node: &'a StyledNode<'a>,
    mut containing_block: Dimensions,
    fonts: Option<&Fonts>,
    images: &ImageMap,
) -> LayoutBox<'a> {
    // Height starts at 0 so children accumulate into it.
    containing_block.content.height = 0.0;
    let mut root_box = build_layout_tree(node);
    root_box.layout(containing_block, fonts, images);
    root_box
}

struct TextPiece {
    text: String,
    size: f32,
    color: Color,
    href: Option<String>,
}

/// Walk an inline box subtree, collecting each text node with its (inherited)
/// font size, color, and nearest ancestor `<a href>`. This is what makes
/// `<a>`/`<span>` text render and become clickable.
fn collect_inline_text(bx: &LayoutBox, default_size: f32, href: Option<&str>, out: &mut Vec<TextPiece>) {
    // If this inline box is an <a href>, it becomes the link context for its subtree.
    let mut current_href = href.map(str::to_string);
    if let BoxType::InlineNode(styled) = bx.box_type {
        if let NodeType::Element(ref e) = styled.node.node_type {
            if e.tag_name == "a" {
                if let Some(h) = e.attributes.get("href") {
                    current_href = Some(h.clone());
                }
            }
        }
        if let NodeType::Text(ref t) = styled.node.node_type {
            let size = styled
                .value("font-size")
                .map(|v| v.to_px())
                .filter(|s| *s > 0.0)
                .unwrap_or(default_size);
            let color = match styled.value("color") {
                Some(Value::ColorValue(c)) => c,
                _ => Color { r: 0, g: 0, b: 0, a: 255 },
            };
            out.push(TextPiece { text: t.clone(), size, color, href: current_href.clone() });
        }
    }
    for child in &bx.children {
        collect_inline_text(child, default_size, current_href.as_deref(), out);
    }
}

/// Gather every clickable link region from the laid-out tree (absolute coords).
pub fn collect_links(bx: &LayoutBox, out: &mut Vec<LinkArea>) {
    out.extend(bx.link_areas.iter().cloned());
    for child in &bx.children {
        collect_links(child, out);
    }
}

fn build_layout_tree<'a>(style_node: &'a StyledNode<'a>) -> LayoutBox<'a> {
    let mut root = LayoutBox::new(match style_node.display() {
        Display::Block => BoxType::BlockNode(style_node),
        Display::Inline => BoxType::InlineNode(style_node),
        Display::None => panic!("Root node has display: none."),
    });

    for child in &style_node.children {
        match child.display() {
            Display::Block => root.children.push(build_layout_tree(child)),
            Display::Inline => root.get_inline_container().children.push(build_layout_tree(child)),
            Display::None => {} // skip
        }
    }
    root
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::css::{Unit, Value};
    use crate::dom;
    use std::collections::HashMap;

    #[test]
    fn explicit_block_width_is_respected() {
        let node = dom::elem("div".into(), HashMap::new(), vec![]);
        let mut values = HashMap::new();
        values.insert("display".to_string(), Value::Keyword("block".into()));
        values.insert("width".to_string(), Value::Length(200.0, Unit::Px));
        let styled = StyledNode { node: &node, specified_values: values, children: vec![] };

        let mut viewport: Dimensions = Default::default();
        viewport.content.width = 800.0;

        let root = layout_tree(&styled, viewport, None, &ImageMap::new());
        assert_eq!(root.dimensions.content.width, 200.0);
    }

    #[test]
    fn auto_width_fills_containing_block() {
        let node = dom::elem("div".into(), HashMap::new(), vec![]);
        let mut values = HashMap::new();
        values.insert("display".to_string(), Value::Keyword("block".into()));
        let styled = StyledNode { node: &node, specified_values: values, children: vec![] };

        let mut viewport: Dimensions = Default::default();
        viewport.content.width = 800.0;

        let root = layout_tree(&styled, viewport, None, &ImageMap::new());
        assert_eq!(root.dimensions.content.width, 800.0);
    }
}

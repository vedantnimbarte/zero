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

use crate::css::{Color, LengthContext, Unit, Value};
use crate::dom::NodeType;
use crate::resource::ImageMap;
use crate::style::{Display, StyledNode};
use crate::text::{shape_run, FontSet, PositionedGlyph};

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
    /// Which font in the [`FontSet`] shaped this run (fallback picks per word).
    pub font_index: usize,
}

/// The painted area of an element, for hit-testing clicks against scripts.
#[derive(Clone)]
pub struct ElementRect {
    pub node_id: usize,
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
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

    /// True when this box is taken out of normal flow (`absolute` or `fixed`).
    fn is_out_of_flow(&self) -> bool {
        match self.box_type {
            BoxType::AnonymousBlock => false,
            _ => matches!(
                self.get_style_node().value("position"),
                Some(Value::Keyword(ref k)) if k == "absolute" || k == "fixed"
            ),
        }
    }

    /// Where an out-of-flow box's margin box should start, from `top`/`left`/
    /// `right`/`bottom`. Requires the box to have been measured already, because
    /// `right`/`bottom` depend on its own size.
    fn positioned_origin(&self, containing_block: Dimensions) -> (f32, f32) {
        let style = self.get_style_node();
        let cb = containing_block.content;
        let ctx_x = style.length_context(cb.width);
        let ctx_y = style.length_context(cb.height);
        let outer = self.dimensions.margin_box();

        let x = match (style.value("left"), style.value("right")) {
            (Some(left), _) => cb.x + left.resolve(ctx_x),
            (None, Some(right)) => cb.x + cb.width - right.resolve(ctx_x) - outer.width,
            _ => outer.x,
        };
        let y = match (style.value("top"), style.value("bottom")) {
            (Some(top), _) => cb.y + top.resolve(ctx_y),
            (None, Some(bottom)) => cb.y + cb.height - bottom.resolve(ctx_y) - outer.height,
            _ => outer.y,
        };
        (x, y)
    }

    fn layout(&mut self, containing_block: Dimensions, fonts: Option<&FontSet>, images: &ImageMap) {
        match self.box_type {
            BoxType::BlockNode(_) => self.layout_block(containing_block, fonts, images),
            BoxType::AnonymousBlock => self.layout_inline(containing_block, fonts),
            // A bare inline node is laid out by its anonymous-block parent, not here.
            BoxType::InlineNode(_) => {}
        }
    }

    fn layout_block(&mut self, containing_block: Dimensions, fonts: Option<&FontSet>, images: &ImageMap) {
        // Width depends on the parent, so it's computed top-down first.
        self.calculate_block_width(containing_block);
        self.calculate_block_position(containing_block);
        // Then children are laid out to discover this box's height.
        match self.get_style_node().display() {
            Display::Flex => self.layout_flex_children(fonts, images),
            Display::Grid => self.layout_grid_children(fonts, images),
            _ => self.layout_block_children(fonts, images),
        }
        self.calculate_block_height(containing_block);
        // A replaced element (<img>) overrides content size with its resolved dimensions.
        if let Some((w, h)) = self.resolved_image_size(images) {
            self.dimensions.content.width = w;
            self.dimensions.content.height = h;
        }
        // Positioned children resolve against this box's *final* size, so they run
        // last — `bottom`/`right` are meaningless until the height/width are known.
        self.layout_positioned_children(fonts, images);
    }

    fn layout_positioned_children(&mut self, fonts: Option<&FontSet>, images: &ImageMap) {
        let container = self.dimensions;
        for child in &mut self.children {
            if !child.is_out_of_flow() {
                continue;
            }
            // Pass 1 measures the box so `right`/`bottom` can be resolved.
            child.layout(container, fonts, images);
            let (x, y) = child.positioned_origin(container);
            // Pass 2 lays the whole subtree out at its final origin, so descendants
            // and text land in the right place instead of being moved afterwards.
            let mut slot = container;
            slot.content.x = x;
            slot.content.y = y;
            slot.content.height = 0.0;
            child.layout(slot, fonts, images);
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
        let css_px = |name: &str| styled.px(name, 0.0).filter(|v| *v > 0.0);
        let attr_px = |name: &str| elem.attributes.get(name).and_then(|s| s.trim().parse::<f32>().ok());

        let w = css_px("width").or_else(|| attr_px("width")).or_else(|| img.map(|i| i.width as f32))?;
        let h = css_px("height")
            .or_else(|| attr_px("height"))
            .or_else(|| img.map(|i| i.height as f32))
            .unwrap_or(w);
        Some((w, h))
    }

    /// Grid layout: place items into a track grid, honouring explicit placement.
    ///
    /// ponytail: no named lines, `grid-area`, `auto-fit/minmax`, or alignment.
    /// Rows beyond `grid-template-rows` are sized to their tallest item.
    fn layout_grid_children(&mut self, fonts: Option<&FontSet>, images: &ImageMap) {
        let style = self.get_style_node();
        let container = self.dimensions.content;
        let ctx = style.length_context(container.width);
        let gap = style.value("gap").map(|v| v.resolve(ctx)).unwrap_or(0.0);

        let spec = match style.value("grid-template-columns") {
            Some(Value::Raw(spec)) => spec,
            _ => return self.layout_block_children_gapped(fonts, images, gap),
        };
        let columns = resolve_tracks(&spec, container.width, gap, ctx);
        if columns.is_empty() {
            return self.layout_block_children_gapped(fonts, images, gap);
        }
        // Explicit row sizes, if any; extra rows fall back to content height.
        let row_sizes = match style.value("grid-template-rows") {
            Some(Value::Raw(spec)) => resolve_tracks(&spec, container.height, gap, ctx),
            _ => Vec::new(),
        };

        // Assign every in-flow item a (row, column, span), honouring `grid-column`.
        let mut placements: Vec<(usize, usize, usize, usize)> = Vec::new(); // (index,row,col,span)
        let mut occupied: Vec<Vec<bool>> = Vec::new();
        let (mut row, mut col) = (0usize, 0usize);

        for index in 0..self.children.len() {
            if self.children[index].is_out_of_flow() {
                continue;
            }
            let (explicit_col, span) = match self.children[index].box_type {
                BoxType::AnonymousBlock => (None, 1),
                _ => parse_grid_span(self.children[index].get_style_node(), columns.len()),
            };
            let span = span.clamp(1, columns.len());

            // Find the next free slot that fits the span.
            loop {
                while occupied.len() <= row {
                    occupied.push(vec![false; columns.len()]);
                }
                let start = explicit_col.unwrap_or(col);
                let fits = start + span <= columns.len()
                    && (start..start + span).all(|c| !occupied[row][c]);
                if fits {
                    for c in start..start + span {
                        occupied[row][c] = true;
                    }
                    placements.push((index, row, start, span));
                    col = if explicit_col.is_some() { col } else { start + span };
                    if col >= columns.len() {
                        row += 1;
                        col = 0;
                    }
                    break;
                }
                // Slot taken or item too wide for the remainder: try the next row.
                row += 1;
                col = 0;
                if explicit_col.is_none() && span > columns.len() {
                    break; // cannot ever fit
                }
            }
        }

        // Lay each item out in its cell, then size rows to their tallest member.
        let mut row_heights: Vec<f32> = vec![0.0; occupied.len()];
        for &(index, r, c, span) in &placements {
            let width = columns[c..c + span].iter().sum::<f32>() + gap * (span - 1) as f32;
            let x = container.x + columns[..c].iter().sum::<f32>() + gap * c as f32;
            let mut slot: Dimensions = Default::default();
            slot.content = Rect { x, y: container.y, width, height: 0.0 };
            self.children[index].layout(slot, fonts, images);
            let h = self.children[index].dimensions.margin_box().height;
            if r < row_heights.len() {
                row_heights[r] = row_heights[r].max(h);
            }
        }
        for (r, height) in row_heights.iter_mut().enumerate() {
            if let Some(explicit) = row_sizes.get(r) {
                if *explicit > 0.0 {
                    *height = *explicit;
                }
            }
        }

        // Re-run each item now that its row's y position is known.
        for &(index, r, c, span) in &placements {
            let y = container.y
                + row_heights[..r].iter().sum::<f32>()
                + gap * r as f32;
            let width = columns[c..c + span].iter().sum::<f32>() + gap * (span - 1) as f32;
            let x = container.x + columns[..c].iter().sum::<f32>() + gap * c as f32;
            let mut slot: Dimensions = Default::default();
            slot.content = Rect { x, y, width, height: 0.0 };
            self.children[index].layout(slot, fonts, images);
            if let Some(explicit) = row_sizes.get(r) {
                if *explicit > 0.0 {
                    self.children[index].dimensions.content.height = *explicit
                        - self.children[index].dimensions.padding.top
                        - self.children[index].dimensions.padding.bottom;
                }
            }
        }

        let rows = row_heights.len();
        self.dimensions.content.height =
            row_heights.iter().sum::<f32>() + gap * rows.saturating_sub(1) as f32;
    }

    /// Lay out inline children (text) as wrapped lines, producing text fragments
    /// and this box's height. ponytail: word-level wrapping, no per-glyph breaking.
    fn layout_inline(&mut self, containing_block: Dimensions, fonts: Option<&FontSet>) {
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
            let (_, space_w) = shape_run(&fonts.entries[0], " ", piece.size);

            for word in piece.text.split_whitespace() {
                // Pick a font that can draw this word, then shape it: this is where
                // Indic reordering/conjuncts happen.
                let font_index = fonts.pick(word);
                let (glyphs, word_w) = shape_run(&fonts.entries[font_index], word, piece.size);
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
                    font_index,
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
        // An out-of-flow box is placed explicitly, so it must not absorb the
        // container's leftover space into its margins the way in-flow blocks do.
        let out_of_flow = self.is_out_of_flow();
        let style = self.get_style_node();
        let auto = Value::Keyword("auto".to_string());
        let zero = Value::Length(0.0, Unit::Px);
        // Percentages in the inline axis resolve against the containing block's width.
        let ctx = style.length_context(containing_block.content.width);

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
        .map(|v| v.resolve(ctx))
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
            // Over-constrained: adjust the right margin (in-flow boxes only).
            (false, false, false) => {
                if !out_of_flow {
                    margin_right = Value::Length(margin_right.resolve(ctx) + underflow, Unit::Px);
                }
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
                    margin_right = Value::Length(margin_right.resolve(ctx) + underflow, Unit::Px);
                }
            }
            // Both margins auto: center the box.
            (false, true, true) => {
                margin_left = Value::Length(underflow / 2.0, Unit::Px);
                margin_right = Value::Length(underflow / 2.0, Unit::Px);
            }
        }

        let d = &mut self.dimensions;
        d.content.width = width.resolve(ctx);
        d.padding.left = padding_left.resolve(ctx);
        d.padding.right = padding_right.resolve(ctx);
        d.border.left = border_left.resolve(ctx);
        d.border.right = border_right.resolve(ctx);
        d.margin.left = margin_left.resolve(ctx);
        d.margin.right = margin_right.resolve(ctx);
    }

    fn calculate_block_position(&mut self, containing_block: Dimensions) {
        let style = self.get_style_node();
        let zero = Value::Length(0.0, Unit::Px);
        // Per CSS, vertical padding/margin percentages also resolve against WIDTH.
        let ctx = style.length_context(containing_block.content.width);

        let d = &mut self.dimensions;
        d.margin.top = style.lookup("margin-top", "margin", &zero).resolve(ctx);
        d.margin.bottom = style.lookup("margin-bottom", "margin", &zero).resolve(ctx);
        d.border.top = style.lookup("border-top-width", "border-width", &zero).resolve(ctx);
        d.border.bottom = style.lookup("border-bottom-width", "border-width", &zero).resolve(ctx);
        d.padding.top = style.lookup("padding-top", "padding", &zero).resolve(ctx);
        d.padding.bottom = style.lookup("padding-bottom", "padding", &zero).resolve(ctx);

        d.content.x = containing_block.content.x + d.margin.left + d.border.left + d.padding.left;
        // Stack below the content already placed in the containing block.
        d.content.y = containing_block.content.height
            + containing_block.content.y
            + d.margin.top
            + d.border.top
            + d.padding.top;
    }

    fn layout_block_children(&mut self, fonts: Option<&FontSet>, images: &ImageMap) {
        self.layout_block_children_gapped(fonts, images, 0.0);
    }

    fn layout_block_children_gapped(
        &mut self,
        fonts: Option<&FontSet>,
        images: &ImageMap,
        gap: f32,
    ) {
        let count = self.children.len();
        let d = &mut self.dimensions;
        for (i, child) in self.children.iter_mut().enumerate() {
            if child.is_out_of_flow() {
                continue; // positioned later, once the container's size is final
            }
            child.layout(*d, fonts, images);
            // Grow this box to contain each child's margin box.
            d.content.height += child.dimensions.margin_box().height;
            if gap > 0.0 && i + 1 < count {
                d.content.height += gap;
            }
        }
    }

    /// Single-line flex layout.
    ///
    /// ponytail: no wrapping, no `flex-grow/shrink/basis`, no `justify-content` or
    /// `align-items`, and no intrinsic content sizing. Items with an explicit main
    /// size keep it; the rest split the leftover space equally (which is what
    /// `flex: 1` does, and covers most real column/nav layouts).
    fn layout_flex_children(&mut self, fonts: Option<&FontSet>, images: &ImageMap) {
        let style = self.get_style_node();
        let is_column = matches!(style.value("flex-direction"), Some(Value::Keyword(ref k)) if k == "column");
        let gap = style.value("gap").map(|v| v.to_px()).filter(|g| *g > 0.0).unwrap_or(0.0);

        if is_column {
            // A column flex container stacks like a block, just with gaps.
            return self.layout_block_children_gapped(fonts, images, gap);
        }

        let container = self.dimensions.content;
        let count = self.children.len();
        if count == 0 {
            return;
        }

        // Each item starts at its base size (explicit width, else content width).
        struct Item {
            base: f32,
            grow: f32,
        }
        let items: Vec<Item> = self
            .children
            .iter()
            .map(|child| match child.box_type {
                BoxType::AnonymousBlock => Item { base: 0.0, grow: 1.0 },
                _ => {
                    let style = child.get_style_node();
                    let ctx = style.length_context(container.width);
                    // Base is the OUTER width, so gaps and free space line up with
                    // what layout actually produces (a border box, not content).
                    let base = match style.value("width") {
                        Some(v @ Value::Length(..)) => v.resolve(ctx) + horizontal_edges(style, ctx),
                        _ => max_content_width(style, fonts, images).min(container.width),
                    };
                    // `flex: 1` and `flex-grow: 1` both mean "take a share".
                    let grow = style
                        .value("flex-grow")
                        .or_else(|| style.value("flex"))
                        .and_then(|v| v.as_number())
                        .unwrap_or(0.0);
                    Item { base, grow }
                }
            })
            .collect();

        let wrap = matches!(style.value("flex-wrap"), Some(Value::Keyword(ref k)) if k.starts_with("wrap"));
        let justify = style.value("justify-content").and_then(keyword_of).unwrap_or_default();
        let align = style.value("align-items").and_then(keyword_of).unwrap_or_default();

        // Group items into lines. Without wrapping everything shares one line.
        let flow: Vec<usize> =
            (0..count).filter(|&i| !self.children[i].is_out_of_flow()).collect();
        let mut lines: Vec<Vec<usize>> = Vec::new();
        let mut current: Vec<usize> = Vec::new();
        let mut line_width = 0.0_f32;
        for &i in &flow {
            let next = line_width + items[i].base + if current.is_empty() { 0.0 } else { gap };
            if wrap && !current.is_empty() && next > container.width {
                lines.push(std::mem::take(&mut current));
                line_width = items[i].base;
            } else {
                line_width = next;
            }
            current.push(i);
        }
        if !current.is_empty() {
            lines.push(current);
        }

        let mut cursor_y = container.y;
        let mut total_height = 0.0_f32;

        for (line_index, line) in lines.iter().enumerate() {
            let total_gap = gap * line.len().saturating_sub(1) as f32;
            let used: f32 = line.iter().map(|&i| items[i].base).sum();
            let leftover = container.width - used - total_gap;
            let total_grow: f32 = line.iter().map(|&i| items[i].grow).sum();

            // Widths first, so justify-content knows how much space is really free.
            let widths: Vec<f32> = line
                .iter()
                .map(|&i| {
                    let mut w = items[i].base;
                    if leftover > 0.0 && total_grow > 0.0 {
                        w += leftover * items[i].grow / total_grow;
                    } else if leftover < 0.0 && used > 0.0 {
                        w += leftover * (items[i].base / used); // shrink proportionally
                    }
                    w.max(0.0)
                })
                .collect();

            let free = (container.width - widths.iter().sum::<f32>() - total_gap).max(0.0);
            let (offset, between) = distribute(&justify, free, line.len());

            let mut cursor_x = container.x + offset;
            let mut tallest = 0.0_f32;
            for (slot_index, &i) in line.iter().enumerate() {
                let mut slot: Dimensions = Default::default();
                slot.content =
                    Rect { x: cursor_x, y: cursor_y, width: widths[slot_index], height: 0.0 };
                self.children[i].layout(slot, fonts, images);
                let placed = self.children[i].dimensions.margin_box();
                cursor_x += placed.width + gap + between;
                tallest = tallest.max(placed.height);
            }

            // Cross-axis alignment happens once the line's height is known.
            for &i in line.iter() {
                align_cross_axis(&mut self.children[i], &align, cursor_y, tallest, fonts, images);
            }

            cursor_y += tallest;
            total_height += tallest;
            if line_index + 1 < lines.len() {
                cursor_y += gap;
                total_height += gap;
            }
        }
        self.dimensions.content.height = total_height;
    }

    fn calculate_block_height(&mut self, containing_block: Dimensions) {
        // An explicit height overrides the content-derived height.
        let style = self.get_style_node();
        let ctx = style.length_context(containing_block.content.height);
        if let Some(value) = style.value("height") {
            if matches!(value, Value::Length(..)) {
                self.dimensions.content.height = value.resolve(ctx);
            }
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
    fonts: Option<&FontSet>,
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
            let size = if styled.font_size() > 0.0 { styled.font_size() } else { default_size };
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

/// Horizontal margin + border + padding for an element, in px.
fn horizontal_edges(style: &StyledNode, ctx: LengthContext) -> f32 {
    let zero = Value::Length(0.0, Unit::Px);
    [
        ("margin-left", "margin"),
        ("margin-right", "margin"),
        ("border-left-width", "border-width"),
        ("border-right-width", "border-width"),
        ("padding-left", "padding"),
        ("padding-right", "padding"),
    ]
    .iter()
    .map(|(name, short)| style.lookup(name, short, &zero).resolve(ctx))
    .sum()
}

/// The keyword form of a value, if it is one.
fn keyword_of(value: Value) -> Option<String> {
    match value {
        Value::Keyword(k) => Some(k),
        _ => None,
    }
}

/// Leading offset and extra spacing between items for a `justify-content` mode.
fn distribute(mode: &str, free: f32, items: usize) -> (f32, f32) {
    if items == 0 || free <= 0.0 {
        return (0.0, 0.0);
    }
    match mode {
        "center" => (free / 2.0, 0.0),
        "flex-end" | "end" | "right" => (free, 0.0),
        "space-between" if items > 1 => (0.0, free / (items - 1) as f32),
        "space-around" => (free / (items * 2) as f32, free / items as f32),
        "space-evenly" => (free / (items + 1) as f32, free / (items + 1) as f32),
        _ => (0.0, 0.0), // flex-start, and space-* with a single item
    }
}

/// Place a flex item within its line on the cross axis.
///
/// ponytail: `stretch` just sets the box height rather than re-running layout,
/// so a stretched item's own children keep their original positions.
fn align_cross_axis(
    child: &mut LayoutBox,
    mode: &str,
    line_top: f32,
    line_height: f32,
    fonts: Option<&FontSet>,
    images: &ImageMap,
) {
    let outer = child.dimensions.margin_box();
    let slack = line_height - outer.height;
    match mode {
        "center" | "flex-end" | "end" if slack > 0.0 => {
            let shift = if mode == "center" { slack / 2.0 } else { slack };
            let mut slot: Dimensions = Default::default();
            slot.content = Rect {
                x: outer.x,
                y: line_top + shift,
                width: child.dimensions.content.width
                    + child.dimensions.padding.left
                    + child.dimensions.padding.right,
                height: 0.0,
            };
            // Re-run layout so descendants move with the box.
            let width = child.dimensions.content.width;
            slot.content.width = width;
            child.layout(slot, fonts, images);
        }
        // `stretch` is the default: fill the line's height.
        "flex-start" | "start" | "baseline" => {}
        _ if slack > 0.0 => child.dimensions.content.height += slack,
        _ => {}
    }
}

/// Read `grid-column` as (zero-based start, span).
///
/// Accepts `span N`, `A / B`, `A / span N`, and a bare line number `A`.
fn parse_grid_span(style: &StyledNode, columns: usize) -> (Option<usize>, usize) {
    let spec = match style.value("grid-column") {
        Some(Value::Raw(spec)) => spec,
        Some(Value::Number(n)) => return (Some((n as usize).saturating_sub(1)), 1),
        _ => return (None, 1),
    };
    let spec = spec.trim();
    if let Some(rest) = spec.strip_prefix("span ") {
        return (None, rest.trim().parse::<usize>().unwrap_or(1));
    }
    let mut parts = spec.split('/').map(str::trim);
    let start = parts.next().and_then(|p| p.parse::<usize>().ok());
    let span = match parts.next() {
        Some(end) if end.starts_with("span ") => {
            end[5..].trim().parse::<usize>().unwrap_or(1)
        }
        Some(end) => match (start, end.parse::<usize>().ok()) {
            (Some(a), Some(b)) if b > a => b - a,
            _ => 1,
        },
        None => 1,
    };
    let start = start.map(|line| line.saturating_sub(1)).filter(|s| *s < columns);
    (start, span.max(1))
}

/// Expand a `grid-template-columns` track list into concrete pixel widths.
///
/// Handles `repeat(n, <tracks>)`, lengths, and `fr` units, which share whatever
/// space the fixed tracks leave over.
pub fn resolve_tracks(spec: &str, available: f32, gap: f32, ctx: LengthContext) -> Vec<f32> {
    let mut tokens: Vec<String> = Vec::new();
    let mut rest = spec.trim();
    while !rest.is_empty() {
        // Flatten `repeat(count, pattern)` into plain tokens.
        if let Some(after) = rest.strip_prefix("repeat(") {
            if let Some(close) = after.find(')') {
                let (inside, tail) = after.split_at(close);
                let mut parts = inside.splitn(2, ',');
                let count = parts.next().unwrap_or("").trim().parse::<usize>().unwrap_or(0);
                let pattern: Vec<&str> = parts.next().unwrap_or("").split_whitespace().collect();
                for _ in 0..count.min(1000) {
                    tokens.extend(pattern.iter().map(|p| p.to_string()));
                }
                rest = tail[1..].trim_start();
                continue;
            }
        }
        match rest.split_once(char::is_whitespace) {
            Some((token, tail)) => {
                tokens.push(token.to_string());
                rest = tail.trim_start();
            }
            None => {
                tokens.push(rest.to_string());
                break;
            }
        }
    }

    // Fixed tracks take their size; `fr` tracks divide the remainder.
    let fractions: Vec<Option<f32>> = tokens
        .iter()
        .map(|t| t.strip_suffix("fr").and_then(|n| n.trim().parse::<f32>().ok()))
        .collect();
    let fixed: Vec<f32> = tokens
        .iter()
        .zip(&fractions)
        .map(|(t, fr)| if fr.is_some() { 0.0 } else { parse_track_length(t, ctx) })
        .collect();

    let total_gap = gap * tokens.len().saturating_sub(1) as f32;
    let free = (available - fixed.iter().sum::<f32>() - total_gap).max(0.0);
    let total_fr: f32 = fractions.iter().flatten().sum();

    (0..tokens.len())
        .map(|i| match fractions[i] {
            Some(fr) if total_fr > 0.0 => free * fr / total_fr,
            Some(_) => 0.0,
            None => fixed[i],
        })
        .collect()
}

fn parse_track_length(token: &str, ctx: LengthContext) -> f32 {
    for (suffix, unit) in
        [("px", Unit::Px), ("rem", Unit::Rem), ("em", Unit::Em), ("%", Unit::Percent)]
    {
        if let Some(n) = token.strip_suffix(suffix) {
            if let Ok(v) = n.trim().parse::<f32>() {
                return Value::Length(v, unit).resolve(ctx);
            }
        }
    }
    0.0
}

/// The widest a subtree wants to be if nothing wraps it (CSS `max-content`).
///
/// Needed for content-based sizing: a flex item with `width: auto` and no
/// `flex-grow` should be as wide as its content, not as wide as its container.
///
/// ponytail: measures text without wrapping and ignores min-content (longest word).
/// Good enough for shrink-to-fit; a full intrinsic pass would compute both.
pub fn max_content_width(
    style: &StyledNode,
    fonts: Option<&FontSet>,
    images: &ImageMap,
) -> f32 {
    if style.display() == Display::None {
        return 0.0;
    }
    let ctx = style.length_context(0.0);
    let edges = horizontal_edges(style, ctx);

    // An explicit, absolute width settles it.
    if let Some(Value::Length(w, Unit::Px)) = style.value("width") {
        return w + edges;
    }

    let content = match style.node.node_type {
        NodeType::Text(ref t) => measure_text(t, style.font_size(), fonts),
        NodeType::Element(ref e) if e.tag_name == "img" => e
            .attributes
            .get("src")
            .and_then(|src| images.get(src))
            .map(|img| img.width as f32)
            .unwrap_or(0.0),
        NodeType::Element(_) => {
            let row_flex = style.display() == Display::Flex
                && !matches!(style.value("flex-direction"), Some(Value::Keyword(ref k)) if k == "column");
            let gap = style.value("gap").map(|v| v.resolve(ctx)).unwrap_or(0.0);

            // Block children stack, so take the widest. Inline children share a
            // line, so they add up. A flex row also adds up.
            let mut widest: f32 = 0.0;
            let mut inline_run = 0.0;
            let mut flex_total = 0.0;
            let mut items: usize = 0;
            for child in &style.children {
                let w = max_content_width(child, fonts, images);
                if row_flex {
                    flex_total += w;
                    items += 1;
                } else if child.display() == Display::Inline {
                    inline_run += w;
                } else {
                    widest = widest.max(w);
                }
            }
            if row_flex {
                flex_total + gap * items.saturating_sub(1) as f32
            } else {
                widest.max(inline_run)
            }
        }
    };
    content + edges
}

fn measure_text(text: &str, size: f32, fonts: Option<&FontSet>) -> f32 {
    let fonts = match fonts {
        Some(f) => f,
        None => return 0.0,
    };
    let trimmed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if trimmed.is_empty() {
        return 0.0;
    }
    shape_run(&fonts.entries[fonts.pick(&trimmed)], &trimmed, size).1
}

/// Gather the painted box of every element, so the embedder can hit-test clicks.
pub fn collect_element_rects(bx: &LayoutBox, out: &mut Vec<ElementRect>) {
    if let BoxType::BlockNode(styled) | BoxType::InlineNode(styled) = bx.box_type {
        if let NodeType::Element(ref e) = styled.node.node_type {
            let b = bx.dimensions.border_box();
            out.push(ElementRect {
                node_id: e.node_id,
                x: b.x,
                y: b.y,
                width: b.width,
                height: b.height,
            });
        }
    }
    for child in &bx.children {
        collect_element_rects(child, out);
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
    build_box(style_node, false)
}

/// `force_block` blockifies a node because its parent is a flex container —
/// flex items are always block-level, whatever their own `display` says.
fn build_box<'a>(style_node: &'a StyledNode<'a>, force_block: bool) -> LayoutBox<'a> {
    let display = style_node.display();
    let mut root = LayoutBox::new(match display {
        Display::Block | Display::Flex | Display::Grid => BoxType::BlockNode(style_node),
        Display::Inline if force_block => BoxType::BlockNode(style_node),
        Display::Inline => BoxType::InlineNode(style_node),
        Display::None => panic!("Root node has display: none."),
    });

    // Flex and grid items are always block-level, whatever their own display says.
    let blockifies = matches!(display, Display::Flex | Display::Grid);
    for child in &style_node.children {
        match child.display() {
            Display::None => {} // skip
            Display::Block | Display::Flex | Display::Grid => {
                root.children.push(build_box(child, false))
            }
            Display::Inline if blockifies => root.children.push(build_box(child, true)),
            Display::Inline => root.get_inline_container().children.push(build_box(child, false)),
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

    /// Build a styled node with the given `display`/`width`/`flex-grow`, plus children.
    fn styled<'a>(
        node: &'a dom::Node,
        display: &str,
        width: Option<f32>,
        grow: Option<f32>,
        children: Vec<StyledNode<'a>>,
    ) -> StyledNode<'a> {
        let mut values = HashMap::new();
        values.insert("display".to_string(), Value::Keyword(display.into()));
        if let Some(w) = width {
            values.insert("width".to_string(), Value::Length(w, Unit::Px));
        }
        if let Some(g) = grow {
            values.insert("flex-grow".to_string(), Value::Number(g));
        }
        StyledNode { node, specified_values: values, children }
    }

    #[test]
    fn flex_grow_shares_leftover_space_proportionally() {
        let node = dom::elem("div".into(), HashMap::new(), vec![]);
        // 900 wide: fixed 300px, then grow 1 and grow 2 split the remaining 600.
        let root = styled(
            &node,
            "flex",
            None,
            None,
            vec![
                styled(&node, "block", Some(300.0), None, vec![]),
                styled(&node, "block", None, Some(1.0), vec![]),
                styled(&node, "block", None, Some(2.0), vec![]),
            ],
        );
        let mut viewport: Dimensions = Default::default();
        viewport.content.width = 900.0;

        let laid = layout_tree(&root, viewport, None, &ImageMap::new());
        let widths: Vec<f32> = laid.children.iter().map(|c| c.dimensions.content.width).collect();
        assert_eq!(widths, vec![300.0, 200.0, 400.0]);

        // Items sit side by side, not stacked.
        let xs: Vec<f32> = laid.children.iter().map(|c| c.dimensions.content.x).collect();
        assert_eq!(xs, vec![0.0, 300.0, 500.0]);
        let ys: Vec<f32> = laid.children.iter().map(|c| c.dimensions.content.y).collect();
        assert_eq!(ys, vec![0.0, 0.0, 0.0]);
    }

    #[test]
    fn grid_tracks_expand_repeat_and_share_fr_space() {
        let ctx = crate::css::LengthContext::default();
        // 3 equal columns across 920 with 20px gaps -> (920 - 40) / 3.
        assert_eq!(resolve_tracks("repeat(3, 1fr)", 920.0, 20.0, ctx), vec![293.33334; 3]);
        // A fixed track keeps its size; fr splits what's left (600 - 200 - 10 = 390).
        assert_eq!(resolve_tracks("200px 1fr 2fr", 600.0, 5.0, ctx), vec![200.0, 130.0, 260.0]);
    }

    #[test]
    fn grid_spans_flow_onto_the_next_row() {
        let node = dom::elem("div".into(), HashMap::new(), vec![]);
        let cell = |span: Option<&str>| {
            let mut v = HashMap::new();
            v.insert("display".to_string(), Value::Keyword("block".into()));
            v.insert("height".to_string(), Value::Length(50.0, Unit::Px));
            if let Some(s) = span {
                v.insert("grid-column".to_string(), Value::Raw(s.to_string()));
            }
            StyledNode { node: &node, specified_values: v, children: vec![] }
        };
        let mut values = HashMap::new();
        values.insert("display".to_string(), Value::Keyword("grid".into()));
        values.insert(
            "grid-template-columns".to_string(),
            Value::Raw("repeat(4, 1fr)".to_string()),
        );
        // span2, single, single -> fills row 0; span3 must start row 1.
        let root = StyledNode {
            node: &node,
            specified_values: values,
            children: vec![cell(Some("span 2")), cell(None), cell(None), cell(Some("span 3"))],
        };
        let mut viewport: Dimensions = Default::default();
        viewport.content.width = 400.0; // 4 columns of 100

        let laid = layout_tree(&root, viewport, None, &ImageMap::new());
        let xs: Vec<f32> = laid.children.iter().map(|c| c.dimensions.content.x).collect();
        let ys: Vec<f32> = laid.children.iter().map(|c| c.dimensions.content.y).collect();
        let ws: Vec<f32> = laid.children.iter().map(|c| c.dimensions.content.width).collect();

        assert_eq!(ws[0], 200.0, "span 2 covers two 100px tracks");
        assert_eq!(ws[3], 300.0, "span 3 covers three tracks");
        assert_eq!(xs, vec![0.0, 200.0, 300.0, 0.0]);
        // Row 0 holds the first three; the span-3 item wraps to row 1.
        assert_eq!(ys[0], ys[1]);
        assert_eq!(ys[1], ys[2]);
        assert_eq!(ys[3], 50.0, "second row starts below a 50px first row");
        // The container must report both rows, or following content overlaps it.
        assert_eq!(laid.dimensions.content.height, 100.0);
    }

    #[test]
    fn absolute_child_positions_from_right_and_bottom() {
        let node = dom::elem("div".into(), HashMap::new(), vec![]);
        let mut child_values = HashMap::new();
        child_values.insert("display".to_string(), Value::Keyword("block".into()));
        child_values.insert("position".to_string(), Value::Keyword("absolute".into()));
        child_values.insert("width".to_string(), Value::Length(100.0, Unit::Px));
        child_values.insert("height".to_string(), Value::Length(40.0, Unit::Px));
        child_values.insert("right".to_string(), Value::Length(20.0, Unit::Px));
        child_values.insert("bottom".to_string(), Value::Length(10.0, Unit::Px));
        let child = StyledNode { node: &node, specified_values: child_values, children: vec![] };

        let mut root_values = HashMap::new();
        root_values.insert("display".to_string(), Value::Keyword("block".into()));
        root_values.insert("height".to_string(), Value::Length(200.0, Unit::Px));
        let root =
            StyledNode { node: &node, specified_values: root_values, children: vec![child] };

        let mut viewport: Dimensions = Default::default();
        viewport.content.width = 900.0;
        let laid = layout_tree(&root, viewport, None, &ImageMap::new());

        let placed = laid.children[0].dimensions.margin_box();
        assert_eq!(placed.x, 900.0 - 20.0 - 100.0); // right edge is 20 from the container's
        assert_eq!(placed.y, 200.0 - 10.0 - 40.0); // bottom edge is 10 from the container's
    }

    #[test]
    fn justify_content_distributes_free_space() {
        // 3 items of 100 in a 500 container leaves 200 free.
        assert_eq!(distribute("flex-start", 200.0, 3), (0.0, 0.0));
        assert_eq!(distribute("center", 200.0, 3), (100.0, 0.0));
        assert_eq!(distribute("flex-end", 200.0, 3), (200.0, 0.0));
        assert_eq!(distribute("space-between", 200.0, 3), (0.0, 100.0));
        assert_eq!(distribute("space-evenly", 200.0, 3), (50.0, 50.0));
        // A lone item has no gaps to spread into.
        assert_eq!(distribute("space-between", 200.0, 1), (0.0, 0.0));
    }

    #[test]
    fn flex_wrap_breaks_items_onto_new_lines() {
        let node = dom::elem("div".into(), HashMap::new(), vec![]);
        // Items need a height, or every line would sit at the same y.
        let item = |w: f32| {
            let mut v = HashMap::new();
            v.insert("display".to_string(), Value::Keyword("block".into()));
            v.insert("width".to_string(), Value::Length(w, Unit::Px));
            v.insert("height".to_string(), Value::Length(50.0, Unit::Px));
            StyledNode { node: &node, specified_values: v, children: vec![] }
        };
        let mut values = HashMap::new();
        values.insert("display".to_string(), Value::Keyword("flex".into()));
        values.insert("flex-wrap".to_string(), Value::Keyword("wrap".into()));
        let root = StyledNode {
            node: &node,
            specified_values: values,
            children: vec![item(200.0), item(200.0), item(200.0)],
        };
        let mut viewport: Dimensions = Default::default();
        viewport.content.width = 450.0; // fits two per line

        let laid = layout_tree(&root, viewport, None, &ImageMap::new());
        let ys: Vec<f32> = laid.children.iter().map(|c| c.dimensions.content.y).collect();
        // First two share a line; the third wraps below them.
        assert_eq!(ys[0], ys[1]);
        assert!(ys[2] > ys[1], "third item should wrap, got {ys:?}");
        let xs: Vec<f32> = laid.children.iter().map(|c| c.dimensions.content.x).collect();
        assert_eq!(xs[2], xs[0], "wrapped item restarts at the line start");
    }

    #[test]
    fn flex_items_without_grow_are_content_sized() {
        let node = dom::elem("div".into(), HashMap::new(), vec![]);
        // No flex-grow means shrink-to-fit, so an empty item takes no width and
        // leftover space stays empty — which is what real CSS does.
        let root = styled(
            &node,
            "flex",
            None,
            None,
            vec![styled(&node, "block", Some(200.0), None, vec![]), styled(&node, "block", None, None, vec![])],
        );
        let mut viewport: Dimensions = Default::default();
        viewport.content.width = 900.0;

        let laid = layout_tree(&root, viewport, None, &ImageMap::new());
        let widths: Vec<f32> = laid.children.iter().map(|c| c.dimensions.content.width).collect();
        assert_eq!(widths, vec![200.0, 0.0]);
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

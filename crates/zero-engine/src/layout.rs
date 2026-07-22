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
    /// The source word, kept because shaping throws characters away and
    /// find-in-page still has to match on them.
    pub text: String,
    /// Advance width of the run, so a highlight can be drawn behind it.
    pub width: f32,
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
    /// The element's `id` attribute, so an embedder can identify chrome controls.
    pub id: String,
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

/// The painted box of an inline element (`<span>`, `<a>`, `<code>`) on one line.
/// An inline element that wraps produces one fragment per line it covers.
#[derive(Clone)]
pub struct InlineBox {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    pub background: Option<Color>,
    pub border_color: Option<Color>,
    pub radius: f32,
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
    /// Backgrounds/borders for inline elements, painted beneath their text.
    pub inline_boxes: Vec<InlineBox>,
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
            inline_boxes: Vec::new(),
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
            BoxType::AnonymousBlock => self.layout_inline(containing_block, fonts, images),
            // A bare inline node is laid out by its anonymous-block parent, not here.
            BoxType::InlineNode(_) => {}
        }
    }

    fn layout_block(
        &mut self,
        containing_block: Dimensions,
        fonts: Option<&FontSet>,
        images: &ImageMap,
    ) {
        // Width depends on the parent, so it's computed top-down first.
        self.calculate_block_width(containing_block);
        self.calculate_block_position(containing_block);
        // Children accumulate into this height, so it must start at zero. A box can
        // be laid out more than once (tables, grid, flex, positioned two-pass), and
        // a stale value would push every descendant down by the previous height.
        self.dimensions.content.height = 0.0;
        // Then children are laid out to discover this box's height.
        match self.get_style_node().display() {
            Display::Flex => self.layout_flex_children(fonts, images),
            Display::Grid => self.layout_grid_children(fonts, images),
            Display::Table => self.layout_table_children(fonts, images),
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
        let attr_px = |name: &str| {
            elem.attributes
                .get(name)
                .and_then(|s| s.trim().parse::<f32>().ok())
        };

        let w = css_px("width")
            .or_else(|| attr_px("width"))
            .or_else(|| img.map(|i| i.width as f32))?;
        let h = css_px("height")
            .or_else(|| attr_px("height"))
            .or_else(|| img.map(|i| i.height as f32))
            .unwrap_or(w);
        Some((w, h))
    }

    /// Table layout: align cells into shared columns, honouring colspan/rowspan.
    ///
    /// Column widths come from each column's widest single-column cell, then scale
    /// to fill the table. Rows take the height of their tallest cell.
    ///
    /// ponytail: no `<caption>`/`<colgroup>` or border-collapse; one level of
    /// `thead`/`tbody`/`tfoot` nesting is understood.
    fn layout_table_children(&mut self, fonts: Option<&FontSet>, images: &ImageMap) {
        let container = self.dimensions.content;
        let style = self.get_style_node();
        let ctx = style.length_context(container.width);
        let spacing = style
            .value("border-spacing")
            .map(|v| v.resolve(ctx))
            .unwrap_or(0.0);

        // Rows may sit directly under the table or inside a section element.
        let mut rows: Vec<(usize, Option<usize>)> = Vec::new();
        for (i, child) in self.children.iter().enumerate() {
            match tag_name_of(child) {
                Some(tag) if tag == "tr" => rows.push((i, None)),
                Some(tag) if matches!(tag.as_str(), "tbody" | "thead" | "tfoot") => {
                    for (j, grandchild) in child.children.iter().enumerate() {
                        if tag_name_of(grandchild).as_deref() == Some("tr") {
                            rows.push((i, Some(j)));
                        }
                    }
                }
                _ => {}
            }
        }
        if rows.is_empty() {
            return self.layout_block_children(fonts, images);
        }

        // Assign every cell a column, skipping slots still covered by a rowspan
        // from an earlier row. `spans` counts how many further rows each column owes.
        struct Placed {
            row: usize,
            cell: usize,
            column: usize,
            colspan: usize,
            rowspan: usize,
        }
        let mut placed: Vec<Placed> = Vec::new();
        let mut carry: Vec<usize> = Vec::new(); // remaining rowspan rows per column
        let mut column_count = 0usize;

        for (r, &path) in rows.iter().enumerate() {
            let row = row_at(&self.children, path);
            let mut column = 0usize;
            for (cell_index, cell) in row.children.iter().enumerate() {
                let (colspan, rowspan) = match cell.box_type {
                    BoxType::AnonymousBlock => (1, 1),
                    _ => cell_spans(cell.get_style_node()),
                };
                // Step over columns a previous row is still occupying.
                while carry.get(column).copied().unwrap_or(0) > 0 {
                    column += 1;
                }
                placed.push(Placed {
                    row: r,
                    cell: cell_index,
                    column,
                    colspan,
                    rowspan,
                });
                if rowspan > 1 {
                    while carry.len() < column + colspan {
                        carry.push(0);
                    }
                    // Counted down once at the end of every row, including this
                    // one, so `rowspan: 2` still covers the row below.
                    for c in column..column + colspan {
                        carry[c] = rowspan;
                    }
                }
                column += colspan;
                column_count = column_count.max(column);
            }
            // One row consumed from every outstanding rowspan.
            for slot in carry.iter_mut() {
                *slot = slot.saturating_sub(1);
            }
            let _ = r;
        }
        if column_count == 0 {
            return self.layout_block_children(fonts, images);
        }

        // Only single-column cells drive column widths; spanning cells would
        // otherwise inflate whichever column they happen to start in.
        let mut widths: Vec<f32> = vec![0.0; column_count];
        for item in &placed {
            if item.colspan != 1 {
                continue;
            }
            let cell = &row_at(&self.children, rows[item.row]).children[item.cell];
            let wanted = match cell.box_type {
                BoxType::AnonymousBlock => 0.0,
                _ => max_content_width(cell.get_style_node(), fonts, images),
            };
            widths[item.column] = widths[item.column].max(wanted);
        }
        let gaps = spacing * column_count.saturating_sub(1) as f32;
        let available = (container.width - gaps).max(1.0);
        let natural: f32 = widths.iter().sum();
        if natural > 0.0 {
            let scale = available / natural;
            widths.iter_mut().for_each(|w| *w *= scale);
        } else {
            let even = available / column_count as f32;
            widths.iter_mut().for_each(|w| *w = even);
        }

        // Row heights: measure each cell, attributing a rowspan cell to its last row.
        let mut row_heights: Vec<f32> = vec![0.0; rows.len()];
        for item in &placed {
            let width = span_width(&widths, item.column, item.colspan, spacing);
            let row_path = rows[item.row];
            let cell = &mut row_at_mut(&mut self.children, row_path).children[item.cell];
            let mut slot: Dimensions = Default::default();
            slot.content = Rect {
                x: container.x,
                y: container.y,
                width,
                height: 0.0,
            };
            cell.layout(slot, fonts, images);
            if item.rowspan == 1 {
                let height = cell.dimensions.margin_box().height;
                row_heights[item.row] = row_heights[item.row].max(height);
            }
        }
        // A spanning cell only needs the rows it covers to add up to its height.
        for item in placed.iter().filter(|p| p.rowspan > 1) {
            let last = (item.row + item.rowspan - 1).min(rows.len() - 1);
            let covered: f32 = row_heights[item.row..=last].iter().sum::<f32>()
                + spacing * (last - item.row) as f32;
            let cell = &row_at(&self.children, rows[item.row]).children[item.cell];
            let needed = cell.dimensions.margin_box().height;
            if needed > covered {
                row_heights[last] += needed - covered;
            }
        }

        // Final placement, now that column x offsets and row y offsets are known.
        let row_tops: Vec<f32> = row_heights
            .iter()
            .scan(container.y, |y, h| {
                let top = *y;
                *y += h + spacing;
                Some(top)
            })
            .collect();

        for item in &placed {
            let width = span_width(&widths, item.column, item.colspan, spacing);
            let x = container.x
                + widths[..item.column].iter().sum::<f32>()
                + spacing * item.column as f32;
            let last = (item.row + item.rowspan - 1).min(rows.len() - 1);
            let height: f32 = row_heights[item.row..=last].iter().sum::<f32>()
                + spacing * (last - item.row) as f32;

            let row_path = rows[item.row];
            let top = row_tops[item.row];
            let cell = &mut row_at_mut(&mut self.children, row_path).children[item.cell];
            let mut slot: Dimensions = Default::default();
            slot.content = Rect {
                x,
                y: top,
                width,
                height: 0.0,
            };
            cell.layout(slot, fonts, images);
            // Stretch so a row's backgrounds share one baseline.
            let outer = cell.dimensions.margin_box().height;
            if outer < height {
                cell.dimensions.content.height += height - outer;
            }
        }

        for (r, &path) in rows.iter().enumerate() {
            let row = row_at_mut(&mut self.children, path);
            row.dimensions.content = Rect {
                x: container.x,
                y: row_tops[r],
                width: container.width,
                height: row_heights[r],
            };
        }

        // Sections wrap their rows, so give them the union of what they contain.
        for child in self.children.iter_mut() {
            if matches!(
                tag_name_of(child).as_deref(),
                Some("tbody" | "thead" | "tfoot")
            ) {
                let top = child.children.first().map(|r| r.dimensions.content.y);
                let bottom = child
                    .children
                    .last()
                    .map(|r| r.dimensions.content.y + r.dimensions.content.height);
                if let (Some(top), Some(bottom)) = (top, bottom) {
                    child.dimensions.content = Rect {
                        x: container.x,
                        y: top,
                        width: container.width,
                        height: bottom - top,
                    };
                }
            }
        }
        self.dimensions.content.height =
            row_heights.iter().sum::<f32>() + spacing * rows.len().saturating_sub(1) as f32;
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
            // Sheets often set the tracks through the `grid-template` shorthand
            // instead; without reading it the whole grid falls back to blocks.
            _ => match shorthand_tracks(&style, Axis::Columns) {
                Some(spec) => spec,
                None => return self.layout_block_children_gapped(fonts, images, gap),
            },
        };
        let columns = resolve_tracks(&spec, container.width, gap, ctx);
        if columns.is_empty() {
            return self.layout_block_children_gapped(fonts, images, gap);
        }
        // Explicit row sizes, if any; extra rows fall back to content height.
        let row_spec = match style.value("grid-template-rows") {
            Some(Value::Raw(spec)) => Some(spec),
            _ => shorthand_tracks(&style, Axis::Rows),
        };
        let row_sizes = match row_spec {
            Some(spec) => resolve_tracks(&spec, container.height, gap, ctx),
            None => Vec::new(),
        };

        // Named areas, which pages use to lay out whole page columns.
        let areas = match style.value("grid-template-areas") {
            Some(Value::Raw(spec)) => parse_grid_areas(&spec),
            _ => Default::default(),
        };

        // Assign every in-flow item a (row, column, span), honouring `grid-column`.
        let mut placements: Vec<(usize, usize, usize, usize)> = Vec::new(); // (index,row,col,span)
        let mut occupied: Vec<Vec<bool>> = Vec::new();
        let (mut row, mut col) = (0usize, 0usize);

        for index in 0..self.children.len() {
            if self.children[index].is_out_of_flow() {
                continue;
            }
            // A named area pins the item outright; otherwise fall back to
            // `grid-column` and auto-placement.
            let placed_area = match self.children[index].box_type {
                BoxType::AnonymousBlock => None,
                _ => named_area(self.children[index].get_style_node(), &areas),
            };
            let (explicit_col, span) = match placed_area {
                Some((area_row, area_col, area_span)) => {
                    row = area_row;
                    (Some(area_col), area_span)
                }
                None => match self.children[index].box_type {
                    BoxType::AnonymousBlock => (None, 1),
                    _ => parse_grid_span(self.children[index].get_style_node(), columns.len()),
                },
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
                    col = if explicit_col.is_some() {
                        col
                    } else {
                        start + span
                    };
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
            slot.content = Rect {
                x,
                y: container.y,
                width,
                height: 0.0,
            };
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
            let y = container.y + row_heights[..r].iter().sum::<f32>() + gap * r as f32;
            let width = columns[c..c + span].iter().sum::<f32>() + gap * (span - 1) as f32;
            let x = container.x + columns[..c].iter().sum::<f32>() + gap * c as f32;
            let mut slot: Dimensions = Default::default();
            slot.content = Rect {
                x,
                y,
                width,
                height: 0.0,
            };
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
    fn layout_inline(
        &mut self,
        containing_block: Dimensions,
        fonts: Option<&FontSet>,
        images: &ImageMap,
    ) {
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

        // An anonymous block has no style of its own, but white-space inherits,
        // so its inline children carry what its parent asked for.
        let styled = match self.box_type {
            BoxType::BlockNode(styled) | BoxType::InlineNode(styled) => Some(styled),
            BoxType::AnonymousBlock => self.children.iter().find_map(|child| match child.box_type {
                BoxType::BlockNode(styled) | BoxType::InlineNode(styled) => Some(styled),
                BoxType::AnonymousBlock => None,
            }),
        };
        let preserve_whitespace = matches!(
            styled.and_then(|s| s.value("white-space")),
            Some(Value::Keyword(ref k)) if k == "pre" || k == "pre-wrap"
        );

        // Flatten the inline subtree into a stream of text runs and element
        // boundaries, carrying the nearest ancestor link's href with each run.
        let mut pieces: Vec<InlinePiece> = Vec::new();
        for (index, child) in self.children.iter().enumerate() {
            // Inside an inline container, a block-level box can only be an
            // inline-block, so it joins the line as one indivisible item.
            if matches!(child.box_type, BoxType::BlockNode(_)) {
                pieces.push(InlinePiece::Atomic(index));
            } else {
                collect_inline_text(child, default_size, None, &mut pieces);
            }
        }

        let mut inline_boxes: Vec<InlineBox> = Vec::new();
        let mut open: Vec<OpenInline> = Vec::new();
        // Spaces are inserted *before* the next item rather than after the previous
        // one, so an element's painted box stops at its text and adjacent
        // inline-blocks still get separated.
        let mut pending_space = false;
        let (_, default_space) = shape_run(&fonts.entries[0], " ", default_size);
        // An inline-block occupies a line even when it contributes no text of its
        // own, so height can't be inferred from text fragments alone.
        let mut placed_any = false;

        for piece in &pieces {
            let piece = match piece {
                InlinePiece::Enter(style) => {
                    // Any pending word gap belongs *outside* the element's box.
                    if pending_space && cursor_x > start_x {
                        cursor_x += default_space;
                        pending_space = false;
                    }
                    // Padding before the element's first word reserves real space.
                    cursor_x += style.pad_left;
                    open.push(OpenInline {
                        style: style.clone(),
                        start_x: cursor_x - style.pad_left,
                        line_y: cursor_y,
                        height: line_height,
                    });
                    continue;
                }
                InlinePiece::Exit => {
                    if let Some(mut boxed) = open.pop() {
                        boxed.height = boxed.height.max(line_height);
                        inline_boxes.push(boxed.close(cursor_x + boxed.style.pad_right));
                        cursor_x += boxed.style.pad_right;
                    }
                    continue;
                }
                InlinePiece::Atomic(index) => {
                    let index = *index;
                    // Shrink-to-fit: an explicit width wins, else the content width,
                    // never wider than the line.
                    let outer = {
                        let style = self.children[index].get_style_node();
                        let ctx = style.length_context(max_width);
                        match style.value("width") {
                            Some(v @ Value::Length(..)) => {
                                v.resolve(ctx) + horizontal_edges(style, ctx)
                            }
                            // `fonts` is already unwrapped in this scope.
                            _ => max_content_width(style, Some(fonts), images).min(max_width),
                        }
                    };
                    let mut lead = if pending_space && cursor_x > start_x {
                        default_space
                    } else {
                        0.0
                    };
                    if cursor_x > start_x && cursor_x + lead + outer > start_x + max_width {
                        lead = 0.0;
                        for boxed in open.iter_mut() {
                            boxed.height = boxed.height.max(line_height);
                            inline_boxes.push(boxed.close(cursor_x));
                            boxed.start_x = start_x;
                            boxed.line_y = cursor_y + line_height;
                            boxed.height = 0.0;
                        }
                        cursor_y += line_height;
                        cursor_x = start_x;
                        line_height = 0.0;
                    }
                    cursor_x += lead;
                    let mut slot: Dimensions = Default::default();
                    slot.content = Rect {
                        x: cursor_x,
                        y: cursor_y,
                        width: outer,
                        height: 0.0,
                    };
                    self.children[index].layout(slot, Some(fonts), images);
                    let placed = self.children[index].dimensions.margin_box();
                    line_height = line_height.max(placed.height);
                    for boxed in open.iter_mut() {
                        boxed.height = boxed.height.max(placed.height);
                    }
                    cursor_x += placed.width;
                    pending_space = true;
                    placed_any = true;
                    continue;
                }
                InlinePiece::Text(piece) => piece,
            };

            let word_height = piece.size * 1.25;
            let (_, space_w) = shape_run(&fonts.entries[0], " ", piece.size);

            // `white-space: pre` keeps the text exactly as written: newlines end
            // lines and runs of spaces survive, which is the whole point of a
            // code block.
            //
            // ponytail: each line is shaped as one run, so font fallback happens
            // per line rather than per word — fine for code, less so for a
            // preformatted block mixing scripts. `pre-wrap` does not wrap yet.
            if preserve_whitespace {
                for (i, line) in piece.text.split('\n').enumerate() {
                    if i > 0 {
                        for boxed in open.iter_mut() {
                            boxed.height = boxed.height.max(line_height.max(word_height));
                            inline_boxes.push(boxed.close(cursor_x));
                            boxed.start_x = start_x;
                            boxed.line_y = cursor_y + line_height.max(word_height);
                            boxed.height = 0.0;
                        }
                        cursor_y += line_height.max(word_height);
                        cursor_x = start_x;
                        line_height = 0.0;
                    }
                    line_height = line_height.max(word_height);
                    placed_any = true;
                    if line.is_empty() {
                        continue; // a blank line still occupies its height
                    }
                    let font_index = fonts.pick(line);
                    let (glyphs, width) = shape_run(&fonts.entries[font_index], line, piece.size);
                    fragments.push(TextFragment {
                        glyphs,
                        text: line.to_string(),
                        width,
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
                            width,
                            height: word_height,
                        });
                    }
                    cursor_x += width;
                }
                pending_space = false;
                continue;
            }

            // Only space, tab and newline collapse in CSS. A non-breaking space
            // is an ordinary character that happens to look like a space, so it
            // must stay inside the word — pages use it to hold spacing.
            for word in piece.text.split_ascii_whitespace() {
                // Pick a font that can draw this word, then shape it: this is where
                // Indic reordering/conjuncts happen.
                let font_index = fonts.pick(word);
                let (glyphs, word_w) = shape_run(&fonts.entries[font_index], word, piece.size);
                let mut lead = if pending_space && cursor_x > start_x {
                    space_w
                } else {
                    0.0
                };
                // Wrap if this word overflows and we're not at line start.
                if cursor_x > start_x && cursor_x + lead + word_w > start_x + max_width {
                    lead = 0.0;
                    // Close each open element on the line it is leaving, then
                    // reopen it on the next one, so a wrapped span paints twice.
                    for boxed in open.iter_mut() {
                        boxed.height = boxed.height.max(line_height);
                        inline_boxes.push(boxed.close(cursor_x));
                        boxed.start_x = start_x;
                        boxed.line_y = cursor_y + line_height;
                        boxed.height = 0.0;
                    }
                    cursor_y += line_height;
                    cursor_x = start_x;
                    line_height = 0.0;
                }
                cursor_x += lead;
                line_height = line_height.max(word_height);
                for boxed in open.iter_mut() {
                    boxed.height = boxed.height.max(word_height);
                }
                fragments.push(TextFragment {
                    glyphs,
                    text: word.to_string(),
                    width: word_w,
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
                cursor_x += word_w;
                pending_space = true;
                placed_any = true;
            }
        }
        // Anything still open ran to the end of the content.
        while let Some(mut boxed) = open.pop() {
            boxed.height = boxed.height.max(line_height);
            inline_boxes.push(boxed.close(cursor_x));
        }
        self.inline_boxes = inline_boxes;

        self.dimensions.content.height = if placed_any {
            (cursor_y - top) + line_height
        } else {
            0.0
        };
        self.text_fragments = fragments;
        self.link_areas = link_areas;
        self.align_lines(start_x, max_width);
    }

    /// Shift each finished line for `text-align`.
    ///
    /// Alignment is a whole-line property, but lines only exist implicitly here:
    /// every item placed at the same `y` belongs to one. Shifting afterwards
    /// keeps wrapping (which needs a left-to-right cursor) independent of it.
    fn align_lines(&mut self, start_x: f32, max_width: f32) {
        // An anonymous block has no style of its own, but text-align inherits,
        // so its inline children carry the value its parent set.
        let styled = match self.box_type {
            BoxType::BlockNode(styled) | BoxType::InlineNode(styled) => Some(styled),
            BoxType::AnonymousBlock => self.children.iter().find_map(|child| match child.box_type {
                BoxType::BlockNode(styled) | BoxType::InlineNode(styled) => Some(styled),
                BoxType::AnonymousBlock => None,
            }),
        };
        let align = match styled.and_then(|s| s.value("text-align")) {
            Some(Value::Keyword(word)) => word,
            _ => return,
        };
        let factor = match align.as_str() {
            "center" => 0.5,
            "right" | "end" => 1.0,
            _ => return, // left/start/justify: already where they belong
        };

        // Line identity is the y coordinate the items were placed at.
        let mut lines: Vec<f32> = self.text_fragments.iter().map(|f| f.y).collect();
        lines.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        lines.dedup();

        for line_y in lines {
            let right = self
                .text_fragments
                .iter()
                .filter(|f| f.y == line_y)
                .map(|f| f.x + f.width)
                .fold(f32::MIN, f32::max);
            let shift = line_shift(right, start_x + max_width, factor);
            if shift <= 0.0 {
                continue;
            }
            for frag in self.text_fragments.iter_mut().filter(|f| f.y == line_y) {
                frag.x += shift;
            }
            for link in self.link_areas.iter_mut().filter(|l| l.y == line_y) {
                link.x += shift;
            }
            for boxed in self.inline_boxes.iter_mut().filter(|b| b.y == line_y) {
                boxed.x += shift;
            }
        }
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
            &margin_left,
            &margin_right,
            &border_left,
            &border_right,
            &padding_left,
            &padding_right,
            &width,
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
        d.border.top = style
            .lookup("border-top-width", "border-width", &zero)
            .resolve(ctx);
        d.border.bottom = style
            .lookup("border-bottom-width", "border-width", &zero)
            .resolve(ctx);
        d.padding.top = style.lookup("padding-top", "padding", &zero).resolve(ctx);
        d.padding.bottom = style
            .lookup("padding-bottom", "padding", &zero)
            .resolve(ctx);

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
        let is_column =
            matches!(style.value("flex-direction"), Some(Value::Keyword(ref k)) if k == "column");
        let gap = style
            .value("gap")
            .map(|v| v.to_px())
            .filter(|g| *g > 0.0)
            .unwrap_or(0.0);

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
                BoxType::AnonymousBlock => Item {
                    base: 0.0,
                    grow: 1.0,
                },
                _ => {
                    let style = child.get_style_node();
                    let ctx = style.length_context(container.width);
                    // Base is the OUTER width, so gaps and free space line up with
                    // what layout actually produces (a border box, not content).
                    let base = match style.value("width") {
                        Some(v @ Value::Length(..)) => {
                            v.resolve(ctx) + horizontal_edges(style, ctx)
                        }
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
        let justify = style
            .value("justify-content")
            .and_then(keyword_of)
            .unwrap_or_default();
        let align = style
            .value("align-items")
            .and_then(keyword_of)
            .unwrap_or_default();

        // Group items into lines. Without wrapping everything shares one line.
        let flow: Vec<usize> = (0..count)
            .filter(|&i| !self.children[i].is_out_of_flow())
            .collect();
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
                slot.content = Rect {
                    x: cursor_x,
                    y: cursor_y,
                    width: widths[slot_index],
                    height: 0.0,
                };
                self.children[i].layout(slot, fonts, images);
                let placed = self.children[i].dimensions.margin_box();
                cursor_x += placed.width + gap + between;
                tallest = tallest.max(placed.height);
            }

            // Cross-axis alignment happens once the line's height is known.
            for &i in line.iter() {
                align_cross_axis(
                    &mut self.children[i],
                    &align,
                    cursor_y,
                    tallest,
                    fonts,
                    images,
                );
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

/// Style an inline element paints with, captured when layout enters it.
#[derive(Clone)]
struct InlineStyle {
    background: Option<Color>,
    border_color: Option<Color>,
    radius: f32,
    pad_left: f32,
    pad_right: f32,
    pad_top: f32,
    pad_bottom: f32,
}

/// The inline stream: text runs, element boundaries, and atomic inline-blocks.
enum InlinePiece {
    Text(TextPiece),
    Enter(InlineStyle),
    Exit,
    /// An `inline-block` child, laid out as a block but placed on the line.
    /// Holds its index among this box's children.
    Atomic(usize),
}

/// A decorated inline element currently open on the line being built.
struct OpenInline {
    style: InlineStyle,
    start_x: f32,
    line_y: f32,
    height: f32,
}

impl OpenInline {
    fn close(&self, end_x: f32) -> InlineBox {
        InlineBox {
            x: self.start_x,
            y: self.line_y - self.style.pad_top,
            width: (end_x - self.start_x).max(0.0),
            height: self.height + self.style.pad_top + self.style.pad_bottom,
            background: self.style.background,
            border_color: self.style.border_color,
            radius: self.style.radius,
        }
    }
}

/// Walk an inline box subtree, emitting text runs plus the boundaries of any
/// inline element that paints something (background, border, or padding).
///
/// Boundaries are what let a wrapped `<span>` produce one painted box per line.
fn collect_inline_text(
    bx: &LayoutBox,
    default_size: f32,
    href: Option<&str>,
    out: &mut Vec<InlinePiece>,
) {
    let mut current_href = href.map(str::to_string);
    let mut decorated = false;

    if let BoxType::InlineNode(styled) = bx.box_type {
        if let NodeType::Element(_) = styled.node.node_type {
            if let Some(h) = href_of(styled) {
                current_href = Some(h.to_string());
            }
            if let Some(style) = inline_style_of(styled) {
                out.push(InlinePiece::Enter(style));
                decorated = true;
            }
        }
        if let NodeType::Text(ref t) = styled.node.node_type {
            let size = if styled.font_size() > 0.0 {
                styled.font_size()
            } else {
                default_size
            };
            let color = match styled.value("color") {
                Some(Value::ColorValue(c)) => c,
                _ => Color {
                    r: 0,
                    g: 0,
                    b: 0,
                    a: 255,
                },
            };
            out.push(InlinePiece::Text(TextPiece {
                text: t.clone(),
                size,
                color,
                href: current_href.clone(),
            }));
        }
    }
    for child in &bx.children {
        collect_inline_text(child, default_size, current_href.as_deref(), out);
    }
    if decorated {
        out.push(InlinePiece::Exit);
    }
}

/// The link target this node carries, whatever box it turned into.
fn href_of<'a>(styled: &'a StyledNode<'a>) -> Option<&'a str> {
    match &styled.node.node_type {
        NodeType::Element(e) if e.tag_name == "a" => e.attributes.get("href").map(String::as_str),
        _ => None,
    }
}

/// The paintable style of an inline element, or `None` if it draws nothing.
fn inline_style_of(styled: &StyledNode) -> Option<InlineStyle> {
    let ctx = styled.length_context(0.0);
    let zero = Value::Length(0.0, Unit::Px);
    let pad = |side: &str| styled.lookup(side, "padding", &zero).resolve(ctx);
    let color_of = |name: &str| match styled.value(name) {
        Some(Value::ColorValue(c)) => Some(c),
        _ => None,
    };
    let background = color_of("background").or_else(|| color_of("background-color"));
    let border_color = color_of("border-color");
    let (pad_left, pad_right) = (pad("padding-left"), pad("padding-right"));
    let (pad_top, pad_bottom) = (pad("padding-top"), pad("padding-bottom"));

    if background.is_none()
        && border_color.is_none()
        && pad_left == 0.0
        && pad_right == 0.0
        && pad_top == 0.0
        && pad_bottom == 0.0
    {
        return None;
    }
    Some(InlineStyle {
        background,
        border_color,
        radius: styled.px("border-radius", 0.0).unwrap_or(0.0),
        pad_left,
        pad_right,
        pad_top,
        pad_bottom,
    })
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
#[derive(Clone, Copy, PartialEq)]
enum Axis {
    Rows,
    Columns,
}

/// Track sizes from the `grid-template` shorthand, which writes them as
/// `rows / columns`. Area strings may appear before the slash, so only the
/// track list on each side is taken.
fn shorthand_tracks(style: &StyledNode, axis: Axis) -> Option<String> {
    let Some(Value::Raw(spec)) = style.value("grid-template") else { return None };
    let (rows, columns) = spec.split_once('/')?;
    let text = match axis {
        Axis::Rows => rows,
        Axis::Columns => columns,
    };
    // Drop any quoted area strings; they are placement, not sizes.
    let quotes: [char; 2] = ['"', '\''];
    let text: String = text
        .split(quotes)
        .step_by(2)
        .collect::<Vec<_>>()
        .join(" ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    (!text.is_empty()).then_some(text)
}

/// Where a named area sits: (row, column, column span).
type Area = (usize, usize, usize);

/// Parse `grid-template-areas: 'a a b' 'c c b'` into a placement per name.
///
/// Each quoted string is a row of cell names; a name repeated across cells
/// spans them. `.` is an empty cell.
///
/// ponytail: only column spans are recorded. A name spanning several *rows*
/// still lands on its first row, which is enough for the page layouts that use
/// this — a full implementation would track row spans too.
fn parse_grid_areas(spec: &str) -> std::collections::HashMap<String, Area> {
    let mut areas: std::collections::HashMap<String, Area> = Default::default();
    let quotes: [char; 2] = ['"', '\''];
    for (row, line) in spec.split(quotes).filter(|s| !s.trim().is_empty()).enumerate() {
        for (col, name) in line.split_whitespace().enumerate() {
            if name == "." {
                continue;
            }
            areas
                .entry(name.to_string())
                .and_modify(|(_, start, span)| *span = (col + 1).saturating_sub(*start).max(*span))
                .or_insert((row, col, 1));
        }
    }
    areas
}

/// The area a child asks for by name, if the container defines one.
fn named_area(style: &StyledNode, areas: &std::collections::HashMap<String, Area>) -> Option<Area> {
    match style.value("grid-area") {
        Some(Value::Raw(name)) => areas.get(name.trim()).copied(),
        Some(Value::Keyword(name)) => areas.get(name.trim()).copied(),
        _ => None,
    }
}

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
        Some(end) if end.starts_with("span ") => end[5..].trim().parse::<usize>().unwrap_or(1),
        Some(end) => match (start, end.parse::<usize>().ok()) {
            (Some(a), Some(b)) if b > a => b - a,
            _ => 1,
        },
        None => 1,
    };
    let start = start
        .map(|line| line.saturating_sub(1))
        .filter(|s| *s < columns);
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
                let count = parts
                    .next()
                    .unwrap_or("")
                    .trim()
                    .parse::<usize>()
                    .unwrap_or(0);
                let pattern: Vec<&str> = parts.next().unwrap_or("").split_whitespace().collect();
                for _ in 0..count.min(1000) {
                    tokens.extend(pattern.iter().map(|p| p.to_string()));
                }
                rest = tail[1..].trim_start();
                continue;
            }
        }
        // `minmax(min, max)` sizes as its max, which is what decides the layout.
        if let Some(after) = rest.strip_prefix("minmax(") {
            if let Some(close) = after.find(')') {
                let (inside, tail) = after.split_at(close);
                let max = inside.rsplit(',').next().unwrap_or("").trim();
                tokens.push(max.to_string());
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
        .map(|t| {
            t.strip_suffix("fr")
                .and_then(|n| n.trim().parse::<f32>().ok())
        })
        .collect();
    let fixed: Vec<f32> = tokens
        .iter()
        .zip(&fractions)
        .map(|(t, fr)| {
            if fr.is_some() {
                0.0
            } else {
                parse_track_length(t, ctx)
            }
        })
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
    0.0
}

/// The widest a subtree wants to be if nothing wraps it (CSS `max-content`).
///
/// Needed for content-based sizing: a flex item with `width: auto` and no
/// `flex-grow` should be as wide as its content, not as wide as its container.
///
/// ponytail: measures text without wrapping and ignores min-content (longest word).
/// Good enough for shrink-to-fit; a full intrinsic pass would compute both.
pub fn max_content_width(style: &StyledNode, fonts: Option<&FontSet>, images: &ImageMap) -> f32 {
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
                } else if matches!(child.display(), Display::Inline | Display::InlineBlock) {
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
    let trimmed = text.split_ascii_whitespace().collect::<Vec<_>>().join(" ");
    if trimmed.is_empty() {
        return 0.0;
    }
    shape_run(&fonts.entries[fonts.pick(&trimmed)], &trimmed, size).1
}

/// `colspan`/`rowspan` for a cell, defaulting to 1 and clamped to something sane.
fn cell_spans(style: &StyledNode) -> (usize, usize) {
    let attr = |name: &str| match style.node.node_type {
        NodeType::Element(ref e) => e
            .attributes
            .get(name)
            .and_then(|v| v.trim().parse::<usize>().ok())
            .unwrap_or(1)
            .clamp(1, 1000),
        _ => 1,
    };
    (attr("colspan"), attr("rowspan"))
}

/// Total width of `colspan` columns starting at `column`, including the gaps
/// between them, which a spanning cell absorbs.
fn span_width(widths: &[f32], column: usize, colspan: usize, spacing: f32) -> f32 {
    let end = (column + colspan).min(widths.len());
    widths[column..end].iter().sum::<f32>() + spacing * (end - column).saturating_sub(1) as f32
}

/// The tag of an element box, if it is one.
fn tag_name_of(bx: &LayoutBox) -> Option<String> {
    let style = match bx.box_type {
        BoxType::BlockNode(s) | BoxType::InlineNode(s) => s,
        BoxType::AnonymousBlock => return None,
    };
    match style.node.node_type {
        NodeType::Element(ref e) => Some(e.tag_name.clone()),
        _ => None,
    }
}

/// A table row, addressed either directly or through a section element.
fn row_at<'b, 'a>(
    children: &'b [LayoutBox<'a>],
    path: (usize, Option<usize>),
) -> &'b LayoutBox<'a> {
    match path.1 {
        None => &children[path.0],
        Some(j) => &children[path.0].children[j],
    }
}

fn row_at_mut<'b, 'a>(
    children: &'b mut [LayoutBox<'a>],
    path: (usize, Option<usize>),
) -> &'b mut LayoutBox<'a> {
    match path.1 {
        None => &mut children[path.0],
        Some(j) => &mut children[path.0].children[j],
    }
}

/// The lowest painted edge in the tree.
///
/// The root box's own height is not enough: `overflow` defaults to visible, so
/// content sticking out of a fixed-height box (`body { height: 100% }` is common)
/// still paints and still has to be scrollable.
pub fn content_bottom(bx: &LayoutBox) -> f32 {
    let own = bx.dimensions.margin_box();
    let mut bottom = own.y + own.height;
    for frag in &bx.text_fragments {
        bottom = bottom.max(frag.y + frag.size * 1.25);
    }
    for child in &bx.children {
        bottom = bottom.max(content_bottom(child));
    }
    bottom
}

/// Gather the painted box of every element, so the embedder can hit-test clicks.
pub fn collect_element_rects(bx: &LayoutBox, out: &mut Vec<ElementRect>) {
    if let BoxType::BlockNode(styled) | BoxType::InlineNode(styled) = bx.box_type {
        if let NodeType::Element(ref e) = styled.node.node_type {
            let b = bx.dimensions.border_box();
            out.push(ElementRect {
                node_id: e.node_id,
                id: e.id().cloned().unwrap_or_default(),
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
    // An `<a>` that is a box in its own right — `display:inline-block`, or a
    // block — never gets an inline link area: its text is laid out by an
    // anonymous child that has no idea it sits inside a link. The box itself is
    // the region to click, padding and all, which is also exactly what is
    // painted. Without this every control on zero://settings looked like a
    // button and did nothing.
    if let BoxType::BlockNode(styled) = bx.box_type {
        if let Some(href) = href_of(styled) {
            let area = bx.dimensions.border_box();
            out.push(LinkArea {
                href: href.to_string(),
                x: area.x,
                y: area.y,
                width: area.width,
                height: area.height,
            });
        }
    }
    for child in &bx.children {
        collect_links(child, out);
    }
}

fn build_layout_tree<'a>(style_node: &'a StyledNode<'a>) -> LayoutBox<'a> {
    build_box(style_node, false)
}

/// How far to move a line whose content ends at `line_right` inside a box whose
/// content ends at `content_right`. A line that overflows is never pulled back.
fn line_shift(line_right: f32, content_right: f32, factor: f32) -> f32 {
    (content_right - line_right).max(0.0) * factor
}

/// A text node holding nothing but collapsible whitespace.
fn is_whitespace_text(style_node: &StyledNode) -> bool {
    matches!(&style_node.node.node_type, NodeType::Text(text) if text.trim().is_empty())
}

/// Does this node have a block-level child? Inline-block does not count: it is
/// inline-level and sits happily in a line.
fn contains_block(style_node: &StyledNode) -> bool {
    style_node.children.iter().any(|child| {
        matches!(
            child.display(),
            Display::Block | Display::Flex | Display::Grid | Display::Table
        )
    })
}

/// `force_block` blockifies a node because its parent is a flex container —
/// flex items are always block-level, whatever their own `display` says.
fn build_box<'a>(style_node: &'a StyledNode<'a>, force_block: bool) -> LayoutBox<'a> {
    // An inline element holding block-level children cannot lay out as a line of
    // text — doing so flattens whole tables into a run of words. CSS splits the
    // inline around its block children; treating the parent as a block keeps the
    // children's structure, which is the part that matters.
    //
    // ponytail: no anonymous block splitting, so text before and after a block
    // child becomes its own block line instead of continuing the same line.
    let force_block = force_block || contains_block(style_node);
    let display = style_node.display();
    let mut root = LayoutBox::new(match display {
        Display::Block | Display::Flex | Display::Grid | Display::Table => {
            BoxType::BlockNode(style_node)
        }
        // An inline-block lays out as a block; its parent decides where it sits.
        Display::InlineBlock => BoxType::BlockNode(style_node),
        Display::Inline if force_block => BoxType::BlockNode(style_node),
        Display::Inline => BoxType::InlineNode(style_node),
        Display::None => panic!("Root node has display: none."),
    });

    // Flex and grid items are always block-level, whatever their own display says.
    let blockifies = matches!(display, Display::Flex | Display::Grid);
    for child in &style_node.children {
        // Whitespace between flex or grid items produces no box at all — the
        // newlines in a page's source must not become items of their own.
        if blockifies && is_whitespace_text(child) {
            continue;
        }
        match child.display() {
            Display::None => {} // skip
            Display::Block | Display::Flex | Display::Grid | Display::Table => {
                root.children.push(build_box(child, false))
            }
            Display::Inline if blockifies => root.children.push(build_box(child, true)),
            Display::Inline => root
                .get_inline_container()
                .children
                .push(build_box(child, false)),
            // Blockified by a flex/grid parent, otherwise it joins the line.
            Display::InlineBlock if blockifies => root.children.push(build_box(child, true)),
            Display::InlineBlock => root
                .get_inline_container()
                .children
                .push(build_box(child, true)),
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
    fn named_areas_place_items_by_name() {
        let areas = parse_grid_areas("'notice notice' 'menu article' 'footer footer'");
        // (row, column, column span)
        assert_eq!(areas.get("notice"), Some(&(0, 0, 2)));
        assert_eq!(areas.get("menu"), Some(&(1, 0, 1)));
        assert_eq!(areas.get("article"), Some(&(1, 1, 1)));
        assert_eq!(areas.get("footer"), Some(&(2, 0, 2)));
        // A `.` cell names nothing.
        assert!(parse_grid_areas("'. a'").get(".").is_none());
    }

    #[test]
    fn minmax_tracks_size_by_their_maximum() {
        let ctx = crate::css::LengthContext::default();
        // minmax(0,1fr) behaves as 1fr: the fixed track takes its size, the rest
        // of the space goes to the flexible one.
        let tracks = resolve_tracks("200px minmax(0,1fr)", 600.0, 0.0, ctx);
        assert_eq!(tracks, vec![200.0, 400.0]);
    }

    #[test]
    fn lines_shift_by_their_leftover_space() {
        // A 100px-wide line in a 300px box: centred moves half the slack, right all.
        assert_eq!(line_shift(100.0, 300.0, 0.5), 100.0);
        assert_eq!(line_shift(100.0, 300.0, 1.0), 200.0);
        // A full line does not move, and an overflowing one is not pulled back.
        assert_eq!(line_shift(300.0, 300.0, 1.0), 0.0);
        assert_eq!(line_shift(420.0, 300.0, 1.0), 0.0);
    }

    #[test]
    fn explicit_block_width_is_respected() {
        let node = dom::elem("div".into(), HashMap::new(), vec![]);
        let mut values = HashMap::new();
        values.insert("display".to_string(), Value::Keyword("block".into()));
        values.insert("width".to_string(), Value::Length(200.0, Unit::Px));
        let styled = StyledNode {
            node: &node,
            specified_values: values,
            children: vec![],
        };

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
        StyledNode {
            node,
            specified_values: values,
            children,
        }
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
        let widths: Vec<f32> = laid
            .children
            .iter()
            .map(|c| c.dimensions.content.width)
            .collect();
        assert_eq!(widths, vec![300.0, 200.0, 400.0]);

        // Items sit side by side, not stacked.
        let xs: Vec<f32> = laid
            .children
            .iter()
            .map(|c| c.dimensions.content.x)
            .collect();
        assert_eq!(xs, vec![0.0, 300.0, 500.0]);
        let ys: Vec<f32> = laid
            .children
            .iter()
            .map(|c| c.dimensions.content.y)
            .collect();
        assert_eq!(ys, vec![0.0, 0.0, 0.0]);
    }

    #[test]
    fn grid_tracks_expand_repeat_and_share_fr_space() {
        let ctx = crate::css::LengthContext::default();
        // 3 equal columns across 920 with 20px gaps -> (920 - 40) / 3.
        assert_eq!(
            resolve_tracks("repeat(3, 1fr)", 920.0, 20.0, ctx),
            vec![293.33334; 3]
        );
        // A fixed track keeps its size; fr splits what's left (600 - 200 - 10 = 390).
        assert_eq!(
            resolve_tracks("200px 1fr 2fr", 600.0, 5.0, ctx),
            vec![200.0, 130.0, 260.0]
        );
    }

    #[test]
    fn span_width_absorbs_the_gaps_it_covers() {
        let widths = [100.0, 150.0, 200.0];
        assert_eq!(span_width(&widths, 0, 1, 10.0), 100.0);
        // Spanning two columns also swallows the 10px gap between them.
        assert_eq!(span_width(&widths, 0, 2, 10.0), 260.0);
        assert_eq!(span_width(&widths, 0, 3, 10.0), 470.0);
        // A span running past the last column is clamped, not a panic.
        assert_eq!(span_width(&widths, 2, 5, 10.0), 200.0);
    }

    #[test]
    fn table_cells_span_columns_and_rows() {
        let node = dom::elem("div".into(), HashMap::new(), vec![]);
        // Cells need real element nodes so colspan/rowspan attributes are visible.
        let cell_node = |colspan: &str, rowspan: &str| {
            let mut attrs = HashMap::new();
            if !colspan.is_empty() {
                attrs.insert("colspan".to_string(), colspan.to_string());
            }
            if !rowspan.is_empty() {
                attrs.insert("rowspan".to_string(), rowspan.to_string());
            }
            dom::elem("td".into(), attrs, vec![])
        };
        let plain = cell_node("", "");
        let wide = cell_node("2", "");
        let tall = cell_node("", "2");

        fn block<'a>(n: &'a dom::Node) -> StyledNode<'a> {
            let mut v = HashMap::new();
            v.insert("display".to_string(), Value::Keyword("block".into()));
            v.insert("height".to_string(), Value::Length(20.0, Unit::Px));
            StyledNode {
                node: n,
                specified_values: v,
                children: vec![],
            }
        }
        let tr = dom::elem("tr".into(), HashMap::new(), vec![]);
        fn row<'a>(n: &'a dom::Node, cells: Vec<StyledNode<'a>>) -> StyledNode<'a> {
            let mut v = HashMap::new();
            v.insert("display".to_string(), Value::Keyword("block".into()));
            StyledNode {
                node: n,
                specified_values: v,
                children: cells,
            }
        }
        // Row 0: [rowspan=2][colspan=2]   Row 1: [a][b]  (col 0 covered by the rowspan)
        let mut table_values = HashMap::new();
        table_values.insert("display".to_string(), Value::Keyword("table".into()));
        let table = StyledNode {
            node: &node,
            specified_values: table_values,
            children: vec![
                row(&tr, vec![block(&tall), block(&wide)]),
                row(&tr, vec![block(&plain), block(&plain)]),
            ],
        };

        let mut viewport: Dimensions = Default::default();
        viewport.content.width = 300.0;
        let laid = layout_tree(&table, viewport, None, &ImageMap::new());

        let row1: Vec<f32> = laid.children[1]
            .children
            .iter()
            .map(|c| c.dimensions.content.x)
            .collect();
        // Column 0 is still owned by the rowspan, so row 1 starts at column 1.
        assert!(
            row1[0] > 0.0,
            "second row must skip the spanned column, got {row1:?}"
        );
        assert!(row1[1] > row1[0]);
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
            StyledNode {
                node: &node,
                specified_values: v,
                children: vec![],
            }
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
            children: vec![
                cell(Some("span 2")),
                cell(None),
                cell(None),
                cell(Some("span 3")),
            ],
        };
        let mut viewport: Dimensions = Default::default();
        viewport.content.width = 400.0; // 4 columns of 100

        let laid = layout_tree(&root, viewport, None, &ImageMap::new());
        let xs: Vec<f32> = laid
            .children
            .iter()
            .map(|c| c.dimensions.content.x)
            .collect();
        let ys: Vec<f32> = laid
            .children
            .iter()
            .map(|c| c.dimensions.content.y)
            .collect();
        let ws: Vec<f32> = laid
            .children
            .iter()
            .map(|c| c.dimensions.content.width)
            .collect();

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
        let child = StyledNode {
            node: &node,
            specified_values: child_values,
            children: vec![],
        };

        let mut root_values = HashMap::new();
        root_values.insert("display".to_string(), Value::Keyword("block".into()));
        root_values.insert("height".to_string(), Value::Length(200.0, Unit::Px));
        let root = StyledNode {
            node: &node,
            specified_values: root_values,
            children: vec![child],
        };

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
            StyledNode {
                node: &node,
                specified_values: v,
                children: vec![],
            }
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
        let ys: Vec<f32> = laid
            .children
            .iter()
            .map(|c| c.dimensions.content.y)
            .collect();
        // First two share a line; the third wraps below them.
        assert_eq!(ys[0], ys[1]);
        assert!(ys[2] > ys[1], "third item should wrap, got {ys:?}");
        let xs: Vec<f32> = laid
            .children
            .iter()
            .map(|c| c.dimensions.content.x)
            .collect();
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
            vec![
                styled(&node, "block", Some(200.0), None, vec![]),
                styled(&node, "block", None, None, vec![]),
            ],
        );
        let mut viewport: Dimensions = Default::default();
        viewport.content.width = 900.0;

        let laid = layout_tree(&root, viewport, None, &ImageMap::new());
        let widths: Vec<f32> = laid
            .children
            .iter()
            .map(|c| c.dimensions.content.width)
            .collect();
        assert_eq!(widths, vec![200.0, 0.0]);
    }

    #[test]
    fn auto_width_fills_containing_block() {
        let node = dom::elem("div".into(), HashMap::new(), vec![]);
        let mut values = HashMap::new();
        values.insert("display".to_string(), Value::Keyword("block".into()));
        let styled = StyledNode {
            node: &node,
            specified_values: values,
            children: vec![],
        };

        let mut viewport: Dimensions = Default::default();
        viewport.content.width = 800.0;

        let root = layout_tree(&styled, viewport, None, &ImageMap::new());
        assert_eq!(root.dimensions.content.width, 800.0);
    }
}

//! A small SVG rasterizer: shapes and paths to pixels.
//!
//! SVG is the web's icon format, and a browser that skips it draws a page full
//! of holes where the logos and buttons should be — Zero's own chrome included,
//! which is drawn with inline `<svg>`.
//!
//! Shapes, paths, fills, strokes and `viewBox` are the core. On top of those:
//! `<use>`/`<symbol>` for icon sprites, linear and radial gradients,
//! `<pattern>`, `clipPath`, `<text>` (through the same shaper page text uses, so
//! Indic works inside an SVG too) and enough of the filter primitives to draw a
//! drop shadow.
//!
//! Everything defined anywhere in the document is indexed by `id` once, up
//! front, because every one of those features is a reference to something
//! elsewhere and they all want the same lookup.
//!
//! ponytail: `textPath` is not implemented — placing glyphs along a curve needs
//! the path parameterized by arc length, which nothing else here wants. Filters
//! are the drop-shadow chain (`feGaussianBlur`, `feOffset`, `feFlood`,
//! `feMerge`) rather than a general filter graph. Curves are flattened to line
//! segments and every shape is filled by one scanline pass with 3x3
//! supersampling, which is slower than an active-edge rasterizer and far
//! shorter. Anything unrecognised is skipped rather than guessed at, so an
//! unsupported feature costs one shape, not the picture.

use crate::css::{Color, Mat};
use crate::dom::{Node, NodeType};
use crate::resource::DecodedImage;
use crate::text::FontSet;
use std::collections::HashMap;

/// Samples per axis inside each pixel. 3×3 is enough to make a diagonal edge
/// read as smooth at icon sizes.
const SAMPLES: usize = 3;

/// Does this look like an SVG document rather than a raster image?
pub fn looks_like_svg(bytes: &[u8]) -> bool {
    let head = &bytes[..bytes.len().min(512)];
    let text = String::from_utf8_lossy(head);
    let text = text.trim_start();
    text.starts_with("<svg") || (text.starts_with("<?xml") && text.contains("<svg"))
}

/// The size an SVG asks to be drawn at: its `width`/`height`, else its
/// `viewBox`, else the 300×150 the spec falls back to.
pub fn intrinsic_size(source: &str) -> (usize, usize) {
    let dom = crate::html::parse(source.to_string());
    let Some(svg) = find_svg(&dom) else {
        return (300, 150);
    };
    let attr = |name: &str| element_of(svg).and_then(|e| attr_of(e, name)).cloned();
    let number = |name: &str| attr(name).and_then(|v| length(&v));
    if let (Some(w), Some(h)) = (number("width"), number("height")) {
        return (w.max(1.0) as usize, h.max(1.0) as usize);
    }
    match attr("viewBox").and_then(|v| view_box(&v)) {
        Some((_, _, w, h)) => (w.max(1.0) as usize, h.max(1.0) as usize),
        None => (300, 150),
    }
}

/// Rasterize `source` into a `width`×`height` RGBA image.
///
/// Without a font set, `<text>` is skipped: the engine never owns font bytes
/// (the embedder supplies them), and an image decoded before the page's fonts
/// are resolved has none to shape with.
pub fn rasterize(source: &str, width: usize, height: usize) -> Option<DecodedImage> {
    rasterize_with(source, width, height, None)
}

/// The same, with the fonts `<text>` needs.
pub fn rasterize_with(
    source: &str,
    width: usize,
    height: usize,
    fonts: Option<&FontSet>,
) -> Option<DecodedImage> {
    let (width, height) = (width.clamp(1, 2048), height.clamp(1, 2048));
    let dom = crate::html::parse(source.to_string());
    let svg = find_svg(&dom)?;
    let elem = element_of(svg)?;

    // The viewBox is the coordinate system the shapes are written in; the image
    // is what we are drawing into. Everything else is that one scale factor.
    let (vx, vy, vw, vh) = attr_of(elem, "viewBox")
        .and_then(|v| view_box(v))
        .or_else(|| {
            let w = attr_of(elem, "width").and_then(|v| length(v))?;
            let h = attr_of(elem, "height").and_then(|v| length(v))?;
            Some((0.0, 0.0, w, h))
        })
        .unwrap_or((0.0, 0.0, 300.0, 150.0));
    if vw <= 0.0 || vh <= 0.0 {
        return None;
    }
    // Uniform scale, centred — the default `preserveAspectRatio`.
    let scale = (width as f32 / vw).min(height as f32 / vh);
    let view = View {
        scale,
        matrix: Mat {
            a: scale,
            b: 0.0,
            c: 0.0,
            d: scale,
            e: (width as f32 - vw * scale) / 2.0 - vx * scale,
            f: (height as f32 - vh * scale) / 2.0 - vy * scale,
        },
    };

    let mut canvas = vec![
        Color {
            r: 0,
            g: 0,
            b: 0,
            a: 0
        };
        width * height
    ];
    // Every reference in the document — a gradient, a symbol, a clip path — is
    // an id lookup, so they share one index built in a single walk.
    let defs = Defs::index(&dom);
    let mut ctx = Ctx {
        canvas: &mut canvas,
        width,
        height,
        view,
        defs: &defs,
        fonts,
        clip: None,
        depth: 0,
    };
    // The root `<svg>` carries presentation attributes like any other element —
    // `fill="none" stroke="…"` on the root is how icon sets state a line style
    // once for every path inside. Skipping it filled every one of them black.
    let inherited = Paint::root().with(elem);
    draw_children(svg, &mut ctx, inherited);
    Some(DecodedImage {
        width,
        height,
        pixels: canvas,
    })
}

/// The mapping from user units to pixels.
///
/// A full matrix, not a scale and an offset, because an element's `transform`
/// may rotate or skew — and a rotated icon is exactly what a chevron or a
/// spinner is.
#[derive(Clone, Copy)]
struct View {
    /// How much a length grows, for stroke widths and blur radii, which are
    /// scalars and have no direction to be transformed.
    scale: f32,
    matrix: Mat,
}

impl View {
    fn point(&self, (x, y): (f32, f32)) -> (f32, f32) {
        self.matrix.apply(x, y)
    }

    /// This view with `inner` applied first — an element's own `transform`,
    /// which composes with whatever its ancestors already did.
    fn with(&self, inner: Mat) -> View {
        let matrix = self.matrix.then(inner);
        View {
            // The scale a stroke width and a blur are measured in. A rotation
            // keeps it; a non-uniform scale does not have one, so the average of
            // the two axes stands in.
            scale: ((matrix.a * matrix.a + matrix.b * matrix.b).sqrt()
                + (matrix.c * matrix.c + matrix.d * matrix.d).sqrt())
                / 2.0,
            matrix,
        }
    }
}

struct Ctx<'a> {
    canvas: &'a mut Vec<Color>,
    width: usize,
    height: usize,
    view: View,
    defs: &'a Defs<'a>,
    fonts: Option<&'a FontSet<'a>>,
    /// Per-pixel coverage a `clip-path` or `mask` allows, or `None` for all of
    /// it. Multiplied into every plot, which is what makes clipping one test
    /// rather than a second geometry pass.
    clip: Option<Vec<f32>>,
    /// How deep `<use>` and filters have recursed, so a document that
    /// references itself stops rather than running out of stack.
    depth: usize,
}

/// The limit on `<use>` and filter nesting. Icon sprites are one level deep; a
/// document that goes further than this is referencing itself.
const MAX_DEPTH: usize = 12;

/// Everything in the document that has an `id`, so a reference can find it.
struct Defs<'a> {
    by_id: HashMap<String, &'a Node>,
}

impl<'a> Defs<'a> {
    fn index(root: &'a Node) -> Defs<'a> {
        let mut by_id = HashMap::new();
        collect_ids(root, &mut by_id);
        Defs { by_id }
    }

    /// Resolve a `url(#id)` or a bare `#id` reference.
    fn get(&self, reference: &str) -> Option<&'a Node> {
        let reference = reference.trim();
        let inner = reference
            .strip_prefix("url(")
            .and_then(|rest| rest.strip_suffix(')'))
            .unwrap_or(reference)
            .trim()
            .trim_matches(['"', '\'']);
        let id = inner.strip_prefix('#')?;
        self.by_id.get(id).copied()
    }
}

fn collect_ids<'a>(node: &'a Node, out: &mut HashMap<String, &'a Node>) {
    if let NodeType::Element(ref e) = node.node_type {
        if let Some(id) = attr_of(e, "id") {
            out.entry(id.clone()).or_insert(node);
        }
    }
    for child in &node.children {
        collect_ids(child, out);
    }
}

/// Where a fill or a stroke gets its colour.
///
/// A paint server is kept as the reference it was written as, not resolved here:
/// resolving it needs the shape's own bounding box, which only exists once the
/// geometry does.
#[derive(Clone, PartialEq)]
enum Brush {
    /// `fill="none"` — not painted at all, which is different from missing.
    None,
    Solid(Color),
    /// `url(#gradient)` / `url(#pattern)`.
    Server(String),
}

/// Painting state, which inherits down the tree the way SVG says it does.
#[derive(Clone)]
struct Paint {
    fill: Brush,
    stroke: Brush,
    stroke_width: f32,
    opacity: f32,
    font_size: f32,
    font_family: String,
    /// `text-anchor`: where `x` sits relative to the text.
    anchor: Anchor,
}

#[derive(Clone, Copy, PartialEq)]
enum Anchor {
    Start,
    Middle,
    End,
}

impl Paint {
    /// SVG's initial state: black fill, no stroke.
    fn root() -> Paint {
        Paint {
            fill: Brush::Solid(Color {
                r: 0,
                g: 0,
                b: 0,
                a: 255,
            }),
            stroke: Brush::None,
            stroke_width: 1.0,
            opacity: 1.0,
            // SVG's initial font-size is `medium`, which is 16px like everywhere
            // else in this engine.
            font_size: 16.0,
            font_family: String::new(),
            anchor: Anchor::Start,
        }
    }

    /// This element's own presentation attributes over what it inherited.
    fn with(self, elem: &crate::dom::ElementData) -> Paint {
        let mut paint = self;
        // `style="fill:red"` beats the attribute, as it does in CSS.
        let from_style = |name: &str| {
            let style = attr_of(elem, "style")?;
            style
                .split(';')
                .filter_map(|d| d.split_once(':'))
                .find(|(key, _)| key.trim() == name)
                .map(|(_, value)| value.trim().to_string())
        };
        let attr = |name: &str| from_style(name).or_else(|| attr_of(elem, name).cloned());

        if let Some(fill) = attr("fill") {
            paint.fill = brush(&fill);
        }
        if let Some(stroke) = attr("stroke") {
            paint.stroke = brush(&stroke);
        }
        if let Some(width) = attr("stroke-width").and_then(|v| length(&v)) {
            paint.stroke_width = width;
        }
        if let Some(opacity) = attr("opacity").and_then(|v| v.trim().parse::<f32>().ok()) {
            paint.opacity *= opacity.clamp(0.0, 1.0);
        }
        if let Some(size) = attr("font-size").and_then(|v| length(&v)) {
            paint.font_size = size;
        }
        if let Some(family) = attr("font-family") {
            paint.font_family = family;
        }
        if let Some(anchor) = attr("text-anchor") {
            paint.anchor = match anchor.trim() {
                "middle" => Anchor::Middle,
                "end" => Anchor::End,
                _ => Anchor::Start,
            };
        }
        paint
    }
}

/// A `fill`/`stroke` value: a colour, a paint server reference, or nothing.
fn brush(text: &str) -> Brush {
    let text = text.trim();
    if text.starts_with("url(") {
        // A fallback colour may follow the reference: `url(#g) red`.
        return Brush::Server(text.to_string());
    }
    match paint_color(text) {
        Some(color) => Brush::Solid(color),
        None => Brush::None,
    }
}

/// `none` means "do not paint this", which is different from a missing value.
fn paint_color(text: &str) -> Option<Color> {
    let text = text.trim();
    if text.eq_ignore_ascii_case("none") || text.eq_ignore_ascii_case("transparent") {
        return None;
    }
    // `currentColor` has no cascade to read here; black is the initial colour.
    if text.eq_ignore_ascii_case("currentcolor") {
        return Some(Color {
            r: 0,
            g: 0,
            b: 0,
            a: 255,
        });
    }
    match crate::css::parse_value(text) {
        Some(crate::css::Value::ColorValue(color)) => Some(color),
        _ => None,
    }
}

/// An attribute, however the document spelled its case.
///
/// SVG has camelCase attribute names (`viewBox`) and the HTML parser folds tag
/// and attribute names to lowercase, so a lookup by the spec's spelling alone
/// would never find them.
fn attr_of<'a>(elem: &'a crate::dom::ElementData, name: &str) -> Option<&'a String> {
    elem.attributes
        .get(name)
        .or_else(|| elem.attributes.get(&name.to_ascii_lowercase()))
}

fn element_of(node: &Node) -> Option<&crate::dom::ElementData> {
    match node.node_type {
        NodeType::Element(ref e) => Some(e),
        NodeType::Text(_) => None,
    }
}

fn find_svg(node: &Node) -> Option<&Node> {
    if matches!(node.node_type, NodeType::Element(ref e) if e.tag_name == "svg") {
        return Some(node);
    }
    node.children.iter().find_map(find_svg)
}

fn draw_children(parent: &Node, ctx: &mut Ctx, inherited: Paint) {
    for child in &parent.children {
        let Some(elem) = element_of(child) else {
            continue;
        };
        let paint = inherited.clone().with(elem);
        // A clip path, a mask or a filter applies to the whole element, so both
        // are set up around whatever it turns out to be rather than inside each
        // shape's case below.
        if let Some(mask) = clip_mask(ctx, elem) {
            let outer = ctx.clip.take();
            ctx.clip = Some(match &outer {
                // Nested clips intersect: a clipped child of a clipped parent
                // shows only where both allow it.
                Some(outer) => mask
                    .iter()
                    .zip(outer.iter())
                    .map(|(inner, outer)| inner * outer)
                    .collect(),
                None => mask,
            });
            draw_one(child, elem, ctx, paint);
            ctx.clip = outer;
            continue;
        }
        if let Some(filter) = attr_of(elem, "filter").and_then(|f| ctx.defs.get(f)) {
            draw_filtered(child, elem, filter, ctx, paint);
            continue;
        }
        draw_one(child, elem, ctx, paint);
    }
}

/// Draw an element through a filter: its subtree into a buffer of its own, the
/// filter chain over that, then the result composited.
fn draw_filtered(
    node: &Node,
    elem: &crate::dom::ElementData,
    filter: &Node,
    ctx: &mut Ctx,
    paint: Paint,
) {
    if ctx.depth > MAX_DEPTH {
        return;
    }
    let (width, height) = (ctx.width, ctx.height);
    let mut layer = vec![TRANSPARENT; width * height];
    {
        let mut inner = Ctx {
            canvas: &mut layer,
            width,
            height,
            view: ctx.view,
            defs: ctx.defs,
            fonts: ctx.fonts,
            clip: None,
            depth: ctx.depth + 1,
        };
        draw_one(node, elem, &mut inner, paint);
    }
    let shadow = filter_chain(filter, &layer, width, height, ctx.view.scale);
    // The filtered result goes under the element itself, which is what makes the
    // usual chain — blur, offset, tint, merge — read as a drop shadow rather
    // than as a blurred copy of the thing it belongs to.
    for source in [&shadow, &layer] {
        for y in 0..height {
            for x in 0..width {
                let pixel = source[y * width + x];
                if pixel.a > 0 {
                    blend(ctx, x, y, pixel, 1.0);
                }
            }
        }
    }
}

/// One element, once its clip and filter have been dealt with.
fn draw_one(child: &Node, elem: &crate::dom::ElementData, ctx: &mut Ctx, paint: Paint) {
    // An element's own `transform` scales and moves everything it draws. Kept as
    // a change to the view rather than applied per point, so it composes with
    // the enclosing one for free.
    let restore = match attr_of(elem, "transform") {
        Some(text) => {
            let previous = ctx.view;
            ctx.view = previous.with(svg_transform(text));
            Some(previous)
        }
        None => None,
    };
    draw_element(child, elem, ctx, paint);
    if let Some(previous) = restore {
        ctx.view = previous;
    }
}

fn draw_element(child: &Node, elem: &crate::dom::ElementData, ctx: &mut Ctx, paint: Paint) {
    match elem.tag_name.as_str() {
        // A group paints nothing itself; it only passes its state down.
        "g" | "svg" | "a" => draw_children(child, ctx, paint),
        // The standard way to build an icon sprite: one shape defined once,
        // referenced many times. Without this a sprite sheet draws nothing
        // at all.
        "use" => draw_use(child, elem, ctx, paint),
        "text" => draw_text(child, ctx, &paint),
        "rect" => {
            let get = |name: &str| number_attr(elem, name);
            let (x, y) = (get("x"), get("y"));
            let (w, h) = (get("width"), get("height"));
            if w > 0.0 && h > 0.0 {
                let rect = vec![(x, y), (x + w, y), (x + w, y + h), (x, y + h), (x, y)];
                fill_and_stroke(ctx, &[rect], &paint);
            }
        }
        "circle" => {
            let (cx, cy) = (number_attr(elem, "cx"), number_attr(elem, "cy"));
            let r = number_attr(elem, "r");
            if r > 0.0 {
                fill_and_stroke(ctx, &[ellipse(cx, cy, r, r)], &paint);
            }
        }
        "ellipse" => {
            let (cx, cy) = (number_attr(elem, "cx"), number_attr(elem, "cy"));
            let (rx, ry) = (number_attr(elem, "rx"), number_attr(elem, "ry"));
            if rx > 0.0 && ry > 0.0 {
                fill_and_stroke(ctx, &[ellipse(cx, cy, rx, ry)], &paint);
            }
        }
        "line" => {
            let line = vec![
                (number_attr(elem, "x1"), number_attr(elem, "y1")),
                (number_attr(elem, "x2"), number_attr(elem, "y2")),
            ];
            // A line has no interior, so it is stroke or nothing.
            stroke(ctx, &[line], &paint);
        }
        "polygon" | "polyline" => {
            let mut points = points_of(attr_of(elem, "points").map_or("", |v| v));
            if elem.tag_name == "polygon" {
                if let Some(first) = points.first().copied() {
                    points.push(first);
                }
            }
            if points.len() > 1 {
                match elem.tag_name.as_str() {
                    "polygon" => fill_and_stroke(ctx, &[points], &paint),
                    _ => stroke(ctx, &[points], &paint),
                }
            }
        }
        "path" => {
            if let Some(d) = attr_of(elem, "d") {
                let subpaths = flatten_path(d);
                if !subpaths.is_empty() {
                    fill_and_stroke(ctx, &subpaths, &paint);
                }
            }
        }
        // defs, style, title, filters, gradients: nothing to draw.
        _ => {}
    }
}

/// `<use href="#id">`: draw what it points at, here.
///
/// A `<symbol>` exists only to be used, so using one draws its children; any
/// other element is drawn as itself. `x`/`y` shift it, which is how one shape
/// becomes a row of them.
fn draw_use(node: &Node, elem: &crate::dom::ElementData, ctx: &mut Ctx, paint: Paint) {
    if ctx.depth > MAX_DEPTH {
        return; // a document that references itself, directly or in a ring
    }
    let Some(reference) = attr_of(elem, "href").or_else(|| attr_of(elem, "xlink:href")) else {
        return;
    };
    let Some(target) = ctx.defs.get(reference) else {
        return;
    };
    // A `<use>` of an ancestor of itself is the cycle the depth limit catches;
    // a `<use>` of itself is one that catching costs nothing.
    if std::ptr::eq(target, node) {
        return;
    }
    let (x, y) = (number_attr(elem, "x"), number_attr(elem, "y"));
    let previous = ctx.view;
    if x != 0.0 || y != 0.0 {
        ctx.view = previous.with(Mat::translate(x, y));
    }
    ctx.depth += 1;
    let Some(target_elem) = element_of(target) else {
        ctx.depth -= 1;
        ctx.view = previous;
        return;
    };
    let inherited = paint.with(target_elem);
    match target_elem.tag_name.as_str() {
        // A symbol is a definition, so what is used is its content.
        "symbol" => draw_children(target, ctx, inherited),
        _ => draw_one(target, target_elem, ctx, inherited),
    }
    ctx.depth -= 1;
    ctx.view = previous;
}

/// `<text>`: labels inside diagrams, charts and logos.
///
/// Routed through the same shaper page text uses, so Indic reordering,
/// conjuncts and mark positioning work inside an SVG exactly as they do in a
/// paragraph — writing a second, simpler text path here is how an engine ends up
/// with two different answers for the same script.
///
/// ponytail: `textPath` is not implemented. Placing glyphs along a curve needs
/// the path parameterized by arc length, which nothing else here wants; `x`,
/// `y`, `dx`, `dy` and `text-anchor` are.
fn draw_text(node: &Node, ctx: &mut Ctx, paint: &Paint) {
    let Some(fonts) = ctx.fonts else {
        return; // no font bytes to shape with; see `rasterize`
    };
    if fonts.entries.is_empty() {
        return;
    }
    let Some(elem) = element_of(node) else {
        return;
    };
    let origin = (
        number_attr(elem, "x") + number_attr(elem, "dx"),
        number_attr(elem, "y") + number_attr(elem, "dy"),
    );
    // A `<tspan>` may move the pen; anything else contributes its text where the
    // pen already is.
    let mut pen = origin;
    draw_text_spans(node, ctx, paint, &mut pen);
}

fn draw_text_spans(node: &Node, ctx: &mut Ctx, paint: &Paint, pen: &mut (f32, f32)) {
    for child in &node.children {
        match &child.node_type {
            crate::dom::NodeType::Text(text) => {
                // SVG collapses whitespace like HTML does by default.
                let collapsed = text.split_whitespace().collect::<Vec<&str>>().join(" ");
                let spaced = match text.starts_with(char::is_whitespace) && !collapsed.is_empty() {
                    true => format!(" {collapsed}"),
                    false => collapsed,
                };
                if !spaced.is_empty() {
                    draw_glyph_run(ctx, &spaced, pen, paint);
                }
            }
            crate::dom::NodeType::Element(elem) => {
                let own = paint.clone().with(elem);
                let mut span_pen = (
                    attr_of(elem, "x").and_then(|v| length(v)).unwrap_or(pen.0)
                        + attr_of(elem, "dx").and_then(|v| length(v)).unwrap_or(0.0),
                    attr_of(elem, "y").and_then(|v| length(v)).unwrap_or(pen.1)
                        + attr_of(elem, "dy").and_then(|v| length(v)).unwrap_or(0.0),
                );
                draw_text_spans(child, ctx, &own, &mut span_pen);
                *pen = span_pen;
            }
        }
    }
}

/// Shape one run and paint its glyph coverage with the current fill.
fn draw_glyph_run(ctx: &mut Ctx, text: &str, pen: &mut (f32, f32), paint: &Paint) {
    let Some(fonts) = ctx.fonts else { return };
    let families = crate::css::family_list(&paint.font_family);
    let index = fonts.pick_in(&families, text);
    let Some(entry) = fonts.entries.get(index) else {
        return;
    };
    // Shaped at the device size, so the glyphs are rasterized at the size they
    // are drawn rather than scaled up from user units.
    let size = paint.font_size * ctx.view.scale;
    if size <= 0.0 {
        return;
    }
    let (glyphs, advance) = crate::text::shape_run(entry, text, size);
    let Some(raster) = entry.raster() else {
        return;
    };
    let ascent = raster
        .horizontal_line_metrics(size)
        .map_or(size, |m| m.ascent);
    let (ox, oy) = ctx.view.point(*pen);
    // `text-anchor` moves the whole run relative to the point given.
    let shift = match paint.anchor {
        Anchor::Start => 0.0,
        Anchor::Middle => -advance / 2.0,
        Anchor::End => -advance,
    };
    let shading = match &paint.fill {
        Brush::None => return,
        Brush::Solid(color) => Shading::Solid(*color),
        // A gradient-filled label needs the run's box, which is known now.
        server => {
            let box_points = vec![vec![
                (ox + shift, oy - ascent),
                (ox + shift + advance, oy - ascent),
                (ox + shift + advance, oy),
                (ox + shift, oy),
            ]];
            resolve_brush(ctx, server, &box_points)
        }
    };
    for glyph in &glyphs {
        let (metrics, coverage) = raster.rasterize_indexed(glyph.id, size);
        let gx = (ox + shift + glyph.x + metrics.xmin as f32).round() as i32;
        let gy = (oy - glyph.y - metrics.ymin as f32 - metrics.height as f32).round() as i32;
        for row in 0..metrics.height {
            for col in 0..metrics.width {
                let alpha = coverage[row * metrics.width + col];
                if alpha == 0 {
                    continue;
                }
                let (px, py) = (gx + col as i32, gy + row as i32);
                if px < 0 || py < 0 {
                    continue;
                }
                let color = shading.color_at(px as f32 + 0.5, py as f32 + 0.5);
                blend(
                    ctx,
                    px as usize,
                    py as usize,
                    color,
                    alpha as f32 / 255.0 * paint.opacity,
                );
            }
        }
    }
    // The pen advances in user units, which is what the next span continues from.
    pen.0 += advance / ctx.view.scale.max(f32::EPSILON);
}

/// The coverage a `clip-path` or `mask` on this element allows, if any.
///
/// A clipped illustration that renders unclipped is usually worse than one that
/// does not render at all, which is why this is worth having before the fancier
/// paint servers.
///
/// ponytail: a `<mask>`'s shapes contribute their coverage, not their luminance,
/// so a mask painted in shades of grey clips to its shape rather than fading.
/// Luminance masking needs the mask rendered to a buffer first, which is the
/// filter path's machinery.
fn clip_mask(ctx: &mut Ctx, elem: &crate::dom::ElementData) -> Option<Vec<f32>> {
    let reference = attr_of(elem, "clip-path")
        .or_else(|| attr_of(elem, "mask"))
        .or_else(|| attr_of(elem, "clippath"))?;
    let node = ctx.defs.get(reference)?;
    let clip_elem = element_of(node)?;
    if !matches!(clip_elem.tag_name.as_str(), "clippath" | "mask") {
        return None;
    }
    // `clipPathUnits="objectBoundingBox"` would need the shape's own box, which
    // is not known before its geometry is; `userSpaceOnUse` is the default and
    // what clip paths are nearly always written in.
    let mut mask = vec![0.0f32; ctx.width * ctx.height];
    let (width, height) = (ctx.width, ctx.height);
    let mut any = false;
    for polygons in clip_shapes(node, ctx.view, ctx.defs, 0) {
        any = true;
        scan_polygons(&polygons, width, height, &mut |x, y, coverage| {
            let slot = &mut mask[y * width + x];
            *slot = slot.max(coverage);
        });
    }
    // An empty clip path clips everything away, per spec — but so does a clip
    // path we failed to read, and those are not the same thing. Only a clip we
    // actually found shapes for is honoured.
    any.then_some(mask)
}

/// Every shape a clip path contains, in pixel coordinates.
fn clip_shapes(node: &Node, view: View, defs: &Defs, depth: usize) -> Vec<Vec<Vec<(f32, f32)>>> {
    let mut out = Vec::new();
    if depth > MAX_DEPTH {
        return out;
    }
    for child in &node.children {
        let Some(elem) = element_of(child) else {
            continue;
        };
        let view = match attr_of(elem, "transform") {
            Some(text) => view.with(svg_transform(text)),
            None => view,
        };
        let user: Vec<Vec<(f32, f32)>> = match elem.tag_name.as_str() {
            "rect" => {
                let get = |name: &str| number_attr(elem, name);
                let (x, y, w, h) = (get("x"), get("y"), get("width"), get("height"));
                match w > 0.0 && h > 0.0 {
                    true => vec![vec![(x, y), (x + w, y), (x + w, y + h), (x, y + h)]],
                    false => Vec::new(),
                }
            }
            "circle" => {
                let r = number_attr(elem, "r");
                match r > 0.0 {
                    true => vec![ellipse(
                        number_attr(elem, "cx"),
                        number_attr(elem, "cy"),
                        r,
                        r,
                    )],
                    false => Vec::new(),
                }
            }
            "ellipse" => {
                let (rx, ry) = (number_attr(elem, "rx"), number_attr(elem, "ry"));
                match rx > 0.0 && ry > 0.0 {
                    true => vec![ellipse(
                        number_attr(elem, "cx"),
                        number_attr(elem, "cy"),
                        rx,
                        ry,
                    )],
                    false => Vec::new(),
                }
            }
            "polygon" => vec![points_of(attr_of(elem, "points").map_or("", |v| v))],
            "path" => attr_of(elem, "d")
                .map(|d| flatten_path(d))
                .unwrap_or_default(),
            // A clip path may `<use>` a shape defined elsewhere.
            "use" => {
                let target = attr_of(elem, "href")
                    .or_else(|| attr_of(elem, "xlink:href"))
                    .and_then(|r| defs.get(r));
                if let Some(target) = target {
                    out.extend(clip_shapes(target, view, defs, depth + 1));
                }
                Vec::new()
            }
            _ => Vec::new(),
        };
        let device: Vec<Vec<(f32, f32)>> = user
            .iter()
            .filter(|points| points.len() > 2)
            .map(|points| points.iter().map(|p| view.point(*p)).collect())
            .collect();
        if !device.is_empty() {
            out.push(device);
        }
    }
    out
}

/// Apply a `<filter>`'s primitives to an already-rendered buffer.
///
/// The chain that matters is the drop shadow: blur the alpha, offset it, tint
/// it, and put it under the original. The full filter graph — named results,
/// arbitrary inputs, colour matrices — is a much larger job and is not this.
fn filter_chain(
    filter: &Node,
    source: &[Color],
    width: usize,
    height: usize,
    scale: f32,
) -> Vec<Color> {
    let mut buffer = source.to_vec();
    let mut flood: Option<Color> = None;
    for child in &filter.children {
        let Some(elem) = element_of(child) else {
            continue;
        };
        match elem.tag_name.as_str() {
            "fegaussianblur" => {
                let deviation = attr_of(elem, "stdDeviation")
                    .and_then(|v| length(v))
                    .unwrap_or(0.0);
                crate::paint::blur_pixels(&mut buffer, width, height, deviation * scale);
            }
            "feoffset" => {
                let dx = (number_attr(elem, "dx") * scale).round() as isize;
                let dy = (number_attr(elem, "dy") * scale).round() as isize;
                buffer = offset_pixels(&buffer, width, height, dx, dy);
            }
            // `feFlood` in a shadow chain is the shadow's colour, kept for the
            // `feComposite`/`feMerge` that tints the blurred alpha with it.
            "feflood" => {
                flood = attr_of(elem, "flood-color")
                    .and_then(|v| paint_color(v))
                    .or(Some(Color {
                        r: 0,
                        g: 0,
                        b: 0,
                        a: 255,
                    }));
            }
            "fecomposite" | "feblend" | "femerge" | "femergenode" => {}
            _ => {}
        }
    }
    if let Some(tint) = flood {
        for pixel in buffer.iter_mut() {
            *pixel = Color {
                r: tint.r,
                g: tint.g,
                b: tint.b,
                a: (pixel.a as u32 * tint.a as u32 / 255) as u8,
            };
        }
    }
    buffer
}

/// Shift a buffer, leaving transparency behind.
fn offset_pixels(
    source: &[Color],
    width: usize,
    height: usize,
    dx: isize,
    dy: isize,
) -> Vec<Color> {
    let mut out = vec![TRANSPARENT; source.len()];
    for y in 0..height as isize {
        for x in 0..width as isize {
            let (sx, sy) = (x - dx, y - dy);
            if sx < 0 || sy < 0 || sx >= width as isize || sy >= height as isize {
                continue;
            }
            out[y as usize * width + x as usize] = source[sy as usize * width + sx as usize];
        }
    }
    out
}

fn number_attr(elem: &crate::dom::ElementData, name: &str) -> f32 {
    attr_of(elem, name).and_then(|v| length(v)).unwrap_or(0.0)
}

/// A length in user units. `px`, `pt` and bare numbers all end up the same
/// here; percentages of an unknown box do not, so they are refused.
fn length(text: &str) -> Option<f32> {
    let text = text.trim();
    if text.ends_with('%') {
        return None;
    }
    let number: String = text
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.' || *c == '-' || *c == '+' || *c == 'e')
        .collect();
    number.parse().ok()
}

fn view_box(text: &str) -> Option<(f32, f32, f32, f32)> {
    let parts: Vec<f32> = text
        .split([',', ' ', '\t', '\n'])
        .filter(|p| !p.is_empty())
        .filter_map(|p| p.parse().ok())
        .collect();
    match parts[..] {
        [x, y, w, h] => Some((x, y, w, h)),
        _ => None,
    }
}

fn points_of(text: &str) -> Vec<(f32, f32)> {
    let numbers: Vec<f32> = text
        .split([',', ' ', '\t', '\n', '\r'])
        .filter(|p| !p.is_empty())
        .filter_map(|p| p.parse().ok())
        .collect();
    numbers
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| (pair[0], pair[1]))
        .collect()
}

/// A circle or ellipse as a closed polygon. 64 segments is under a third of a
/// pixel of error at any icon size.
fn ellipse(cx: f32, cy: f32, rx: f32, ry: f32) -> Vec<(f32, f32)> {
    const STEPS: usize = 64;
    (0..=STEPS)
        .map(|i| {
            let angle = i as f32 / STEPS as f32 * std::f32::consts::TAU;
            (cx + rx * angle.cos(), cy + ry * angle.sin())
        })
        .collect()
}

// --- path data -------------------------------------------------------------

/// Turn a `d` attribute into subpaths of straight segments.
///
/// Curves are flattened here rather than at fill time, so the rasterizer only
/// ever sees polygons and the two stay independent.
fn flatten_path(d: &str) -> Vec<Vec<(f32, f32)>> {
    let mut out: Vec<Vec<(f32, f32)>> = Vec::new();
    let mut current: Vec<(f32, f32)> = Vec::new();
    let mut cursor = (0.0f32, 0.0f32);
    let mut start = (0.0f32, 0.0f32);
    // The reflection point for a smooth curve continuation (`S`/`T`).
    let mut last_control: Option<(f32, f32)> = None;

    for (command, args) in path_commands(d) {
        let absolute = command.is_ascii_uppercase();
        let at = |cursor: (f32, f32), x: f32, y: f32| match absolute {
            true => (x, y),
            false => (cursor.0 + x, cursor.1 + y),
        };
        match command.to_ascii_uppercase() {
            'M' => {
                for (i, pair) in args.as_chunks::<2>().0.iter().enumerate() {
                    let point = at(cursor, pair[0], pair[1]);
                    if i == 0 {
                        // A new subpath starts here; the previous one ends.
                        if current.len() > 1 {
                            out.push(std::mem::take(&mut current));
                        } else {
                            current.clear();
                        }
                        start = point;
                        current.push(point);
                    } else {
                        // Extra pairs after a moveto are implicit linetos.
                        current.push(point);
                    }
                    cursor = point;
                }
                last_control = None;
            }
            'L' => {
                for pair in args.as_chunks::<2>().0 {
                    cursor = at(cursor, pair[0], pair[1]);
                    current.push(cursor);
                }
                last_control = None;
            }
            'H' => {
                for x in &args {
                    cursor = match absolute {
                        true => (*x, cursor.1),
                        false => (cursor.0 + x, cursor.1),
                    };
                    current.push(cursor);
                }
                last_control = None;
            }
            'V' => {
                for y in &args {
                    cursor = match absolute {
                        true => (cursor.0, *y),
                        false => (cursor.0, cursor.1 + y),
                    };
                    current.push(cursor);
                }
                last_control = None;
            }
            'C' | 'S' => {
                let stride = if command.eq_ignore_ascii_case(&'C') {
                    6
                } else {
                    4
                };
                for group in args.chunks_exact(stride) {
                    let (c1, c2, end) = match stride {
                        6 => (
                            at(cursor, group[0], group[1]),
                            at(cursor, group[2], group[3]),
                            at(cursor, group[4], group[5]),
                        ),
                        _ => {
                            // `S` mirrors the previous control point about the cursor.
                            let mirrored = match last_control {
                                Some((cx, cy)) => (2.0 * cursor.0 - cx, 2.0 * cursor.1 - cy),
                                None => cursor,
                            };
                            (
                                mirrored,
                                at(cursor, group[0], group[1]),
                                at(cursor, group[2], group[3]),
                            )
                        }
                    };
                    flatten_cubic(cursor, c1, c2, end, &mut current);
                    last_control = Some(c2);
                    cursor = end;
                }
            }
            'Q' | 'T' => {
                let stride = if command.eq_ignore_ascii_case(&'Q') {
                    4
                } else {
                    2
                };
                for group in args.chunks_exact(stride) {
                    let (control, end) = match stride {
                        4 => (
                            at(cursor, group[0], group[1]),
                            at(cursor, group[2], group[3]),
                        ),
                        _ => {
                            let mirrored = match last_control {
                                Some((cx, cy)) => (2.0 * cursor.0 - cx, 2.0 * cursor.1 - cy),
                                None => cursor,
                            };
                            (mirrored, at(cursor, group[0], group[1]))
                        }
                    };
                    // A quadratic is a cubic whose controls are two thirds along.
                    let c1 = (
                        cursor.0 + 2.0 / 3.0 * (control.0 - cursor.0),
                        cursor.1 + 2.0 / 3.0 * (control.1 - cursor.1),
                    );
                    let c2 = (
                        end.0 + 2.0 / 3.0 * (control.0 - end.0),
                        end.1 + 2.0 / 3.0 * (control.1 - end.1),
                    );
                    flatten_cubic(cursor, c1, c2, end, &mut current);
                    last_control = Some(control);
                    cursor = end;
                }
            }
            // `rx ry x-rotation large-arc sweep x y`. Icons are full of these —
            // every rounded corner drawn as a path, every refresh swirl — so an
            // arc is walked rather than cut across.
            'A' => {
                for group in args.as_chunks::<7>().0 {
                    let end = at(cursor, group[5], group[6]);
                    flatten_arc(
                        cursor,
                        (group[0], group[1]),
                        group[2],
                        group[3] != 0.0,
                        group[4] != 0.0,
                        end,
                        &mut current,
                    );
                    cursor = end;
                }
                last_control = None;
            }
            'Z' => {
                current.push(start);
                if current.len() > 1 {
                    out.push(std::mem::take(&mut current));
                } else {
                    current.clear();
                }
                cursor = start;
                current.push(start);
                last_control = None;
            }
            _ => {}
        }
    }
    if current.len() > 1 {
        out.push(current);
    }
    out
}

/// Split path data into `(command, numbers)`, which is all the grammar we need.
fn path_commands(d: &str) -> Vec<(char, Vec<f32>)> {
    let mut out = Vec::new();
    let mut command = None;
    let mut number = String::new();
    let mut args: Vec<f32> = Vec::new();

    let flush = |number: &mut String, args: &mut Vec<f32>| {
        if let Ok(value) = number.parse::<f32>() {
            args.push(value);
        }
        number.clear();
    };
    for c in d.chars() {
        match c {
            // `e` part-way through a number is an exponent, not the arc command.
            'e' | 'E' if !number.is_empty() => number.push(c),
            'a'..='z' | 'A'..='Z' => {
                flush(&mut number, &mut args);
                if let Some(previous) = command.take() {
                    out.push((previous, std::mem::take(&mut args)));
                }
                command = Some(c);
            }
            ',' | ' ' | '\t' | '\n' | '\r' => flush(&mut number, &mut args),
            // A sign starts a new number unless it is an exponent's sign.
            '-' | '+' if !number.is_empty() && !number.ends_with(['e', 'E']) => {
                flush(&mut number, &mut args);
                number.push(c);
            }
            // So does a second point: `3.87.56` is two coordinates, which is how
            // every tool that exports an icon writes them. Read as one number it
            // parses as nothing, and dropping it shifts every pair after it —
            // the path stays a path, but it is the wrong shape.
            '.' if number.contains('.') => {
                flush(&mut number, &mut args);
                number.push(c);
            }
            _ => number.push(c),
        }
    }
    flush(&mut number, &mut args);
    if let Some(previous) = command {
        out.push((previous, args));
    }
    out
}

/// An elliptical arc as line segments.
///
/// SVG states an arc by where it *ends* ("curve to here, bending like this"),
/// but sampling one needs the centre it turns around. Recovering that centre is
/// the spec's F.6.5, and it is the whole of the arithmetic below. The spec's own
/// degenerate cases — a zero radius, or an arc ending where it began — are a
/// straight line rather than an error.
fn flatten_arc(
    from: (f32, f32),
    (rx, ry): (f32, f32),
    rotation: f32,
    large: bool,
    sweep: bool,
    to: (f32, f32),
    out: &mut Vec<(f32, f32)>,
) {
    let (mut rx, mut ry) = (rx.abs(), ry.abs());
    let degenerate = rx < f32::EPSILON
        || ry < f32::EPSILON
        || ((from.0 - to.0).abs() < f32::EPSILON && (from.1 - to.1).abs() < f32::EPSILON);
    if degenerate {
        out.push(to);
        return;
    }
    let (sin, cos) = rotation.to_radians().sin_cos();
    // Rotate the chord into the ellipse's own frame, where it is axis-aligned.
    let (dx, dy) = ((from.0 - to.0) / 2.0, (from.1 - to.1) / 2.0);
    let x1 = cos * dx + sin * dy;
    let y1 = -sin * dx + cos * dy;
    // Radii too small to reach across the chord are scaled up until they just do.
    let lambda = x1 * x1 / (rx * rx) + y1 * y1 / (ry * ry);
    if lambda > 1.0 {
        rx *= lambda.sqrt();
        ry *= lambda.sqrt();
    }
    // Of the two ellipses through both endpoints, the flags pick which centre.
    let numerator = (rx * rx * ry * ry - rx * rx * y1 * y1 - ry * ry * x1 * x1).max(0.0);
    let denominator = rx * rx * y1 * y1 + ry * ry * x1 * x1;
    let mut scale = (numerator / denominator).sqrt();
    if large == sweep {
        scale = -scale;
    }
    let (cx1, cy1) = (scale * rx * y1 / ry, -scale * ry * x1 / rx);
    let cx = cos * cx1 - sin * cy1 + (from.0 + to.0) / 2.0;
    let cy = sin * cx1 + cos * cy1 + (from.1 + to.1) / 2.0;

    let start = ((y1 - cy1) / ry).atan2((x1 - cx1) / rx);
    let finish = ((-y1 - cy1) / ry).atan2((-x1 - cx1) / rx);
    // `sweep` says which way round; without it the short way is not the one asked for.
    let mut delta = finish - start;
    if !sweep && delta > 0.0 {
        delta -= std::f32::consts::TAU;
    } else if sweep && delta < 0.0 {
        delta += std::f32::consts::TAU;
    }
    // One segment per ~6°, the error budget `ellipse` already spends.
    let steps = (delta.abs() / (std::f32::consts::TAU / 64.0))
        .ceil()
        .max(1.0) as usize;
    for step in 1..=steps {
        let angle = start + delta * step as f32 / steps as f32;
        let (s, c) = angle.sin_cos();
        out.push((
            cx + cos * rx * c - sin * ry * s,
            cy + sin * rx * c + cos * ry * s,
        ));
    }
}

/// A cubic Bézier as line segments. 16 steps holds under half a pixel at icon
/// sizes, and costs nothing next to the fill.
fn flatten_cubic(
    from: (f32, f32),
    c1: (f32, f32),
    c2: (f32, f32),
    to: (f32, f32),
    out: &mut Vec<(f32, f32)>,
) {
    const STEPS: usize = 16;
    for i in 1..=STEPS {
        let t = i as f32 / STEPS as f32;
        let u = 1.0 - t;
        let point = |a: f32, b: f32, c: f32, d: f32| {
            u * u * u * a + 3.0 * u * u * t * b + 3.0 * u * t * t * c + t * t * t * d
        };
        out.push((
            point(from.0, c1.0, c2.0, to.0),
            point(from.1, c1.1, c2.1, to.1),
        ));
    }
}

// --- paint servers ---------------------------------------------------------

/// Where a pixel's colour comes from while a shape is being filled.
///
/// Solid is the common case and costs nothing; the others exist because a logo
/// or an illustration is mostly gradients, and a shape filled flat where the
/// author asked for a gradient reads as a mistake rather than a simplification.
enum Shading {
    Solid(Color),
    /// A gradient in its own coordinate space, plus the inverse of the map from
    /// that space to pixels — so a pixel is turned back into a gradient
    /// coordinate rather than the gradient being resampled per shape.
    Linear {
        from: (f32, f32),
        to: (f32, f32),
        stops: Vec<(f32, Color)>,
        inverse: Mat,
    },
    Radial {
        centre: (f32, f32),
        radius: f32,
        stops: Vec<(f32, Color)>,
        inverse: Mat,
    },
    /// A tile, already rasterized, repeated across the shape.
    Tile {
        image: DecodedImage,
        origin: (f32, f32),
        step: (f32, f32),
    },
}

impl Shading {
    fn color_at(&self, x: f32, y: f32) -> Color {
        match self {
            Shading::Solid(color) => *color,
            Shading::Linear {
                from,
                to,
                stops,
                inverse,
            } => {
                let (ux, uy) = inverse.apply(x, y);
                // How far along the gradient vector this point projects.
                let (dx, dy) = (to.0 - from.0, to.1 - from.1);
                let length_squared = dx * dx + dy * dy;
                let t = match length_squared > 0.0 {
                    true => ((ux - from.0) * dx + (uy - from.1) * dy) / length_squared,
                    false => 0.0,
                };
                stop_color(stops, t)
            }
            Shading::Radial {
                centre,
                radius,
                stops,
                inverse,
            } => {
                let (ux, uy) = inverse.apply(x, y);
                let (dx, dy) = (ux - centre.0, uy - centre.1);
                let t = match *radius > 0.0 {
                    true => (dx * dx + dy * dy).sqrt() / radius,
                    false => 1.0,
                };
                stop_color(stops, t)
            }
            Shading::Tile {
                image,
                origin,
                step,
            } => {
                if step.0 <= 0.0 || step.1 <= 0.0 || image.width == 0 || image.height == 0 {
                    return TRANSPARENT;
                }
                let tx = (x - origin.0).rem_euclid(step.0) / step.0;
                let ty = (y - origin.1).rem_euclid(step.1) / step.1;
                let px = ((tx * image.width as f32) as usize).min(image.width - 1);
                let py = ((ty * image.height as f32) as usize).min(image.height - 1);
                image.pixels[py * image.width + px]
            }
        }
    }
}

const TRANSPARENT: Color = Color {
    r: 0,
    g: 0,
    b: 0,
    a: 0,
};

/// The colour at `t` along a stop list, which is a straight interpolation
/// between the two stops it falls between and the end colours outside them.
fn stop_color(stops: &[(f32, Color)], t: f32) -> Color {
    let Some((first_at, first)) = stops.first().copied() else {
        return TRANSPARENT;
    };
    if t <= first_at {
        return first;
    }
    let (last_at, last) = stops.last().copied().expect("non-empty");
    if t >= last_at {
        return last;
    }
    for pair in stops.windows(2) {
        let (a_at, a) = pair[0];
        let (b_at, b) = pair[1];
        if t < a_at || t > b_at {
            continue;
        }
        let span = b_at - a_at;
        let local = match span > 0.0 {
            true => (t - a_at) / span,
            false => 0.0,
        };
        let mix = |x: u8, y: u8| {
            (x as f32 + (y as f32 - x as f32) * local)
                .round()
                .clamp(0.0, 255.0) as u8
        };
        return Color {
            r: mix(a.r, b.r),
            g: mix(a.g, b.g),
            b: mix(a.b, b.b),
            a: mix(a.a, b.a),
        };
    }
    last
}

/// Turn a brush into something that can colour a pixel.
///
/// `polygons` are the shape being filled, in pixel coordinates — needed because
/// `objectBoundingBox` units, which are the default, are fractions of it.
fn resolve_brush(ctx: &mut Ctx, brush: &Brush, polygons: &[Vec<(f32, f32)>]) -> Shading {
    let fallback = Color {
        r: 0,
        g: 0,
        b: 0,
        a: 255,
    };
    match brush {
        Brush::None => Shading::Solid(TRANSPARENT),
        Brush::Solid(color) => Shading::Solid(*color),
        Brush::Server(reference) => {
            // `fill="url(#g) red"` names a fallback for exactly this case.
            let written_fallback = reference
                .rsplit(')')
                .next()
                .and_then(|rest| paint_color(rest.trim()));
            match ctx
                .defs
                .get(reference)
                .and_then(|node| paint_server(ctx, node, polygons))
            {
                Some(shading) => shading,
                // A reference that resolves to nothing: better a visible shape
                // than a hole where the logo was.
                None => Shading::Solid(written_fallback.unwrap_or(fallback)),
            }
        }
    }
}

fn paint_server(ctx: &mut Ctx, node: &Node, polygons: &[Vec<(f32, f32)>]) -> Option<Shading> {
    let elem = element_of(node)?;
    match elem.tag_name.as_str() {
        "lineargradient" | "radialgradient" => gradient(ctx, node, polygons),
        "pattern" => pattern(ctx, node, polygons),
        _ => None,
    }
}

/// The bounding box of a shape in pixel coordinates.
fn bounds_of(polygons: &[Vec<(f32, f32)>]) -> Option<(f32, f32, f32, f32)> {
    let mut bounds: Option<(f32, f32, f32, f32)> = None;
    for point in polygons.iter().flatten() {
        bounds = Some(match bounds {
            None => (point.0, point.1, point.0, point.1),
            Some((x0, y0, x1, y1)) => (
                x0.min(point.0),
                y0.min(point.1),
                x1.max(point.0),
                y1.max(point.1),
            ),
        });
    }
    bounds
}

/// The map from a paint server's own coordinates to pixels.
///
/// `objectBoundingBox` — the default — makes every coordinate a fraction of the
/// shape's box, which is what lets one gradient definition serve shapes of
/// different sizes. `userSpaceOnUse` puts them in the document's own units.
fn units_matrix(
    ctx: &Ctx,
    elem: &crate::dom::ElementData,
    attribute: &str,
    polygons: &[Vec<(f32, f32)>],
) -> Option<Mat> {
    let user_space = attr_of(elem, attribute)
        .map(|v| v.trim() == "userSpaceOnUse")
        .unwrap_or(false);
    if user_space {
        return Some(ctx.view.matrix);
    }
    let (x0, y0, x1, y1) = bounds_of(polygons)?;
    let (w, h) = (x1 - x0, y1 - y0);
    if w <= 0.0 || h <= 0.0 {
        return None;
    }
    Some(Mat {
        a: w,
        b: 0.0,
        c: 0.0,
        d: h,
        e: x0,
        f: y0,
    })
}

/// `<linearGradient>` / `<radialGradient>`.
fn gradient(ctx: &mut Ctx, node: &Node, polygons: &[Vec<(f32, f32)>]) -> Option<Shading> {
    let elem = element_of(node)?;
    let stops = gradient_stops(ctx, node, 0)?;
    if stops.is_empty() {
        return None;
    }
    let to_pixels = units_matrix(ctx, elem, "gradientUnits", polygons)?;
    // `gradientTransform` is applied in the gradient's own space, before the
    // mapping to pixels.
    let own = attr_of(elem, "gradientTransform")
        .map(|text| svg_transform(text))
        .unwrap_or(Mat::IDENTITY);
    let inverse = to_pixels.then(own).invert()?;
    // A missing coordinate takes its initial value, which for a linear gradient
    // is a left-to-right sweep across the whole box.
    let number = |name: &str, default: f32| {
        attr_of(elem, name)
            .and_then(|v| fraction(v))
            .unwrap_or(default)
    };
    if elem.tag_name == "radialgradient" {
        return Some(Shading::Radial {
            centre: (number("cx", 0.5), number("cy", 0.5)),
            radius: number("r", 0.5),
            stops,
            inverse,
        });
    }
    Some(Shading::Linear {
        from: (number("x1", 0.0), number("y1", 0.0)),
        to: (number("x2", 1.0), number("y2", 0.0)),
        stops,
        inverse,
    })
}

/// A gradient's stops, following `href` when it has none of its own.
///
/// Icon sets define one stop list and point several gradients at it, so a
/// gradient with no stops is usually a reference rather than a mistake.
fn gradient_stops(ctx: &Ctx, node: &Node, depth: usize) -> Option<Vec<(f32, Color)>> {
    if depth > MAX_DEPTH {
        return None;
    }
    let mut stops: Vec<(f32, Color)> = Vec::new();
    for child in &node.children {
        let Some(elem) = element_of(child) else {
            continue;
        };
        if elem.tag_name != "stop" {
            continue;
        }
        let from_style = |name: &str| {
            let style = attr_of(elem, "style")?;
            style
                .split(';')
                .filter_map(|d| d.split_once(':'))
                .find(|(key, _)| key.trim() == name)
                .map(|(_, value)| value.trim().to_string())
        };
        let attr = |name: &str| from_style(name).or_else(|| attr_of(elem, name).cloned());
        let offset = attr("offset").and_then(|v| fraction(&v)).unwrap_or(0.0);
        let mut color = attr("stop-color")
            .and_then(|v| paint_color(&v))
            .unwrap_or(Color {
                r: 0,
                g: 0,
                b: 0,
                a: 255,
            });
        if let Some(opacity) = attr("stop-opacity").and_then(|v| v.trim().parse::<f32>().ok()) {
            color.a = (color.a as f32 * opacity.clamp(0.0, 1.0)).round() as u8;
        }
        stops.push((offset.clamp(0.0, 1.0), color));
    }
    if !stops.is_empty() {
        // Out-of-order stops are a parse error the spec says to clamp, not to
        // reject.
        stops.sort_by(|a, b| a.0.total_cmp(&b.0));
        return Some(stops);
    }
    let elem = element_of(node)?;
    let inherited = attr_of(elem, "href").or_else(|| attr_of(elem, "xlink:href"))?;
    let target = ctx.defs.get(inherited)?;
    gradient_stops(ctx, target, depth + 1)
}

/// `<pattern>`: rasterize its content once and tile it.
fn pattern(ctx: &mut Ctx, node: &Node, polygons: &[Vec<(f32, f32)>]) -> Option<Shading> {
    if ctx.depth > MAX_DEPTH {
        return None;
    }
    let elem = element_of(node)?;
    let to_pixels = units_matrix(ctx, elem, "patternUnits", polygons)?;
    let number = |name: &str, default: f32| {
        attr_of(elem, name)
            .and_then(|v| fraction(v))
            .unwrap_or(default)
    };
    // The tile, in pixels.
    let (ox, oy) = to_pixels.apply(number("x", 0.0), number("y", 0.0));
    let (fx, fy) = to_pixels.apply(
        number("x", 0.0) + number("width", 0.0),
        number("y", 0.0) + number("height", 0.0),
    );
    let (tile_w, tile_h) = ((fx - ox).abs(), (fy - oy).abs());
    if tile_w < 1.0 || tile_h < 1.0 || tile_w > 2048.0 || tile_h > 2048.0 {
        return None;
    }
    let (width, height) = (tile_w.ceil() as usize, tile_h.ceil() as usize);
    let mut tile = vec![TRANSPARENT; width * height];
    // The tile's content is drawn in the pattern's own viewBox, or in user units
    // scaled to the tile.
    let view = match attr_of(elem, "viewBox").and_then(|v| view_box(v)) {
        Some((vx, vy, vw, vh)) if vw > 0.0 && vh > 0.0 => {
            let scale = (width as f32 / vw).min(height as f32 / vh);
            View {
                scale,
                matrix: Mat {
                    a: scale,
                    b: 0.0,
                    c: 0.0,
                    d: scale,
                    e: -vx * scale,
                    f: -vy * scale,
                },
            }
        }
        _ => View {
            scale: ctx.view.scale,
            matrix: Mat::scale(ctx.view.scale, ctx.view.scale),
        },
    };
    let mut inner = Ctx {
        canvas: &mut tile,
        width,
        height,
        view,
        defs: ctx.defs,
        fonts: ctx.fonts,
        clip: None,
        depth: ctx.depth + 1,
    };
    draw_children(node, &mut inner, Paint::root());
    Some(Shading::Tile {
        image: DecodedImage {
            width,
            height,
            pixels: tile,
        },
        origin: (ox, oy),
        step: (tile_w, tile_h),
    })
}

/// A number that may be written as a percentage, as gradient and pattern
/// coordinates both may be.
fn fraction(text: &str) -> Option<f32> {
    let text = text.trim();
    match text.strip_suffix('%') {
        Some(percent) => percent.trim().parse::<f32>().ok().map(|n| n / 100.0),
        None => text.parse().ok(),
    }
}

/// SVG's `transform` attribute, as one matrix.
///
/// Shares [`Mat`] with CSS transforms, but not the parser: SVG separates
/// arguments with spaces as well as commas and writes angles as bare numbers of
/// degrees, where CSS requires a unit.
fn svg_transform(text: &str) -> Mat {
    let mut matrix = Mat::IDENTITY;
    let mut rest = text.trim();
    while let Some(open) = rest.find('(') {
        let name = rest[..open]
            .trim()
            .trim_start_matches(',')
            .trim()
            .to_ascii_lowercase();
        let Some(close) = rest[open + 1..].find(')') else {
            break;
        };
        let args: Vec<f32> = rest[open + 1..open + 1 + close]
            .split([',', ' ', '\t', '\n', '\r'])
            .filter(|p| !p.is_empty())
            .filter_map(|p| p.parse().ok())
            .collect();
        rest = &rest[open + 1 + close + 1..];
        let at = |i: usize| args.get(i).copied();
        let radians = |degrees: f32| degrees * std::f32::consts::PI / 180.0;
        let step = match name.as_str() {
            "translate" => Mat::translate(at(0).unwrap_or(0.0), at(1).unwrap_or(0.0)),
            "scale" => {
                let x = at(0).unwrap_or(1.0);
                Mat::scale(x, at(1).unwrap_or(x))
            }
            // `rotate(a cx cy)` turns about a point rather than the origin.
            "rotate" => {
                let turn = Mat::rotate(radians(at(0).unwrap_or(0.0)));
                match (at(1), at(2)) {
                    (Some(cx), Some(cy)) => Mat::translate(cx, cy)
                        .then(turn)
                        .then(Mat::translate(-cx, -cy)),
                    _ => turn,
                }
            }
            "skewx" => Mat::skew(radians(at(0).unwrap_or(0.0)), 0.0),
            "skewy" => Mat::skew(0.0, radians(at(0).unwrap_or(0.0))),
            "matrix" if args.len() == 6 => Mat {
                a: args[0],
                b: args[1],
                c: args[2],
                d: args[3],
                e: args[4],
                f: args[5],
            },
            _ => continue,
        };
        matrix = matrix.then(step);
    }
    matrix
}

// --- rasterizing -----------------------------------------------------------

fn fill_and_stroke(ctx: &mut Ctx, subpaths: &[Vec<(f32, f32)>], paint: &Paint) {
    fill(ctx, subpaths, paint);
    stroke(ctx, subpaths, paint);
}

/// Fill by the non-zero winding rule, sampling `SAMPLES`² points per pixel.
fn fill(ctx: &mut Ctx, subpaths: &[Vec<(f32, f32)>], paint: &Paint) {
    if paint.fill == Brush::None {
        return;
    }
    let device: Vec<Vec<(f32, f32)>> = subpaths
        .iter()
        .map(|points| points.iter().map(|p| ctx.view.point(*p)).collect())
        .collect();
    let shading = resolve_brush(ctx, &paint.fill, &device);
    fill_device(ctx, &device, &shading, paint.opacity);
}

/// Fill polygons that are already in pixel coordinates.
///
/// Every polygon goes into one winding pass rather than being filled in turn. A
/// stroke is a pile of overlapping quads and discs, and filling them one after
/// another blends each one's soft edge over the last — which shows up as a dark
/// seam down the middle of every thick line.
///
/// Each sub-scanline is solved once: find where the edges cross it, sort those
/// crossings, and walk them to get the spans that are inside. Testing every edge
/// at every pixel instead is the obvious way to write this and costs
/// `pixels × edges` — which a stroked curve, being hundreds of small polygons,
/// makes unaffordable at any size bigger than an icon.
fn fill_device(ctx: &mut Ctx, polygons: &[Vec<(f32, f32)>], shading: &Shading, opacity: f32) {
    // Split so a clip path and a text glyph can reuse the scan: one produces
    // coverage into a mask, the others paint it.
    let (width, height) = (ctx.width, ctx.height);
    let mut plotted: Vec<(usize, usize, f32)> = Vec::new();
    scan_polygons(polygons, width, height, &mut |x, y, coverage| {
        plotted.push((x, y, coverage));
    });
    for (x, y, coverage) in plotted {
        let color = shading.color_at(x as f32 + 0.5, y as f32 + 0.5);
        blend(ctx, x, y, color, coverage * opacity);
    }
}

/// Walk the coverage of a set of polygons, in pixel coordinates.
fn scan_polygons(
    polygons: &[Vec<(f32, f32)>],
    canvas_width: usize,
    canvas_height: usize,
    plot: &mut impl FnMut(usize, usize, f32),
) {
    // Every polygon is closed for filling, whether or not it said `Z`.
    let edges: Vec<Edge> = polygons
        .iter()
        .flat_map(|points| closed_edges(points))
        .collect();
    if edges.is_empty() {
        return;
    }
    let (min_y, max_y) = vertical_span(&edges, canvas_height);
    let (min_x, max_x) = horizontal_span(&edges, canvas_width);
    if min_x >= max_x {
        return;
    }
    // How many of a pixel's sample points were inside, for one row at a time.
    let mut hits = vec![0u8; max_x - min_x];
    let mut crossings: Vec<(f32, i32)> = Vec::new();
    let step = 1.0 / SAMPLES as f32;

    for y in min_y..max_y {
        hits.iter_mut().for_each(|h| *h = 0);
        for sy in 0..SAMPLES {
            let py = y as f32 + (sy as f32 + 0.5) * step;
            crossings.clear();
            for (a, b) in &edges {
                if (a.1 <= py) == (b.1 <= py) {
                    continue; // the edge does not cross this scanline
                }
                let t = (py - a.1) / (b.1 - a.1);
                crossings.push((a.0 + t * (b.0 - a.0), if b.1 > a.1 { 1 } else { -1 }));
            }
            if crossings.len() < 2 {
                continue;
            }
            crossings.sort_by(|a, b| a.0.total_cmp(&b.0));

            // Between two crossings the winding number is constant, so the run
            // between them is either wholly inside the shape or wholly outside.
            let mut winding = 0;
            for pair in 0..crossings.len() - 1 {
                winding += crossings[pair].1;
                if winding == 0 {
                    continue;
                }
                let (from, to) = (crossings[pair].0, crossings[pair + 1].0);
                let first = (from.floor().max(min_x as f32)) as usize;
                let last = (to.ceil().min(max_x as f32)) as usize;
                for x in first..last.min(max_x) {
                    for sx in 0..SAMPLES {
                        let px = x as f32 + (sx as f32 + 0.5) * step;
                        // `from <= px` matches the winding this run stands for.
                        if px >= from && px < to {
                            hits[x - min_x] += 1;
                        }
                    }
                }
            }
        }
        for (i, count) in hits.iter().enumerate() {
            if *count > 0 {
                let coverage = *count as f32 / (SAMPLES * SAMPLES) as f32;
                plot(min_x + i, y, coverage);
            }
        }
    }
}

/// Stroke by laying a quad along every segment and a disc at every joint.
///
/// The disc is what makes a corner a corner: without one, each segment ends
/// square and the wedge between it and the next is simply missing, so a curve
/// flattened into segments comes out serrated. A round joint is also the only
/// one that needs no special case for how sharp the turn is.
///
/// ponytail: round joins and caps whatever `stroke-linejoin` and
/// `stroke-linecap` say. At the sizes an icon or a logo is drawn, round and
/// mitre differ by a fraction of a pixel — but no join at all is visible.
fn stroke(ctx: &mut Ctx, subpaths: &[Vec<(f32, f32)>], paint: &Paint) {
    if paint.stroke == Brush::None {
        return;
    }
    let width = (paint.stroke_width * ctx.view.scale).max(1.0);
    let half = width / 2.0;
    let mut pieces: Vec<Vec<(f32, f32)>> = Vec::new();
    for points in subpaths {
        for pair in points.windows(2) {
            let (a, b) = (ctx.view.point(pair[0]), ctx.view.point(pair[1]));
            let (dx, dy) = (b.0 - a.0, b.1 - a.1);
            let len = (dx * dx + dy * dy).sqrt();
            if len < f32::EPSILON {
                continue;
            }
            // The pen's offset, perpendicular to the segment. Rotating the
            // direction the same way every time keeps every quad wound the same
            // way, which is what lets them share one winding pass.
            let (nx, ny) = (-dy / len * half, dx / len * half);
            pieces.push(vec![
                (a.0 + nx, a.1 + ny),
                (b.0 + nx, b.1 + ny),
                (b.0 - nx, b.1 - ny),
                (a.0 - nx, a.1 - ny),
            ]);
        }
        // A pen one pixel wide has no joint worth filling.
        if width > 2.0 && points.len() > 2 {
            for vertex in &points[1..points.len() - 1] {
                pieces.push(disc(ctx.view.point(*vertex), half));
            }
        }
    }
    let shading = resolve_brush(ctx, &paint.stroke, &pieces);
    fill_device(ctx, &pieces, &shading, paint.opacity);
}

/// A circle as a polygon, wound the same way the stroke's quads are so the two
/// add up rather than cancelling out. Sixteen sides is smooth at any width a
/// joint is visible at.
fn disc(centre: (f32, f32), radius: f32) -> Vec<(f32, f32)> {
    const SIDES: usize = 16;
    (0..=SIDES)
        .map(|i| {
            let angle = i as f32 / SIDES as f32 * std::f32::consts::TAU;
            (
                centre.0 + radius * angle.cos(),
                centre.1 - radius * angle.sin(),
            )
        })
        .collect()
}

/// A line segment between two points, as the scanline filler passes them around.
type Edge = ((f32, f32), (f32, f32));

/// The columns a set of edges can possibly touch. Without this every fill
/// walked the whole image width per scanline, which a stroke made of dozens of
/// small quads pays for dozens of times over.
fn horizontal_span(edges: &[Edge], width: usize) -> (usize, usize) {
    let (mut min, mut max) = (f32::MAX, f32::MIN);
    for (a, b) in edges {
        min = min.min(a.0).min(b.0);
        max = max.max(a.0).max(b.0);
    }
    let low = min.floor().max(0.0) as usize;
    let high = (max.ceil() + 1.0).max(0.0) as usize;
    (low.min(width), high.min(width))
}

fn closed_edges(points: &[(f32, f32)]) -> Vec<Edge> {
    let mut edges: Vec<Edge> = points.windows(2).map(|pair| (pair[0], pair[1])).collect();
    match (points.first(), points.last()) {
        (Some(first), Some(last)) if first != last => edges.push((*last, *first)),
        _ => {}
    }
    edges
}

/// The rows a shape can possibly touch, so a small icon in a big canvas does
/// not cost a full-canvas sweep per shape.
fn vertical_span(edges: &[Edge], height: usize) -> (usize, usize) {
    let min = edges
        .iter()
        .flat_map(|(a, b)| [a.1, b.1])
        .fold(f32::MAX, f32::min);
    let max = edges
        .iter()
        .flat_map(|(a, b)| [a.1, b.1])
        .fold(f32::MIN, f32::max);
    (
        min.floor().max(0.0) as usize,
        (max.ceil().max(0.0) as usize + 1).min(height),
    )
}

/// Source-over compositing into the transparent canvas.
fn blend(ctx: &mut Ctx, x: usize, y: usize, color: Color, coverage: f32) {
    if x >= ctx.width || y >= ctx.height {
        return;
    }
    // A clip path is coverage, not geometry, by this point — so it multiplies in
    // like any other partial pixel and an unclipped draw costs one branch.
    let coverage = match &ctx.clip {
        Some(mask) => coverage * mask[y * ctx.width + x],
        None => coverage,
    };
    let alpha = (color.a as f32 / 255.0) * coverage.clamp(0.0, 1.0);
    if alpha <= 0.0 {
        return;
    }
    let dst = ctx.canvas[y * ctx.width + x];
    let dst_a = dst.a as f32 / 255.0;
    let out_a = alpha + dst_a * (1.0 - alpha);
    if out_a <= 0.0 {
        return;
    }
    let mix = |src: u8, dst: u8| {
        ((src as f32 * alpha + dst as f32 * dst_a * (1.0 - alpha)) / out_a).round() as u8
    };
    ctx.canvas[y * ctx.width + x] = Color {
        r: mix(color.r, dst.r),
        g: mix(color.g, dst.g),
        b: mix(color.b, dst.b),
        a: (out_a * 255.0).round() as u8,
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(img: &DecodedImage, x: usize, y: usize) -> Color {
        img.pixels[y * img.width + x]
    }

    #[test]
    fn a_circle_fills_its_middle_and_leaves_the_corners_alone() {
        let img = rasterize(
            "<svg viewBox='0 0 100 100'><circle cx='50' cy='50' r='40' fill='#ff0000'/></svg>",
            100,
            100,
        )
        .expect("rasterized");
        assert_eq!(
            at(&img, 50, 50),
            Color {
                r: 255,
                g: 0,
                b: 0,
                a: 255
            }
        );
        // Outside the circle the image stays transparent, not black.
        assert_eq!(at(&img, 2, 2).a, 0);
        // The rim is anti-aliased rather than a hard jump — a circle drawn with
        // whole pixels alone would have no partial coverage anywhere.
        let soft = img.pixels.iter().filter(|p| p.a > 0 && p.a < 255).count();
        assert!(
            soft > 20,
            "expected a soft rim, found {soft} partial pixels"
        );
    }

    const RED: Color = Color {
        r: 255,
        g: 0,
        b: 0,
        a: 255,
    };
    const BLUE: Color = Color {
        r: 0,
        g: 0,
        b: 255,
        a: 255,
    };

    #[test]
    fn use_and_symbol_draw_what_they_point_at() {
        // The sprite-sheet idiom: one shape defined once, placed twice.
        let img = rasterize(
            "<svg viewBox='0 0 100 100'>\
               <defs><rect id='box' x='0' y='0' width='20' height='20' fill='#ff0000'/></defs>\
               <use href='#box' x='10' y='10'/><use href='#box' x='60' y='60'/>\
             </svg>",
            100,
            100,
        )
        .expect("rasterized");
        assert_eq!(at(&img, 20, 20), RED, "the first use drew nothing");
        assert_eq!(at(&img, 70, 70), RED, "the second use drew nothing");
        // Nothing was drawn at the definition's own position.
        assert_eq!(at(&img, 5, 5).a, 0);

        // A `<symbol>` exists only to be used, so using one draws its contents.
        let img = rasterize(
            "<svg viewBox='0 0 100 100'>\
               <symbol id='mark'><circle cx='10' cy='10' r='8' fill='#0000ff'/></symbol>\
               <use href='#mark' x='40' y='40'/>\
             </svg>",
            100,
            100,
        )
        .expect("rasterized");
        assert_eq!(at(&img, 50, 50), BLUE);

        // A reference ring stops rather than running out of stack.
        let img = rasterize(
            "<svg viewBox='0 0 100 100'>\
               <g id='a'><use href='#b'/></g><g id='b'><use href='#a'/></g>\
               <use href='#a'/>\
             </svg>",
            50,
            50,
        );
        assert!(
            img.is_some(),
            "a reference cycle took the whole picture down"
        );
    }

    #[test]
    fn gradients_interpolate_across_the_shape() {
        let img = rasterize(
            "<svg viewBox='0 0 100 100'>\
               <defs><linearGradient id='g'>\
                 <stop offset='0' stop-color='#ff0000'/>\
                 <stop offset='1' stop-color='#0000ff'/>\
               </linearGradient></defs>\
               <rect x='0' y='0' width='100' height='100' fill='url(#g)'/>\
             </svg>",
            100,
            100,
        )
        .expect("rasterized");
        // Left end red, right end blue, and a real mix in between rather than a
        // flat fill of either.
        assert!(at(&img, 2, 50).r > 240 && at(&img, 2, 50).b < 20);
        assert!(at(&img, 97, 50).b > 240 && at(&img, 97, 50).r < 20);
        let middle = at(&img, 50, 50);
        assert!(
            middle.r > 80 && middle.r < 180 && middle.b > 80 && middle.b < 180,
            "the middle came out {middle:?}, not a blend"
        );

        // A radial gradient runs from its centre outward, not left to right.
        let img = rasterize(
            "<svg viewBox='0 0 100 100'>\
               <defs><radialGradient id='r'>\
                 <stop offset='0' stop-color='#ff0000'/>\
                 <stop offset='1' stop-color='#0000ff'/>\
               </radialGradient></defs>\
               <rect x='0' y='0' width='100' height='100' fill='url(#r)'/>\
             </svg>",
            100,
            100,
        )
        .expect("rasterized");
        assert!(
            at(&img, 50, 50).r > 240,
            "the centre was not the first stop"
        );
        assert!(at(&img, 2, 50).b > 200 && at(&img, 50, 2).b > 200);

        // Stops may be inherited through `href`, which is how icon sets define
        // one palette and point several gradients at it.
        let img = rasterize(
            "<svg viewBox='0 0 100 100'>\
               <defs>\
                 <linearGradient id='base'>\
                   <stop offset='0' stop-color='#ff0000'/>\
                   <stop offset='1' stop-color='#ff0000'/>\
                 </linearGradient>\
                 <linearGradient id='g' href='#base' x1='0' y1='0' x2='1' y2='0'/>\
               </defs>\
               <rect x='0' y='0' width='100' height='100' fill='url(#g)'/>\
             </svg>",
            60,
            60,
        )
        .expect("rasterized");
        assert_eq!(at(&img, 30, 30), RED, "inherited stops were not followed");

        // A reference to nothing leaves a visible shape rather than a hole.
        let img = rasterize(
            "<svg viewBox='0 0 100 100'>\
               <rect x='0' y='0' width='100' height='100' fill='url(#gone)'/></svg>",
            60,
            60,
        )
        .expect("rasterized");
        assert!(at(&img, 30, 30).a > 0, "an unresolved fill drew nothing");
    }

    #[test]
    fn a_clip_path_clips() {
        let img = rasterize(
            "<svg viewBox='0 0 100 100'>\
               <defs><clipPath id='c'>\
                 <rect x='0' y='0' width='50' height='100'/>\
               </clipPath></defs>\
               <rect x='0' y='0' width='100' height='100' fill='#ff0000' clip-path='url(#c)'/>\
             </svg>",
            100,
            100,
        )
        .expect("rasterized");
        // Inside the clip the fill is there; outside it, nothing is.
        assert_eq!(at(&img, 25, 50), RED);
        assert_eq!(at(&img, 75, 50).a, 0, "the clip path did not clip");
    }

    #[test]
    fn a_pattern_tiles() {
        let img = rasterize(
            "<svg viewBox='0 0 100 100'>\
               <defs><pattern id='p' patternUnits='userSpaceOnUse' x='0' y='0' \
                              width='20' height='20'>\
                 <rect x='0' y='0' width='10' height='10' fill='#ff0000'/>\
               </pattern></defs>\
               <rect x='0' y='0' width='100' height='100' fill='url(#p)'/>\
             </svg>",
            100,
            100,
        )
        .expect("rasterized");
        // The same cell of the tile, one repeat apart, has the same colour.
        let first = at(&img, 3, 3);
        assert!(first.r > 200, "the first tile did not draw: {first:?}");
        assert!(at(&img, 23, 23).r > 200, "the pattern did not repeat");
        // And the empty part of the tile stays empty.
        assert!(at(&img, 15, 15).a < 128, "the tile's gap was filled in");
    }

    #[test]
    fn an_element_transform_rotates_what_it_draws() {
        // A thin bar along the top, turned a quarter turn about the centre: it
        // ends up down the left-hand side.
        let img = rasterize(
            "<svg viewBox='0 0 100 100'>\
               <rect x='40' y='0' width='20' height='100' fill='#ff0000' \
                     transform='rotate(90 50 50)'/>\
             </svg>",
            100,
            100,
        )
        .expect("rasterized");
        assert_eq!(at(&img, 10, 50), RED, "the rotation was not applied");
        assert_eq!(at(&img, 50, 10).a, 0, "the shape stayed where it was");
    }

    #[test]
    fn a_filter_puts_a_soft_shadow_under_the_shape() {
        let img = rasterize(
            "<svg viewBox='0 0 100 100'>\
               <defs><filter id='s'>\
                 <feGaussianBlur stdDeviation='3'/>\
                 <feOffset dx='6' dy='6'/>\
                 <feFlood flood-color='#0000ff'/>\
               </filter></defs>\
               <rect x='20' y='20' width='40' height='40' fill='#ff0000' filter='url(#s)'/>\
             </svg>",
            100,
            100,
        )
        .expect("rasterized");
        // The shape itself is untouched...
        assert_eq!(at(&img, 40, 40), RED, "the filtered element was lost");
        // ...and there is soft blue below and to the right of it, where nothing
        // was drawn before.
        let shadow = at(&img, 64, 64);
        assert!(
            shadow.a > 0 && shadow.b > shadow.r,
            "no shadow under the shape: {shadow:?}"
        );
    }

    #[test]
    fn svg_transforms_parse_the_way_svg_writes_them() {
        // Spaces rather than commas, and bare degrees rather than a unit.
        let m = svg_transform("translate(10 20)");
        assert_eq!(m.apply(0.0, 0.0), (10.0, 20.0));
        let m = svg_transform("rotate(90)");
        let (x, y) = m.apply(10.0, 0.0);
        assert!(x.abs() < 1.0e-4 && (y - 10.0).abs() < 1.0e-4, "({x}, {y})");
        // `rotate(a cx cy)` turns about a point.
        let m = svg_transform("rotate(180 5 5)");
        let (x, y) = m.apply(0.0, 0.0);
        assert!(
            (x - 10.0).abs() < 1.0e-4 && (y - 10.0).abs() < 1.0e-4,
            "({x}, {y})"
        );
        // A list composes left to right.
        let m = svg_transform("translate(10 0) scale(2)");
        assert_eq!(m.apply(1.0, 0.0), (12.0, 0.0));
        assert_eq!(
            svg_transform("matrix(1 0 0 1 3 4)").apply(0.0, 0.0),
            (3.0, 4.0)
        );
    }

    #[test]
    fn a_path_draws_the_shape_its_commands_describe() {
        // A triangle over the left half, via absolute and relative commands.
        let img = rasterize(
            "<svg viewBox='0 0 100 100'><path d='M10 10 L 90 50 l -80 40 z' fill='#0000ff'/></svg>",
            100,
            100,
        )
        .expect("rasterized");
        assert_eq!(
            at(&img, 20, 50),
            Color {
                r: 0,
                g: 0,
                b: 255,
                a: 255
            }
        );
        // ...and not the corner outside it.
        assert_eq!(at(&img, 95, 95).a, 0);
    }

    #[test]
    fn compacted_path_numbers_are_read_as_separate_coordinates() {
        // The same square three ways: spaced out, with the points run together
        // the way an exporter writes them, and with an exponent in it.
        let square = |d: &str| {
            let img = rasterize(
                &format!("<svg viewBox='0 0 10 10'><path fill='#ff0000' d='{d}'/></svg>"),
                10,
                10,
            )
            .expect("rasterized");
            (at(&img, 5, 5).a, at(&img, 9, 9).a)
        };
        assert_eq!(square("M2.5 2.5L7.5 2.5L7.5 7.5L2.5 7.5z"), (255, 0));
        assert_eq!(square("M2.5 2.5l5 0 0 5-5 0z"), (255, 0));
        // `l5.0.0` is "5.0 then 0.0", not one unreadable number.
        assert_eq!(square("M2.5 2.5l5.0.0.0 5-5 0z"), (255, 0));
        assert_eq!(square("M2.5e0 2.5L7.5 2.5L7.5 7.5L2.5 7.5z"), (255, 0));
    }

    #[test]
    fn an_arc_bulges_the_way_its_flags_ask() {
        // The same two endpoints twice, differing only in `sweep`: one arc has
        // to bulge up and the other down, or the flag is being ignored. Cutting
        // straight across — which is what a chord approximation does — leaves
        // both halves empty and fails either way.
        let disc = |sweep: u8| {
            rasterize(
                &format!(
                    "<svg viewBox='0 0 100 100'>\
                     <path d='M10 50 A40 40 0 0 {sweep} 90 50 Z' fill='#ff0000'/></svg>"
                ),
                100,
                100,
            )
            .expect("rasterized")
        };
        let up = disc(1);
        assert_eq!(
            at(&up, 50, 20),
            Color {
                r: 255,
                g: 0,
                b: 0,
                a: 255
            }
        );
        assert_eq!(at(&up, 50, 80).a, 0);

        let down = disc(0);
        assert_eq!(
            at(&down, 50, 80),
            Color {
                r: 255,
                g: 0,
                b: 0,
                a: 255
            }
        );
        assert_eq!(at(&down, 50, 20).a, 0);

        // A radius too small to span the endpoints is grown until it fits,
        // rather than dropping the arc: r=10 cannot reach across 80 units.
        let wide = rasterize(
            "<svg viewBox='0 0 100 100'><path d='M10 50 A10 10 0 0 1 90 50 Z' fill='#ff0000'/></svg>",
            100,
            100,
        )
        .expect("rasterized");
        assert_eq!(
            at(&wide, 50, 20),
            Color {
                r: 255,
                g: 0,
                b: 0,
                a: 255
            }
        );
    }

    #[test]
    fn the_root_svg_states_a_line_style_for_everything_inside_it() {
        // How every icon set writes it: the style is on the root, the paths
        // carry only geometry.
        let img = rasterize(
            "<svg viewBox='0 0 20 20' fill='none' stroke='#ff0000' stroke-width='4'>\
             <path d='M10 0V20'/></svg>",
            20,
            20,
        )
        .expect("rasterized");
        assert_eq!(
            at(&img, 10, 10),
            Color {
                r: 255,
                g: 0,
                b: 0,
                a: 255
            }
        );
        // `fill:none` from the root holds too, or the line would be a filled blob.
        assert_eq!(at(&img, 2, 10).a, 0);
    }

    #[test]
    fn fills_stroke_and_nesting_all_apply() {
        let img = rasterize(
            "<svg viewBox='0 0 20 20'>\
             <g fill='#00ff00'><rect x='0' y='0' width='10' height='20'/></g>\
             <line x1='15' y1='0' x2='15' y2='20' stroke='#ff0000' stroke-width='4'/>\
             <rect x='0' y='0' width='20' height='20' fill='none'/>\
             </svg>",
            20,
            20,
        )
        .expect("rasterized");
        // The group's fill reached its child.
        assert_eq!(
            at(&img, 5, 10),
            Color {
                r: 0,
                g: 255,
                b: 0,
                a: 255
            }
        );
        // The stroked line is four units wide around x=15.
        assert_eq!(
            at(&img, 15, 10),
            Color {
                r: 255,
                g: 0,
                b: 0,
                a: 255
            }
        );
        // `fill: none` painted nothing over the rest.
        assert_eq!(at(&img, 11, 2).a, 0);
    }

    #[test]
    fn the_view_box_scales_and_the_intrinsic_size_is_read() {
        assert_eq!(
            intrinsic_size("<svg width='24' height='16'></svg>"),
            (24, 16)
        );
        assert_eq!(intrinsic_size("<svg viewBox='0 0 48 12'></svg>"), (48, 12));
        assert_eq!(intrinsic_size("<svg></svg>"), (300, 150));

        // Half the viewBox filled means half the pixels, whatever the size.
        let img = rasterize(
            "<svg viewBox='0 0 10 10'><rect x='0' y='0' width='5' height='10' fill='#000000'/></svg>",
            40,
            40,
        )
        .expect("rasterized");
        assert_eq!(at(&img, 5, 20).a, 255);
        assert_eq!(at(&img, 35, 20).a, 0);
    }

    #[test]
    fn recognises_svg_bytes() {
        assert!(looks_like_svg(b"<svg xmlns='...'>"));
        assert!(looks_like_svg(b"  <?xml version='1.0'?><svg>"));
        assert!(!looks_like_svg(b"\x89PNG\r\n"));
        assert!(!looks_like_svg(b""));
    }
}

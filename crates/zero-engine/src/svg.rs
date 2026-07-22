//! A small SVG rasterizer: shapes and paths to pixels.
//!
//! SVG is the web's icon format, and a browser that skips it draws a page full
//! of holes where the logos and buttons should be. This covers what icons and
//! logos actually use — shapes, paths, fills, strokes, `viewBox` — and nothing
//! else.
//!
//! ponytail: no gradients, patterns, filters, clip paths, text, or `<use>`; a
//! `transform` is honoured only on the element it sits on (translate/scale).
//! Curves are flattened to line segments and every shape is filled by one
//! scanline pass with 3×3 supersampling, which is slower than an active-edge
//! rasterizer and far shorter. Anything unrecognised is skipped rather than
//! guessed at, so an unsupported feature costs one shape, not the picture.

use crate::css::Color;
use crate::dom::{Node, NodeType};
use crate::resource::DecodedImage;

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
    let Some(svg) = find_svg(&dom) else { return (300, 150) };
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
pub fn rasterize(source: &str, width: usize, height: usize) -> Option<DecodedImage> {
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
        dx: (width as f32 - vw * scale) / 2.0 - vx * scale,
        dy: (height as f32 - vh * scale) / 2.0 - vy * scale,
    };

    let mut canvas = vec![
        Color { r: 0, g: 0, b: 0, a: 0 };
        width * height
    ];
    let mut ctx = Ctx { canvas: &mut canvas, width, height, view };
    let inherited = Paint::root();
    draw_children(svg, &mut ctx, inherited);
    Some(DecodedImage { width, height, pixels: canvas })
}

/// The mapping from user units to pixels.
#[derive(Clone, Copy)]
struct View {
    scale: f32,
    dx: f32,
    dy: f32,
}

impl View {
    fn point(&self, (x, y): (f32, f32)) -> (f32, f32) {
        (x * self.scale + self.dx, y * self.scale + self.dy)
    }
}

struct Ctx<'a> {
    canvas: &'a mut Vec<Color>,
    width: usize,
    height: usize,
    view: View,
}

/// Painting state, which inherits down the tree the way SVG says it does.
#[derive(Clone, Copy)]
struct Paint {
    fill: Option<Color>,
    stroke: Option<Color>,
    stroke_width: f32,
    opacity: f32,
}

impl Paint {
    /// SVG's initial state: black fill, no stroke.
    fn root() -> Paint {
        Paint {
            fill: Some(Color { r: 0, g: 0, b: 0, a: 255 }),
            stroke: None,
            stroke_width: 1.0,
            opacity: 1.0,
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
            paint.fill = paint_color(&fill);
        }
        if let Some(stroke) = attr("stroke") {
            paint.stroke = paint_color(&stroke);
        }
        if let Some(width) = attr("stroke-width").and_then(|v| length(&v)) {
            paint.stroke_width = width;
        }
        if let Some(opacity) = attr("opacity").and_then(|v| v.trim().parse::<f32>().ok()) {
            paint.opacity *= opacity.clamp(0.0, 1.0);
        }
        paint
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
        return Some(Color { r: 0, g: 0, b: 0, a: 255 });
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
        let Some(elem) = element_of(child) else { continue };
        let paint = inherited.with(elem);
        match elem.tag_name.as_str() {
            // A group paints nothing itself; it only passes its state down.
            "g" | "svg" | "a" => draw_children(child, ctx, paint),
            "rect" => {
                let get = |name: &str| number_attr(elem, name);
                let (x, y) = (get("x"), get("y"));
                let (w, h) = (get("width"), get("height"));
                if w > 0.0 && h > 0.0 {
                    let rect =
                        vec![(x, y), (x + w, y), (x + w, y + h), (x, y + h), (x, y)];
                    fill_and_stroke(ctx, &[rect], paint);
                }
            }
            "circle" => {
                let (cx, cy) = (number_attr(elem, "cx"), number_attr(elem, "cy"));
                let r = number_attr(elem, "r");
                if r > 0.0 {
                    fill_and_stroke(ctx, &[ellipse(cx, cy, r, r)], paint);
                }
            }
            "ellipse" => {
                let (cx, cy) = (number_attr(elem, "cx"), number_attr(elem, "cy"));
                let (rx, ry) = (number_attr(elem, "rx"), number_attr(elem, "ry"));
                if rx > 0.0 && ry > 0.0 {
                    fill_and_stroke(ctx, &[ellipse(cx, cy, rx, ry)], paint);
                }
            }
            "line" => {
                let line = vec![
                    (number_attr(elem, "x1"), number_attr(elem, "y1")),
                    (number_attr(elem, "x2"), number_attr(elem, "y2")),
                ];
                // A line has no interior, so it is stroke or nothing.
                stroke(ctx, &[line], paint);
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
                        "polygon" => fill_and_stroke(ctx, &[points], paint),
                        _ => stroke(ctx, &[points], paint),
                    }
                }
            }
            "path" => {
                if let Some(d) = attr_of(elem, "d") {
                    let subpaths = flatten_path(d);
                    if !subpaths.is_empty() {
                        fill_and_stroke(ctx, &subpaths, paint);
                    }
                }
            }
            // defs, style, title, filters, gradients: nothing to draw.
            _ => {}
        }
    }
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
    numbers.chunks_exact(2).map(|pair| (pair[0], pair[1])).collect()
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
                for (i, pair) in args.chunks_exact(2).enumerate() {
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
                for pair in args.chunks_exact(2) {
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
                let stride = if command.to_ascii_uppercase() == 'C' { 6 } else { 4 };
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
                let stride = if command.to_ascii_uppercase() == 'Q' { 4 } else { 2 };
                for group in args.chunks_exact(stride) {
                    let (control, end) = match stride {
                        4 => (at(cursor, group[0], group[1]), at(cursor, group[2], group[3])),
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
            // An elliptical arc is approximated by its chord: wrong, but a
            // closed shape rather than a hole. ponytail: real arc flattening if
            // a page needs it.
            'A' => {
                for group in args.chunks_exact(7) {
                    cursor = at(cursor, group[5], group[6]);
                    current.push(cursor);
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
            _ => number.push(c),
        }
    }
    flush(&mut number, &mut args);
    if let Some(previous) = command {
        out.push((previous, args));
    }
    out
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

// --- rasterizing -----------------------------------------------------------

fn fill_and_stroke(ctx: &mut Ctx, subpaths: &[Vec<(f32, f32)>], paint: Paint) {
    fill(ctx, subpaths, paint);
    stroke(ctx, subpaths, paint);
}

/// Fill by the non-zero winding rule, sampling `SAMPLES`² points per pixel.
fn fill(ctx: &mut Ctx, subpaths: &[Vec<(f32, f32)>], paint: Paint) {
    let Some(color) = paint.fill else { return };
    // Every subpath is closed for filling, whether or not it said `Z`.
    let edges: Vec<((f32, f32), (f32, f32))> = subpaths
        .iter()
        .flat_map(|points| closed_edges(points))
        .map(|(a, b)| (ctx.view.point(a), ctx.view.point(b)))
        .collect();
    if edges.is_empty() {
        return;
    }
    let (min_y, max_y) = vertical_span(&edges, ctx.height);
    for y in min_y..max_y {
        for x in 0..ctx.width {
            let mut hits = 0;
            for sy in 0..SAMPLES {
                let py = y as f32 + (sy as f32 + 0.5) / SAMPLES as f32;
                let mut winding = [0i32; SAMPLES];
                for (a, b) in &edges {
                    if (a.1 <= py) == (b.1 <= py) {
                        continue; // the edge does not cross this scanline
                    }
                    let t = (py - a.1) / (b.1 - a.1);
                    let crossing = a.0 + t * (b.0 - a.0);
                    let direction = if b.1 > a.1 { 1 } else { -1 };
                    for (sx, count) in winding.iter_mut().enumerate() {
                        let px = x as f32 + (sx as f32 + 0.5) / SAMPLES as f32;
                        if crossing <= px {
                            *count += direction;
                        }
                    }
                }
                hits += winding.iter().filter(|w| **w != 0).count();
            }
            if hits > 0 {
                let coverage = hits as f32 / (SAMPLES * SAMPLES) as f32;
                blend(ctx, x, y, color, coverage * paint.opacity);
            }
        }
    }
}

/// Stroke by filling a quad per segment: a rectangle as wide as the pen, with
/// a square cap at each end.
///
/// ponytail: no joins, so a sharp corner has a small notch. At icon sizes it is
/// invisible; a real stroker is the fix if it ever is not.
fn stroke(ctx: &mut Ctx, subpaths: &[Vec<(f32, f32)>], paint: Paint) {
    let Some(color) = paint.stroke else { return };
    let width = (paint.stroke_width * ctx.view.scale).max(1.0);
    let half = width / 2.0;
    for points in subpaths {
        for pair in points.windows(2) {
            let (a, b) = (ctx.view.point(pair[0]), ctx.view.point(pair[1]));
            let (dx, dy) = (b.0 - a.0, b.1 - a.1);
            let len = (dx * dx + dy * dy).sqrt();
            if len < f32::EPSILON {
                continue;
            }
            // The pen's offset, perpendicular to the segment.
            let (nx, ny) = (-dy / len * half, dx / len * half);
            let quad = vec![
                (a.0 + nx, a.1 + ny),
                (b.0 + nx, b.1 + ny),
                (b.0 - nx, b.1 - ny),
                (a.0 - nx, a.1 - ny),
            ];
            fill_device_polygon(ctx, &quad, color, paint.opacity);
        }
    }
}

/// Fill a polygon already in pixel coordinates (the stroker works there).
fn fill_device_polygon(ctx: &mut Ctx, points: &[(f32, f32)], color: Color, opacity: f32) {
    let edges: Vec<((f32, f32), (f32, f32))> = closed_edges(points);
    let (min_y, max_y) = vertical_span(&edges, ctx.height);
    for y in min_y..max_y {
        for x in 0..ctx.width {
            let mut hits = 0;
            for sy in 0..SAMPLES {
                let py = y as f32 + (sy as f32 + 0.5) / SAMPLES as f32;
                for sx in 0..SAMPLES {
                    let px = x as f32 + (sx as f32 + 0.5) / SAMPLES as f32;
                    let mut winding = 0;
                    for (a, b) in &edges {
                        if (a.1 <= py) == (b.1 <= py) {
                            continue;
                        }
                        let t = (py - a.1) / (b.1 - a.1);
                        if a.0 + t * (b.0 - a.0) <= px {
                            winding += if b.1 > a.1 { 1 } else { -1 };
                        }
                    }
                    hits += (winding != 0) as usize;
                }
            }
            if hits > 0 {
                blend(ctx, x, y, color, hits as f32 / (SAMPLES * SAMPLES) as f32 * opacity);
            }
        }
    }
}

fn closed_edges(points: &[(f32, f32)]) -> Vec<((f32, f32), (f32, f32))> {
    let mut edges: Vec<((f32, f32), (f32, f32))> =
        points.windows(2).map(|pair| (pair[0], pair[1])).collect();
    match (points.first(), points.last()) {
        (Some(first), Some(last)) if first != last => edges.push((*last, *first)),
        _ => {}
    }
    edges
}

/// The rows a shape can possibly touch, so a small icon in a big canvas does
/// not cost a full-canvas sweep per shape.
fn vertical_span(edges: &[((f32, f32), (f32, f32))], height: usize) -> (usize, usize) {
    let min = edges.iter().flat_map(|(a, b)| [a.1, b.1]).fold(f32::MAX, f32::min);
    let max = edges.iter().flat_map(|(a, b)| [a.1, b.1]).fold(f32::MIN, f32::max);
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
        assert_eq!(at(&img, 50, 50), Color { r: 255, g: 0, b: 0, a: 255 });
        // Outside the circle the image stays transparent, not black.
        assert_eq!(at(&img, 2, 2).a, 0);
        // The rim is anti-aliased rather than a hard jump — a circle drawn with
        // whole pixels alone would have no partial coverage anywhere.
        let soft = img.pixels.iter().filter(|p| p.a > 0 && p.a < 255).count();
        assert!(soft > 20, "expected a soft rim, found {soft} partial pixels");
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
        assert_eq!(at(&img, 20, 50), Color { r: 0, g: 0, b: 255, a: 255 });
        // ...and not the corner outside it.
        assert_eq!(at(&img, 95, 95).a, 0);
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
        assert_eq!(at(&img, 5, 10), Color { r: 0, g: 255, b: 0, a: 255 });
        // The stroked line is four units wide around x=15.
        assert_eq!(at(&img, 15, 10), Color { r: 255, g: 0, b: 0, a: 255 });
        // `fill: none` painted nothing over the rest.
        assert_eq!(at(&img, 11, 2).a, 0);
    }

    #[test]
    fn the_view_box_scales_and_the_intrinsic_size_is_read() {
        assert_eq!(intrinsic_size("<svg width='24' height='16'></svg>"), (24, 16));
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

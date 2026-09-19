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
        Color {
            r: 0,
            g: 0,
            b: 0,
            a: 0
        };
        width * height
    ];
    let mut ctx = Ctx {
        canvas: &mut canvas,
        width,
        height,
        view,
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
            fill: Some(Color {
                r: 0,
                g: 0,
                b: 0,
                a: 255,
            }),
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
        let paint = inherited.with(elem);
        match elem.tag_name.as_str() {
            // A group paints nothing itself; it only passes its state down.
            "g" | "svg" | "a" => draw_children(child, ctx, paint),
            "rect" => {
                let get = |name: &str| number_attr(elem, name);
                let (x, y) = (get("x"), get("y"));
                let (w, h) = (get("width"), get("height"));
                if w > 0.0 && h > 0.0 {
                    let rect = vec![(x, y), (x + w, y), (x + w, y + h), (x, y + h), (x, y)];
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
    numbers
        .chunks_exact(2)
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
                let stride = if command.to_ascii_uppercase() == 'C' {
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
                let stride = if command.to_ascii_uppercase() == 'Q' {
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
                for group in args.chunks_exact(7) {
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

// --- rasterizing -----------------------------------------------------------

fn fill_and_stroke(ctx: &mut Ctx, subpaths: &[Vec<(f32, f32)>], paint: Paint) {
    fill(ctx, subpaths, paint);
    stroke(ctx, subpaths, paint);
}

/// Fill by the non-zero winding rule, sampling `SAMPLES`² points per pixel.
fn fill(ctx: &mut Ctx, subpaths: &[Vec<(f32, f32)>], paint: Paint) {
    let Some(color) = paint.fill else { return };
    let device: Vec<Vec<(f32, f32)>> = subpaths
        .iter()
        .map(|points| points.iter().map(|p| ctx.view.point(*p)).collect())
        .collect();
    fill_device(ctx, &device, color, paint.opacity);
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
fn fill_device(ctx: &mut Ctx, polygons: &[Vec<(f32, f32)>], color: Color, opacity: f32) {
    // Every polygon is closed for filling, whether or not it said `Z`.
    let edges: Vec<((f32, f32), (f32, f32))> = polygons
        .iter()
        .flat_map(|points| closed_edges(points))
        .collect();
    if edges.is_empty() {
        return;
    }
    let (min_y, max_y) = vertical_span(&edges, ctx.height);
    let (min_x, max_x) = horizontal_span(&edges, ctx.width);
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
                blend(ctx, min_x + i, y, color, coverage * opacity);
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
fn stroke(ctx: &mut Ctx, subpaths: &[Vec<(f32, f32)>], paint: Paint) {
    let Some(color) = paint.stroke else { return };
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
    fill_device(ctx, &pieces, color, paint.opacity);
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

/// The columns a set of edges can possibly touch. Without this every fill
/// walked the whole image width per scanline, which a stroke made of dozens of
/// small quads pays for dozens of times over.
fn horizontal_span(edges: &[((f32, f32), (f32, f32))], width: usize) -> (usize, usize) {
    let (mut min, mut max) = (f32::MAX, f32::MIN);
    for (a, b) in edges {
        min = min.min(a.0).min(b.0);
        max = max.max(a.0).max(b.0);
    }
    let low = min.floor().max(0.0) as usize;
    let high = (max.ceil() + 1.0).max(0.0) as usize;
    (low.min(width), high.min(width))
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

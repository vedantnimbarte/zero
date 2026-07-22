//! Paint: turn layout boxes into a display list, then rasterize to a pixel canvas.
//!
//! ponytail: solid-color backgrounds + borders + anti-aliased text + nearest-neighbor
//! images. No gradients, shadows, border-radius, or GPU compositing yet
//! (docs/01-ARCHITECTURE.md §3 [6]-[7]).

use crate::css::{Color, Value};
use crate::dom::NodeType;
use crate::layout::{BoxType, LayoutBox, Rect, TextFragment};
use crate::resource::{DecodedImage, ImageMap};
use crate::text::FontSet;

pub struct Canvas {
    pub pixels: Vec<Color>,
    pub width: usize,
    pub height: usize,
}

enum DisplayCommand {
    SolidColor(Color, Rect),
    /// A rounded rectangle: same as SolidColor but with a corner radius.
    RoundedColor(Color, Rect, f32),
    /// A linear gradient between stops, vertical unless `horizontal`.
    Gradient {
        rect: Rect,
        radius: f32,
        stops: Vec<Color>,
        horizontal: bool,
    },
    /// A soft drop shadow behind a box.
    Shadow {
        rect: Rect,
        radius: f32,
        blur: f32,
        color: Color,
    },
    Text(TextFragment),
    Image(String, Rect), // image src, destination content box
}

type DisplayList = Vec<DisplayCommand>;

impl Canvas {
    fn new(width: usize, height: usize) -> Canvas {
        let white = Color {
            r: 255,
            g: 255,
            b: 255,
            a: 255,
        };
        Canvas {
            pixels: vec![white; width * height],
            width,
            height,
        }
    }

    /// Fill a rect, blending when the colour is translucent.
    ///
    /// Overwriting regardless of alpha would paint `transparent` as solid black
    /// and every `rgba()` overlay as opaque.
    fn paint_solid(&mut self, color: Color, rect: Rect) {
        if color.a == 0 {
            return;
        }
        let x0 = rect.x.clamp(0.0, self.width as f32) as usize;
        let y0 = rect.y.clamp(0.0, self.height as f32) as usize;
        let x1 = (rect.x + rect.width).clamp(0.0, self.width as f32) as usize;
        let y1 = (rect.y + rect.height).clamp(0.0, self.height as f32) as usize;
        for y in y0..y1 {
            for x in x0..x1 {
                let idx = y * self.width + x;
                self.pixels[idx] = match color.a {
                    255 => color,
                    _ => blend(self.pixels[idx], color, 255),
                };
            }
        }
    }

    /// Fill a rounded rectangle with analytic anti-aliasing on the corner arcs.
    fn paint_rounded(&mut self, color: Color, rect: Rect, radius: f32) {
        let radius = radius.min(rect.width / 2.0).min(rect.height / 2.0).max(0.0);
        let x0 = rect.x.clamp(0.0, self.width as f32) as usize;
        let y0 = rect.y.clamp(0.0, self.height as f32) as usize;
        let x1 = (rect.x + rect.width).clamp(0.0, self.width as f32) as usize;
        let y1 = (rect.y + rect.height).clamp(0.0, self.height as f32) as usize;

        // Corner centres: inside the rect by `radius` on each axis.
        let (left, right) = (rect.x + radius, rect.x + rect.width - radius);
        let (top, bottom) = (rect.y + radius, rect.y + rect.height - radius);

        for y in y0..y1 {
            for x in x0..x1 {
                let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
                // Distance outside the nearest corner circle, or 0 in the straight parts.
                let dx = if px < left {
                    left - px
                } else if px > right {
                    px - right
                } else {
                    0.0
                };
                let dy = if py < top {
                    top - py
                } else if py > bottom {
                    py - bottom
                } else {
                    0.0
                };
                let coverage = if dx == 0.0 || dy == 0.0 {
                    1.0
                } else {
                    // Soften across one pixel at the arc edge.
                    (radius + 0.5 - (dx * dx + dy * dy).sqrt()).clamp(0.0, 1.0)
                };
                if coverage <= 0.0 {
                    continue;
                }
                let idx = y * self.width + x;
                let alpha = (coverage * 255.0) as u8;
                self.pixels[idx] = blend(self.pixels[idx], color, alpha);
            }
        }
    }

    /// Fill a rect by interpolating between colour stops along one axis.
    fn paint_gradient(&mut self, rect: Rect, radius: f32, stops: &[Color], horizontal: bool) {
        if stops.is_empty() {
            return;
        }
        let x0 = rect.x.clamp(0.0, self.width as f32) as usize;
        let y0 = rect.y.clamp(0.0, self.height as f32) as usize;
        let x1 = (rect.x + rect.width).clamp(0.0, self.width as f32) as usize;
        let y1 = (rect.y + rect.height).clamp(0.0, self.height as f32) as usize;
        let span = if horizontal { rect.width } else { rect.height };
        if span <= 0.0 {
            return;
        }
        let (left, right) = (rect.x + radius, rect.x + rect.width - radius);
        let (top, bottom) = (rect.y + radius, rect.y + rect.height - radius);

        for y in y0..y1 {
            for x in x0..x1 {
                let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
                // Reuse the rounded-corner coverage so gradients can be rounded too.
                let coverage = if radius > 0.0 {
                    let dx = if px < left {
                        left - px
                    } else if px > right {
                        px - right
                    } else {
                        0.0
                    };
                    let dy = if py < top {
                        top - py
                    } else if py > bottom {
                        py - bottom
                    } else {
                        0.0
                    };
                    if dx == 0.0 || dy == 0.0 {
                        1.0
                    } else {
                        (radius + 0.5 - (dx * dx + dy * dy).sqrt()).clamp(0.0, 1.0)
                    }
                } else {
                    1.0
                };
                if coverage <= 0.0 {
                    continue;
                }
                let t = if horizontal {
                    (px - rect.x) / span
                } else {
                    (py - rect.y) / span
                };
                let color = sample_stops(stops, t.clamp(0.0, 1.0));
                let idx = y * self.width + x;
                self.pixels[idx] = blend(self.pixels[idx], color, (coverage * 255.0) as u8);
            }
        }
    }

    /// Draw a blurred rectangle behind a box. Alpha falls off linearly across
    /// `blur`, which reads close enough to a Gaussian at these sizes.
    fn paint_shadow(&mut self, rect: Rect, radius: f32, blur: f32, color: Color) {
        let blur = blur.max(0.0);
        let x0 = (rect.x - blur).clamp(0.0, self.width as f32) as usize;
        let y0 = (rect.y - blur).clamp(0.0, self.height as f32) as usize;
        let x1 = (rect.x + rect.width + blur).clamp(0.0, self.width as f32) as usize;
        let y1 = (rect.y + rect.height + blur).clamp(0.0, self.height as f32) as usize;
        let (left, right) = (rect.x + radius, rect.x + rect.width - radius);
        let (top, bottom) = (rect.y + radius, rect.y + rect.height - radius);

        for y in y0..y1 {
            for x in x0..x1 {
                let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
                let dx = if px < left {
                    left - px
                } else if px > right {
                    px - right
                } else {
                    0.0
                };
                let dy = if py < top {
                    top - py
                } else if py > bottom {
                    py - bottom
                } else {
                    0.0
                };
                let dist = (dx * dx + dy * dy).sqrt() - radius;
                let coverage = if dist <= 0.0 {
                    1.0
                } else if blur > 0.0 {
                    1.0 - dist / blur
                } else {
                    0.0
                };
                if coverage <= 0.0 {
                    continue;
                }
                let idx = y * self.width + x;
                let alpha = (coverage * color.a as f32) as u8;
                self.pixels[idx] = blend(self.pixels[idx], color, alpha);
            }
        }
    }

    /// Rasterize a shaped run glyph-by-glyph and alpha-blend it onto the canvas.
    /// Positions come from the shaper, so scripts that reorder or stack marks land correctly.
    /// Uses the same font the shaper picked, so glyph ids resolve correctly.
    fn paint_text(&mut self, frag: &TextFragment, fonts: &FontSet) {
        let font = match fonts.entries.get(frag.font_index) {
            Some(entry) => entry.raster,
            None => return,
        };
        let ascent = font
            .horizontal_line_metrics(frag.size)
            .map_or(frag.size, |m| m.ascent);
        let baseline = frag.y + ascent;

        for glyph in &frag.glyphs {
            let (m, coverage) = font.rasterize_indexed(glyph.id, frag.size);
            // fontdue gives per-pixel coverage (0..=255); place relative to the baseline.
            let gx = (frag.x + glyph.x + m.xmin as f32).round() as i32;
            let gy = (baseline - glyph.y - m.ymin as f32 - m.height as f32).round() as i32;

            for row in 0..m.height {
                for col in 0..m.width {
                    let a = coverage[row * m.width + col];
                    if a == 0 {
                        continue;
                    }
                    let px = gx + col as i32;
                    let py = gy + row as i32;
                    if px < 0 || py < 0 || px >= self.width as i32 || py >= self.height as i32 {
                        continue;
                    }
                    let idx = py as usize * self.width + px as usize;
                    self.pixels[idx] = blend(self.pixels[idx], frag.color, a);
                }
            }
        }
    }

    /// Blit a decoded image into `dest`, nearest-neighbor scaled and alpha-blended.
    fn paint_image(&mut self, img: &DecodedImage, dest: Rect) {
        let (dw, dh) = (dest.width as i32, dest.height as i32);
        if dw <= 0 || dh <= 0 || img.width == 0 || img.height == 0 {
            return;
        }
        let (x0, y0) = (dest.x as i32, dest.y as i32);
        for dy in 0..dh {
            let sy = ((dy as f32 / dh as f32) * img.height as f32) as usize;
            let sy = sy.min(img.height - 1);
            let py = y0 + dy;
            if py < 0 || py >= self.height as i32 {
                continue;
            }
            for dx in 0..dw {
                let sx = ((dx as f32 / dw as f32) * img.width as f32) as usize;
                let sx = sx.min(img.width - 1);
                let px = x0 + dx;
                if px < 0 || px >= self.width as i32 {
                    continue;
                }
                let src = img.pixels[sy * img.width + sx];
                let idx = py as usize * self.width + px as usize;
                self.pixels[idx] = blend(self.pixels[idx], src, src.a);
            }
        }
    }
}

/// Alpha-blend `src` (scaled by `coverage`) over `dst`.
fn blend(dst: Color, src: Color, coverage: u8) -> Color {
    let a = (coverage as f32 / 255.0) * (src.a as f32 / 255.0);
    let mix = |d: u8, s: u8| (s as f32 * a + d as f32 * (1.0 - a)).round() as u8;
    Color {
        r: mix(dst.r, src.r),
        g: mix(dst.g, src.g),
        b: mix(dst.b, src.b),
        a: 255,
    }
}

/// Paint a laid-out page. `find` highlights the runs matching a find-in-page
/// query and reports where they are, so the embedder can scroll to them.
pub fn paint(
    layout_root: &LayoutBox,
    bounds: Rect,
    fonts: Option<&FontSet>,
    images: &ImageMap,
    find: Option<&str>,
) -> (Canvas, Vec<Rect>) {
    let display_list = build_display_list(layout_root);
    let mut canvas = Canvas::new(bounds.width as usize, bounds.height as usize);
    let matches = find
        .map(|q| highlight_rects(&display_list, q))
        .unwrap_or_default();
    // The root background paints the whole canvas, not just the root's box, so a
    // short dark page doesn't leave white below it (CSS 2.1 §14.2).
    if let Some(color) = canvas_background(layout_root) {
        canvas.paint_solid(color, bounds);
    }
    // Two passes: everything under the text, then the find highlights, then the
    // text itself — a highlight must cover page backgrounds but sit under words.
    for pass in [Pass::Boxes, Pass::Text] {
        if pass == Pass::Text {
            for rect in &matches {
                canvas.paint_solid(HIGHLIGHT, *rect);
            }
        }
        for item in display_list.iter().filter(|i| pass_of(i) == pass) {
            match item {
                DisplayCommand::SolidColor(color, rect) => canvas.paint_solid(*color, *rect),
                DisplayCommand::RoundedColor(color, rect, radius) => {
                    canvas.paint_rounded(*color, *rect, *radius)
                }
                DisplayCommand::Gradient {
                    rect,
                    radius,
                    stops,
                    horizontal,
                } => canvas.paint_gradient(*rect, *radius, stops, *horizontal),
                DisplayCommand::Shadow {
                    rect,
                    radius,
                    blur,
                    color,
                } => canvas.paint_shadow(*rect, *radius, *blur, *color),
                DisplayCommand::Text(frag) => {
                    if let Some(fonts) = fonts {
                        canvas.paint_text(frag, fonts);
                    }
                }
                DisplayCommand::Image(src, rect) => {
                    if let Some(img) = images.get(src) {
                        canvas.paint_image(img, *rect);
                    }
                }
            }
        }
    }
    (canvas, matches)
}

/// Text paints above every box, so highlights can slot between the two.
#[derive(PartialEq, Clone, Copy)]
enum Pass {
    Boxes,
    Text,
}

fn pass_of(item: &DisplayCommand) -> Pass {
    match item {
        DisplayCommand::Text(_) => Pass::Text,
        _ => Pass::Boxes,
    }
}

/// Amber, matching the bookmark star: visible on light and dark pages alike.
const HIGHLIGHT: Color = Color {
    r: 245,
    g: 165,
    b: 36,
    a: 190,
};

/// Boxes of the text runs containing `query`, case-insensitively.
///
/// ponytail: highlights the whole word a match falls in, not the exact
/// substring — runs are shaped per word, and glyphs no longer map to characters.
fn highlight_rects(list: &DisplayList, query: &str) -> Vec<Rect> {
    let needle = query.to_lowercase();
    if needle.is_empty() {
        return Vec::new();
    }
    list.iter()
        .filter_map(|item| match item {
            DisplayCommand::Text(frag) if frag.text.to_lowercase().contains(&needle) => {
                Some(Rect {
                    x: frag.x,
                    y: frag.y,
                    width: frag.width,
                    height: frag.size * 1.25,
                })
            }
            _ => None,
        })
        .collect()
}

fn build_display_list(layout_root: &LayoutBox) -> DisplayList {
    let mut list = Vec::new();
    render_layout_box(&mut list, layout_root, UNCLIPPED);
    list
}

/// A clip large enough to hold any page, so the root needs no special case.
const UNCLIPPED: Rect = Rect {
    x: -1.0e7,
    y: -1.0e7,
    width: 2.0e7,
    height: 2.0e7,
};

/// The overlap of two rects, or `None` when they miss each other.
fn intersect(a: Rect, b: Rect) -> Option<Rect> {
    let x = a.x.max(b.x);
    let y = a.y.max(b.y);
    let right = (a.x + a.width).min(b.x + b.width);
    let bottom = (a.y + a.height).min(b.y + b.height);
    match right > x && bottom > y {
        true => Some(Rect {
            x,
            y,
            width: right - x,
            height: bottom - y,
        }),
        false => None,
    }
}

/// Trim one command to a clip rect, dropping it if nothing is left.
///
/// ponytail: text is kept or dropped whole. A word straddling the clip edge is
/// drawn in full, which needs a per-glyph clip in the rasterizer to fix — and
/// the common case, a box collapsed to `height: 0`, drops everything cleanly.
fn clip_command(item: DisplayCommand, clip: Rect) -> Option<DisplayCommand> {
    Some(match item {
        DisplayCommand::SolidColor(color, rect) => {
            DisplayCommand::SolidColor(color, intersect(rect, clip)?)
        }
        // Trimming a rounded box would square off the corner that survives, so
        // it is kept whole unless the clip removes it entirely.
        DisplayCommand::RoundedColor(color, rect, radius) => {
            intersect(rect, clip)?;
            DisplayCommand::RoundedColor(color, rect, radius)
        }
        DisplayCommand::Image(src, rect) => DisplayCommand::Image(src, intersect(rect, clip)?),
        DisplayCommand::Text(frag) => {
            let rect = Rect {
                x: frag.x,
                y: frag.y,
                width: frag.width,
                height: frag.size * 1.25,
            };
            intersect(rect, clip)?;
            DisplayCommand::Text(frag)
        }
        other => other,
    })
}

/// The clip a box imposes on its descendants: its padding box, when `overflow`
/// says content may not escape it.
fn child_clip(layout_box: &LayoutBox, clip: Rect) -> Option<Rect> {
    let style = match layout_box.box_type {
        BoxType::BlockNode(s) | BoxType::InlineNode(s) => s,
        BoxType::AnonymousBlock => return Some(clip),
    };
    let overflow = match style.value("overflow") {
        Some(Value::Keyword(k)) => k,
        Some(Value::Raw(raw)) => raw.split_whitespace().next()?.to_string(),
        _ => return Some(clip),
    };
    match overflow.as_str() {
        // No scrollbars: a scrollable box shows its first screenful, which is
        // what a collapsed menu or a clipped banner needs.
        "hidden" | "clip" | "auto" | "scroll" => intersect(layout_box.dimensions.padding_box(), clip),
        _ => Some(clip),
    }
}

/// `visibility: hidden` — the box and its text are not painted, though a
/// descendant that sets `visibility: visible` still is.
fn is_invisible(layout_box: &LayoutBox) -> bool {
    let style = match layout_box.box_type {
        BoxType::BlockNode(s) | BoxType::InlineNode(s) => s,
        BoxType::AnonymousBlock => return false,
    };
    matches!(style.value("visibility"), Some(Value::Keyword(ref k)) if k == "hidden" || k == "collapse")
}

fn render_layout_box(list: &mut DisplayList, layout_box: &LayoutBox, clip: Rect) {
    if !is_invisible(layout_box) {
        let mut own = Vec::new();
        render_own(&mut own, layout_box);
        list.extend(own.into_iter().filter_map(|item| clip_command(item, clip)));
    }
    // An empty clip still has to be handed down: a child of a hidden box is
    // hidden too, however visible it declares itself.
    let inner = child_clip(layout_box, clip).unwrap_or(Rect::default());
    for child in &layout_box.children {
        render_layout_box(list, child, inner);
    }
}

fn render_own(list: &mut DisplayList, layout_box: &LayoutBox) {
    render_shadow(list, layout_box);
    render_background(list, layout_box);
    render_borders(list, layout_box);
    if let Some(src) = image_src(layout_box) {
        list.push(DisplayCommand::Image(src, layout_box.dimensions.content));
    }
    // Inline element backgrounds sit under their own text but over the block's.
    for inline in &layout_box.inline_boxes {
        let rect = Rect {
            x: inline.x,
            y: inline.y,
            width: inline.width,
            height: inline.height,
        };
        if let Some(background) = inline.background {
            if inline.radius > 0.0 {
                list.push(DisplayCommand::RoundedColor(
                    background,
                    rect,
                    inline.radius,
                ));
            } else {
                list.push(DisplayCommand::SolidColor(background, rect));
            }
        }
        if let Some(border) = inline.border_color {
            // A hairline outline is enough until inline border widths are modelled.
            list.push(DisplayCommand::SolidColor(
                border,
                Rect {
                    height: 1.0,
                    ..rect
                },
            ));
            list.push(DisplayCommand::SolidColor(
                border,
                Rect {
                    y: rect.y + rect.height - 1.0,
                    height: 1.0,
                    ..rect
                },
            ));
        }
    }
    // Text sits above this box's background/borders.
    for frag in &layout_box.text_fragments {
        list.push(DisplayCommand::Text(frag.clone()));
    }
}

fn image_src(layout_box: &LayoutBox) -> Option<String> {
    let style = match layout_box.box_type {
        BoxType::BlockNode(s) | BoxType::InlineNode(s) => s,
        BoxType::AnonymousBlock => return None,
    };
    match style.node.node_type {
        NodeType::Element(ref e) if e.tag_name == "img" => e.attributes.get("src").cloned(),
        _ => None,
    }
}

/// `box-shadow: <x> <y> <blur> <color>` — drawn before the background so it sits behind.
fn render_shadow(list: &mut DisplayList, layout_box: &LayoutBox) {
    let style = match layout_box.box_type {
        BoxType::BlockNode(s) | BoxType::InlineNode(s) => s,
        BoxType::AnonymousBlock => return,
    };
    let spec = match style.value("box-shadow") {
        Some(Value::Raw(spec)) => spec,
        _ => return,
    };
    let ctx = style.length_context(0.0);
    let mut offset = [0.0_f32; 3]; // x, y, blur
    let mut color = Color {
        r: 0,
        g: 0,
        b: 0,
        a: 80,
    };
    let mut lengths = 0;
    for token in spec.split_whitespace() {
        if let Some(hex) = token.strip_prefix('#') {
            if let Some(Value::ColorValue(c)) = crate::css::parse_color_token(hex) {
                color = c;
            }
        } else if lengths < 3 {
            offset[lengths] = crate::css::parse_length_token(token, ctx);
            lengths += 1;
        }
    }
    let b = layout_box.dimensions.border_box();
    let rect = Rect {
        x: b.x + offset[0],
        y: b.y + offset[1],
        width: b.width,
        height: b.height,
    };
    list.push(DisplayCommand::Shadow {
        rect,
        radius: border_radius(layout_box, b),
        blur: offset[2],
        color,
    });
}

fn render_background(list: &mut DisplayList, layout_box: &LayoutBox) {
    // A gradient wins over a flat colour, like `background-image` over `background-color`.
    if let Some(style) = match layout_box.box_type {
        BoxType::BlockNode(s) | BoxType::InlineNode(s) => Some(s),
        BoxType::AnonymousBlock => None,
    } {
        let spec = style
            .value("background-image")
            .or_else(|| style.value("background"))
            .and_then(|v| match v {
                Value::Raw(spec) => Some(spec),
                _ => None,
            });
        if let Some(spec) = spec {
            if let Some((stops, horizontal)) = parse_gradient(&spec) {
                let rect = layout_box.dimensions.border_box();
                let radius = border_radius(layout_box, rect);
                list.push(DisplayCommand::Gradient {
                    rect,
                    radius,
                    stops,
                    horizontal,
                });
                return;
            }
        }
    }
    let color = match get_color(layout_box, "background")
        .or_else(|| get_color(layout_box, "background-color"))
    {
        Some(c) => c,
        None => return,
    };
    let box_rect = layout_box.dimensions.border_box();
    let radius = border_radius(layout_box, box_rect);
    if radius > 0.0 {
        list.push(DisplayCommand::RoundedColor(color, box_rect, radius));
    } else {
        list.push(DisplayCommand::SolidColor(color, box_rect));
    }
}

/// Parse `linear-gradient(<direction>?, stop, stop, ...)` into colour stops.
/// ponytail: no angles, no explicit stop positions — stops are spaced evenly.
fn parse_gradient(spec: &str) -> Option<(Vec<Color>, bool)> {
    let inner = spec
        .trim()
        .strip_prefix("linear-gradient(")?
        .strip_suffix(')')?;
    let mut horizontal = false;
    let mut stops = Vec::new();
    for (i, part) in inner.split(',').enumerate() {
        let part = part.trim();
        if i == 0 && part.starts_with("to ") {
            horizontal = part.contains("right") || part.contains("left");
            continue;
        }
        // Take the colour token, ignoring any stop position that follows it.
        if let Some(token) = part.split_whitespace().next() {
            if let Some(hex) = token.strip_prefix('#') {
                if let Some(Value::ColorValue(c)) = crate::css::parse_color_token(hex) {
                    stops.push(c);
                }
            }
        }
    }
    if stops.len() < 2 {
        return None;
    }
    Some((stops, horizontal))
}

/// Interpolate between evenly spaced stops at position `t` in 0..=1.
fn sample_stops(stops: &[Color], t: f32) -> Color {
    if stops.len() == 1 {
        return stops[0];
    }
    let scaled = t * (stops.len() - 1) as f32;
    let i = (scaled.floor() as usize).min(stops.len() - 2);
    let f = scaled - i as f32;
    let (a, b) = (stops[i], stops[i + 1]);
    let mix = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * f).round() as u8;
    Color {
        r: mix(a.r, b.r),
        g: mix(a.g, b.g),
        b: mix(a.b, b.b),
        a: mix(a.a, b.a),
    }
}

/// Resolve `border-radius`, with percentages relative to the box's smaller side.
fn border_radius(layout_box: &LayoutBox, box_rect: Rect) -> f32 {
    let style = match layout_box.box_type {
        BoxType::BlockNode(s) | BoxType::InlineNode(s) => s,
        BoxType::AnonymousBlock => return 0.0,
    };
    let base = box_rect.width.min(box_rect.height);
    style.px("border-radius", base).unwrap_or(0.0).max(0.0)
}

fn render_borders(list: &mut DisplayList, layout_box: &LayoutBox) {
    let color = match get_color(layout_box, "border-color") {
        Some(color) => color,
        None => return,
    };
    let d = &layout_box.dimensions;
    let b = d.border_box();

    // Left, right, top, bottom border strips.
    list.push(DisplayCommand::SolidColor(
        color,
        Rect {
            x: b.x,
            y: b.y,
            width: d.border.left,
            height: b.height,
        },
    ));
    list.push(DisplayCommand::SolidColor(
        color,
        Rect {
            x: b.x + b.width - d.border.right,
            y: b.y,
            width: d.border.right,
            height: b.height,
        },
    ));
    list.push(DisplayCommand::SolidColor(
        color,
        Rect {
            x: b.x,
            y: b.y,
            width: b.width,
            height: d.border.top,
        },
    ));
    list.push(DisplayCommand::SolidColor(
        color,
        Rect {
            x: b.x,
            y: b.y + b.height - d.border.bottom,
            width: b.width,
            height: d.border.bottom,
        },
    ));
}

/// The colour that propagates to the canvas: the root element's own background,
/// or `<body>`'s if the root has none.
fn canvas_background(root: &LayoutBox) -> Option<Color> {
    let of =
        |b: &LayoutBox| get_color(b, "background").or_else(|| get_color(b, "background-color"));
    // Only `<body>` inherits this privilege. Taking it from whichever child
    // happened to have a background flooded the page with, say, a hidden
    // dropdown's colour.
    let body = root
        .children
        .iter()
        .find(|child| matches!(child.box_type,
            BoxType::BlockNode(s) | BoxType::InlineNode(s)
                if matches!(&s.node.node_type, NodeType::Element(e) if e.tag_name == "body")));
    of(root).or_else(|| body.and_then(of))
}

fn get_color(layout_box: &LayoutBox, name: &str) -> Option<Color> {
    match layout_box.box_type {
        BoxType::BlockNode(style) | BoxType::InlineNode(style) => match style.value(name) {
            Some(Value::ColorValue(color)) => Some(color),
            _ => None,
        },
        BoxType::AnonymousBlock => None,
    }
}

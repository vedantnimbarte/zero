//! Paint: turn layout boxes into a display list, then rasterize to a pixel canvas.
//!
//! Backgrounds (solid, gradient, rounded), borders, shadows, anti-aliased text
//! and nearest-neighbour images, with `opacity`, `z-index`, `visibility`,
//! `overflow` clipping and `transform: translate` applied as the list is built.
//!
//! ponytail: the display list is flat, so `z-index` orders siblings rather than
//! establishing stacking contexts, and opacity is applied per command rather
//! than to a composited layer. Rasterizing is on the CPU — no GPU compositor
//! yet (docs/01-ARCHITECTURE.md §3 [6]-[7]).

use crate::css::{Color, LengthContext, Value};
use crate::dom::NodeType;
use crate::layout::{BoxType, LayoutBox, Rect, TextFragment};
use crate::resource::{DecodedImage, ImageMap};
use crate::text::FontSet;

pub struct Canvas {
    pub pixels: Vec<Color>,
    pub width: usize,
    pub height: usize,
}

#[derive(Clone)]
enum DisplayCommand {
    SolidColor(Color, Rect),
    /// A rounded rectangle: same as SolidColor but with a corner radius.
    RoundedColor(Color, Rect, f32),
    /// A linear or radial gradient between stops.
    Gradient {
        rect: Rect,
        radius: f32,
        /// Each stop's colour and position (0.0..=1.0 along the gradient line
        /// for `Linear`, or from the centre outward for `Radial`).
        stops: Vec<(Color, f32)>,
        shape: GradientShape,
    },
    /// A soft drop shadow behind a box.
    Shadow {
        rect: Rect,
        radius: f32,
        blur: f32,
        color: Color,
    },
    /// A shaped run, and the rectangle it may draw inside. Text is the one
    /// command the painter cannot trim by moving its box — the glyphs are
    /// already placed — so the clip travels with it and is applied per pixel.
    Text(TextFragment, Rect),
    /// image src, destination content box, and how it fits that box.
    Image(String, Rect, ObjectFit),
    /// A subtree drawn into its own buffer and mapped through a transform that
    /// is not a scale-and-offset — a rotation, a skew, a flip, or a scale that
    /// differs per axis.
    ///
    /// Everything else folds into [`Xf`] and paints straight onto the canvas.
    /// This is the one path that needs an offscreen buffer, because a rotated
    /// rectangle is not a rectangle and every primitive the painter has is.
    Layer {
        list: DisplayList,
        /// The area of the page the buffer stands for, in the subtree's own
        /// (untransformed) coordinates.
        source: Rect,
        /// Maps those coordinates to where they are drawn.
        matrix: crate::css::Mat,
        /// What the ancestors allow this layer to cover, in drawn coordinates.
        clip: Rect,
        /// `filter`, applied to the finished buffer before it is composited.
        /// Which is why a filter needs a layer at all: it is defined over the
        /// element's rendered result, not over each thing it paints.
        filters: Vec<FilterOp>,
    },
    /// `backdrop-filter`: filter what is already painted beneath a box, in
    /// place, before the box paints over it.
    Backdrop {
        rect: Rect,
        radius: f32,
        filters: Vec<FilterOp>,
        clip: Rect,
    },
    /// `background-image: url(...)`, resolved and tiled at paint time once the
    /// image's own intrinsic size is known — see `Canvas::paint_background_image`.
    BackgroundImage {
        src: String,
        rect: Rect,
        size: BgSize,
        position: (BgAxis, BgAxis),
        repeat: BgRepeat,
    },
}

/// Walk a run's rasterized coverage, pixel by pixel, in canvas coordinates.
///
/// The one place glyph ink is produced, so the shadow mask and the painted
/// glyphs are guaranteed to be the same shape — including the synthesized bold
/// and italic below, which are part of that shape.
fn run_ink(
    frag: &TextFragment,
    font: &fontdue::Font,
    baseline: f32,
    mut plot: impl FnMut(i32, i32, u8),
) {
    // No bold/italic font file is loaded (Track D4), so a heavier weight and a
    // slant are synthesized here instead of picked from a face: bold redraws
    // each row a pixel wider, italic shears columns toward the top by `row`'s
    // distance from the baseline.
    let stroke = if frag.bold {
        (frag.size / 24.0).max(1.0).round() as i32
    } else {
        0
    };
    let shear = if frag.italic { 0.22 } else { 0.0 };

    for glyph in &frag.glyphs {
        let (m, coverage) = font.rasterize_indexed(glyph.id, frag.size);
        // fontdue gives per-pixel coverage (0..=255); place relative to the baseline.
        let gx = (frag.x + glyph.x + m.xmin as f32).round() as i32;
        let gy = (baseline - glyph.y - m.ymin as f32 - m.height as f32).round() as i32;

        for row in 0..m.height {
            let row_shear = (shear * (m.height - row) as f32).round() as i32;
            let py = gy + row as i32;
            for col in 0..m.width {
                let a = coverage[row * m.width + col];
                if a == 0 {
                    continue;
                }
                for dx in 0..=stroke {
                    plot(gx + col as i32 + row_shear + dx, py, a);
                }
            }
        }
    }
}

/// One entry of a `filter` or `backdrop-filter` chain.
///
/// Each carries its amount already resolved from the percentage or number the
/// page wrote, so applying a chain is arithmetic and no longer parsing.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum FilterOp {
    /// The CSS blur *radius*; sigma is half of it.
    Blur(f32),
    Brightness(f32),
    Contrast(f32),
    Grayscale(f32),
    /// In radians.
    HueRotate(f32),
    Invert(f32),
    Opacity(f32),
    Saturate(f32),
    Sepia(f32),
    DropShadow(ShadowSpec),
}

impl FilterOp {
    /// How far this op spreads past the pixels it is given, so a buffer can be
    /// made big enough to hold the result.
    fn margin(self) -> f32 {
        match self {
            FilterOp::Blur(radius) => mask_margin(sigma_for(radius)) as f32,
            FilterOp::DropShadow(shadow) => {
                shadow.dx.abs() + shadow.dy.abs() + mask_margin(sigma_for(shadow.blur)) as f32
            }
            _ => 0.0,
        }
    }
}

/// `grayscale(amount)`: each channel moves `amount` of the way to the pixel's
/// own luminance.
fn mix_towards_luminance(l: [f32; 3], amount: f32) -> [[f32; 3]; 3] {
    let keep = 1.0 - amount;
    [
        [l[0] * amount + keep, l[1] * amount, l[2] * amount],
        [l[0] * amount, l[1] * amount + keep, l[2] * amount],
        [l[0] * amount, l[1] * amount, l[2] * amount + keep],
    ]
}

/// `saturate(amount)` — the same shape, with `amount` above 1 pushing away
/// from luminance rather than towards it.
fn saturate_matrix(l: [f32; 3], amount: f32) -> [[f32; 3]; 3] {
    mix_towards_luminance(l, 1.0 - amount)
}

/// The sepia matrix from the filter spec, faded in by `amount`.
fn sepia_matrix(amount: f32) -> [[f32; 3]; 3] {
    let full = [
        [0.393, 0.769, 0.189],
        [0.349, 0.686, 0.168],
        [0.272, 0.534, 0.131],
    ];
    let identity = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];
    std::array::from_fn(|r| {
        std::array::from_fn(|c| identity[r][c] * (1.0 - amount) + full[r][c] * amount)
    })
}

/// The hue-rotation matrix from the filter spec.
fn hue_rotate_matrix(radians: f32) -> [[f32; 3]; 3] {
    let (sin, cos) = radians.sin_cos();
    [
        [
            0.213 + cos * 0.787 - sin * 0.213,
            0.715 - cos * 0.715 - sin * 0.715,
            0.072 - cos * 0.072 + sin * 0.928,
        ],
        [
            0.213 - cos * 0.213 + sin * 0.143,
            0.715 + cos * 0.285 + sin * 0.140,
            0.072 - cos * 0.072 - sin * 0.283,
        ],
        [
            0.213 - cos * 0.213 - sin * 0.787,
            0.715 - cos * 0.715 + sin * 0.715,
            0.072 + cos * 0.928 + sin * 0.072,
        ],
    ]
}

/// Whether a point falls outside a rounded rectangle — the corner test, shared
/// by the frosted-glass clip.
fn outside_rounded(rect: Rect, radius: f32, px: f32, py: f32) -> bool {
    let (left, right) = (rect.x + radius, rect.x + rect.width - radius);
    let (top, bottom) = (rect.y + radius, rect.y + rect.height - radius);
    let dx = (left - px).max(px - right).max(0.0);
    let dy = (top - py).max(py - bottom).max(0.0);
    (dx * dx + dy * dy).sqrt() > radius
}

/// Parse a `filter` / `backdrop-filter` function list.
///
/// An unrecognised function is skipped rather than failing the whole chain: a
/// page that asks for `url(#svg-filter)` alongside a blur should still get the
/// blur.
fn parse_filter_list(spec: &str, ctx: crate::css::LengthContext, color: Color) -> Vec<FilterOp> {
    let mut out = Vec::new();
    let mut rest = spec.trim();
    while let Some(open) = rest.find('(') {
        let name = rest[..open].trim().to_ascii_lowercase();
        let Some(close) = crate::css::matching_paren(&rest[open + 1..]) else {
            break;
        };
        let args = rest[open + 1..open + 1 + close].trim();
        rest = &rest[open + 1 + close + 1..];
        // `50%` and `0.5` mean the same thing to every one of these but blur,
        // hue-rotate and drop-shadow.
        let amount = |default: f32| match args.strip_suffix('%') {
            Some(pct) => pct
                .trim()
                .parse::<f32>()
                .map(|n| n / 100.0)
                .unwrap_or(default),
            None => args.parse::<f32>().unwrap_or(default),
        };
        let op = match name.as_str() {
            "blur" => FilterOp::Blur(crate::css::parse_length_token(args, ctx)),
            "brightness" => FilterOp::Brightness(amount(1.0)),
            "contrast" => FilterOp::Contrast(amount(1.0)),
            "grayscale" => FilterOp::Grayscale(amount(1.0).clamp(0.0, 1.0)),
            "hue-rotate" => FilterOp::HueRotate(crate::css::parse_angle(args).unwrap_or(0.0)),
            "invert" => FilterOp::Invert(amount(1.0).clamp(0.0, 1.0)),
            "opacity" => FilterOp::Opacity(amount(1.0).clamp(0.0, 1.0)),
            "saturate" => FilterOp::Saturate(amount(1.0)),
            "sepia" => FilterOp::Sepia(amount(1.0).clamp(0.0, 1.0)),
            "drop-shadow" => match parse_shadow_list(args, ctx, color).first() {
                Some(shadow) => FilterOp::DropShadow(*shadow),
                None => continue,
            },
            _ => continue,
        };
        out.push(op);
    }
    out
}

/// Blur a buffer of pixels in place, `sigma` being the Gaussian standard
/// deviation.
///
/// The one blur in the engine, shared with the SVG rasterizer's
/// `feGaussianBlur`: a shadow, a CSS filter and an SVG filter all want the same
/// three box passes, and a second implementation would only be a second thing
/// to get wrong.
pub(crate) fn blur_pixels(pixels: &mut [Color], width: usize, height: usize, sigma: f32) {
    if sigma <= 0.0 || pixels.len() != width * height {
        return;
    }
    let mut canvas = Canvas {
        pixels: pixels.to_vec(),
        width,
        height,
    };
    canvas.blur(sigma);
    pixels.copy_from_slice(&canvas.pixels);
}

/// Apply a filter chain to a buffer, in order.
fn apply_filters(canvas: &mut Canvas, ops: &[FilterOp]) {
    for op in ops {
        match *op {
            FilterOp::Blur(radius) => canvas.blur(sigma_for(radius)),
            FilterOp::DropShadow(shadow) => canvas.drop_shadow(shadow),
            // Everything else is one pass over the pixels: either a 3x3 colour
            // matrix or a per-channel curve, which are the same loop with
            // different coefficients.
            other => canvas.recolor(other),
        }
    }
}

/// One shadow from a `box-shadow` or `text-shadow` list.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct ShadowSpec {
    pub dx: f32,
    pub dy: f32,
    /// The CSS blur *radius*, which is not the Gaussian sigma — see
    /// [`sigma_for`].
    pub blur: f32,
    pub color: Color,
}

/// Per spec the blur radius is twice the standard deviation.
fn sigma_for(blur_radius: f32) -> f32 {
    (blur_radius / 2.0).max(0.0)
}

/// How far a blur bleeds past the shape it came from, so the mask is big
/// enough to hold it. Three sigma covers over 99% of a Gaussian.
fn mask_margin(sigma: f32) -> i32 {
    (sigma * 3.0).ceil() as i32 + 1
}

/// An 8-bit coverage buffer in canvas coordinates.
///
/// Every soft effect in the engine comes down to the same four steps —
/// rasterize coverage, blur it, tint it, composite it — so they share one
/// mask and one blur rather than each growing an approximation of its own.
struct Mask {
    x: i32,
    y: i32,
    width: usize,
    height: usize,
    alpha: Vec<u8>,
}

impl Mask {
    /// A mask covering `rect` plus `margin` on every side, clipped to the
    /// canvas so a shadow off the edge of a long page costs nothing.
    fn covering(rect: Rect, margin: i32, canvas_w: usize, canvas_h: usize) -> Self {
        let x0 = (rect.x.floor() as i32 - margin).max(-margin);
        let y0 = (rect.y.floor() as i32 - margin).max(-margin);
        let x1 = ((rect.x + rect.width).ceil() as i32 + margin).min(canvas_w as i32 + margin);
        let y1 = ((rect.y + rect.height).ceil() as i32 + margin).min(canvas_h as i32 + margin);
        Self::bounded(x0, y0, x1, y1)
    }

    fn bounded(x0: i32, y0: i32, x1: i32, y1: i32) -> Self {
        let width = (x1 - x0).max(0) as usize;
        let height = (y1 - y0).max(0) as usize;
        Mask {
            x: x0,
            y: y0,
            width,
            height,
            alpha: vec![0; width * height],
        }
    }

    /// Record coverage at a canvas pixel, keeping the strongest — overlapping
    /// glyphs must not add up to a darker patch where they cross.
    fn cover(&mut self, px: i32, py: i32, a: u8) {
        let (x, y) = (px - self.x, py - self.y);
        if x < 0 || y < 0 || x >= self.width as i32 || y >= self.height as i32 {
            return;
        }
        let idx = y as usize * self.width + x as usize;
        self.alpha[idx] = self.alpha[idx].max(a);
    }

    /// Blur in place.
    ///
    /// Three box passes per axis, which is the approximation the SVG filter
    /// spec itself prescribes: visually indistinguishable from a Gaussian and
    /// O(1) per pixel, where a two-dimensional convolution is O(radius^2).
    fn blur(&mut self, sigma: f32) {
        if sigma <= 0.0 || self.alpha.is_empty() {
            return;
        }
        // The box width that makes three passes match a Gaussian of this
        // standard deviation.
        let radius = ((sigma * 3.0 * (2.0 * std::f32::consts::PI).sqrt() / 4.0 + 0.5) / 2.0)
            .round()
            .max(1.0) as usize;
        let mut scratch = vec![0u8; self.alpha.len()];
        for _ in 0..3 {
            box_pass(
                &self.alpha,
                &mut scratch,
                self.width,
                self.height,
                radius,
                true,
            );
            box_pass(
                &scratch,
                &mut self.alpha,
                self.width,
                self.height,
                radius,
                false,
            );
        }
    }
}

/// One separable box-blur pass. `horizontal` picks the axis; the running sum
/// makes each output pixel two adds regardless of the radius.
fn box_pass(
    src: &[u8],
    dst: &mut [u8],
    width: usize,
    height: usize,
    radius: usize,
    horizontal: bool,
) {
    let (outer, inner) = if horizontal {
        (height, width)
    } else {
        (width, height)
    };
    let index = |o: usize, i: usize| {
        if horizontal {
            o * width + i
        } else {
            i * width + o
        }
    };
    let window = (radius * 2 + 1) as u32;
    for o in 0..outer {
        // Clamp at the edges, which is what keeps a shadow from fading out
        // where it runs off the mask.
        let at = |i: isize| src[index(o, i.clamp(0, inner as isize - 1) as usize)] as u32;
        let mut sum: u32 = (-(radius as isize)..=(radius as isize)).map(at).sum();
        for i in 0..inner {
            // Round rather than truncate: six passes of flooring throw away
            // most of a faint shadow before it ever reaches the canvas.
            dst[index(o, i)] = ((sum + window / 2) / window) as u8;
            sum = sum + at(i as isize + radius as isize + 1) - at(i as isize - radius as isize);
        }
    }
}

/// Parse a `box-shadow` / `text-shadow` value: a comma-separated list of
/// `<x> <y> <blur>? <color>?`, where the colour may come first or last and
/// defaults to the element's own `color`.
///
/// ponytail: `inset` and the spread radius are read past rather than applied —
/// neither is expressible in the single blurred mask this paints.
fn parse_shadow_list(
    spec: &str,
    ctx: crate::css::LengthContext,
    default: Color,
) -> Vec<ShadowSpec> {
    let mut out = Vec::new();
    let mut rest = spec.trim();
    while !rest.is_empty() {
        let (head, tail) = match crate::css::split_top_level_comma(rest) {
            Some((head, tail)) => (head, tail),
            None => (rest, ""),
        };
        rest = tail.trim_start();
        let mut lengths = [0.0_f32; 3];
        let mut seen = 0;
        let mut color = default;
        for token in split_shadow_tokens(head) {
            if token.eq_ignore_ascii_case("inset") {
                continue;
            }
            if let Some(Value::ColorValue(c)) = crate::css::parse_value(&token) {
                color = c;
            } else if seen < lengths.len() {
                lengths[seen] = crate::css::parse_length_token(&token, ctx);
                seen += 1;
            }
        }
        // Two offsets are the minimum a shadow can be written with.
        if seen >= 2 {
            out.push(ShadowSpec {
                dx: lengths[0],
                dy: lengths[1],
                blur: lengths[2],
                color,
            });
        }
    }
    out
}

/// Split one shadow on whitespace, keeping `rgba(0, 0, 0, .5)` in one piece.
fn split_shadow_tokens(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut depth = 0usize;
    for c in text.chars() {
        match c {
            '(' => {
                depth += 1;
                current.push(c);
            }
            ')' => {
                depth = depth.saturating_sub(1);
                current.push(c);
            }
            c if c.is_whitespace() && depth == 0 => {
                if !current.is_empty() {
                    out.push(std::mem::take(&mut current));
                }
            }
            c => current.push(c),
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

/// A gradient's geometry — everything but its colour stops.
#[derive(Clone, Copy, PartialEq)]
enum GradientShape {
    /// A direction (already resolved from an angle or `to <side>` keyword) as
    /// a unit vector, CSS's own convention: `(0, -1)` is "to top", `(0, 1)` is
    /// "to bottom" (the default with no direction given), etc.
    Linear { dx: f32, dy: f32 },
    /// Centred in the box, sized to its half-width/half-height — an ellipse
    /// matching the box's own aspect ratio rather than a circle.
    ///
    /// ponytail: only `ellipse ... at center` — no `circle`, no explicit
    /// position/size keywords (`closest-side`, `at top left`, ...). Covers the
    /// overwhelming common case (`radial-gradient(red, blue)`); add a keyword
    /// parser alongside `linear-gradient`'s `to <side>` parsing if a page
    /// needs more.
    Radial,
}

/// `object-fit` on a replaced element (`<img>`, inline `<svg>`).
#[derive(Clone, Copy, PartialEq)]
enum ObjectFit {
    Fill,
    Contain,
    Cover,
    None,
    ScaleDown,
}

/// `background-size`. Percentages/lengths are resolved to px against the box
/// at display-list build time; `None` on an axis means "auto" — the intrinsic
/// size for that axis, only knowable once the image itself is decoded.
#[derive(Clone, Copy, PartialEq)]
enum BgSize {
    Auto,
    Cover,
    Contain,
    Explicit(Option<f32>, Option<f32>),
}

/// One axis of `background-position`.
#[derive(Clone, Copy, PartialEq)]
enum BgAxis {
    /// A fraction of `(box size - image size)` — `center` is `Percent(0.5)`.
    Percent(f32),
    /// A literal offset from the box's edge, independent of the image size.
    Px(f32),
}

#[derive(Clone, Copy, PartialEq)]
enum BgRepeat {
    Repeat,
    RepeatX,
    RepeatY,
    NoRepeat,
}

type DisplayList = Vec<DisplayCommand>;

impl Canvas {
    /// A buffer that starts with nothing in it, for an off-axis subtree that
    /// will be composited rather than shown directly.
    fn transparent(width: usize, height: usize) -> Canvas {
        Canvas {
            pixels: vec![
                Color {
                    r: 0,
                    g: 0,
                    b: 0,
                    a: 0,
                };
                width * height
            ],
            width,
            height,
        }
    }

    /// Sample this canvas at a fractional position, bilinearly.
    ///
    /// This is what antialiases a rotated edge: the destination pixel lands
    /// between four source pixels and takes a weighted share of each, where
    /// nearest-neighbour would step.
    fn sample(&self, x: f32, y: f32) -> Color {
        let (x0, y0) = (x.floor() as i32, y.floor() as i32);
        let (fx, fy) = (x - x0 as f32, y - y0 as f32);
        // Premultiplied, or a transparent neighbour drags its colour in.
        let mut acc = [0.0f32; 4];
        for (dx, dy, weight) in [
            (0, 0, (1.0 - fx) * (1.0 - fy)),
            (1, 0, fx * (1.0 - fy)),
            (0, 1, (1.0 - fx) * fy),
            (1, 1, fx * fy),
        ] {
            if weight <= 0.0 {
                continue;
            }
            let (px, py) = (x0 + dx, y0 + dy);
            if px < 0 || py < 0 || px >= self.width as i32 || py >= self.height as i32 {
                continue;
            }
            let c = self.pixels[py as usize * self.width + px as usize];
            let a = c.a as f32 / 255.0;
            acc[0] += c.r as f32 * a * weight;
            acc[1] += c.g as f32 * a * weight;
            acc[2] += c.b as f32 * a * weight;
            acc[3] += a * weight;
        }
        if acc[3] <= 0.0 {
            return Color {
                r: 0,
                g: 0,
                b: 0,
                a: 0,
            };
        }
        Color {
            r: (acc[0] / acc[3]).round().clamp(0.0, 255.0) as u8,
            g: (acc[1] / acc[3]).round().clamp(0.0, 255.0) as u8,
            b: (acc[2] / acc[3]).round().clamp(0.0, 255.0) as u8,
            a: (acc[3] * 255.0).round().clamp(0.0, 255.0) as u8,
        }
    }

    /// Blur every channel of this buffer.
    ///
    /// Premultiplied and per plane, so the same box passes a shadow mask uses
    /// serve here too rather than a second blur being written.
    fn blur(&mut self, sigma: f32) {
        if sigma <= 0.0 || self.pixels.is_empty() {
            return;
        }
        let (width, height) = (self.width, self.height);
        let mut planes: [Vec<u8>; 4] = std::array::from_fn(|_| vec![0u8; self.pixels.len()]);
        for (i, p) in self.pixels.iter().enumerate() {
            let a = p.a as u32;
            planes[0][i] = (p.r as u32 * a / 255) as u8;
            planes[1][i] = (p.g as u32 * a / 255) as u8;
            planes[2][i] = (p.b as u32 * a / 255) as u8;
            planes[3][i] = p.a;
        }
        for plane in planes.iter_mut() {
            let mut mask = Mask {
                x: 0,
                y: 0,
                width,
                height,
                alpha: std::mem::take(plane),
            };
            mask.blur(sigma);
            *plane = mask.alpha;
        }
        for (i, p) in self.pixels.iter_mut().enumerate() {
            let a = planes[3][i];
            let unpremultiply = |v: u8| match a {
                0 => 0,
                a => ((v as u32 * 255 / a as u32).min(255)) as u8,
            };
            *p = Color {
                r: unpremultiply(planes[0][i]),
                g: unpremultiply(planes[1][i]),
                b: unpremultiply(planes[2][i]),
                a,
            };
        }
    }

    /// The colour-matrix and per-channel filters, which are one pass each.
    fn recolor(&mut self, op: FilterOp) {
        // Each is written as `out = M * in`, with the luminance weights the
        // filter spec gives, so one loop serves all of them.
        let luminance = [0.2126f32, 0.7152, 0.0722];
        let matrix: [[f32; 3]; 3] = match op {
            FilterOp::Grayscale(amount) => mix_towards_luminance(luminance, amount),
            FilterOp::Saturate(amount) => saturate_matrix(luminance, amount),
            FilterOp::Sepia(amount) => sepia_matrix(amount),
            FilterOp::HueRotate(radians) => hue_rotate_matrix(radians),
            _ => [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
        };
        for p in self.pixels.iter_mut() {
            let channels = [p.r as f32 / 255.0, p.g as f32 / 255.0, p.b as f32 / 255.0];
            let mut out = [0.0f32; 3];
            for (row, weights) in matrix.iter().enumerate() {
                out[row] = weights
                    .iter()
                    .zip(channels.iter())
                    .map(|(w, c)| w * c)
                    .sum();
            }
            // Then the per-channel curves, which do not mix channels at all.
            for value in out.iter_mut() {
                *value = match op {
                    FilterOp::Brightness(amount) => *value * amount,
                    FilterOp::Contrast(amount) => (*value - 0.5) * amount + 0.5,
                    FilterOp::Invert(amount) => *value * (1.0 - amount) + (1.0 - *value) * amount,
                    _ => *value,
                };
            }
            let byte = |v: f32| (v * 255.0).round().clamp(0.0, 255.0) as u8;
            p.r = byte(out[0]);
            p.g = byte(out[1]);
            p.b = byte(out[2]);
            if let FilterOp::Opacity(amount) = op {
                p.a = byte(p.a as f32 / 255.0 * amount);
            }
        }
    }

    /// `drop-shadow`: the buffer's own alpha, offset, blurred, tinted, and put
    /// underneath what is already there.
    ///
    /// Against a transparent background, which is what a layer buffer is — so
    /// the shadow shows around the shape rather than filling its box.
    fn drop_shadow(&mut self, shadow: ShadowSpec) {
        let mut mask = Mask {
            x: 0,
            y: 0,
            width: self.width,
            height: self.height,
            alpha: self.pixels.iter().map(|p| p.a).collect(),
        };
        mask.blur(sigma_for(shadow.blur));
        let mut beneath = Canvas::transparent(self.width, self.height);
        beneath.composite_mask(
            &mask,
            shadow.color,
            shadow.dx.round() as i32,
            shadow.dy.round() as i32,
            UNCLIPPED,
        );
        for (under, over) in beneath.pixels.iter_mut().zip(self.pixels.iter()) {
            *under = blend(*under, *over, 255);
        }
        self.pixels = beneath.pixels;
    }

    /// Filter what is already painted beneath `rect`, for `backdrop-filter`.
    ///
    /// Reads this canvas's own output back, which is exactly why it comes after
    /// `filter`: the compositor has to be able to sample itself.
    fn filter_backdrop(&mut self, rect: Rect, radius: f32, ops: &[FilterOp], clip: Rect) {
        let Some(area) = intersect(rect, clip) else {
            return;
        };
        let x0 = area.x.floor().max(0.0) as usize;
        let y0 = area.y.floor().max(0.0) as usize;
        let x1 = (area.x + area.width).ceil().clamp(0.0, self.width as f32) as usize;
        let y1 = (area.y + area.height).ceil().clamp(0.0, self.height as f32) as usize;
        if x1 <= x0 || y1 <= y0 {
            return;
        }
        // A margin, so a blur samples the page outside the element rather than
        // fading into nothing at its edge.
        let margin = ops.iter().map(|op| op.margin()).fold(0.0, f32::max).ceil() as usize;
        let sx0 = x0.saturating_sub(margin);
        let sy0 = y0.saturating_sub(margin);
        let sx1 = (x1 + margin).min(self.width);
        let sy1 = (y1 + margin).min(self.height);
        let mut snapshot = Canvas::transparent(sx1 - sx0, sy1 - sy0);
        for y in sy0..sy1 {
            for x in sx0..sx1 {
                snapshot.pixels[(y - sy0) * snapshot.width + (x - sx0)] =
                    self.pixels[y * self.width + x];
            }
        }
        apply_filters(&mut snapshot, ops);
        for y in y0..y1 {
            for x in x0..x1 {
                // The element's own rounded corners clip the frosted area, or a
                // sheet's blur would show as a square behind a rounded card.
                if radius > 0.0 && outside_rounded(rect, radius, x as f32 + 0.5, y as f32 + 0.5) {
                    continue;
                }
                let sampled = snapshot.pixels[(y - sy0) * snapshot.width + (x - sx0)];
                self.pixels[y * self.width + x] =
                    blend(self.pixels[y * self.width + x], sampled, 255);
            }
        }
    }

    /// Map an already-rendered layer onto this canvas through `matrix`.
    ///
    /// Destination-driven: every pixel the transformed shape covers is walked
    /// once and pulled back through the inverse, which is what keeps a rotation
    /// free of the gaps a forward mapping leaves.
    fn composite_layer(
        &mut self,
        layer: &Canvas,
        source: Rect,
        matrix: crate::css::Mat,
        clip: Rect,
    ) {
        let Some(inverse) = matrix.invert() else {
            return;
        };
        let dest = match intersect(transformed_bounds(matrix, source), clip) {
            Some(dest) => dest,
            None => return,
        };
        let x0 = dest.x.floor().max(0.0) as usize;
        let y0 = dest.y.floor().max(0.0) as usize;
        let x1 = (dest.x + dest.width).ceil().clamp(0.0, self.width as f32) as usize;
        let y1 = (dest.y + dest.height).ceil().clamp(0.0, self.height as f32) as usize;
        for y in y0..y1 {
            for x in x0..x1 {
                let (sx, sy) = inverse.apply(x as f32 + 0.5, y as f32 + 0.5);
                // Half a pixel out on each side, so the layer's own edge is
                // antialiased against whatever is behind it rather than cut.
                if sx < source.x - 1.0
                    || sy < source.y - 1.0
                    || sx > source.x + source.width + 1.0
                    || sy > source.y + source.height + 1.0
                {
                    continue;
                }
                let sampled = layer.sample(sx - source.x - 0.5, sy - source.y - 0.5);
                if sampled.a == 0 {
                    continue;
                }
                let idx = y * self.width + x;
                self.pixels[idx] = blend(self.pixels[idx], sampled, 255);
            }
        }
    }

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

    /// Fill a rect by interpolating between colour stops along a gradient's geometry.
    fn paint_gradient(
        &mut self,
        rect: Rect,
        radius: f32,
        stops: &[(Color, f32)],
        shape: GradientShape,
    ) {
        if stops.is_empty() || rect.width <= 0.0 || rect.height <= 0.0 {
            return;
        }
        let x0 = rect.x.clamp(0.0, self.width as f32) as usize;
        let y0 = rect.y.clamp(0.0, self.height as f32) as usize;
        let x1 = (rect.x + rect.width).clamp(0.0, self.width as f32) as usize;
        let y1 = (rect.y + rect.height).clamp(0.0, self.height as f32) as usize;
        let (left, right) = (rect.x + radius, rect.x + rect.width - radius);
        let (top, bottom) = (rect.y + radius, rect.y + rect.height - radius);
        // A linear gradient's line runs the length of the box's own diagonal
        // projection onto its direction — computed once, not per pixel, the
        // same way `rect`/`radius` already are.
        let linear_span = match shape {
            GradientShape::Linear { dx, dy } => Some(linear_gradient_span(rect, dx, dy)),
            GradientShape::Radial => None,
        };

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
                let t = match (shape, linear_span) {
                    (GradientShape::Linear { dx, dy }, Some((lo, span))) => {
                        ((px * dx + py * dy - lo) / span).clamp(0.0, 1.0)
                    }
                    _ => {
                        // Radial: distance from the box's centre, normalized by
                        // its own half-width/half-height — an ellipse matching
                        // the box's aspect ratio rather than a circle.
                        let (cx, cy) = (rect.x + rect.width / 2.0, rect.y + rect.height / 2.0);
                        let (nx, ny) = (
                            (px - cx) / (rect.width / 2.0).max(0.001),
                            (py - cy) / (rect.height / 2.0).max(0.001),
                        );
                        (nx * nx + ny * ny).sqrt().clamp(0.0, 1.0)
                    }
                };
                let color = sample_stops(stops, t);
                let idx = y * self.width + x;
                self.pixels[idx] = blend(self.pixels[idx], color, (coverage * 255.0) as u8);
            }
        }
    }

    /// Draw a blurred rounded rectangle behind a box.
    ///
    /// The shape is rasterized into an alpha mask and the mask is blurred —
    /// the same path `text-shadow` takes, so there is one blur in the engine
    /// rather than one per effect that wants one.
    fn paint_shadow(&mut self, rect: Rect, radius: f32, blur: f32, color: Color) {
        let sigma = sigma_for(blur);
        let spread = mask_margin(sigma);
        let mut mask = Mask::covering(rect, spread, self.width, self.height);
        let (left, right) = (rect.x + radius, rect.x + rect.width - radius);
        let (top, bottom) = (rect.y + radius, rect.y + rect.height - radius);
        for y in 0..mask.height {
            for x in 0..mask.width {
                let px = (mask.x + x as i32) as f32 + 0.5;
                let py = (mask.y + y as i32) as f32 + 0.5;
                let dx = (left - px).max(px - right).max(0.0);
                let dy = (top - py).max(py - bottom).max(0.0);
                // Coverage of the rounded rectangle itself, antialiased across
                // the one pixel its edge crosses; the blur does the rest.
                let dist = (dx * dx + dy * dy).sqrt() - radius;
                let coverage = (0.5 - dist).clamp(0.0, 1.0);
                mask.alpha[y * mask.width + x] = (coverage * 255.0) as u8;
            }
        }
        mask.blur(sigma);
        self.composite_mask(&mask, color, 0, 0, UNCLIPPED);
    }

    /// Blend a blurred alpha mask onto the canvas, tinted and offset.
    fn composite_mask(&mut self, mask: &Mask, color: Color, dx: i32, dy: i32, clip: Rect) {
        let (cx0, cy0) = (clip.x.max(0.0) as i32, clip.y.max(0.0) as i32);
        let cx1 = (clip.x + clip.width).min(self.width as f32).max(0.0) as i32;
        let cy1 = (clip.y + clip.height).min(self.height as f32).max(0.0) as i32;
        for y in 0..mask.height {
            let py = mask.y + y as i32 + dy;
            if py < cy0 || py >= cy1 || py < 0 || py >= self.height as i32 {
                continue;
            }
            for x in 0..mask.width {
                let a = mask.alpha[y * mask.width + x];
                if a == 0 {
                    continue;
                }
                let px = mask.x + x as i32 + dx;
                if px < cx0 || px >= cx1 || px < 0 || px >= self.width as i32 {
                    continue;
                }
                let idx = py as usize * self.width + px as usize;
                let alpha = (a as u32 * color.a as u32 / 255) as u8;
                self.pixels[idx] = blend(self.pixels[idx], color, alpha);
            }
        }
    }

    /// Rasterize a shaped run glyph-by-glyph and alpha-blend it onto the canvas.
    /// Positions come from the shaper, so scripts that reorder or stack marks land correctly.
    /// Uses the same font the shaper picked, so glyph ids resolve correctly.
    ///
    /// `clip` is where the run is allowed to draw. It and the canvas bound the
    /// same thing — which pixels may be written — so they fold into one pair of
    /// tests per pixel rather than two, and clipping costs nothing.
    fn paint_text(&mut self, frag: &TextFragment, clip: Rect, fonts: &FontSet) {
        let font = match fonts
            .entries
            .get(frag.font_index)
            .and_then(|entry| entry.raster())
        {
            Some(raster) => raster,
            None => return,
        };
        let (x0, y0) = (clip.x.max(0.0) as i32, clip.y.max(0.0) as i32);
        let x1 = (clip.x + clip.width).min(self.width as f32).max(0.0) as i32;
        let y1 = (clip.y + clip.height).min(self.height as f32).max(0.0) as i32;
        let ascent = font
            .horizontal_line_metrics(frag.size)
            .map_or(frag.size, |m| m.ascent);
        let baseline = frag.y + ascent;

        // Shadows are the glyph *coverage*, offset, blurred and tinted — not
        // the run drawn again in another colour, which gives visibly wrong
        // results wherever glyphs overlap or the blur is soft.
        if !frag.shadows.is_empty() {
            let mut ink = Mask::bounded(x0, y0, x1, y1);
            run_ink(frag, font, baseline, |px, py, a| ink.cover(px, py, a));
            // Back to front: the last shadow in the list paints first, so the
            // first one ends up nearest the text.
            for shadow in frag.shadows.iter().rev() {
                let mut mask = Mask {
                    alpha: ink.alpha.clone(),
                    ..ink
                };
                mask.blur(sigma_for(shadow.blur));
                self.composite_mask(
                    &mask,
                    shadow.color,
                    shadow.dx.round() as i32,
                    shadow.dy.round() as i32,
                    clip,
                );
            }
        }

        run_ink(frag, font, baseline, |px, py, a| {
            if px < x0 || px >= x1 || py < y0 || py >= y1 {
                return;
            }
            let idx = py as usize * self.width + px as usize;
            self.pixels[idx] = blend(self.pixels[idx], frag.color, a);
        });

        // A stroke's own thickness, not part of any glyph's rasterized coverage.
        // A rule under clipped-away words must not outlive them, so both are
        // trimmed to the same clip the glyphs were.
        let stroke = (frag.size / 16.0).max(1.0);
        let mut rule = |y: f32| {
            let line = Rect {
                x: frag.x,
                y,
                width: frag.width,
                height: stroke,
            };
            if let Some(visible) = intersect(line, clip) {
                self.paint_solid(frag.color, visible);
            }
        };
        if frag.underline {
            rule(baseline + stroke);
        }
        if frag.strikethrough {
            rule(baseline - frag.size * 0.3);
        }
    }

    /// Blit a *sub-rectangle* of a decoded image (`src_rect`, in the image's own
    /// pixel space) into `dest`, nearest-neighbor scaled and alpha-blended. The
    /// one blit primitive both a whole `<img>` and a background tile use: an
    /// `<img>` samples the whole image into `dest`; a background tile that hangs
    /// off the box's edge samples only the visible slice of it, so it shows the
    /// right pixels instead of being squeezed to fit the clipped area.
    fn paint_image_region(&mut self, img: &DecodedImage, dest: Rect, src_rect: Rect) {
        let (dw, dh) = (dest.width as i32, dest.height as i32);
        if dw <= 0 || dh <= 0 || img.width == 0 || img.height == 0 {
            return;
        }
        let (x0, y0) = (dest.x as i32, dest.y as i32);
        for dy in 0..dh {
            let sy = src_rect.y + (dy as f32 / dh as f32) * src_rect.height;
            let sy = (sy as usize).min(img.height - 1);
            let py = y0 + dy;
            if py < 0 || py >= self.height as i32 {
                continue;
            }
            for dx in 0..dw {
                let sx = src_rect.x + (dx as f32 / dw as f32) * src_rect.width;
                let sx = (sx as usize).min(img.width - 1);
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

    /// Draw `img` into `dest` per `object-fit`.
    fn paint_fitted_image(&mut self, img: &DecodedImage, dest: Rect, fit: ObjectFit) {
        if dest.width <= 0.0 || dest.height <= 0.0 || img.width == 0 || img.height == 0 {
            return;
        }
        let natural = (img.width as f32, img.height as f32);
        if fit == ObjectFit::Fill {
            let full_src = Rect {
                x: 0.0,
                y: 0.0,
                width: natural.0,
                height: natural.1,
            };
            return self.paint_image_region(img, dest, full_src);
        }
        let cover = (dest.width / natural.0).max(dest.height / natural.1);
        let contain = (dest.width / natural.0).min(dest.height / natural.1);
        let scale = match fit {
            ObjectFit::Cover => cover,
            ObjectFit::Contain => contain,
            // Shrink to fit like `contain`, but never enlarge past natural size.
            ObjectFit::ScaleDown => contain.min(1.0),
            ObjectFit::None => 1.0,
            ObjectFit::Fill => unreachable!(),
        };
        let (fw, fh) = (natural.0 * scale, natural.1 * scale);
        let tile = Rect {
            x: dest.x + (dest.width - fw) / 2.0,
            y: dest.y + (dest.height - fh) / 2.0,
            width: fw,
            height: fh,
        };
        // `cover` overflows `dest` on one axis and gets cropped by the
        // intersection; `contain`/`scale-down`/`none` are letterboxed instead,
        // since `tile` is the smaller of the two rects there.
        if let Some(clipped) = intersect(tile, dest) {
            let src = Rect {
                x: (clipped.x - tile.x) * (natural.0 / fw),
                y: (clipped.y - tile.y) * (natural.1 / fh),
                width: clipped.width * (natural.0 / fw),
                height: clipped.height * (natural.1 / fh),
            };
            self.paint_image_region(img, clipped, src);
        }
    }

    /// Tile `img` across `rect` per `background-size`/`-position`/`-repeat`.
    fn paint_background_image(
        &mut self,
        img: &DecodedImage,
        rect: Rect,
        size: BgSize,
        position: (BgAxis, BgAxis),
        repeat: BgRepeat,
    ) {
        if rect.width <= 0.0 || rect.height <= 0.0 || img.width == 0 || img.height == 0 {
            return;
        }
        let natural = (img.width as f32, img.height as f32);
        let (iw, ih) = resolve_bg_size(size, natural, rect);
        if iw <= 0.0 || ih <= 0.0 {
            return;
        }
        let (ax, ay) = position;
        let ox = resolve_bg_axis(ax, rect.width, iw);
        let oy = resolve_bg_axis(ay, rect.height, ih);
        let (repeat_x, repeat_y) = match repeat {
            BgRepeat::Repeat => (true, true),
            BgRepeat::RepeatX => (true, false),
            BgRepeat::RepeatY => (false, true),
            BgRepeat::NoRepeat => (false, false),
        };
        // Step back from the first tile's offset to the tile that first
        // touches the box, so a tile only partly inside the top/left edge is
        // still drawn (just clipped), not skipped.
        let start_x = if repeat_x {
            rect.x + ox - (ox / iw).ceil() * iw
        } else {
            rect.x + ox
        };
        let start_y = if repeat_y {
            rect.y + oy - (oy / ih).ceil() * ih
        } else {
            rect.y + oy
        };
        let (scale_x, scale_y) = (natural.0 / iw, natural.1 / ih);

        let mut y = start_y;
        loop {
            let mut x = start_x;
            loop {
                let tile = Rect {
                    x,
                    y,
                    width: iw,
                    height: ih,
                };
                if let Some(dest) = intersect(tile, rect) {
                    let src = Rect {
                        x: (dest.x - tile.x) * scale_x,
                        y: (dest.y - tile.y) * scale_y,
                        width: dest.width * scale_x,
                        height: dest.height * scale_y,
                    };
                    self.paint_image_region(img, dest, src);
                }
                x += iw;
                if !repeat_x || x >= rect.x + rect.width {
                    break;
                }
            }
            y += ih;
            if !repeat_y || y >= rect.y + rect.height {
                break;
            }
        }
    }
}

/// Resolve `background-size` to a concrete (width, height) in px, given the
/// image's own natural size and the box it's painted into.
fn resolve_bg_size(size: BgSize, natural: (f32, f32), rect: Rect) -> (f32, f32) {
    let (nw, nh) = natural;
    if nw <= 0.0 || nh <= 0.0 {
        return (0.0, 0.0);
    }
    match size {
        BgSize::Cover => {
            let scale = (rect.width / nw).max(rect.height / nh);
            (nw * scale, nh * scale)
        }
        BgSize::Contain => {
            let scale = (rect.width / nw).min(rect.height / nh);
            (nw * scale, nh * scale)
        }
        BgSize::Auto => (nw, nh),
        // One axis given, `auto` on the other: keep the image's own aspect ratio.
        BgSize::Explicit(Some(w), Some(h)) => (w, h),
        BgSize::Explicit(Some(w), None) => (w, w * nh / nw),
        BgSize::Explicit(None, Some(h)) => (h * nw / nh, h),
        BgSize::Explicit(None, None) => (nw, nh),
    }
}

/// Resolve one axis of `background-position` against how much room the box
/// leaves once the (already-sized) image is placed in it.
fn resolve_bg_axis(axis: BgAxis, box_dim: f32, image_dim: f32) -> f32 {
    match axis {
        BgAxis::Percent(f) => (box_dim - image_dim) * f,
        BgAxis::Px(v) => v,
    }
}

/// Alpha-blend `src` (scaled by `coverage`) over `dst`.
fn blend(dst: Color, src: Color, coverage: u8) -> Color {
    let sa = (coverage as f32 / 255.0) * (src.a as f32 / 255.0);
    let da = dst.a as f32 / 255.0;
    // Source-over. On the page canvas, which starts opaque, this reduces to the
    // straight mix it always was; an offscreen layer starts transparent and
    // needs the alpha carried through so it can be composited afterwards.
    let out = sa + da * (1.0 - sa);
    if out <= 0.0 {
        return Color {
            r: 0,
            g: 0,
            b: 0,
            a: 0,
        };
    }
    let mix = |d: u8, s: u8| {
        ((s as f32 * sa + d as f32 * da * (1.0 - sa)) / out)
            .round()
            .clamp(0.0, 255.0) as u8
    };
    Color {
        r: mix(dst.r, src.r),
        g: mix(dst.g, src.g),
        b: mix(dst.b, src.b),
        a: (out * 255.0).round() as u8,
    }
}

/// Paint a laid-out page. `find` highlights the runs matching a find-in-page
/// query and reports where they are, so the embedder can scroll to them.
/// Paint `layout_root` into a canvas the size of `bounds`.
///
/// `bounds.y` is the first document row the canvas stands for: 0 paints the page
/// from the top, and any other value paints the band starting there. A browser
/// only ever shows one screenful, and a long article is tens of megabytes of
/// pixels it would otherwise have to paint, hold and hand over in full.
pub fn paint(
    layout_root: &LayoutBox,
    bounds: Rect,
    fonts: Option<&FontSet>,
    images: &ImageMap,
    find: Option<&str>,
) -> (Canvas, Vec<Rect>) {
    let display_list = build_display_list(layout_root);
    let mut canvas = Canvas::new(bounds.width as usize, bounds.height as usize);
    // Matches are reported in document coordinates — the embedder scrolls to
    // them, and it may not be looking at this band — so they are taken before
    // the list is moved onto the canvas.
    let matches = find
        .map(|q| highlight_rects(&display_list, q))
        .unwrap_or_default();

    // `bounds.y` is the first document row this canvas stands for, so drawing it
    // is the whole document shifted up by that much. A band is exactly that and
    // nothing else, which is why painting one costs no special cases below.
    let onto_canvas = Xf {
        scale: 1.0,
        dx: 0.0,
        dy: -bounds.y,
    };
    let display_list: DisplayList = match onto_canvas.is_none() {
        true => display_list,
        false => display_list
            .into_iter()
            .map(|item| transform(item, onto_canvas))
            .collect(),
    };
    let matches_on_canvas: Vec<Rect> = match onto_canvas.is_none() {
        true => matches.clone(),
        false => matches.iter().map(|r| onto_canvas.rect(*r)).collect(),
    };

    // The root background paints the whole canvas, not just the root's box, so a
    // short dark page doesn't leave white below it (CSS 2.1 §14.2).
    if let Some(color) = canvas_background(layout_root) {
        canvas.paint_solid(
            color,
            Rect {
                x: 0.0,
                y: 0.0,
                ..bounds
            },
        );
    }
    rasterize(
        &mut canvas,
        &display_list,
        &matches_on_canvas,
        fonts,
        images,
        MAX_LAYER_DEPTH,
    );
    (canvas, matches)
}

/// How deeply off-axis transforms may nest before the painter stops following
/// them. Each level is an offscreen buffer, and a page can nest as it likes.
const MAX_LAYER_DEPTH: usize = 8;

/// Draw a display list onto a canvas.
///
/// Its own function because a [`DisplayCommand::Layer`] renders its subtree the
/// same way, into a buffer of its own, before being mapped through a transform
/// the primitives here cannot express.
fn rasterize(
    canvas: &mut Canvas,
    display_list: &DisplayList,
    highlights: &[Rect],
    fonts: Option<&FontSet>,
    images: &ImageMap,
    depth: usize,
) {
    // Two passes: everything under the text, then the find highlights, then the
    // text itself — a highlight must cover page backgrounds but sit under words.
    for pass in [Pass::Boxes, Pass::Text] {
        if pass == Pass::Text {
            for rect in highlights {
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
                    shape,
                } => canvas.paint_gradient(*rect, *radius, stops, *shape),
                DisplayCommand::Shadow {
                    rect,
                    radius,
                    blur,
                    color,
                } => canvas.paint_shadow(*rect, *radius, *blur, *color),
                DisplayCommand::Text(frag, clip) => {
                    if let Some(fonts) = fonts {
                        canvas.paint_text(frag, *clip, fonts);
                    }
                }
                DisplayCommand::Image(src, rect, fit) => {
                    if let Some(img) = images.get(src) {
                        canvas.paint_fitted_image(img, *rect, *fit);
                    }
                }
                DisplayCommand::BackgroundImage {
                    src,
                    rect,
                    size,
                    position,
                    repeat,
                } => {
                    if let Some(img) = images.get(src) {
                        canvas.paint_background_image(img, *rect, *size, *position, *repeat);
                    }
                }
                DisplayCommand::Backdrop {
                    rect,
                    radius,
                    filters,
                    clip,
                } => canvas.filter_backdrop(*rect, *radius, filters, *clip),
                DisplayCommand::Layer {
                    list,
                    source,
                    matrix,
                    clip,
                    filters,
                } => {
                    if depth == 0 {
                        continue;
                    }
                    let width = source.width.ceil().max(0.0) as usize;
                    let height = source.height.ceil().max(0.0) as usize;
                    // A transform on an empty or absurd box is not worth a
                    // buffer; the page still renders, just without the effect.
                    if width == 0 || height == 0 || width * height > 16_000_000 {
                        continue;
                    }
                    let mut layer = Canvas::transparent(width, height);
                    // The subtree was built in page coordinates, so drawing it
                    // into a buffer that starts at the source's corner is one
                    // translation.
                    let shifted: DisplayList = list
                        .iter()
                        .cloned()
                        .map(|item| {
                            transform(
                                item,
                                Xf {
                                    scale: 1.0,
                                    dx: -source.x,
                                    dy: -source.y,
                                },
                            )
                        })
                        .collect();
                    rasterize(&mut layer, &shifted, &[], fonts, images, depth - 1);
                    apply_filters(&mut layer, filters);
                    canvas.composite_layer(&layer, *source, *matrix, *clip);
                }
            }
        }
    }
}

/// Text paints above every box, so highlights can slot between the two.
#[derive(PartialEq, Clone, Copy)]
enum Pass {
    Boxes,
    Text,
}

fn pass_of(item: &DisplayCommand) -> Pass {
    match item {
        DisplayCommand::Text(..) => Pass::Text,
        // A layer holds its own boxes and text and orders them internally, so
        // it has to paint in one pass. The text pass is the right one: a
        // transformed element is a badge or a label over the page, not under it.
        DisplayCommand::Layer { .. } => Pass::Text,
        // A backdrop filter reads the canvas back, so it has to run after
        // everything beneath it has been drawn but before the box on top.
        DisplayCommand::Backdrop { .. } => Pass::Boxes,
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
            // Generated content is not in the document, so it is not findable.
            DisplayCommand::Text(frag, ..)
                if !frag.generated && frag.text.to_lowercase().contains(&needle) =>
            {
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
    render_layout_box(&mut list, layout_root, UNCLIPPED, 1.0, Xf::NONE);
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
        DisplayCommand::Image(src, rect, fit) => {
            DisplayCommand::Image(src, intersect(rect, clip)?, fit)
        }
        // The glyphs are already placed, so a run cannot be trimmed by moving
        // its box the way a background can. It carries the clip instead, and
        // the rasterizer drops the pixels that fall outside — which is what
        // makes a box collapsed to 1x1 (how every large site hides a
        // screen-reader heading) hide its text rather than print it in full.
        DisplayCommand::Text(frag, existing) => {
            let clip = intersect(existing, clip)?;
            let rect = Rect {
                x: frag.x,
                y: frag.y,
                width: frag.width,
                height: frag.size * 1.25,
            };
            intersect(rect, clip)?;
            DisplayCommand::Text(frag, clip)
        }
        other => other,
    })
}

/// The clip a box imposes on its descendants: its padding box, when `overflow`
/// says content may not escape it.
fn child_clip(layout_box: &LayoutBox, clip: Rect, xf: Xf) -> Option<Rect> {
    let style = match layout_box.box_type {
        BoxType::BlockNode(s) | BoxType::InlineNode(s) => s,
        BoxType::AnonymousBlock => return Some(clip),
    };
    // `html`/`body { overflow: hidden }` is how a page says "the *viewport*
    // does not scroll" — it is not a request to hide the document. The embedder
    // owns scrolling here, and the whole page is painted for it to scroll
    // through, so honouring this one would blank every site that sets it.
    if matches!(style.node.node_type,
        NodeType::Element(ref e) if e.tag_name == "html" || e.tag_name == "body")
    {
        return Some(clip);
    }
    let overflow = match style.value("overflow") {
        Some(Value::Keyword(k)) => k,
        Some(Value::Raw(raw)) => raw.split_whitespace().next()?.to_string(),
        _ => return Some(clip),
    };
    match overflow.as_str() {
        // No scrollbars: a scrollable box shows its first screenful, which is
        // what a collapsed menu or a clipped banner needs.
        "hidden" | "clip" | "auto" | "scroll" => {
            let mut drawn = xf.rect(layout_box.dimensions.padding_box());
            // Vertically, only a height the page *stated* is worth clipping to.
            // A content-sized box already wraps its content, so a clip there can
            // only ever hide something this engine mis-measured — which is
            // exactly what it did: a results list inside an auto-height
            // `overflow: hidden` wrapper vanished entirely.
            if !has_explicit_height(style) {
                drawn.y = UNCLIPPED.y;
                drawn.height = UNCLIPPED.height;
            }
            intersect(drawn, clip)
        }
        _ => Some(clip),
    }
}

/// Did the page give this box a height, rather than letting its content decide?
fn has_explicit_height(style: &crate::style::StyledNode) -> bool {
    ["height", "max-height"]
        .iter()
        .any(|name| matches!(style.value(name), Some(Value::Length(..) | Value::Calc(..))))
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

/// `opacity`, as a factor to scale everything this box paints by.
///
/// ponytail: applied per command, not to a composited layer, so overlapping
/// children of a half-transparent box show through each other. Real group
/// opacity needs an offscreen buffer.
fn opacity_of(layout_box: &LayoutBox) -> f32 {
    let style = match layout_box.box_type {
        BoxType::BlockNode(s) | BoxType::InlineNode(s) => s,
        BoxType::AnonymousBlock => return 1.0,
    };
    match style.value("opacity") {
        Some(Value::Number(n)) => n.clamp(0.0, 1.0),
        _ => 1.0,
    }
}

/// Scale a command's alpha. Images have no per-command colour, so a translucent
/// `<img>` still paints solid.
fn fade(item: DisplayCommand, alpha: f32) -> DisplayCommand {
    let dim = |c: Color| Color {
        a: (c.a as f32 * alpha) as u8,
        ..c
    };
    match item {
        DisplayCommand::SolidColor(c, rect) => DisplayCommand::SolidColor(dim(c), rect),
        DisplayCommand::RoundedColor(c, rect, radius) => {
            DisplayCommand::RoundedColor(dim(c), rect, radius)
        }
        DisplayCommand::Gradient {
            rect,
            radius,
            stops,
            shape,
        } => DisplayCommand::Gradient {
            rect,
            radius,
            stops: stops.into_iter().map(|(c, pos)| (dim(c), pos)).collect(),
            shape,
        },
        DisplayCommand::Shadow {
            rect,
            radius,
            blur,
            color,
        } => DisplayCommand::Shadow {
            rect,
            radius,
            blur,
            color: dim(color),
        },
        DisplayCommand::Text(frag, clip) => DisplayCommand::Text(
            TextFragment {
                color: dim(frag.color),
                ..frag
            },
            clip,
        ),
        other => other,
    }
}

/// A uniform scale and an offset: `p' = p * scale + (dx, dy)`.
///
/// Every transform this engine honours is of that shape, and two of them
/// compose into a third — which is what lets a transformed subtree carry its
/// ancestors' transforms in one value rather than a matrix stack.
#[derive(Clone, Copy)]
struct Xf {
    scale: f32,
    dx: f32,
    dy: f32,
}

impl Xf {
    const NONE: Xf = Xf {
        scale: 1.0,
        dx: 0.0,
        dy: 0.0,
    };

    fn is_none(&self) -> bool {
        self.scale == 1.0 && self.dx == 0.0 && self.dy == 0.0
    }

    /// `self` applied after `inner`.
    fn then(self, inner: Xf) -> Xf {
        Xf {
            scale: self.scale * inner.scale,
            dx: self.scale * inner.dx + self.dx,
            dy: self.scale * inner.dy + self.dy,
        }
    }

    fn rect(&self, r: Rect) -> Rect {
        Rect {
            x: r.x * self.scale + self.dx,
            y: r.y * self.scale + self.dy,
            width: r.width * self.scale,
            height: r.height * self.scale,
        }
    }
}

/// What `transform` does to this box and everything inside it, as one matrix.
///
/// The function list composes in the order written, and the whole of it is
/// measured about `transform-origin` — the translate/rotate/untranslate
/// sandwich, which is also what makes an existing `scale()` grow about the
/// box's centre rather than its corner.
pub(crate) fn transform_matrix_of(layout_box: &LayoutBox) -> crate::css::Mat {
    let style = match layout_box.box_type {
        BoxType::BlockNode(s) | BoxType::InlineNode(s) => s,
        BoxType::AnonymousBlock => return crate::css::Mat::IDENTITY,
    };
    let spec = match style.value("transform") {
        Some(Value::Raw(text)) => text,
        _ => return crate::css::Mat::IDENTITY,
    };
    let box_rect = layout_box.dimensions.border_box();
    let size = (box_rect.width, box_rect.height);
    let ctx = style.length_context(0.0);
    let matrix = crate::css::parse_transform(&spec, ctx, size);
    if matrix.is_identity() {
        return matrix;
    }
    // `transform-origin` is kept as raw text, so one arm covers every spelling.
    let spec = match style.value("transform-origin") {
        Some(Value::Raw(spec)) => Some(spec),
        _ => None,
    };
    let origin = crate::css::parse_transform_origin(spec.as_deref(), ctx, size);
    // The origin is a point on the page, not an offset in the box.
    let pivot = (box_rect.x + origin.0, box_rect.y + origin.1);
    crate::css::Mat::translate(pivot.0, pivot.1)
        .then(matrix)
        .then(crate::css::Mat::translate(-pivot.0, -pivot.1))
}

/// The axis-aligned scale-and-offset a matrix is equivalent to, when it is one.
fn upright_xf(matrix: crate::css::Mat) -> Option<Xf> {
    matrix.is_upright().then_some(Xf {
        scale: matrix.a,
        dx: matrix.e,
        dy: matrix.f,
    })
}

/// The bounding box of `rect` after `matrix` — all four corners, because a
/// rotated rectangle's extent is not given by two of them.
pub(crate) fn transformed_bounds(matrix: crate::css::Mat, rect: Rect) -> Rect {
    let corners = [
        matrix.apply(rect.x, rect.y),
        matrix.apply(rect.x + rect.width, rect.y),
        matrix.apply(rect.x, rect.y + rect.height),
        matrix.apply(rect.x + rect.width, rect.y + rect.height),
    ];
    let xs = corners.map(|(x, _)| x);
    let ys = corners.map(|(_, y)| y);
    let x0 = xs.iter().copied().fold(f32::INFINITY, f32::min);
    let x1 = xs.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    let y0 = ys.iter().copied().fold(f32::INFINITY, f32::min);
    let y1 = ys.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    Rect {
        x: x0,
        y: y0,
        width: x1 - x0,
        height: y1 - y0,
    }
}

/// Everything a subtree paints within, so a layer's buffer is big enough.
///
/// Its own box is not enough: a descendant may stick out of it, and an absolute
/// child may sit well outside.
fn subtree_bounds(layout_box: &LayoutBox) -> Rect {
    let mut bounds = layout_box.dimensions.border_box();
    let mut grow = |r: Rect| {
        let x0 = bounds.x.min(r.x);
        let y0 = bounds.y.min(r.y);
        let x1 = (bounds.x + bounds.width).max(r.x + r.width);
        let y1 = (bounds.y + bounds.height).max(r.y + r.height);
        bounds = Rect {
            x: x0,
            y: y0,
            width: x1 - x0,
            height: y1 - y0,
        };
    };
    for frag in &layout_box.text_fragments {
        grow(Rect {
            x: frag.x,
            y: frag.y,
            width: frag.width,
            height: frag.line_height,
        });
    }
    for child in &layout_box.children {
        grow(subtree_bounds(child));
    }
    bounds
}

/// Move and scale one command.
fn transform(item: DisplayCommand, xf: Xf) -> DisplayCommand {
    if xf.is_none() {
        return item;
    }
    match item {
        DisplayCommand::Backdrop {
            rect,
            radius,
            filters,
            clip,
        } => DisplayCommand::Backdrop {
            rect: xf.rect(rect),
            radius: radius * xf.scale,
            filters,
            clip: xf.rect(clip),
        },
        // A layer's contents are in their own coordinates and stay there; what
        // an enclosing transform changes is where the result lands.
        DisplayCommand::Layer {
            list,
            source,
            matrix,
            clip,
            filters,
        } => DisplayCommand::Layer {
            list,
            source,
            filters,
            matrix: crate::css::Mat {
                a: xf.scale,
                b: 0.0,
                c: 0.0,
                d: xf.scale,
                e: xf.dx,
                f: xf.dy,
            }
            .then(matrix),
            clip: xf.rect(clip),
        },
        DisplayCommand::SolidColor(c, rect) => DisplayCommand::SolidColor(c, xf.rect(rect)),
        DisplayCommand::RoundedColor(c, rect, radius) => {
            DisplayCommand::RoundedColor(c, xf.rect(rect), radius * xf.scale)
        }
        DisplayCommand::Gradient {
            rect,
            radius,
            stops,
            shape,
        } => DisplayCommand::Gradient {
            rect: xf.rect(rect),
            radius: radius * xf.scale,
            stops,
            shape,
        },
        DisplayCommand::Shadow {
            rect,
            radius,
            blur,
            color,
        } => DisplayCommand::Shadow {
            rect: xf.rect(rect),
            radius: radius * xf.scale,
            blur: blur * xf.scale,
            color,
        },
        DisplayCommand::Image(src, rect, fit) => DisplayCommand::Image(src, xf.rect(rect), fit),
        DisplayCommand::BackgroundImage {
            src,
            rect,
            size,
            position,
            repeat,
        } => {
            // `cover`/`contain`/percentages are resolved from `rect` at paint
            // time, so they already track a scaled box for free; an explicit
            // px size/offset was baked in before this transform ran, so it
            // needs the same scale applied here or it would stay fixed size
            // while the box around it grows or shrinks.
            let size = match size {
                BgSize::Explicit(w, h) => {
                    BgSize::Explicit(w.map(|v| v * xf.scale), h.map(|v| v * xf.scale))
                }
                other => other,
            };
            let scale_axis = |axis: BgAxis| match axis {
                BgAxis::Px(v) => BgAxis::Px(v * xf.scale),
                percent => percent,
            };
            DisplayCommand::BackgroundImage {
                src,
                rect: xf.rect(rect),
                size,
                position: (scale_axis(position.0), scale_axis(position.1)),
                repeat,
            }
        }
        // Glyphs were shaped at `size`, and their offsets are in those units, so
        // both scale together or the run comes apart.
        // The clip is in the same space as the run, so it moves with it —
        // otherwise a transformed box would keep clipping where it used to be.
        DisplayCommand::Text(frag, clip) => DisplayCommand::Text(
            TextFragment {
                x: frag.x * xf.scale + xf.dx,
                y: frag.y * xf.scale + xf.dy,
                width: frag.width * xf.scale,
                size: frag.size * xf.scale,
                line_height: frag.line_height * xf.scale,
                shadows: std::rc::Rc::new(
                    frag.shadows
                        .iter()
                        .map(|s| ShadowSpec {
                            dx: s.dx * xf.scale,
                            dy: s.dy * xf.scale,
                            blur: s.blur * xf.scale,
                            ..*s
                        })
                        .collect(),
                ),
                glyphs: frag
                    .glyphs
                    .into_iter()
                    .map(|g| crate::text::PositionedGlyph {
                        x: g.x * xf.scale,
                        y: g.y * xf.scale,
                        ..g
                    })
                    .collect(),
                ..frag
            },
            xf.rect(clip),
        ),
    }
}

/// Whether `position` is anything but `static` — which decides, among siblings
/// sharing a `z-index`, who paints on top.
fn is_positioned(layout_box: &LayoutBox) -> bool {
    let style = match layout_box.box_type {
        BoxType::BlockNode(s) | BoxType::InlineNode(s) => s,
        BoxType::AnonymousBlock => return false,
    };
    matches!(
        style.value("position"),
        Some(Value::Keyword(ref k))
            if k == "relative" || k == "absolute" || k == "fixed" || k == "sticky"
    )
}

/// `z-index`, which decides paint order among siblings. Everything else keeps
/// document order, so 0 is both the default and what an unpositioned box gets.
fn z_index_of(layout_box: &LayoutBox) -> i32 {
    let style = match layout_box.box_type {
        BoxType::BlockNode(s) | BoxType::InlineNode(s) => s,
        BoxType::AnonymousBlock => return 0,
    };
    match style.value("z-index") {
        Some(Value::Number(n)) => n as i32,
        _ => 0,
    }
}

/// `clip` is in drawn coordinates; `offset` is how far every ancestor's
/// `transform` has already moved this subtree.
fn render_layout_box(
    list: &mut DisplayList,
    layout_box: &LayoutBox,
    clip: Rect,
    alpha: f32,
    outer: Xf,
) {
    // A transform applies to the box and everything inside it, so it composes
    // with whatever its ancestors already did.
    let matrix = transform_matrix_of(layout_box);
    let filters = filters_of(layout_box, "filter");
    match upright_xf(matrix).filter(|_| filters.is_empty()) {
        // A scale-and-offset and no filter needs no buffer: it folds into the
        // ancestors' and paints straight onto the canvas, which keeps text sharp.
        Some(local) => render_subtree(list, layout_box, clip, alpha, outer.then(local)),
        // Anything else — a rotation, a skew, a flip, a per-axis scale, or a
        // filter, which is defined over the rendered result — is drawn upright
        // into its own buffer, filtered, and mapped.
        None => {
            let mut source = subtree_bounds(layout_box);
            // A blur or a drop shadow reaches past the content it came from.
            let margin = filters.iter().map(|op| op.margin()).fold(0.0, f32::max);
            if margin > 0.0 {
                source = Rect {
                    x: source.x - margin,
                    y: source.y - margin,
                    width: source.width + margin * 2.0,
                    height: source.height + margin * 2.0,
                };
            }
            let mut sub = Vec::new();
            render_subtree(&mut sub, layout_box, UNCLIPPED, alpha, Xf::NONE);
            list.push(transform(
                DisplayCommand::Layer {
                    list: sub,
                    source,
                    matrix,
                    clip,
                    filters,
                },
                outer,
            ));
        }
    }
}

/// A `filter` or `backdrop-filter` chain, already resolved to amounts.
fn filters_of(layout_box: &LayoutBox, property: &str) -> Vec<FilterOp> {
    let style = match layout_box.box_type {
        BoxType::BlockNode(s) | BoxType::InlineNode(s) => s,
        BoxType::AnonymousBlock => return Vec::new(),
    };
    let Some(Value::Raw(spec)) = style.value(property) else {
        return Vec::new();
    };
    // `drop-shadow()` with no colour of its own uses the element's `color`.
    let color = match style.value("color") {
        Some(Value::ColorValue(c)) => c,
        _ => Color {
            r: 0,
            g: 0,
            b: 0,
            a: 255,
        },
    };
    parse_filter_list(&spec, style.length_context(style.font_size()), color)
}

fn render_subtree(list: &mut DisplayList, layout_box: &LayoutBox, clip: Rect, alpha: f32, xf: Xf) {
    let alpha = alpha * opacity_of(layout_box);
    if !is_invisible(layout_box) && alpha > 0.0 {
        let mut own = Vec::new();
        render_own(&mut own, layout_box);
        list.extend(
            own.into_iter()
                .map(|item| transform(item, xf))
                .filter_map(|item| clip_command(item, clip))
                .map(|item| fade(item, alpha)),
        );
    }
    // This box's own clip comes from where it is *drawn*, not where it was laid
    // out. An empty clip still has to be handed down: a child of a hidden box is
    // hidden too, however visible it declares itself.
    let inner = child_clip(layout_box, clip, xf).unwrap_or_default();
    // A higher `z-index` paints later, and equal ones keep document order.
    //
    // ponytail: one flat order rather than real stacking contexts, so a child's
    // z-index competes with its uncles. Nested contexts need the display list to
    // become a tree.
    // Within one z-index, a positioned box paints above the in-flow content it
    // overlaps — CSS puts positioned descendants in a later layer than block
    // ones. Without this a sticky header's *background* painted under the rows
    // it was pinned over while its text, which paints in a later pass, did not.
    let mut order: Vec<&LayoutBox> = layout_box.children.iter().collect();
    order.sort_by_key(|child| (z_index_of(child), is_positioned(child)));
    for child in order {
        render_layout_box(list, child, inner, alpha, xf);
    }
}

fn render_own(list: &mut DisplayList, layout_box: &LayoutBox) {
    render_shadow(list, layout_box);
    render_backdrop(list, layout_box);
    render_background(list, layout_box);
    render_borders(list, layout_box);
    render_outline(list, layout_box);
    if let Some(src) = image_src(layout_box) {
        list.push(DisplayCommand::Image(
            src,
            layout_box.dimensions.content,
            object_fit_of(layout_box),
        ));
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
    let shadows = match layout_box.box_type {
        BoxType::BlockNode(s) | BoxType::InlineNode(s) => text_shadows(s),
        BoxType::AnonymousBlock => std::rc::Rc::new(Vec::new()),
    };
    for frag in &layout_box.text_fragments {
        let mut frag = frag.clone();
        frag.shadows = std::rc::Rc::clone(&shadows);
        list.push(DisplayCommand::Text(frag, UNCLIPPED));
    }
}

fn object_fit_of(layout_box: &LayoutBox) -> ObjectFit {
    let style = match layout_box.box_type {
        BoxType::BlockNode(s) | BoxType::InlineNode(s) => s,
        BoxType::AnonymousBlock => return ObjectFit::Fill,
    };
    match style.value("object-fit") {
        Some(Value::Keyword(k)) => match k.as_str() {
            "contain" => ObjectFit::Contain,
            "cover" => ObjectFit::Cover,
            "none" => ObjectFit::None,
            "scale-down" => ObjectFit::ScaleDown,
            _ => ObjectFit::Fill,
        },
        _ => ObjectFit::Fill,
    }
}

fn image_src(layout_box: &LayoutBox) -> Option<String> {
    let style = match layout_box.box_type {
        BoxType::BlockNode(s) | BoxType::InlineNode(s) => s,
        BoxType::AnonymousBlock => return None,
    };
    match style.node.node_type {
        NodeType::Element(ref e) if e.tag_name == "img" => e.attributes.get("src").cloned(),
        NodeType::Element(ref e) if e.tag_name == "svg" => {
            Some(crate::inline_svg_key_of(e.node_id))
        }
        _ => None,
    }
}

/// `box-shadow` — drawn before the background so it sits behind.
fn render_shadow(list: &mut DisplayList, layout_box: &LayoutBox) {
    let style = match layout_box.box_type {
        BoxType::BlockNode(s) | BoxType::InlineNode(s) => s,
        BoxType::AnonymousBlock => return,
    };
    let spec = match style.value("box-shadow") {
        Some(Value::Raw(spec)) => spec,
        _ => return,
    };
    let b = layout_box.dimensions.border_box();
    let radius = border_radius(layout_box, b);
    let faint = Color {
        r: 0,
        g: 0,
        b: 0,
        a: 80,
    };
    // Back to front, so the first shadow in the list ends up on top.
    for shadow in parse_shadow_list(&spec, style.length_context(0.0), faint)
        .into_iter()
        .rev()
    {
        list.push(DisplayCommand::Shadow {
            rect: Rect {
                x: b.x + shadow.dx,
                y: b.y + shadow.dy,
                width: b.width,
                height: b.height,
            },
            radius,
            blur: shadow.blur,
            color: shadow.color,
        });
    }
}

/// `backdrop-filter` — the frosted glass behind a translucent surface.
///
/// Pushed after the box shadow and before the background, which is the order
/// that makes it read as glass: the page beneath is filtered, then the box's own
/// translucent background tints the result.
fn render_backdrop(list: &mut DisplayList, layout_box: &LayoutBox) {
    let filters = filters_of(layout_box, "backdrop-filter");
    if filters.is_empty() {
        return;
    }
    let rect = layout_box.dimensions.border_box();
    list.push(DisplayCommand::Backdrop {
        rect,
        radius: border_radius(layout_box, rect),
        filters,
        clip: UNCLIPPED,
    });
}

/// `text-shadow`, which paints from the glyph coverage of each run in this box
/// and contributes nothing to layout.
fn text_shadows(style: &crate::style::StyledNode) -> std::rc::Rc<Vec<ShadowSpec>> {
    let Some(Value::Raw(spec)) = style.value("text-shadow") else {
        return std::rc::Rc::new(Vec::new());
    };
    // An omitted colour is the element's own `color`.
    let color = match style.value("color") {
        Some(Value::ColorValue(c)) => c,
        _ => Color {
            r: 0,
            g: 0,
            b: 0,
            a: 255,
        },
    };
    std::rc::Rc::new(parse_shadow_list(
        &spec,
        style.length_context(style.font_size()),
        color,
    ))
}

fn render_background(list: &mut DisplayList, layout_box: &LayoutBox) {
    let style = match layout_box.box_type {
        BoxType::BlockNode(s) | BoxType::InlineNode(s) => s,
        BoxType::AnonymousBlock => return,
    };
    let box_rect = layout_box.dimensions.border_box();
    let radius = border_radius(layout_box, box_rect);

    let spec = background_spec(style);
    // A gradient fully covers the box, the same as `background-image` wins
    // over `background-color` in a real cascade — nothing paints beneath it.
    if let Some(spec) = &spec {
        if let Some((shape, stops)) = parse_gradient(spec) {
            list.push(DisplayCommand::Gradient {
                rect: box_rect,
                radius,
                stops,
                shape,
            });
            return;
        }
    }
    // `background-color` still paints first when there's a `url()` image on
    // top, so a transparent PNG (or one still loading) shows something.
    if let Some(color) =
        get_color(layout_box, "background").or_else(|| get_color(layout_box, "background-color"))
    {
        if radius > 0.0 {
            list.push(DisplayCommand::RoundedColor(color, box_rect, radius));
        } else {
            list.push(DisplayCommand::SolidColor(color, box_rect));
        }
    }
    // ponytail: unlike the solid/gradient paths above, a background image
    // isn't rounded to the box's `border-radius` — square corners on a
    // rounded box — matching the same gap `<img>` already has (`Image` isn't
    // masked either). Needs a per-pixel radius test in `paint_image_region`
    // to fix for both at once.
    if let Some(src) = spec.as_deref().and_then(bg_url) {
        let ctx_w = style.length_context(box_rect.width);
        let ctx_h = style.length_context(box_rect.height);
        list.push(DisplayCommand::BackgroundImage {
            src,
            rect: box_rect,
            size: parse_bg_size(style.value("background-size"), ctx_w, ctx_h),
            position: parse_bg_position(style.value("background-position"), ctx_w, ctx_h),
            repeat: parse_bg_repeat(style.value("background-repeat")),
        });
    }
}

/// `url(...)` (optionally quoted) out of a `background`/`background-image` spec.
/// The raw `background-image`/`background` spec text, if either sets a
/// `url()` or `linear-gradient()` — shared by the paint path below and by
/// image collection (`lib.rs`), so both agree on what counts as one.
fn background_spec(style: &crate::style::StyledNode) -> Option<String> {
    style
        .value("background-image")
        .or_else(|| style.value("background"))
        .and_then(|v| match v {
            Value::Raw(spec) => Some(spec),
            _ => None,
        })
}

fn bg_url(spec: &str) -> Option<String> {
    let inner = spec.trim().strip_prefix("url(")?.strip_suffix(')')?;
    Some(
        inner
            .trim()
            .trim_matches(|c| c == '"' || c == '\'')
            .to_string(),
    )
}

/// The `url(...)` this styled node's background paints, if it has one — used
/// to batch-fetch background images alongside `<img src>` before paint runs.
pub(crate) fn background_image_src(style: &crate::style::StyledNode) -> Option<String> {
    background_spec(style).as_deref().and_then(bg_url)
}

fn parse_bg_repeat(value: Option<Value>) -> BgRepeat {
    match value {
        Some(Value::Keyword(k)) => match k.as_str() {
            "no-repeat" => BgRepeat::NoRepeat,
            "repeat-x" => BgRepeat::RepeatX,
            "repeat-y" => BgRepeat::RepeatY,
            _ => BgRepeat::Repeat,
        },
        _ => BgRepeat::Repeat,
    }
}

fn parse_bg_size(value: Option<Value>, ctx_w: LengthContext, ctx_h: LengthContext) -> BgSize {
    let raw = match value {
        Some(Value::Keyword(k)) => k,
        Some(Value::Raw(s)) => s,
        _ => return BgSize::Auto,
    };
    let raw = raw.as_str();
    match raw.trim() {
        "cover" => return BgSize::Cover,
        "contain" => return BgSize::Contain,
        "auto" => return BgSize::Auto,
        _ => {}
    }
    let axis = |t: &str, ctx: LengthContext| -> Option<f32> {
        (t != "auto").then(|| crate::css::parse_length_token(t, ctx))
    };
    match raw.split_whitespace().collect::<Vec<_>>().as_slice() {
        [w] => BgSize::Explicit(axis(w, ctx_w), None),
        [w, h] => BgSize::Explicit(axis(w, ctx_w), axis(h, ctx_h)),
        _ => BgSize::Auto,
    }
}

/// A single `background-position` keyword, if it names a fixed edge (`left`,
/// `top`, ...) rather than a length. `center` is handled by the caller: it is
/// the same 50% on either axis, so it carries no axis of its own.
fn position_keyword(token: &str) -> Option<f32> {
    match token {
        "left" | "top" => Some(0.0),
        "right" | "bottom" => Some(1.0),
        _ => None,
    }
}

fn parse_bg_axis(token: &str, ctx: LengthContext) -> BgAxis {
    if token == "center" {
        return BgAxis::Percent(0.5);
    }
    if let Some(f) = position_keyword(token) {
        return BgAxis::Percent(f);
    }
    if let Some(pct) = token.strip_suffix('%') {
        if let Ok(v) = pct.trim().parse::<f32>() {
            return BgAxis::Percent(v / 100.0);
        }
    }
    BgAxis::Px(crate::css::parse_length_token(token, ctx))
}

fn parse_bg_position(
    value: Option<Value>,
    ctx_x: LengthContext,
    ctx_y: LengthContext,
) -> (BgAxis, BgAxis) {
    let top_left = (BgAxis::Percent(0.0), BgAxis::Percent(0.0));
    let raw = match value {
        Some(Value::Keyword(k)) => k,
        Some(Value::Raw(s)) => s,
        _ => return top_left,
    };
    let raw = raw.as_str();
    match raw.split_whitespace().collect::<Vec<_>>().as_slice() {
        // A lone `top`/`bottom` names the Y axis and centers X; everything
        // else (`left`/`right`/`center`/a length) names X and centers Y.
        [one] if matches!(*one, "top" | "bottom") => {
            (BgAxis::Percent(0.5), parse_bg_axis(one, ctx_y))
        }
        [one] => (parse_bg_axis(one, ctx_x), BgAxis::Percent(0.5)),
        [x, y] => (parse_bg_axis(x, ctx_x), parse_bg_axis(y, ctx_y)),
        _ => top_left,
    }
}

/// Parse `linear-gradient(<direction>?, stop, ...)` or `radial-gradient(stop,
/// ...)` into its geometry and colour stops.
fn parse_gradient(spec: &str) -> Option<(GradientShape, Vec<(Color, f32)>)> {
    let spec = spec.trim();
    let (is_radial, inner) = if let Some(inner) = spec
        .strip_prefix("linear-gradient(")
        .and_then(|s| s.strip_suffix(')'))
    {
        (false, inner)
    } else {
        let inner = spec
            .strip_prefix("radial-gradient(")
            .and_then(|s| s.strip_suffix(')'))?;
        (true, inner)
    };

    let parts = split_top_level_commas(inner);
    let first = parts.first()?;

    let mut start = 0;
    let shape = if is_radial {
        // A leading shape/position descriptor (`ellipse`, `circle at center`,
        // ...) is skipped rather than parsed — see `GradientShape::Radial`.
        if parse_color_stop(first).is_none() {
            start = 1;
        }
        GradientShape::Radial
    } else {
        match parse_linear_direction(first) {
            Some((dx, dy)) => {
                start = 1;
                GradientShape::Linear { dx, dy }
            }
            // No direction given: CSS's own default is "to bottom".
            None => GradientShape::Linear { dx: 0.0, dy: 1.0 },
        }
    };

    let mut stops: Vec<(Color, Option<f32>)> = parts[start..]
        .iter()
        .filter_map(|part| parse_color_stop(part))
        .collect();
    if stops.len() < 2 {
        return None;
    }
    fill_stop_positions(&mut stops);
    let stops = stops.into_iter().map(|(c, p)| (c, p.unwrap())).collect();
    Some((shape, stops))
}

/// `to <side>...` or an angle (`45deg`/`0.5turn`/`1.2rad`/`50grad`) into a
/// unit direction vector, in CSS's own convention: `0deg` is "to top" and
/// angles increase clockwise, so `(0, -1)` is up and `(1, 0)` is right.
fn parse_linear_direction(spec: &str) -> Option<(f32, f32)> {
    let spec = spec.trim();
    if let Some(sides) = spec.strip_prefix("to ") {
        let (mut dx, mut dy): (f32, f32) = (0.0, 0.0);
        for word in sides.split_whitespace() {
            match word {
                "top" => dy -= 1.0,
                "bottom" => dy += 1.0,
                "left" => dx -= 1.0,
                "right" => dx += 1.0,
                _ => return None,
            }
        }
        if dx == 0.0 && dy == 0.0 {
            return None;
        }
        let len = (dx * dx + dy * dy).sqrt();
        return Some((dx / len, dy / len));
    }
    for (suffix, to_deg) in [
        ("deg", 1.0),
        ("grad", 0.9),
        ("rad", 180.0 / std::f32::consts::PI),
        ("turn", 360.0),
    ] {
        if let Some(num) = spec.strip_suffix(suffix) {
            if let Ok(n) = num.trim().parse::<f32>() {
                let radians = (n * to_deg).to_radians();
                return Some((radians.sin(), -radians.cos()));
            }
        }
    }
    None
}

/// One `<color> <position>?` gradient stop. `None` position defers to
/// `fill_stop_positions`; only a percentage position is understood (a length
/// would need the gradient line's px length, not just its direction, to
/// place) — falls back to auto-spacing rather than a wrong placement.
fn parse_color_stop(part: &str) -> Option<(Color, Option<f32>)> {
    let (color_token, position_token) = split_color_and_position(part);
    let color = crate::css::parse_color_str(color_token)?;
    let position = position_token
        .and_then(|p| p.strip_suffix('%'))
        .and_then(|n| n.trim().parse::<f32>().ok())
        .map(|n| n / 100.0);
    Some((color, position))
}

/// Split a gradient's argument list on top-level commas only — one nested
/// inside a colour function (`rgba(0, 0, 0, 0.5)`) must not end a stop early.
fn split_top_level_commas(s: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0;
    let mut start = 0;
    for (i, c) in s.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            ',' if depth == 0 => {
                parts.push(s[start..i].trim());
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(s[start..].trim());
    parts
}

/// A stop's colour token and, if present, its trailing position token — split
/// right after a colour *function*'s closing paren rather than on whitespace,
/// since the modern `rgb(0 0 0 / 50%)` form has spaces of its own.
fn split_color_and_position(part: &str) -> (&str, Option<&str>) {
    let part = part.trim();
    if let Some(open) = part.find('(') {
        let mut depth = 0;
        for (i, c) in part.char_indices().skip(open) {
            match c {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        let rest = part[i + 1..].trim();
                        return (&part[..=i], (!rest.is_empty()).then_some(rest));
                    }
                }
                _ => {}
            }
        }
        (part, None) // unterminated function — colour parsing will reject it
    } else {
        match part.split_once(char::is_whitespace) {
            Some((color, rest)) => (color, Some(rest.trim())),
            None => (part, None),
        }
    }
}

/// Fill in stop positions CSS leaves implicit: the first/last default to
/// 0%/100%, a run of unspecified stops between two given ones spreads evenly
/// across the gap, and a position earlier than its predecessor's is raised to
/// match it (a gradient never runs backwards).
fn fill_stop_positions(stops: &mut [(Color, Option<f32>)]) {
    if stops.is_empty() {
        return;
    }
    if stops[0].1.is_none() {
        stops[0].1 = Some(0.0);
    }
    let last = stops.len() - 1;
    if stops[last].1.is_none() {
        stops[last].1 = Some(1.0);
    }
    for i in 1..stops.len() {
        if let (Some(prev), Some(cur)) = (stops[i - 1].1, stops[i].1) {
            if cur < prev {
                stops[i].1 = Some(prev);
            }
        }
    }
    let mut i = 0;
    while i < stops.len() {
        if stops[i].1.is_some() {
            i += 1;
            continue;
        }
        let start = i - 1; // always Some: stops[0] was defaulted above
        let start_pos = stops[start].1.unwrap();
        let mut end = i;
        while stops[end].1.is_none() {
            end += 1;
        }
        let end_pos = stops[end].1.unwrap();
        let n = (end - start) as f32;
        for (k, stop) in stops[start + 1..end].iter_mut().enumerate() {
            stop.1 = Some(start_pos + (end_pos - start_pos) * (k as f32 + 1.0) / n);
        }
        i = end;
    }
}

/// Interpolate between evenly spaced stops at position `t` in 0..=1.
fn sample_stops(stops: &[(Color, f32)], t: f32) -> Color {
    if stops.len() == 1 {
        return stops[0].0;
    }
    let t = t.clamp(0.0, 1.0);
    for pair in stops.windows(2) {
        let (a, pos_a) = pair[0];
        let (b, pos_b) = pair[1];
        if t <= pos_b {
            let span = (pos_b - pos_a).max(0.0001);
            let f = ((t - pos_a) / span).clamp(0.0, 1.0);
            let mix = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * f).round() as u8;
            return Color {
                r: mix(a.r, b.r),
                g: mix(a.g, b.g),
                b: mix(a.b, b.b),
                a: mix(a.a, b.a),
            };
        }
    }
    // Past the last stop's own position (it need not be 1.0) — clamp to it.
    stops.last().unwrap().0
}

/// The range a linear gradient's direction vector spans across `rect`'s four
/// corners — `t=0` at the corner the gradient line starts from, `t=1` at the
/// one it ends at, exactly reproducing CSS's own "gradient line" geometry
/// (including that a diagonal direction's line is longer than either side).
fn linear_gradient_span(rect: Rect, dx: f32, dy: f32) -> (f32, f32) {
    let corners = [
        (rect.x, rect.y),
        (rect.x + rect.width, rect.y),
        (rect.x, rect.y + rect.height),
        (rect.x + rect.width, rect.y + rect.height),
    ];
    let (mut lo, mut hi) = (f32::MAX, f32::MIN);
    for (cx, cy) in corners {
        let p = cx * dx + cy * dy;
        lo = lo.min(p);
        hi = hi.max(p);
    }
    (lo, (hi - lo).max(0.0001))
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
    // `border: none` (or `border-style: none`/`hidden`) suppresses the border
    // outright, whatever width or colour another rule gave it — the common
    // case being one rule setting a border and a more specific one turning it
    // off. Every other style (solid, dashed, ...) paints as a solid strip:
    // the engine doesn't model dash patterns, and solid is the closer
    // approximation than not drawing anything.
    if let BoxType::BlockNode(style) | BoxType::InlineNode(style) = layout_box.box_type {
        if matches!(style.value("border-style"), Some(Value::Keyword(k)) if k == "none" || k == "hidden")
        {
            return;
        }
    }
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

/// Unlike a border, an outline takes no layout space — it is drawn one ring
/// further out, around the border box rather than inside it.
///
/// ponytail: `outline-offset` is not modelled (always 0), and `outline-style:
/// auto` (the focus-ring default) paints as a solid ring rather than the
/// platform's own focus indicator.
fn render_outline(list: &mut DisplayList, layout_box: &LayoutBox) {
    let (BoxType::BlockNode(style) | BoxType::InlineNode(style)) = layout_box.box_type else {
        return;
    };
    match style.value("outline-style") {
        None => return,
        Some(Value::Keyword(k)) if k == "none" => return,
        _ => {}
    }
    let width = match style.value("outline-width") {
        Some(v @ (Value::Length(..) | Value::Number(_) | Value::Calc(..))) => {
            v.resolve(style.length_context(0.0))
        }
        _ => return,
    };
    if width <= 0.0 {
        return;
    }
    let color = match style.value("outline-color") {
        Some(Value::ColorValue(c)) => c,
        // No colour named: outline still has to be visible, so fall back to
        // the element's own text colour, the way a real focus ring would.
        _ => match get_color(layout_box, "color") {
            Some(c) => c,
            None => return,
        },
    };
    let b = layout_box.dimensions.border_box();
    let outer = Rect {
        x: b.x - width,
        y: b.y - width,
        width: b.width + width * 2.0,
        height: b.height + width * 2.0,
    };
    for strip in [
        Rect {
            x: outer.x,
            y: outer.y,
            width: outer.width,
            height: width,
        },
        Rect {
            x: outer.x,
            y: outer.y + outer.height - width,
            width: outer.width,
            height: width,
        },
        Rect {
            x: outer.x,
            y: outer.y,
            width,
            height: outer.height,
        },
        Rect {
            x: outer.x + outer.width - width,
            y: outer.y,
            width,
            height: outer.height,
        },
    ] {
        list.push(DisplayCommand::SolidColor(color, strip));
    }
}

/// The colour that propagates to the canvas: the root element's own background,
/// or `<body>`'s if the root has none.
fn canvas_background(root: &LayoutBox) -> Option<Color> {
    let of =
        |b: &LayoutBox| get_color(b, "background").or_else(|| get_color(b, "background-color"));
    // Only `<body>` inherits this privilege. Taking it from whichever child
    // happened to have a background flooded the page with, say, a hidden
    // dropdown's colour.
    let body = root.children.iter().find(|child| {
        matches!(child.box_type,
            BoxType::BlockNode(s) | BoxType::InlineNode(s)
                if matches!(&s.node.node_type, NodeType::Element(e) if e.tag_name == "body"))
    });
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

#[cfg(test)]
mod tests {
    use super::*;

    fn shadow_ctx() -> crate::css::LengthContext {
        crate::css::LengthContext {
            percent_base: 0.0,
            font_size: 16.0,
            root_font_size: 16.0,
        }
    }

    fn solid(width: usize, height: usize, color: Color) -> Canvas {
        let mut canvas = Canvas::transparent(width, height);
        canvas.pixels.fill(color);
        canvas
    }

    #[test]
    fn filter_functions_parse_with_percentages_numbers_and_angles() {
        let black = Color {
            r: 0,
            g: 0,
            b: 0,
            a: 255,
        };
        let ops = parse_filter_list(
            "blur(4px) grayscale(50%) hue-rotate(90deg) drop-shadow(2px 3px 4px blue)",
            shadow_ctx(),
            black,
        );
        assert_eq!(ops.len(), 4, "the chain lost a function: {ops:?}");
        assert_eq!(ops[0], FilterOp::Blur(4.0));
        assert_eq!(ops[1], FilterOp::Grayscale(0.5));
        assert!(
            matches!(ops[2], FilterOp::HueRotate(a) if (a - std::f32::consts::FRAC_PI_2).abs() < 1.0e-4)
        );
        assert!(matches!(ops[3], FilterOp::DropShadow(s) if s.dx == 2.0 && s.color.b == 255));
        // A percentage and a bare number mean the same thing.
        assert_eq!(
            parse_filter_list("brightness(150%)", shadow_ctx(), black),
            parse_filter_list("brightness(1.5)", shadow_ctx(), black)
        );
        // An SVG-referencing filter is skipped without taking the chain with it.
        let ops = parse_filter_list("url(#sharpen) invert(1)", shadow_ctx(), black);
        assert_eq!(ops, vec![FilterOp::Invert(1.0)]);
    }

    #[test]
    fn filters_apply_in_the_order_written() {
        let red = Color {
            r: 255,
            g: 0,
            b: 0,
            a: 255,
        };
        // Darkening then inverting is not inverting then darkening.
        let gray = Color {
            r: 128,
            g: 128,
            b: 128,
            a: 255,
        };
        let mut a = solid(2, 2, gray);
        apply_filters(&mut a, &[FilterOp::Brightness(0.5), FilterOp::Invert(1.0)]);
        let mut b = solid(2, 2, gray);
        apply_filters(&mut b, &[FilterOp::Invert(1.0), FilterOp::Brightness(0.5)]);
        assert_ne!(a.pixels[0], b.pixels[0], "the chain order did not matter");
        assert!(a.pixels[0].r > b.pixels[0].r);

        // Grayscale flattens the channels to one luminance.
        let mut gray = solid(1, 1, red);
        apply_filters(&mut gray, &[FilterOp::Grayscale(1.0)]);
        let p = gray.pixels[0];
        assert_eq!((p.r, p.g, p.b), (p.r, p.r, p.r));
        assert!(p.r > 0 && p.r < 255, "luminance came out as {}", p.r);

        // Invert is its own opposite.
        let mut twice = solid(1, 1, red);
        apply_filters(&mut twice, &[FilterOp::Invert(1.0), FilterOp::Invert(1.0)]);
        assert_eq!(twice.pixels[0], red);

        // Opacity works on the alpha channel, which is what lets a filtered
        // layer be composited rather than pasted.
        let mut faded = solid(1, 1, red);
        apply_filters(&mut faded, &[FilterOp::Opacity(0.5)]);
        assert_eq!(faded.pixels[0].a, 128);

        // Brightness scales, and clamps rather than wrapping.
        let mut bright = solid(1, 1, red);
        apply_filters(&mut bright, &[FilterOp::Brightness(2.0)]);
        assert_eq!(bright.pixels[0].r, 255);
    }

    #[test]
    fn blur_and_drop_shadow_work_against_transparency() {
        let opaque = Color {
            r: 0,
            g: 0,
            b: 0,
            a: 255,
        };
        // A small opaque square in a transparent buffer.
        let mut canvas = Canvas::transparent(21, 21);
        for y in 8..13 {
            for x in 8..13 {
                canvas.pixels[y * 21 + x] = opaque;
            }
        }
        let mut blurred = Canvas {
            pixels: canvas.pixels.clone(),
            width: 21,
            height: 21,
        };
        apply_filters(&mut blurred, &[FilterOp::Blur(4.0)]);
        assert!(
            blurred.pixels[10 * 21 + 10].a < 255,
            "the centre stayed hard"
        );
        assert!(
            blurred.pixels[10 * 21 + 14].a > 0,
            "the blur did not spread past the shape"
        );

        // drop-shadow puts tinted alpha *under* the shape, so the shape itself
        // is untouched and the shadow appears where nothing was.
        let mut shadowed = Canvas {
            pixels: canvas.pixels.clone(),
            width: 21,
            height: 21,
        };
        apply_filters(
            &mut shadowed,
            &[FilterOp::DropShadow(ShadowSpec {
                dx: 4.0,
                dy: 4.0,
                blur: 0.0,
                color: Color {
                    r: 255,
                    g: 0,
                    b: 0,
                    a: 255,
                },
            })],
        );
        assert_eq!(
            shadowed.pixels[10 * 21 + 10],
            opaque,
            "the shape was overdrawn"
        );
        let under = shadowed.pixels[14 * 21 + 14];
        assert_eq!(
            (under.r, under.a),
            (255, 255),
            "no shadow beneath the shape"
        );
    }

    #[test]
    fn a_backdrop_filter_reads_back_what_is_already_painted() {
        let red = Color {
            r: 255,
            g: 0,
            b: 0,
            a: 255,
        };
        let mut canvas = solid(10, 10, red);
        let area = Rect {
            x: 2.0,
            y: 2.0,
            width: 4.0,
            height: 4.0,
        };
        canvas.filter_backdrop(area, 0.0, &[FilterOp::Grayscale(1.0)], UNCLIPPED);
        // Inside the box the page beneath has been desaturated in place...
        let inside = canvas.pixels[4 * 10 + 4];
        assert_eq!(
            (inside.r, inside.g, inside.b),
            (inside.r, inside.r, inside.r)
        );
        // ...and outside it, nothing changed.
        assert_eq!(canvas.pixels[9 * 10 + 9], red);
    }

    #[test]
    fn shadow_lists_parse_with_colours_offsets_and_blur() {
        let black = Color {
            r: 0,
            g: 0,
            b: 0,
            a: 255,
        };
        let list = parse_shadow_list(
            "1px 1px 2px rgba(0, 0, 0, 0.5), 0 0 8px blue",
            shadow_ctx(),
            black,
        );
        assert_eq!(list.len(), 2, "the comma inside rgba() split the list");
        assert_eq!(list[0].dx, 1.0);
        assert_eq!(list[0].dy, 1.0);
        assert_eq!(list[0].blur, 2.0);
        assert_eq!(list[0].color.a, 128);
        assert_eq!(list[1].blur, 8.0);
        assert_eq!(list[1].color.b, 255);

        // An omitted colour is whatever the caller passed as the element's own.
        let red = Color {
            r: 255,
            g: 0,
            b: 0,
            a: 255,
        };
        let list = parse_shadow_list("2px 3px", shadow_ctx(), red);
        assert_eq!(list[0].color, red);
        assert_eq!(list[0].blur, 0.0);
        // The colour may lead as well as trail.
        let list = parse_shadow_list("blue 2px 3px 4px", shadow_ctx(), red);
        assert_eq!(list[0].color.b, 255);
        assert_eq!(list[0].dx, 2.0);
        // One offset is not a shadow.
        assert!(parse_shadow_list("4px", shadow_ctx(), red).is_empty());
    }

    #[test]
    fn a_blurred_mask_spreads_coverage_without_inventing_any() {
        // A small solid blob in the middle of a mask.
        let mut mask = Mask::bounded(0, 0, 21, 21);
        for y in 8..13 {
            for x in 8..13 {
                mask.cover(x, y, 255);
            }
        }
        let before: u32 = mask.alpha.iter().map(|&a| a as u32).sum();
        mask.blur(sigma_for(4.0));

        let at = |x: usize, y: usize| mask.alpha[y * mask.width + x];
        assert!(at(10, 10) < 255, "the blur did not soften the centre");
        assert!(at(13, 10) > 0, "the blur did not spread sideways");
        assert!(at(10, 13) > 0, "the blur did not spread vertically");
        // Softening moves coverage around; it must not manufacture ink.
        let after: u32 = mask.alpha.iter().map(|&a| a as u32).sum();
        assert!(
            after <= before,
            "blurring brightened the mask: {before} -> {after}"
        );

        // Zero blur leaves the mask exactly as it was.
        let mut sharp = Mask::bounded(0, 0, 5, 5);
        sharp.cover(2, 2, 200);
        sharp.blur(sigma_for(0.0));
        assert_eq!(sharp.alpha[2 * 5 + 2], 200);
    }

    #[test]
    fn text_shadows_paint_back_to_front_beneath_the_glyphs() {
        // The list order is reading order; the paint order is its reverse, so
        // the first shadow written ends up nearest the text.
        let red = Color {
            r: 255,
            g: 0,
            b: 0,
            a: 255,
        };
        let list = parse_shadow_list("1px 0 red, 2px 0 blue", shadow_ctx(), red);
        let painted: Vec<f32> = list.iter().rev().map(|s| s.dx).collect();
        assert_eq!(painted, vec![2.0, 1.0]);
    }

    fn run(x: f32, y: f32, width: f32) -> TextFragment {
        TextFragment {
            glyphs: Vec::new(),
            text: "word".to_string(),
            width,
            x,
            y,
            size: 16.0,
            line_height: 20.0,
            color: Color {
                r: 0,
                g: 0,
                b: 0,
                a: 255,
            },
            underline: false,
            strikethrough: false,
            bold: false,
            italic: false,
            font_index: 0,
            transform: crate::layout::TextTransform::None,
            shadows: std::rc::Rc::new(Vec::new()),
            generated: false,
        }
    }

    #[test]
    fn a_clipped_run_carries_its_clip_instead_of_being_kept_whole() {
        // The glyphs are already placed, so a run cannot be trimmed by moving
        // its box. It has to carry the clip to the rasterizer — otherwise a box
        // collapsed to a pixel, which is how every large site hides a heading
        // it keeps for screen readers, prints that heading across the page.
        let narrow = Rect {
            x: 10.0,
            y: 100.0,
            width: 1.0,
            height: 1.0,
        };
        let Some(DisplayCommand::Text(_, clip)) = clip_command(
            DisplayCommand::Text(run(10.0, 100.0, 90.0), UNCLIPPED),
            narrow,
        ) else {
            panic!("a run overlapping its clip should survive, carrying it");
        };
        assert_eq!((clip.width, clip.height), (1.0, 1.0));

        // Clips compose: an inner one can only ever narrow an outer one.
        let outer = Rect {
            x: 0.0,
            y: 100.0,
            width: 40.0,
            height: 20.0,
        };
        let Some(DisplayCommand::Text(_, clip)) =
            clip_command(DisplayCommand::Text(run(10.0, 100.0, 90.0), narrow), outer)
        else {
            panic!("overlapping clips should intersect, not cancel");
        };
        assert_eq!((clip.width, clip.height), (1.0, 1.0));

        // A run wholly outside its clip is dropped, as before.
        let elsewhere = Rect {
            x: 500.0,
            y: 500.0,
            width: 10.0,
            height: 10.0,
        };
        assert!(clip_command(
            DisplayCommand::Text(run(10.0, 100.0, 90.0), UNCLIPPED),
            elsewhere
        )
        .is_none());
    }

    #[test]
    fn a_transform_moves_a_runs_clip_along_with_the_run() {
        // Otherwise a transformed box goes on clipping where it used to be,
        // and its text is cut against empty space.
        let clip = Rect {
            x: 10.0,
            y: 10.0,
            width: 20.0,
            height: 20.0,
        };
        let xf = Xf {
            scale: 2.0,
            dx: 5.0,
            dy: 7.0,
        };
        let DisplayCommand::Text(frag, moved) =
            transform(DisplayCommand::Text(run(10.0, 10.0, 20.0), clip), xf)
        else {
            panic!("a text command stays a text command");
        };
        assert_eq!((frag.x, frag.y), (25.0, 27.0));
        assert_eq!((moved.x, moved.y), (25.0, 27.0));
        assert_eq!((moved.width, moved.height), (40.0, 40.0));
    }
}

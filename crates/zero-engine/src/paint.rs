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
    /// image src, destination content box, and how it fits that box.
    Image(String, Rect, ObjectFit),
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

        // No bold/italic font file is loaded (Track D4), so a heavier weight
        // and a slant are synthesized here instead of picked from a face:
        // bold redraws each row a pixel wider, italic shears columns toward
        // the top by `row`'s distance from the baseline.
        let stroke = if frag.bold { (frag.size / 24.0).max(1.0).round() as i32 } else { 0 };
        let shear = if frag.italic { 0.22 } else { 0.0 };

        for glyph in &frag.glyphs {
            let (m, coverage) = font.rasterize_indexed(glyph.id, frag.size);
            // fontdue gives per-pixel coverage (0..=255); place relative to the baseline.
            let gx = (frag.x + glyph.x + m.xmin as f32).round() as i32;
            let gy = (baseline - glyph.y - m.ymin as f32 - m.height as f32).round() as i32;

            for row in 0..m.height {
                let row_shear = (shear * (m.height - row) as f32).round() as i32;
                let py = gy + row as i32;
                if py < 0 || py >= self.height as i32 {
                    continue;
                }
                for col in 0..m.width {
                    let a = coverage[row * m.width + col];
                    if a == 0 {
                        continue;
                    }
                    for dx in 0..=stroke {
                        let px = gx + col as i32 + row_shear + dx;
                        if px < 0 || px >= self.width as i32 {
                            continue;
                        }
                        let idx = py as usize * self.width + px as usize;
                        self.pixels[idx] = blend(self.pixels[idx], frag.color, a);
                    }
                }
            }
        }

        // A stroke's own thickness, not part of any glyph's rasterized coverage.
        let stroke = (frag.size / 16.0).max(1.0);
        if frag.underline {
            let y = baseline + stroke;
            self.paint_solid(frag.color, Rect { x: frag.x, y, width: frag.width, height: stroke });
        }
        if frag.strikethrough {
            let y = baseline - frag.size * 0.3;
            self.paint_solid(frag.color, Rect { x: frag.x, y, width: frag.width, height: stroke });
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
            let full_src = Rect { x: 0.0, y: 0.0, width: natural.0, height: natural.1 };
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
        let start_x = if repeat_x { rect.x + ox - (ox / iw).ceil() * iw } else { rect.x + ox };
        let start_y = if repeat_y { rect.y + oy - (oy / ih).ceil() * ih } else { rect.y + oy };
        let (scale_x, scale_y) = (natural.0 / iw, natural.1 / ih);

        let mut y = start_y;
        loop {
            let mut x = start_x;
            loop {
                let tile = Rect { x, y, width: iw, height: ih };
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
                DisplayCommand::Image(src, rect, fit) => {
                    if let Some(img) = images.get(src) {
                        canvas.paint_fitted_image(img, *rect, *fit);
                    }
                }
                DisplayCommand::BackgroundImage { src, rect, size, position, repeat } => {
                    if let Some(img) = images.get(src) {
                        canvas.paint_background_image(img, *rect, *size, *position, *repeat);
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
        .any(|name| matches!(style.value(name), Some(Value::Length(..))))
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
        DisplayCommand::Gradient { rect, radius, stops, horizontal } => DisplayCommand::Gradient {
            rect,
            radius,
            stops: stops.into_iter().map(dim).collect(),
            horizontal,
        },
        DisplayCommand::Shadow { rect, radius, blur, color } => DisplayCommand::Shadow {
            rect,
            radius,
            blur,
            color: dim(color),
        },
        DisplayCommand::Text(frag) => DisplayCommand::Text(TextFragment {
            color: dim(frag.color),
            ..frag
        }),
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
    const NONE: Xf = Xf { scale: 1.0, dx: 0.0, dy: 0.0 };

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

/// What `transform` does to this box and everything inside it.
///
/// ponytail: `translate` and `scale` only. Percentages resolve against the box's
/// own border box, which is what `translate(-50%, -50%)` — the way the web
/// centres things — needs. `rotate`, `skew` and matrices are ignored rather than
/// approximated: moving and scaling a box is exact, and rotating text would need
/// the rasterizer to turn glyphs, not just place them.
fn transform_of(layout_box: &LayoutBox) -> Xf {
    let style = match layout_box.box_type {
        BoxType::BlockNode(s) | BoxType::InlineNode(s) => s,
        BoxType::AnonymousBlock => return Xf::NONE,
    };
    let spec = match style.value("transform") {
        Some(Value::Raw(text)) => text,
        _ => return Xf::NONE,
    };
    let box_rect = layout_box.dimensions.border_box();
    let mut shift = (0.0f32, 0.0f32);
    let mut scale = 1.0f32;
    let mut rest = spec.as_str();
    // Translations commute and a uniform scale commutes with them up to the
    // origin, which is handled once at the end — so the list can be summed.
    while let Some(open) = rest.find('(') {
        let name = rest[..open].trim().trim_start_matches(',').trim().to_ascii_lowercase();
        let Some(close) = rest[open..].find(')') else { break };
        let args: Vec<&str> = rest[open + 1..open + close].split(',').map(str::trim).collect();
        let ctx = style.length_context(0.0);
        // A percentage is of this box's own size, so each axis has its own base.
        let px = |token: &str, base: f32| match token.strip_suffix('%') {
            Some(pct) => pct.trim().parse::<f32>().unwrap_or(0.0) / 100.0 * base,
            None => crate::css::parse_length_token(token, ctx),
        };
        let number = |token: &str| token.parse::<f32>().ok();
        match name.as_str() {
            "translate" => {
                shift.0 += px(args[0], box_rect.width);
                if let Some(y) = args.get(1) {
                    shift.1 += px(y, box_rect.height);
                }
            }
            "translatex" => shift.0 += px(args[0], box_rect.width),
            "translatey" => shift.1 += px(args[0], box_rect.height),
            // A non-uniform scale would need two factors everywhere below; the
            // larger of the two is closer than ignoring the transform outright.
            "scale" => {
                let x = number(args[0]).unwrap_or(1.0);
                let y = args.get(1).and_then(|a| number(a)).unwrap_or(x);
                scale *= x.abs().max(y.abs());
            }
            "scalex" | "scaley" => scale *= number(args[0]).unwrap_or(1.0).abs(),
            _ => {} // rotate/skew/matrix: not modelled
        }
        rest = &rest[open + close + 1..];
    }
    if shift == (0.0, 0.0) && scale == 1.0 {
        return Xf::NONE;
    }
    // Scaling happens about the box's centre, as `transform-origin` defaults to.
    let centre = (
        box_rect.x + box_rect.width / 2.0,
        box_rect.y + box_rect.height / 2.0,
    );
    Xf {
        scale,
        dx: centre.0 * (1.0 - scale) + shift.0,
        dy: centre.1 * (1.0 - scale) + shift.1,
    }
}

/// Move and scale one command.
fn transform(item: DisplayCommand, xf: Xf) -> DisplayCommand {
    if xf.is_none() {
        return item;
    }
    match item {
        DisplayCommand::SolidColor(c, rect) => DisplayCommand::SolidColor(c, xf.rect(rect)),
        DisplayCommand::RoundedColor(c, rect, radius) => {
            DisplayCommand::RoundedColor(c, xf.rect(rect), radius * xf.scale)
        }
        DisplayCommand::Gradient { rect, radius, stops, horizontal } => DisplayCommand::Gradient {
            rect: xf.rect(rect),
            radius: radius * xf.scale,
            stops,
            horizontal,
        },
        DisplayCommand::Shadow { rect, radius, blur, color } => DisplayCommand::Shadow {
            rect: xf.rect(rect),
            radius: radius * xf.scale,
            blur: blur * xf.scale,
            color,
        },
        DisplayCommand::Image(src, rect, fit) => DisplayCommand::Image(src, xf.rect(rect), fit),
        DisplayCommand::BackgroundImage { src, rect, size, position, repeat } => {
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
        DisplayCommand::Text(frag) => DisplayCommand::Text(TextFragment {
            x: frag.x * xf.scale + xf.dx,
            y: frag.y * xf.scale + xf.dy,
            width: frag.width * xf.scale,
            size: frag.size * xf.scale,
            line_height: frag.line_height * xf.scale,
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
        }),
    }
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
    let alpha = alpha * opacity_of(layout_box);
    // A transform applies to the box and everything inside it, so it composes
    // with whatever its ancestors already did.
    let xf = outer.then(transform_of(layout_box));
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
    let inner = child_clip(layout_box, clip, xf).unwrap_or(Rect::default());
    // A higher `z-index` paints later, and equal ones keep document order.
    //
    // ponytail: one flat order rather than real stacking contexts, so a child's
    // z-index competes with its uncles. Nested contexts need the display list to
    // become a tree.
    let mut order: Vec<&LayoutBox> = layout_box.children.iter().collect();
    order.sort_by_key(|child| z_index_of(child));
    for child in order {
        render_layout_box(list, child, inner, alpha, xf);
    }
}

fn render_own(list: &mut DisplayList, layout_box: &LayoutBox) {
    render_shadow(list, layout_box);
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
    for frag in &layout_box.text_fragments {
        list.push(DisplayCommand::Text(frag.clone()));
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
        if let Some((stops, horizontal)) = parse_gradient(spec) {
            list.push(DisplayCommand::Gradient { rect: box_rect, radius, stops, horizontal });
            return;
        }
    }
    // `background-color` still paints first when there's a `url()` image on
    // top, so a transparent PNG (or one still loading) shows something.
    if let Some(color) = get_color(layout_box, "background")
        .or_else(|| get_color(layout_box, "background-color"))
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
    if let Some(src) = spec.as_deref().and_then(|s| bg_url(s)) {
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
    Some(inner.trim().trim_matches(|c| c == '"' || c == '\'').to_string())
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
        Some(v @ (Value::Length(..) | Value::Number(_))) => v.resolve(style.length_context(0.0)),
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
        Rect { x: outer.x, y: outer.y, width: outer.width, height: width },
        Rect { x: outer.x, y: outer.y + outer.height - width, width: outer.width, height: width },
        Rect { x: outer.x, y: outer.y, width, height: outer.height },
        Rect { x: outer.x + outer.width - width, y: outer.y, width, height: outer.height },
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

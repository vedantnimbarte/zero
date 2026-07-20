//! Paint: turn layout boxes into a display list, then rasterize to a pixel canvas.
//!
//! ponytail: solid-color backgrounds + borders + anti-aliased text. No images,
//! gradients, shadows, border-radius, or GPU compositing yet
//! (docs/01-ARCHITECTURE.md §3 [6]-[7]).

use crate::css::{Color, Value};
use crate::layout::{BoxType, LayoutBox, Rect, TextFragment};
use fontdue::Font;

pub struct Canvas {
    pub pixels: Vec<Color>,
    pub width: usize,
    pub height: usize,
}

enum DisplayCommand {
    SolidColor(Color, Rect),
    Text(TextFragment),
}

type DisplayList = Vec<DisplayCommand>;

impl Canvas {
    fn new(width: usize, height: usize) -> Canvas {
        let white = Color { r: 255, g: 255, b: 255, a: 255 };
        Canvas { pixels: vec![white; width * height], width, height }
    }

    fn paint_solid(&mut self, color: Color, rect: Rect) {
        let x0 = rect.x.clamp(0.0, self.width as f32) as usize;
        let y0 = rect.y.clamp(0.0, self.height as f32) as usize;
        let x1 = (rect.x + rect.width).clamp(0.0, self.width as f32) as usize;
        let y1 = (rect.y + rect.height).clamp(0.0, self.height as f32) as usize;
        for y in y0..y1 {
            for x in x0..x1 {
                self.pixels[y * self.width + x] = color;
            }
        }
    }

    /// Rasterize a text run glyph-by-glyph and alpha-blend it onto the canvas.
    fn paint_text(&mut self, frag: &TextFragment, font: &Font) {
        let ascent = font.horizontal_line_metrics(frag.size).map_or(frag.size, |m| m.ascent);
        let baseline = frag.y + ascent;
        let mut pen_x = frag.x;

        for ch in frag.text.chars() {
            let (m, coverage) = font.rasterize(ch, frag.size);
            // fontdue gives per-pixel coverage (0..=255); place relative to the baseline.
            let gx = (pen_x + m.xmin as f32).round() as i32;
            let gy = (baseline - m.ymin as f32 - m.height as f32).round() as i32;

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
            pen_x += m.advance_width;
        }
    }
}

/// Alpha-blend `src` (scaled by `coverage`) over `dst`.
fn blend(dst: Color, src: Color, coverage: u8) -> Color {
    let a = (coverage as f32 / 255.0) * (src.a as f32 / 255.0);
    let mix = |d: u8, s: u8| (s as f32 * a + d as f32 * (1.0 - a)).round() as u8;
    Color { r: mix(dst.r, src.r), g: mix(dst.g, src.g), b: mix(dst.b, src.b), a: 255 }
}

pub fn paint(layout_root: &LayoutBox, bounds: Rect, font: Option<&Font>) -> Canvas {
    let display_list = build_display_list(layout_root);
    let mut canvas = Canvas::new(bounds.width as usize, bounds.height as usize);
    for item in &display_list {
        match item {
            DisplayCommand::SolidColor(color, rect) => canvas.paint_solid(*color, *rect),
            DisplayCommand::Text(frag) => {
                if let Some(font) = font {
                    canvas.paint_text(frag, font);
                }
            }
        }
    }
    canvas
}

fn build_display_list(layout_root: &LayoutBox) -> DisplayList {
    let mut list = Vec::new();
    render_layout_box(&mut list, layout_root);
    list
}

fn render_layout_box(list: &mut DisplayList, layout_box: &LayoutBox) {
    render_background(list, layout_box);
    render_borders(list, layout_box);
    // Text sits above this box's background/borders.
    for frag in &layout_box.text_fragments {
        list.push(DisplayCommand::Text(frag.clone()));
    }
    for child in &layout_box.children {
        render_layout_box(list, child);
    }
}

fn render_background(list: &mut DisplayList, layout_box: &LayoutBox) {
    if let Some(color) = get_color(layout_box, "background") {
        list.push(DisplayCommand::SolidColor(color, layout_box.dimensions.border_box()));
    }
}

fn render_borders(list: &mut DisplayList, layout_box: &LayoutBox) {
    let color = match get_color(layout_box, "border-color") {
        Some(color) => color,
        None => return,
    };
    let d = &layout_box.dimensions;
    let b = d.border_box();

    // Left, right, top, bottom border strips.
    list.push(DisplayCommand::SolidColor(color, Rect { x: b.x, y: b.y, width: d.border.left, height: b.height }));
    list.push(DisplayCommand::SolidColor(color, Rect { x: b.x + b.width - d.border.right, y: b.y, width: d.border.right, height: b.height }));
    list.push(DisplayCommand::SolidColor(color, Rect { x: b.x, y: b.y, width: b.width, height: d.border.top }));
    list.push(DisplayCommand::SolidColor(color, Rect { x: b.x, y: b.y + b.height - d.border.bottom, width: b.width, height: d.border.bottom }));
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

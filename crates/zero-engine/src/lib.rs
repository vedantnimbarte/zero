//! # Zero Engine
//!
//! An embeddable, UI-agnostic web rendering engine. This is the *engine only* —
//! it knows nothing about windows, tabs, menus, or file formats. You give it a
//! document and a viewport size; it gives you pixels. Build any browser UI you
//! like around it (like CEF around Chromium, or libservo around Servo).
//!
//! ## Embedding API (the stable surface)
//! ```
//! // The embedder supplies font bytes (it knows the platform's font paths).
//! let engine = zero_engine::Engine::shapes_only();          // or ::new(&ttf_bytes)
//! let canvas = engine.render("<div></div>", "div { background: #ff0000; }", 800.0, 600.0);
//! // `canvas.pixels` is `Color` data — blit it to a window, encode a PNG, whatever you need.
//! ```
//!
//! Internals (parsing, style, layout, paint) are public modules for learning/inspection,
//! but the intended entry point is [`Engine::render`]. Pipeline: HTML+CSS -> DOM ->
//! styled tree -> layout boxes -> pixels.

pub mod css;
pub mod dom;
pub mod html;
pub mod layout;
pub mod paint;
pub mod style;

pub use css::Color;
pub use paint::Canvas;

use dom::{Node, NodeType};
use fontdue::Font;

/// Minimal user-agent stylesheet: gives real documents sane default display
/// (block for structural tags, none for head/script/style) so they lay out.
const USER_AGENT_CSS: &str = "
    html, body, div, p, h1, h2, h3, h4, h5, h6, ul, ol, li, section, article,
    header, footer, nav, main, aside, figure, figcaption, blockquote, pre,
    table, tr, form, fieldset, address, hr { display: block; }
    head, script, style, meta, link, title, noscript, base { display: none; }
";

/// A rendering engine instance. Holds the font used for text; construct once, render many.
pub struct Engine {
    font: Option<Font>,
}

impl Engine {
    /// Build an engine that renders text using the given TrueType font bytes.
    pub fn new(font_bytes: &[u8]) -> Result<Engine, &'static str> {
        let font = Font::from_bytes(font_bytes, fontdue::FontSettings::default())?;
        Ok(Engine { font: Some(font) })
    }

    /// Build an engine with no font: boxes/colors render, text is skipped.
    /// Useful for tests and headless shape rendering.
    pub fn shapes_only() -> Engine {
        Engine { font: None }
    }

    /// Render an HTML + CSS document to a pixel [`Canvas`] of the given size.
    ///
    /// The embedder owns everything else: windowing, input, chrome, and what to do
    /// with the returned pixels.
    pub fn render(&self, html_source: &str, css_source: &str, width: f32, height: f32) -> Canvas {
        let root = html::parse(html_source.to_string());

        // Cascade order (later wins on ties): UA stylesheet < page <style> < caller CSS.
        let mut stylesheet = css::parse(USER_AGENT_CSS.to_string());
        let mut page_css = String::new();
        collect_style_text(&root, &mut page_css);
        stylesheet.rules.extend(css::parse(page_css).rules);
        stylesheet.rules.extend(css::parse(css_source.to_string()).rules);

        let style_root = style::style_tree(&root, &stylesheet);

        let mut viewport: layout::Dimensions = Default::default();
        viewport.content.width = width;
        viewport.content.height = height;
        let bounds = viewport.content;

        let layout_root = layout::layout_tree(&style_root, viewport, self.font.as_ref());
        paint::paint(&layout_root, bounds, self.font.as_ref())
    }
}

/// Gather the text inside every `<style>` element into one CSS string.
fn collect_style_text(node: &Node, out: &mut String) {
    if let NodeType::Element(ref e) = node.node_type {
        if e.tag_name == "style" {
            for child in &node.children {
                if let NodeType::Text(ref t) = child.node_type {
                    out.push_str(t);
                    out.push('\n');
                }
            }
        }
    }
    for child in &node.children {
        collect_style_text(child, out);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn render_produces_correctly_sized_canvas() {
        let engine = super::Engine::shapes_only();
        let canvas = engine.render("<div></div>", "div { background: #112233; }", 40.0, 30.0);
        assert_eq!((canvas.width, canvas.height), (40, 30));
        assert_eq!(canvas.pixels.len(), 40 * 30);
    }
}

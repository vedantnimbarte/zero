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
pub mod js;
pub mod layout;
pub mod paint;
pub mod resource;
pub mod style;
pub mod text;

pub use css::Color;
pub use layout::LinkArea;
pub use paint::Canvas;
pub use resource::{DecodedImage, ResourceLoader};

use dom::{Node, NodeType};
use fontdue::Font;
use resource::{ImageMap, NullLoader};
use text::{FontEntry, FontSet};

/// The result of rendering: pixels plus clickable link regions (page coordinates).
pub struct Page {
    pub canvas: Canvas,
    pub links: Vec<LinkArea>,
    /// console.log output and script errors, for the embedder to surface.
    pub console: Vec<String>,
}

/// Minimal user-agent stylesheet: gives real documents sane default display
/// (block for structural tags, none for head/script/style) so they lay out.
const USER_AGENT_CSS: &str = "
    html, body, div, p, h1, h2, h3, h4, h5, h6, ul, ol, li, section, article,
    header, footer, nav, main, aside, figure, figcaption, blockquote, pre,
    table, tr, form, fieldset, address, hr, img { display: block; }
    head, script, style, meta, link, title, noscript, base { display: none; }
";

/// One loaded font: the rasterizer plus the raw bytes a shaping face is built from.
struct LoadedFont {
    raster: Font,
    bytes: Vec<u8>,
}

/// A rendering engine instance. Holds the fonts used for text; construct once, render many.
///
/// Fonts are in priority order and used as a fallback chain — no single font covers
/// every script, so each run is drawn with the first font that can render it.
pub struct Engine {
    fonts: Vec<LoadedFont>,
}

impl Engine {
    /// Build an engine that renders text using the given TrueType font bytes.
    pub fn new(font_bytes: &[u8]) -> Result<Engine, &'static str> {
        let raster = Font::from_bytes(font_bytes, fontdue::FontSettings::default())?;
        Ok(Engine { fonts: vec![LoadedFont { raster, bytes: font_bytes.to_vec() }] })
    }

    /// Build an engine with a prioritized font fallback chain. Fonts that fail to
    /// parse are skipped; the first usable one is primary.
    pub fn with_fonts(fonts: Vec<Vec<u8>>) -> Engine {
        let fonts = fonts
            .into_iter()
            .filter_map(|bytes| {
                Font::from_bytes(bytes.as_slice(), fontdue::FontSettings::default())
                    .ok()
                    .map(|raster| LoadedFont { raster, bytes })
            })
            .collect();
        Engine { fonts }
    }

    /// Build an engine with no font: boxes/colors render, text is skipped.
    /// Useful for tests and headless shape rendering.
    pub fn shapes_only() -> Engine {
        Engine { fonts: Vec::new() }
    }

    /// Render an HTML + CSS document to a pixel [`Canvas`], without loading any
    /// external resources (images render blank). See [`Engine::render_page`].
    pub fn render(&self, html_source: &str, css_source: &str, width: f32, height: f32) -> Canvas {
        self.render_page(html_source, css_source, width, height, &NullLoader).canvas
    }

    /// Render a full [`Page`] (pixels + clickable links), using `loader` to fetch
    /// `<img>` and other subresources.
    ///
    /// The embedder owns everything else: windowing, input, chrome, networking, and
    /// what to do with the returned pixels/links.
    pub fn render_page(
        &self,
        html_source: &str,
        css_source: &str,
        width: f32,
        height: f32,
        loader: &dyn ResourceLoader,
    ) -> Page {
        let mut root = html::parse(html_source.to_string());

        // Run <script> content, then splice any document.write() output into the
        // document so it participates in styling and layout.
        let mut console = Vec::new();
        let mut script_source = String::new();
        collect_script_text(&root, &mut script_source);
        if !script_source.trim().is_empty() {
            let out = js::run_with_dom(&script_source, build_dom_view(&root));
            console.extend(out.console);
            console.extend(out.errors.iter().map(|e| format!("[error] {e}")));
            apply_mutations(&mut root, &out.mutations);
            if !out.writes.trim().is_empty() {
                let written = html::parse(format!("<div>{}</div>", out.writes));
                append_to_body(&mut root, written);
            }
        }

        // Cascade order (later wins on ties): UA stylesheet < page <style> < caller CSS.
        let mut stylesheet = css::parse(USER_AGENT_CSS.to_string());
        let mut page_css = String::new();
        collect_style_text(&root, &mut page_css);
        stylesheet.rules.extend(css::parse(page_css).rules);
        stylesheet.rules.extend(css::parse(css_source.to_string()).rules);

        let style_root = style::style_tree(&root, &stylesheet);

        // Fetch + decode every <img> up front so layout knows their sizes.
        let mut images = ImageMap::new();
        collect_and_load_images(&root, loader, &mut images);

        let mut viewport: layout::Dimensions = Default::default();
        viewport.content.width = width;
        viewport.content.height = height;

        // Shaping faces are built per render; they borrow the stored font bytes.
        let faces: Vec<Option<rustybuzz::Face>> =
            self.fonts.iter().map(|f| rustybuzz::Face::from_slice(&f.bytes, 0)).collect();
        let entries: Vec<FontEntry> = self
            .fonts
            .iter()
            .zip(faces.iter())
            .filter_map(|(f, face)| {
                face.as_ref().map(|shaper| FontEntry { raster: &f.raster, shaper })
            })
            .collect();
        let fonts = if entries.is_empty() { None } else { Some(FontSet { entries }) };

        let layout_root = layout::layout_tree(&style_root, viewport, fonts.as_ref(), &images);
        // Canvas is at least the viewport, but grows to the full document height so
        // the embedder can scroll through overflow.
        let doc_height = layout_root.dimensions.margin_box().height.max(height);
        let bounds = layout::Rect { x: 0.0, y: 0.0, width, height: doc_height };
        let canvas = paint::paint(&layout_root, bounds, fonts.as_ref(), &images);

        let mut links = Vec::new();
        layout::collect_links(&layout_root, &mut links);
        Page { canvas, links, console }
    }
}

/// Find every `<img src>`, ask the loader for its bytes, and decode it.
fn collect_and_load_images(node: &Node, loader: &dyn ResourceLoader, out: &mut ImageMap) {
    if let NodeType::Element(ref e) = node.node_type {
        if e.tag_name == "img" {
            if let Some(src) = e.attributes.get("src") {
                if !out.contains_key(src) {
                    if let Some(img) = loader.load(src).and_then(|b| resource::decode_image(&b)) {
                        out.insert(src.clone(), img);
                    }
                }
            }
        }
    }
    for child in &node.children {
        collect_and_load_images(child, loader, out);
    }
}

/// Snapshot every element so scripts can address them by handle.
fn build_dom_view(root: &Node) -> js::DomView {
    fn walk(node: &Node, path: &mut Vec<usize>, out: &mut Vec<js::ElementInfo>) {
        if let NodeType::Element(ref e) = node.node_type {
            out.push(js::ElementInfo {
                path: path.clone(),
                id: e.id().cloned().unwrap_or_default(),
                tag: e.tag_name.clone(),
                text: text_content(node),
            });
        }
        for (i, child) in node.children.iter().enumerate() {
            path.push(i);
            walk(child, path, out);
            path.pop();
        }
    }
    let mut elements = Vec::new();
    walk(root, &mut Vec::new(), &mut elements);
    js::DomView { elements }
}

fn text_content(node: &Node) -> String {
    match node.node_type {
        NodeType::Text(ref t) => t.clone(),
        _ => node.children.iter().map(text_content).collect(),
    }
}

/// Apply the DOM writes a script recorded, in order.
fn apply_mutations(root: &mut Node, mutations: &[js::Mutation]) {
    if mutations.is_empty() {
        return;
    }
    let view = build_dom_view(root);
    for mutation in mutations {
        let (index, replacement) = match mutation {
            js::Mutation::SetText(i, text) => (*i, vec![dom::text(text.clone())]),
            js::Mutation::SetHtml(i, html) => {
                (*i, html::parse(format!("<div>{html}</div>")).children)
            }
        };
        let path = match view.elements.get(index) {
            Some(e) => e.path.clone(),
            None => continue,
        };
        if let Some(node) = node_at(root, &path) {
            node.children = replacement;
        }
    }
}

fn node_at<'a>(root: &'a mut Node, path: &[usize]) -> Option<&'a mut Node> {
    let mut current = root;
    for &i in path {
        current = current.children.get_mut(i)?;
    }
    Some(current)
}

/// Gather the text inside every `<script>` element into one JS source string.
fn collect_script_text(node: &Node, out: &mut String) {
    if let NodeType::Element(ref e) = node.node_type {
        if e.tag_name == "script" && !e.attributes.contains_key("src") {
            for child in &node.children {
                if let NodeType::Text(ref t) = child.node_type {
                    out.push_str(t);
                    out.push('\n');
                }
            }
        }
    }
    for child in &node.children {
        collect_script_text(child, out);
    }
}

/// Append a node to <body> (or the root if there is none).
fn append_to_body(root: &mut Node, node: Node) {
    fn find_body(n: &mut Node) -> Option<&mut Node> {
        if let NodeType::Element(ref e) = n.node_type {
            if e.tag_name == "body" {
                return Some(n);
            }
        }
        n.children.iter_mut().find_map(find_body)
    }
    match find_body(root) {
        Some(body) => body.children.push(node),
        None => root.children.push(node),
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

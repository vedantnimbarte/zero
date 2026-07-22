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
pub use layout::{ElementRect, LinkArea};
pub use paint::Canvas;
pub use resource::{DecodedImage, KeyValueStore, ResourceLoader};

use dom::{Node, NodeType};
use fontdue::Font;
use resource::{ImageMap, NullLoader};
use std::collections::HashMap;
use text::{FontEntry, FontSet};

/// The result of rendering: pixels plus clickable link regions (page coordinates).
pub struct Page {
    pub canvas: Canvas,
    pub links: Vec<LinkArea>,
    /// console.log output and script errors, for the embedder to surface.
    pub console: Vec<String>,
    /// Painted element boxes, so the embedder can hit-test clicks for scripts.
    pub element_rects: Vec<ElementRect>,
    /// Boxes of the find-in-page matches, in document order.
    pub find_matches: Vec<layout::Rect>,
    /// Whether any rule used `:hover`. Without this the embedder would repaint
    /// on every mouse move for pages that do not react to the cursor at all.
    pub uses_hover: bool,
}

/// A loaded document with a **persistent** JavaScript runtime.
///
/// Event handlers are closures that must outlive the initial script run, so the
/// interpreter lives as long as the page rather than being torn down per render.
pub struct Document {
    root: Node,
    css: String,
    interp: js::interp::Interp,
    next_node_id: usize,
    /// Current text of each form field, keyed by node id so it survives re-renders.
    form_values: HashMap<usize, String>,
    /// The focused text field, if any.
    focused: Option<usize>,
    /// The field's text when it gained focus, so `change` only fires on a real edit.
    focus_baseline: String,
    /// Each `<select>`'s chosen (label, value), captured before its options are
    /// collapsed into the closed control's text.
    selections: HashMap<usize, (String, String)>,
    /// The find-in-page query, highlighted on the next render.
    find: Option<String>,
    /// The parsed stylesheet, kept between renders, keyed by the source text and
    /// the viewport width it was filtered for.
    sheet: Option<((u64, u32), css::Stylesheet, style::RuleIndex)>,
    /// The element under the cursor, plus its ancestors — `:hover` applies to
    /// the whole chain, not just the innermost element.
    hovered: style::HoverChain,
    pub console: Vec<String>,
}

impl Document {
    /// Parse `html` and run its scripts, giving them network access via `loader`.
    pub fn load_with(
        html_source: &str,
        css_source: &str,
        loader: std::rc::Rc<dyn ResourceLoader>,
    ) -> Document {
        Document::load_hosted(html_source, css_source, Some(loader), None)
    }

    /// Parse `html` and run its scripts with whatever host services are available.
    /// Both are optional so tests and headless renders can skip them.
    pub fn load_hosted(
        html_source: &str,
        css_source: &str,
        loader: Option<std::rc::Rc<dyn ResourceLoader>>,
        store: Option<std::rc::Rc<dyn KeyValueStore>>,
    ) -> Document {
        let mut doc = Document::prepare(html_source, css_source);
        if let Some(loader) = loader {
            doc.interp.set_loader(loader);
        }
        if let Some(store) = store {
            doc.interp.set_store(store);
        }
        doc.run_initial_scripts();
        doc
    }

    /// Parse `html`, then run its `<script>` content once.
    pub fn load(html_source: &str, css_source: &str) -> Document {
        let mut doc = Document::prepare(html_source, css_source);
        doc.run_initial_scripts();
        doc
    }

    fn prepare(html_source: &str, css_source: &str) -> Document {
        let mut doc = Document {
            root: html::parse(html_source.to_string()),
            css: css_source.to_string(),
            interp: js::interp::Interp::new(),
            next_node_id: 1, // 0 means "unassigned"
            form_values: HashMap::new(),
            focused: None,
            focus_baseline: String::new(),
            selections: HashMap::new(),
            find: None,
            sheet: None,
            hovered: Default::default(),
            console: Vec::new(),
        };
        doc.assign_node_ids();
        doc.seed_selections();
        doc.seed_form_values();
        doc.sync_form_fields();
        doc.interp.set_dom(build_dom_view(&doc.root));
        doc
    }

    /// Record what each `<select>` has chosen, before rendering collapses it.
    ///
    /// A closed dropdown shows one option; without this the whole option list
    /// lays out as text and swamps the page.
    fn seed_selections(&mut self) {
        fn walk(node: &Node, out: &mut HashMap<usize, (String, String)>) {
            if let NodeType::Element(ref e) = node.node_type {
                if e.tag_name == "select" {
                    let options: Vec<&Node> = node
                        .children
                        .iter()
                        .filter(|c| matches!(&c.node_type, NodeType::Element(o) if o.tag_name == "option"))
                        .collect();
                    // The marked option wins; otherwise a dropdown shows its first.
                    let chosen = options
                        .iter()
                        .find(|o| match &o.node_type {
                            NodeType::Element(o) => o.attributes.contains_key("selected"),
                            NodeType::Text(_) => false,
                        })
                        .or(options.first())
                        .copied();
                    if let Some(option) = chosen {
                        let label = text_content(option).trim().to_string();
                        let value = match &option.node_type {
                            NodeType::Element(o) => o
                                .attributes
                                .get("value")
                                .cloned()
                                .unwrap_or_else(|| label.clone()),
                            NodeType::Text(_) => label.clone(),
                        };
                        out.insert(e.node_id, (label, value));
                    }
                }
            }
            for child in &node.children {
                walk(child, out);
            }
        }
        let mut selections = std::mem::take(&mut self.selections);
        walk(&self.root, &mut selections);
        self.selections = selections;
    }

    /// Text fields start from their `value` attribute.
    fn seed_form_values(&mut self) {
        fn walk(node: &Node, out: &mut HashMap<usize, String>) {
            if let NodeType::Element(ref e) = node.node_type {
                if is_text_field(&e.tag_name) {
                    let initial = e.attributes.get("value").cloned().unwrap_or_default();
                    out.entry(e.node_id).or_insert(initial);
                }
            }
            for child in &node.children {
                walk(child, out);
            }
        }
        let mut values = std::mem::take(&mut self.form_values);
        walk(&self.root, &mut values);
        self.form_values = values;
    }

    /// Mirror each field's value into the DOM so normal text layout renders it,
    /// with a caret on the focused one.
    fn sync_form_fields(&mut self) {
        let (values, focused) = (self.form_values.clone(), self.focused);
        let selections = self.selections.clone();
        fn walk(
            node: &mut Node,
            values: &HashMap<usize, String>,
            focused: Option<usize>,
            selections: &HashMap<usize, (String, String)>,
        ) {
            let mut replacement = None;
            if let NodeType::Element(ref e) = node.node_type {
                if let Some((label, _)) = selections.get(&e.node_id) {
                    // A closed dropdown: its choice, not its option list.
                    replacement = Some(label.clone());
                } else if is_text_field(&e.tag_name) {
                    let mut text = values.get(&e.node_id).cloned().unwrap_or_default();
                    if focused == Some(e.node_id) {
                        text.push('|'); // caret
                    } else if text.is_empty() {
                        text = e.attributes.get("placeholder").cloned().unwrap_or_default();
                    }
                    replacement = Some(text);
                }
            }
            if let Some(text) = replacement {
                // A space keeps the line box (and so the field height) non-zero.
                node.children = vec![dom::text(if text.is_empty() { " ".into() } else { text })];
                return;
            }
            for child in &mut node.children {
                walk(child, values, focused, selections);
            }
        }
        walk(&mut self.root, &values, focused, &selections);
    }

    /// Focus a field. Returns false if the element isn't a text field.
    pub fn focus(&mut self, node_id: usize) -> bool {
        if !self.form_values.contains_key(&node_id) {
            return false;
        }
        self.focused = Some(node_id);
        self.focus_baseline = self.form_values.get(&node_id).cloned().unwrap_or_default();
        self.refresh_fields();
        true
    }

    /// Drop focus, firing `change` if the value actually moved while focused.
    pub fn blur(&mut self) {
        let Some(id) = self.focused.take() else {
            return;
        };
        self.refresh_fields();
        let current = self.form_values.get(&id).cloned().unwrap_or_default();
        if current != self.focus_baseline {
            self.dispatch_event(id, "change");
        }
    }

    pub fn is_focused(&self) -> bool {
        self.focused.is_some()
    }

    /// Put the cursor over an element, so `:hover` rules apply to it and to
    /// everything it sits inside. Returns whether anything changed, so the
    /// embedder only re-renders when it must.
    pub fn set_hover(&mut self, node_id: Option<usize>) -> bool {
        let chain = match node_id {
            Some(id) => ancestor_chain(&self.root, id),
            None => Default::default(),
        };
        let changed = chain != self.hovered;
        self.hovered = chain;
        changed
    }

    /// Highlight every occurrence of `query` on the next render; `None` clears it.
    pub fn set_find(&mut self, query: Option<String>) {
        self.find = query.filter(|q| !q.is_empty());
    }

    /// Which field has focus, so the embedder can act on it (submit, say).
    pub fn focused_node(&self) -> Option<usize> {
        self.focused
    }

    /// Append typed text to the focused field.
    pub fn insert_text(&mut self, text: &str) -> bool {
        let Some(id) = self.focused else { return false };
        self.form_values.entry(id).or_default().push_str(text);
        self.refresh_fields();
        self.dispatch_event(id, "input");
        true
    }

    pub fn backspace(&mut self) -> bool {
        let Some(id) = self.focused else { return false };
        self.form_values.entry(id).or_default().pop();
        self.refresh_fields();
        self.dispatch_event(id, "input");
        true
    }

    /// Re-render field text into the DOM and refresh what scripts can see.
    fn refresh_fields(&mut self) {
        self.sync_form_fields();
        self.interp.set_dom(build_dom_view(&self.root));
    }

    /// The current text of a field, for the embedder or tests.
    pub fn field_value(&self, node_id: usize) -> Option<&str> {
        self.form_values.get(&node_id).map(String::as_str)
    }

    /// Submit the form containing `node_id`, as pressing Enter in a field does.
    ///
    /// Returns where to navigate; the embedder owns the actual navigation since
    /// only it knows about URLs and the network.
    ///
    /// ponytail: GET only, which is what search boxes use. POST needs a request
    /// body the [`ResourceLoader`] cannot express yet.
    pub fn submit(&self, node_id: usize) -> Option<Submission> {
        let form = find_form(&self.root, node_id)?;
        let element = match &form.node_type {
            NodeType::Element(e) => e,
            NodeType::Text(_) => return None,
        };
        if element
            .attributes
            .get("method")
            .is_some_and(|m| m.eq_ignore_ascii_case("post"))
        {
            return None;
        }
        let mut fields = Vec::new();
        collect_fields(form, &self.form_values, &self.selections, &mut fields);
        Some(Submission {
            action: element
                .attributes
                .get("action")
                .cloned()
                .unwrap_or_default(),
            query: fields
                .iter()
                .map(|(k, v)| format!("{}={}", percent_encode(k), percent_encode(v)))
                .collect::<Vec<_>>()
                .join("&"),
        })
    }

    fn run_initial_scripts(&mut self) {
        let mut source = String::new();
        collect_script_text(&self.root, &mut source);
        if !source.trim().is_empty() {
            match js::lexer::tokenize(&source).and_then(js::parser::parse) {
                Ok(program) => self.interp.run(&program),
                Err(e) => self.interp.out.errors.push(format!("SyntaxError: {e}")),
            }
        }
        self.absorb_script_output();
        // Zero-delay timers are a common "run after load" idiom, so drain once.
        self.run_timers();
    }

    /// Fire an element's click handler, applying whatever it changed.
    /// Returns true if a handler ran (so the embedder knows to repaint).
    pub fn click(&mut self, node_id: usize) -> bool {
        self.dispatch_event(node_id, "click")
    }

    /// Run a handler and apply everything it changed.
    fn dispatch_event(&mut self, node_id: usize, event: &str) -> bool {
        if !self.interp.dispatch(node_id, event) {
            return false;
        }
        self.absorb_script_output();
        self.run_timers();
        true
    }

    /// Run any callbacks queued with `setTimeout`, applying what they changed.
    /// Returns true if anything ran, so the embedder knows to repaint.
    pub fn run_timers(&mut self) -> bool {
        if !self.interp.run_timers() {
            return false;
        }
        self.absorb_script_output();
        true
    }

    /// Apply pending DOM writes/document.write output and refresh the script's view.
    fn absorb_script_output(&mut self) {
        let out = std::mem::take(&mut self.interp.out);
        self.console.extend(out.console);
        self.console
            .extend(out.errors.iter().map(|e| format!("[error] {e}")));

        apply_mutations(&mut self.root, &out.mutations);
        if !out.writes.trim().is_empty() {
            let written = html::parse(format!("<div>{}</div>", out.writes));
            append_to_body(&mut self.root, written);
        }
        // New nodes need identities; existing ones keep theirs so handlers stay valid.
        self.assign_node_ids();
        // Scripts may have set .value or added fields. These come from `out`,
        // which already took everything off the interpreter.
        for (id, text) in out.field_writes {
            self.form_values.insert(id, text);
        }
        self.seed_form_values();
        self.sync_form_fields();
        self.interp.set_dom(build_dom_view(&self.root));
    }

    /// The document's readable text, with script/style/head content excluded.
    ///
    /// This is the sanitized view an assistant or reader mode should consume —
    /// never raw markup, and never the contents of `<script>`.
    pub fn page_text(&self) -> String {
        fn walk(node: &Node, out: &mut String) {
            match node.node_type {
                NodeType::Text(ref t) => {
                    let t = t.trim();
                    if !t.is_empty() {
                        out.push_str(t);
                        out.push(' ');
                    }
                }
                NodeType::Element(ref e) => {
                    // `nav` is navigation chrome, not readable content.
                    if matches!(
                        e.tag_name.as_str(),
                        "script" | "style" | "head" | "noscript" | "nav"
                    ) {
                        return;
                    }
                    for child in &node.children {
                        walk(child, out);
                    }
                }
            }
        }
        let mut text = String::new();
        walk(&self.root, &mut text);
        text.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    /// Headings in document order, as (level, text) — a structural outline.
    /// The document's `<title>`, collapsed to one line. Empty when the page has
    /// none, so the embedder can fall back to the URL.
    pub fn title(&self) -> String {
        fn find(node: &Node) -> Option<String> {
            if let NodeType::Element(ref e) = node.node_type {
                if e.tag_name == "title" {
                    return Some(text_content(node));
                }
            }
            node.children.iter().find_map(find)
        }
        find(&self.root)
            .unwrap_or_default()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    }

    pub fn headings(&self) -> Vec<(u8, String)> {
        fn walk(node: &Node, out: &mut Vec<(u8, String)>) {
            if let NodeType::Element(ref e) = node.node_type {
                if let Some(level) = e
                    .tag_name
                    .strip_prefix('h')
                    .and_then(|d| d.parse::<u8>().ok())
                {
                    if (1..=6).contains(&level) {
                        let text = text_content(node)
                            .split_whitespace()
                            .collect::<Vec<_>>()
                            .join(" ");
                        if !text.is_empty() {
                            out.push((level, text));
                        }
                    }
                }
            }
            for child in &node.children {
                walk(child, out);
            }
        }
        let mut out = Vec::new();
        walk(&self.root, &mut out);
        out
    }

    /// Text content of an element, by node id. Mainly useful for tests/inspection.
    pub fn text_of(&self, node_id: usize) -> String {
        fn find(node: &Node, node_id: usize) -> Option<&Node> {
            if let NodeType::Element(ref e) = node.node_type {
                if e.node_id == node_id {
                    return Some(node);
                }
            }
            node.children.iter().find_map(|c| find(c, node_id))
        }
        find(&self.root, node_id)
            .map(text_content)
            .unwrap_or_default()
    }

    fn assign_node_ids(&mut self) {
        // A script that appended a row changed what `:last-child` means for the
        // row before it, so positions are restamped alongside the new ids.
        dom::stamp_positions(&mut self.root);
        fn walk(node: &mut Node, next: &mut usize) {
            if let NodeType::Element(ref mut e) = node.node_type {
                if e.node_id == 0 {
                    e.node_id = *next;
                    *next += 1;
                }
            }
            for child in &mut node.children {
                walk(child, next);
            }
        }
        walk(&mut self.root, &mut self.next_node_id);
    }
}

/// Minimal user-agent stylesheet: gives real documents sane default display
/// (block for structural tags, none for head/script/style) so they lay out.
const USER_AGENT_CSS: &str = "
    html, body, div, p, h1, h2, h3, h4, h5, h6, ul, ol, li, section, article,
    header, footer, nav, main, aside, figure, figcaption, blockquote, pre,
    tr, form, fieldset, address, hr, img, input, textarea, button, select,
    label, tbody, thead, tfoot, center, dl, dt, dd, caption, details, summary,
    legend, menu, dir { display: block; }
    table { display: table; }
    pre { white-space: pre; }
    td, th { display: block; padding: 6px; }
    input, textarea, select { background: #ffffff; color: #111111; padding: 7px;
        border-radius: 4px; width: 260px; }
    button { background: #e6e8ec; color: #111111; padding: 8px; border-radius: 4px;
        width: 160px; }
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
        Ok(Engine {
            fonts: vec![LoadedFont {
                raster,
                bytes: font_bytes.to_vec(),
            }],
        })
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
        self.render_page(html_source, css_source, width, height, &NullLoader)
            .canvas
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
        let mut doc = Document::load(html_source, css_source);
        self.render_document(&mut doc, width, height, loader)
    }

    /// Render a [`Document`], which keeps its JavaScript runtime alive between frames.
    pub fn render_document(
        &self,
        doc: &mut Document,
        width: f32,
        height: f32,
        loader: &dyn ResourceLoader,
    ) -> Page {
        let root = &doc.root;
        let console = std::mem::take(&mut doc.console);
        let css_source = doc.css.clone();
        let find = doc.find.clone();
        let css_source = css_source.as_str();

        // Cascade order (later wins on ties):
        // UA stylesheet < linked stylesheets < page <style> < caller CSS.
        let mut source = USER_AGENT_CSS.to_string();
        collect_linked_css(root, loader, &mut source);
        source.push('\n');
        collect_style_text(root, &mut source);
        source.push('\n');
        source.push_str(css_source);

        // Parsing a real site's CSS costs more than styling, layout and paint
        // together, and it is the same text on every render — so reuse it unless
        // the text or the viewport width (which decides @media) has changed.
        let key = (hash_of(&source), width.to_bits());
        if doc.sheet.as_ref().map(|(cached, ..)| *cached) != Some(key) {
            let mut parsed = css::parse(source);
            parsed.rules.retain(|rule| css::media_matches(rule.media.as_deref(), width));
            let index = style::RuleIndex::build(&parsed);
            doc.sheet = Some((key, parsed, index));
        }
        let (_, stylesheet, rule_index) = doc.sheet.as_ref().expect("just cached");

        // Whether any rule cares about the cursor, so the embedder can skip
        // re-rendering on mouse movement when nothing would change.
        let uses_hover = stylesheet
            .rules
            .iter()
            .any(|rule| {
                rule.selectors.iter().any(|s| {
                    s.parts
                        .iter()
                        .any(|p| p.simple.pseudos.contains(&css::Pseudo::Hover))
                })
            });
        let style_root = style::style_tree_indexed(root, stylesheet, rule_index, &doc.hovered);

        // Fetch + decode every <img> up front so layout knows their sizes.
        let mut images = ImageMap::new();
        collect_and_load_images(root, loader, &mut images);

        let mut viewport: layout::Dimensions = Default::default();
        viewport.content.width = width;
        viewport.content.height = height;

        // Shaping faces are built per render; they borrow the stored font bytes.
        let faces: Vec<Option<rustybuzz::Face>> = self
            .fonts
            .iter()
            .map(|f| rustybuzz::Face::from_slice(&f.bytes, 0))
            .collect();
        let entries: Vec<FontEntry> = self
            .fonts
            .iter()
            .zip(faces.iter())
            .filter_map(|(f, face)| {
                face.as_ref().map(|shaper| FontEntry {
                    raster: &f.raster,
                    shaper,
                })
            })
            .collect();
        let fonts = if entries.is_empty() {
            None
        } else {
            Some(FontSet { entries })
        };

        let layout_root = layout::layout_tree(&style_root, viewport, fonts.as_ref(), &images);
        // Canvas is at least the viewport, but grows to the full document height so
        // the embedder can scroll through overflow.
        let doc_height = layout::content_bottom(&layout_root).max(height);
        let bounds = layout::Rect {
            x: 0.0,
            y: 0.0,
            width,
            height: doc_height,
        };
        let (canvas, find_matches) = paint::paint(
            &layout_root,
            bounds,
            fonts.as_ref(),
            &images,
            find.as_deref(),
        );

        let mut links = Vec::new();
        layout::collect_links(&layout_root, &mut links);
        let mut element_rects = Vec::new();
        layout::collect_element_rects(&layout_root, &mut element_rects);
        Page {
            canvas,
            links,
            console,
            element_rects,
            find_matches,
            uses_hover,
        }
    }
}

/// Elements whose text content is a user-editable value.
pub fn is_text_field(tag: &str) -> bool {
    matches!(tag, "input" | "textarea")
}

/// Find every `<img src>`, ask the loader for its bytes, and decode it.
/// Fetch every `<link rel="stylesheet">`, in document order so later sheets win.
///
/// A sheet that fails to load is skipped: a missing stylesheet should degrade the
/// page's appearance, never stop it rendering.
///
/// ponytail: `@import` inside a fetched sheet is not followed, so a site that
/// splits its CSS that way still renders under-styled. One more pass over the
/// parsed sheet would fix it.
fn collect_linked_css(node: &Node, loader: &dyn ResourceLoader, out: &mut String) {
    let mut hrefs = Vec::new();
    collect_stylesheet_hrefs(node, &mut hrefs);
    for bytes in loader.load_all(&hrefs).into_iter().flatten() {
        if let Ok(text) = String::from_utf8(bytes) {
            out.push('\n');
            expand_imports(&text, loader, out, 0);
        }
    }
}

/// Append a sheet, with anything it `@import`s placed ahead of it.
///
/// An `@import` sits at the top of a sheet and its rules cascade *before* the
/// importing sheet's own, so order matters here.
///
/// ponytail: relative URLs resolve against the page, not the importing sheet,
/// because the engine never learns a sheet's own URL. Absolute and root-relative
/// imports (the common case) are right; a relative one simply fails to load.
fn expand_imports(text: &str, loader: &dyn ResourceLoader, out: &mut String, depth: usize) {
    const MAX_DEPTH: usize = 4; // a sheet importing itself must not spin forever
    let urls = import_urls(text);
    if depth < MAX_DEPTH && !urls.is_empty() {
        for bytes in loader.load_all(&urls).into_iter().flatten() {
            if let Ok(imported) = String::from_utf8(bytes) {
                expand_imports(&imported, loader, out, depth + 1);
                out.push('\n');
            }
        }
    }
    out.push_str(text);
}

/// URLs from `@import "a.css";` and `@import url(a.css) screen;`.
fn import_urls(text: &str) -> Vec<String> {
    let mut urls = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("@import") {
        rest = &rest[start + "@import".len()..];
        // The statement ends at the first `;`, and media conditions may follow.
        let Some(end) = rest.find(';') else { break };
        let (statement, tail) = rest.split_at(end);
        rest = tail;
        let statement = statement.trim();
        // `url(...)` is optional around the URL, and media conditions may follow
        // it — so a quoted URL ends at its closing quote, not at the statement.
        let inner = match statement.strip_prefix("url(") {
            Some(after) => after.split(')').next().unwrap_or("").trim(),
            None => statement,
        };
        let url = match inner.chars().next() {
            Some(quote @ ('"' | '\'')) => inner[1..].split(quote).next().unwrap_or(""),
            _ => inner.split_whitespace().next().unwrap_or(""),
        }
        .trim();
        if !url.is_empty() && !url.starts_with("data:") {
            urls.push(url.to_string());
        }
    }
    urls
}

fn collect_stylesheet_hrefs(node: &Node, out: &mut Vec<String>) {
    if let NodeType::Element(ref e) = node.node_type {
        if e.tag_name == "link" && is_stylesheet(e) {
            if let Some(href) = e.attributes.get("href").filter(|h| !h.is_empty()) {
                out.push(href.clone());
            }
        }
    }
    for child in &node.children {
        collect_stylesheet_hrefs(child, out);
    }
}

/// `rel` is a space-separated token list, and matching is case-insensitive.
fn is_stylesheet(element: &dom::ElementData) -> bool {
    element.attributes.get("rel").is_some_and(|rel| {
        rel.split_whitespace()
            .any(|t| t.eq_ignore_ascii_case("stylesheet"))
    })
}

/// Sources are gathered first and requested in one batch, so an embedder can
/// fetch them concurrently instead of one blocking round trip at a time.
fn collect_and_load_images(node: &Node, loader: &dyn ResourceLoader, out: &mut ImageMap) {
    let mut srcs = Vec::new();
    collect_image_srcs(node, &mut srcs);
    for (src, bytes) in srcs.iter().zip(loader.load_all(&srcs)) {
        if let Some(img) = bytes.and_then(|b| resource::decode_image(&b)) {
            out.insert(src.clone(), img);
        }
    }
}

/// Every distinct `<img src>`, in document order.
fn collect_image_srcs(node: &Node, out: &mut Vec<String>) {
    if let NodeType::Element(ref e) = node.node_type {
        if e.tag_name == "img" {
            if let Some(src) = e.attributes.get("src").filter(|s| !s.is_empty()) {
                if !out.contains(src) {
                    out.push(src.clone());
                }
            }
        }
    }
    for child in &node.children {
        collect_image_srcs(child, out);
    }
}

/// Snapshot every element so scripts can address them by handle.
fn build_dom_view(root: &Node) -> js::DomView {
    fn walk(node: &Node, path: &mut Vec<usize>, out: &mut Vec<js::ElementInfo>) {
        if let NodeType::Element(ref e) = node.node_type {
            out.push(js::ElementInfo {
                path: path.clone(),
                node_id: e.node_id,
                id: e.id().cloned().unwrap_or_default(),
                class: e.attributes.get("class").cloned().unwrap_or_default(),
                tag: e.tag_name.clone(),
                text: text_content(node),
                attributes: e.attributes.clone(),
                pos: e.pos,
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
        // Restyling only touches an attribute, so handle it before the child swaps.
        if let js::Mutation::SetClass(i, class) = mutation {
            if let Some(path) = view.elements.get(*i).map(|e| e.path.clone()) {
                if let Some(node) = node_at(root, &path) {
                    if let NodeType::Element(ref mut e) = node.node_type {
                        e.attributes.insert("class".to_string(), class.clone());
                    }
                }
            }
            continue;
        }
        let (index, replacement) = match mutation {
            js::Mutation::SetText(i, text) => (*i, vec![dom::text(text.clone())]),
            js::Mutation::SetHtml(i, html) => {
                (*i, html::parse(format!("<div>{html}</div>")).children)
            }
            js::Mutation::SetClass(..) => continue, // handled above
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
/// A cheap content hash, to notice when a page's CSS actually changes.
fn hash_of(text: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

/// A node and every element it sits inside, by id.
fn ancestor_chain(root: &Node, node_id: usize) -> style::HoverChain {
    fn walk(node: &Node, node_id: usize, path: &mut Vec<usize>) -> bool {
        if let NodeType::Element(ref e) = node.node_type {
            path.push(e.node_id);
            if e.node_id == node_id {
                return true;
            }
        }
        for child in &node.children {
            if walk(child, node_id, path) {
                return true;
            }
        }
        if matches!(node.node_type, NodeType::Element(_)) {
            path.pop();
        }
        false
    }
    let mut path = Vec::new();
    match walk(root, node_id, &mut path) {
        true => path.into_iter().collect(),
        false => Default::default(),
    }
}

/// Where a submitted form wants to go. The action is whatever the markup said,
/// so the embedder still has to resolve it against the page URL.
#[derive(Debug, PartialEq)]
pub struct Submission {
    pub action: String,
    pub query: String,
}

/// The nearest `<form>` ancestor of `node_id`, if any. Walks down the path to
/// the field, remembering the last form seen, so nested forms resolve inward.
fn find_form(root: &Node, node_id: usize) -> Option<&Node> {
    let is_form = |n: &Node| matches!(&n.node_type, NodeType::Element(e) if e.tag_name == "form");
    let mut best = None;
    let mut node = root;
    loop {
        if is_form(node) {
            best = Some(node);
        }
        match node.children.iter().find(|c| contains(c, node_id)) {
            Some(child) => node = child,
            None => return best,
        }
    }
}

/// Whether `node_id` is this node or below it.
fn contains(node: &Node, node_id: usize) -> bool {
    match &node.node_type {
        NodeType::Element(e) if e.node_id == node_id => true,
        _ => node.children.iter().any(|c| contains(c, node_id)),
    }
}

/// Successful controls, in document order: named fields with a value.
fn collect_fields(
    node: &Node,
    values: &HashMap<usize, String>,
    selections: &HashMap<usize, (String, String)>,
    out: &mut Vec<(String, String)>,
) {
    for child in &node.children {
        if let NodeType::Element(e) = &child.node_type {
            if let Some(name) = e.attributes.get("name").filter(|n| !n.is_empty()) {
                let value = values
                    .get(&e.node_id)
                    .cloned()
                    .or_else(|| selections.get(&e.node_id).map(|(_, value)| value.clone()))
                    .or_else(|| e.attributes.get("value").cloned())
                    .unwrap_or_default();
                out.push((name.clone(), value));
            }
        }
        collect_fields(child, values, selections, out);
    }
}

/// Percent-encode a query component. Unreserved characters pass through and
/// spaces become `+`, matching how browsers encode form data.
pub fn percent_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

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
    use super::{Node, NodeType};

    /// Node ids of every element with this tag, in document order.
    fn ids_of(node: &Node, tag: &str, out: &mut Vec<usize>) {
        if let NodeType::Element(e) = &node.node_type {
            if e.tag_name == tag {
                out.push(e.node_id);
            }
        }
        node.children.iter().for_each(|c| ids_of(c, tag, out));
    }

    fn first_input(doc: &super::Document) -> usize {
        let mut ids = Vec::new();
        ids_of(&doc.root, "input", &mut ids);
        ids[0]
    }

    #[test]
    fn render_produces_correctly_sized_canvas() {
        let engine = super::Engine::shapes_only();
        let canvas = engine.render("<div></div>", "div { background: #112233; }", 40.0, 30.0);
        assert_eq!((canvas.width, canvas.height), (40, 30));
        assert_eq!(canvas.pixels.len(), 40 * 30);
    }

    #[test]
    fn overflow_clips_and_visibility_hides() {
        let engine = super::Engine::shapes_only();
        let canvas = engine.render(
            "<body><div id=\"clip\"><div id=\"big\"></div></div>\
             <div id=\"ghost\"></div></body>",
            "#clip { width: 40px; height: 10px; overflow: hidden; }
             #big { width: 40px; height: 100px; background: #ff0000; }
             #ghost { width: 40px; height: 10px; background: #00ff00;
                      visibility: hidden; }",
            60.0,
            60.0,
        );
        let at = |x: usize, y: usize| canvas.pixels[y * canvas.width + x];
        let rgb = |c: crate::css::Color| (c.r, c.g, c.b);
        const WHITE: (u8, u8, u8) = (255, 255, 255);

        // The tall child paints inside its clipping parent...
        assert_eq!(rgb(at(5, 5)), (255, 0, 0));
        // ...and nowhere below it, though the box itself is 100px tall.
        assert_eq!(rgb(at(5, 12)), WHITE);
        assert_eq!(rgb(at(5, 50)), WHITE);
        // A hidden box leaves no colour where it sits (y 10..20).
        assert_eq!(rgb(at(5, 15)), WHITE);
    }

    /// A search box: type into a field nested inside the form, press Enter.
    #[test]
    fn submitting_a_field_builds_a_get_query() {
        let mut doc = super::Document::load(
            "<form action=\"/search\"><div>\
             <input name=\"q\"><input type=\"hidden\" name=\"lang\" value=\"hi\">\
             </div></form>",
            "",
        );
        let field = first_input(&doc);
        assert!(doc.focus(field));
        doc.insert_text("zero browser");

        let sent = doc.submit(field).expect("form found from a nested field");
        assert_eq!(sent.action, "/search");
        // Spaces become `+`, and hidden fields are submitted too.
        assert_eq!(sent.query, "q=zero+browser&lang=hi");
    }

    /// A dropdown shows its choice, and submits that option's value.
    #[test]
    fn a_select_collapses_to_its_selected_option() {
        let doc = super::Document::load(
            "<form action=\"/s\"><input name=\"q\" value=\"x\">\
             <select name=\"region\">\
             <option value=\"us\">United States</option>\
             <option value=\"in\" selected>India (en)</option>\
             </select></form>",
            "",
        );
        // The rendered text is the one choice, not every option.
        let text = doc.page_text();
        assert!(text.contains("India (en)"), "shows the selection: {text:?}");
        assert!(!text.contains("United States"), "hides the rest: {text:?}");

        let field = first_input(&doc);
        assert_eq!(doc.submit(field).unwrap().query, "q=x&region=in");
    }

    /// Hover crosses parsing, styling and paint, so check the pixels.
    #[test]
    fn hovering_an_element_repaints_it() {
        let engine = super::Engine::shapes_only();
        let html = "<body><div id=\"box\">x</div></body>";
        let css = "div { background: #0000ff; height: 50px; }                    div:hover { background: #ff0000; }";
        let loader = super::NullLoader;

        let mut doc = super::Document::load(html, css);
        let page = engine.render_document(&mut doc, 100.0, 100.0, &loader);
        assert!(page.uses_hover, "the sheet reacts to the cursor");
        let blue = page.canvas.pixels[10 * 100 + 10];
        assert_eq!((blue.r, blue.g, blue.b), (0, 0, 255));

        let box_id = page
            .element_rects
            .iter()
            .find(|r| r.id == "box")
            .expect("the div was painted")
            .node_id;
        assert!(doc.set_hover(Some(box_id)), "hovering is a change");
        let page = engine.render_document(&mut doc, 100.0, 100.0, &loader);
        let red = page.canvas.pixels[10 * 100 + 10];
        assert_eq!((red.r, red.g, red.b), (255, 0, 0));

        // Moving off the element puts it back, and re-hovering the same node
        // reports no change so the embedder can skip the repaint.
        assert!(doc.set_hover(None));
        assert!(doc.set_hover(Some(box_id)));
        assert!(!doc.set_hover(Some(box_id)));
    }

    #[test]
    fn import_statements_are_found_in_every_spelling() {
        let urls = super::import_urls(
            "@import \"base.css\";
             @import url(theme.css);
             @import url('print.css') print;
             @import   \"/abs/spaced.css\"  screen and (min-width: 40em);
             .rule { color: red }",
        );
        assert_eq!(urls, ["base.css", "theme.css", "print.css", "/abs/spaced.css"]);
        // Nothing to import, and a data: sheet is not worth a fetch.
        assert!(super::import_urls("body { color: red }").is_empty());
        assert!(super::import_urls("@import url(data:text/css,body{});").is_empty());
    }

    /// Imported rules must cascade before the importing sheet's own.
    #[test]
    fn imports_are_placed_ahead_of_the_sheet_that_asked_for_them() {
        struct Sheets;
        impl super::ResourceLoader for Sheets {
            fn load(&self, url: &str) -> Option<Vec<u8>> {
                match url {
                    "base.css" => Some(b"p { color: #ff0000 }".to_vec()),
                    _ => None,
                }
            }
        }
        let mut out = String::new();
        super::expand_imports("@import \"base.css\";
p { color: #0000ff }", &Sheets, &mut out, 0);
        let base = out.find("#ff0000").expect("imported rule");
        let own = out.find("#0000ff").expect("the sheet's own rule");
        assert!(base < own, "the import must come first: {out:?}");
    }

    #[test]
    fn the_document_title_is_one_tidy_line() {
        let doc = super::Document::load(
            "<html><head><title>
  Rust (programming language)
  - Wikipedia
</title></head>             <body><title>not this one</title></body></html>",
            "",
        );
        // Whitespace collapses, and the first title wins.
        assert_eq!(doc.title(), "Rust (programming language) - Wikipedia");
        // A page with no title reports nothing, so the embedder can fall back.
        assert_eq!(super::Document::load("<p>hi</p>", "").title(), "");
    }

    #[test]
    fn fields_outside_a_form_do_not_submit() {
        let mut doc = super::Document::load("<input name=\"q\">", "");
        let field = first_input(&doc);
        assert_eq!(doc.submit(field), None);
    }

    #[test]
    fn encoding_escapes_what_would_break_a_url() {
        assert_eq!(super::percent_encode("a&b=c d/e"), "a%26b%3Dc+d%2Fe");
        assert_eq!(super::percent_encode("hindi-~_."), "hindi-~_.");
        // Non-ASCII goes out as UTF-8 bytes.
        assert_eq!(super::percent_encode("हि"), "%E0%A4%B9%E0%A4%BF");
    }
}

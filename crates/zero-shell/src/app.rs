//! The windowed browser app: window, input, tabs, navigation, and compositing.
//!
//! The chrome (vertical tab sidebar + toolbar) is itself rendered *by the engine*
//! as tiny HTML documents and composited around the page, so the shell needs no
//! text-drawing or widget code of its own.
//!
//! Window regions:
//! ```text
//!   +----------+---------------------------+
//!   | sidebar  |         toolbar           |
//!   | (tabs)   +---------------------------+
//!   |          |          page             |
//!   +----------+---------------------------+
//! ```

use crate::ai::{Assistant, LocalAssistant, PageContext};
use crate::net::{load_target, normalize_target, resolve_url, ShellLoader};
use std::num::NonZeroU32;
use std::rc::Rc;
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, KeyEvent, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::{Window, WindowId};
use zero_engine::Engine;

const SIDEBAR_W: u32 = 220;
const SIDEBAR_HEAD_H: u32 = 44; // header row height, so tab hit-testing is exact
const TAB_ROW_H: u32 = 40; // padding(10*2) + height(20)
const TOOLBAR_H: u32 = 48;
const AI_PANEL_W: u32 = 320;

const TOOLBAR_CSS: &str =
    "body{background:#1f2127;color:#f2f3f5;font-size:16px;} #bar{padding:14px;height:20px;}";

const AI_CSS: &str = "body{background:#141519;color:#c9ccd3;font-size:14px;}      #head{background:#26282f;color:#f2f3f5;padding:12px;height:20px;}      .line{padding:2px;} .src{color:#6b7280;padding:10px;}";

const NEW_TAB_HTML: &str = "<html><head><style>\
    body{background:#0e0f12;color:#f2f3f5;padding:48px;font-size:18px;}\
    h1{color:#e5484d;font-size:40px;}\
    </style></head><body><h1>Zero</h1>\
    <p>Type a URL above and press Enter.</p></body></html>";

fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// A short label for a tab: the host for URLs, the file name for paths.
fn tab_title(address: &str) -> String {
    if address.is_empty() {
        return "New Tab".to_string();
    }
    let title = match address.split("://").nth(1) {
        Some(rest) => rest.split('/').next().unwrap_or(rest).to_string(),
        None => std::path::Path::new(address)
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| address.to_string()),
    };
    if title.chars().count() > 22 {
        title.chars().take(21).collect::<String>() + "..."
    } else {
        title
    }
}

/// Everything that belongs to one tab, including its own history and render cache.
struct Tab {
    address: String,
    /// Owns the parsed DOM and a live JS runtime, so handlers survive between frames.
    doc: zero_engine::Document,
    element_rects: Vec<zero_engine::ElementRect>,
    history: Vec<String>,
    history_index: usize,
    scroll_y: f32,
    secure: bool,
    blocked_count: usize,
    page_canvas: Option<zero_engine::Canvas>,
    links: Vec<zero_engine::LinkArea>,
    cache_w: u32,
    cache_h: u32,
}

impl Tab {
    fn new(address: String, html: String, css: String) -> Tab {
        Tab {
            history: vec![address.clone()],
            address,
            doc: zero_engine::Document::load(&html, &css),
            element_rects: Vec::new(),
            history_index: 0,
            scroll_y: 0.0,
            secure: true,
            blocked_count: 0,
            page_canvas: None,
            links: Vec::new(),
            cache_w: 0,
            cache_h: 0,
        }
    }

    fn blank() -> Tab {
        Tab::new(String::new(), NEW_TAB_HTML.to_string(), String::new())
    }
}

pub fn run_window(engine: Engine, html: String, css: String, address: String) {
    let event_loop = EventLoop::new().expect("failed to create event loop");
    let mut app = App {
        engine,
        tabs: vec![Tab::new(address, html, css)],
        active: 0,
        modifiers: ModifiersState::default(),
        cursor: (0.0, 0.0),
        ai_open: false,
        ai_text: String::new(),
        window: None,
        surface: None,
    };
    event_loop.run_app(&mut app).expect("event loop error");
}

struct App {
    engine: Engine,
    tabs: Vec<Tab>,
    active: usize,
    modifiers: ModifiersState,
    cursor: (f32, f32),
    ai_open: bool,
    ai_text: String,
    window: Option<Rc<Window>>,
    surface: Option<softbuffer::Surface<Rc<Window>, Rc<Window>>>,
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        let attrs = Window::default_attributes()
            .with_title("Zero Browser")
            .with_inner_size(LogicalSize::new(1100.0, 720.0));
        let window = Rc::new(event_loop.create_window(attrs).expect("failed to create window"));
        let context = softbuffer::Context::new(window.clone()).expect("softbuffer context");
        let surface =
            softbuffer::Surface::new(&context, window.clone()).expect("softbuffer surface");
        window.request_redraw();
        self.window = Some(window);
        self.surface = Some(surface);
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(_) => self.request_redraw(),
            WindowEvent::RedrawRequested => self.redraw(),
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
                self.handle_key(event);
                self.request_redraw();
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let dy = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y * 48.0,
                    MouseScrollDelta::PixelDelta(pos) => pos.y as f32,
                };
                let tab = self.tab_mut();
                tab.scroll_y = (tab.scroll_y - dy).max(0.0); // clamped to content in redraw
                self.request_redraw();
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor = (position.x as f32, position.y as f32);
            }
            WindowEvent::ModifiersChanged(m) => self.modifiers = m.state(),
            WindowEvent::MouseInput { state: ElementState::Pressed, button, .. } => {
                match button {
                    MouseButton::Left => self.handle_click(),
                    MouseButton::Back => self.back(),
                    MouseButton::Forward => self.forward(),
                    _ => {}
                }
                self.request_redraw();
            }
            _ => {}
        }
    }
}

impl App {
    fn tab(&self) -> &Tab {
        &self.tabs[self.active]
    }

    fn tab_mut(&mut self) -> &mut Tab {
        &mut self.tabs[self.active]
    }

    fn request_redraw(&self) {
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    // --- tab management ---

    fn new_tab(&mut self) {
        self.tabs.push(Tab::blank());
        self.active = self.tabs.len() - 1;
    }

    fn close_tab(&mut self) {
        self.tabs.remove(self.active);
        if self.tabs.is_empty() {
            self.tabs.push(Tab::blank()); // always keep one tab open
        }
        self.active = self.active.min(self.tabs.len() - 1);
    }

    fn next_tab(&mut self) {
        self.active = (self.active + 1) % self.tabs.len();
    }

    // --- input ---

    fn handle_key(&mut self, event: KeyEvent) {
        let ctrl = self.modifiers.control_key();
        match event.logical_key {
            Key::Character(ref c) if ctrl => match c.as_str() {
                "t" => self.new_tab(),
                "w" => self.close_tab(),
                "i" => self.toggle_assistant(),
                _ => {}
            },
            Key::Named(NamedKey::Tab) if ctrl => self.next_tab(),
            Key::Named(NamedKey::ArrowLeft) if self.modifiers.alt_key() => self.back(),
            Key::Named(NamedKey::ArrowRight) if self.modifiers.alt_key() => self.forward(),
            Key::Named(NamedKey::Enter) => self.navigate(),
            Key::Named(NamedKey::Backspace) => {
                self.tab_mut().address.pop();
            }
            Key::Named(NamedKey::ArrowDown) => self.tab_mut().scroll_y += 48.0,
            Key::Named(NamedKey::ArrowUp) => {
                let t = self.tab_mut();
                t.scroll_y = (t.scroll_y - 48.0).max(0.0);
            }
            Key::Named(NamedKey::PageDown) => self.tab_mut().scroll_y += 400.0,
            Key::Named(NamedKey::PageUp) => {
                let t = self.tab_mut();
                t.scroll_y = (t.scroll_y - 400.0).max(0.0);
            }
            Key::Named(NamedKey::Home) => self.tab_mut().scroll_y = 0.0,
            _ => {
                if let Some(text) = &event.text {
                    let chars: Vec<char> = text.chars().filter(|c| !c.is_control()).collect();
                    self.tab_mut().address.extend(chars);
                }
            }
        }
    }

    /// Route a click: sidebar switches tabs, page follows links, toolbar is inert.
    fn handle_click(&mut self) {
        let (cx, cy) = self.cursor;

        if cx < SIDEBAR_W as f32 {
            if cy >= SIDEBAR_HEAD_H as f32 {
                let row = ((cy - SIDEBAR_HEAD_H as f32) / TAB_ROW_H as f32) as usize;
                if row < self.tabs.len() {
                    self.active = row;
                }
            }
            return;
        }
        if cy < TOOLBAR_H as f32 {
            return; // toolbar clicks don't navigate
        }
        if self.ai_open {
            let panel_x0 = self.window.as_ref().map(|w| w.inner_size().width).unwrap_or(0)
                as f32
                - AI_PANEL_W as f32;
            if cx >= panel_x0 {
                return; // clicks in the assistant panel aren't page clicks
            }
        }

        // Window coords -> page coords (undo chrome offsets, add scroll).
        let px = cx - SIDEBAR_W as f32;
        let py = cy - TOOLBAR_H as f32 + self.tab().scroll_y;
        // Innermost element wins, so a handler on a child beats one on its parent.
        let hit = self
            .tab()
            .element_rects
            .iter()
            .filter(|r| px >= r.x && px <= r.x + r.width && py >= r.y && py <= r.y + r.height)
            .map(|r| r.node_id)
            .next_back();
        if let Some(node_id) = hit {
            let tab = self.tab_mut();
            if tab.doc.click(node_id) {
                tab.page_canvas = None; // handler may have changed the DOM
                self.request_redraw();
                return;
            }
        }

        let href = self
            .tab()
            .links
            .iter()
            .find(|l| px >= l.x && px <= l.x + l.width && py >= l.y && py <= l.y + l.height)
            .map(|l| l.href.clone());
        if let Some(href) = href {
            let target = resolve_url(&self.tab().address, &href);
            self.go_to(target);
            self.request_redraw();
        }
    }

    // --- navigation (per tab) ---

    fn navigate(&mut self) {
        let target = normalize_target(&self.tab().address);
        self.go_to(target);
    }

    fn go_to(&mut self, target: String) {
        {
            let tab = self.tab_mut();
            tab.history.truncate(tab.history_index + 1);
            if tab.history.last() != Some(&target) {
                tab.history.push(target.clone());
                tab.history_index = tab.history.len() - 1;
            }
        }
        self.load(target);
    }

    fn back(&mut self) {
        let target = {
            let tab = self.tab_mut();
            if tab.history_index == 0 {
                return;
            }
            tab.history_index -= 1;
            tab.history[tab.history_index].clone()
        };
        self.load(target);
    }

    fn forward(&mut self) {
        let target = {
            let tab = self.tab_mut();
            if tab.history_index + 1 >= tab.history.len() {
                return;
            }
            tab.history_index += 1;
            tab.history[tab.history_index].clone()
        };
        self.load(target);
    }

    /// Load a target into the active tab without touching history.
    fn load(&mut self, target: String) {
        let fetched = load_target(&target);
        let tab = self.tab_mut();
        // An HTTPS upgrade can change the URL, so adopt whatever actually loaded.
        tab.address = fetched.url;
        tab.secure = fetched.secure;
        // A new page means a new document and a fresh JS runtime.
        tab.doc = zero_engine::Document::load(&fetched.body, "");
        tab.scroll_y = 0.0;
        tab.page_canvas = None; // force re-render of the new page
        let title = tab.address.clone();
        if let Some(window) = &self.window {
            window.set_title(&format!("Zero Browser — {title}"));
        }
        if self.ai_open {
            self.run_assistant(); // keep the panel in sync with the new page
        }
    }

    // --- assistant ---

    fn toggle_assistant(&mut self) {
        self.ai_open = !self.ai_open;
        if self.ai_open {
            self.run_assistant();
        }
    }

    /// Build the page context and ask the assistant. Runs on-device by default.
    fn run_assistant(&mut self) {
        let tab = self.tab();
        let ctx = PageContext {
            url: tab.address.clone(),
            text: tab.doc.page_text(),
            headings: tab.doc.headings(),
            blocked_trackers: tab.blocked_count,
            secure: tab.secure,
        };
        let assistant = LocalAssistant;
        self.ai_text = format!("{}

[{}]", assistant.respond(&ctx), assistant.provenance());
    }

    fn ai_html(&self) -> String {
        let body: String = self
            .ai_text
            .lines()
            .map(|line| format!("<div class=\"line\">{}</div>", escape(line)))
            .collect();
        format!("<html><body><div id=\"head\">Assistant</div>{body}</body></html>")
    }

    // --- chrome, rendered by the engine ---

    fn toolbar_html(&self) -> String {
        let tab = self.tab();
        let lock = if tab.secure { "" } else { "Not secure  .  " };
        let shield = if tab.blocked_count > 0 {
            format!("     .     {} trackers blocked", tab.blocked_count)
        } else {
            String::new()
        };
        format!(
            "<html><body><div id=\"bar\">{lock}{}|{shield}</div></body></html>",
            escape(&tab.address)
        )
    }

    fn sidebar_html(&self) -> String {
        let rows: String = self
            .tabs
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let class = if i == self.active { "tab active" } else { "tab" };
                format!("<div class=\"{class}\">{}</div>", escape(&tab_title(&t.address)))
            })
            .collect();
        format!("<html><body><div id=\"head\">ZERO</div>{rows}</body></html>")
    }

    /// Height is injected so the sidebar background fills the window.
    fn sidebar_css(height: u32) -> String {
        format!(
            "body{{background:#141519;color:#c9ccd3;font-size:14px;height:{height}px;}} \
             #head{{color:#6b7280;padding:12px;height:20px;}} \
             .tab{{padding:10px;height:20px;}} \
             .active{{background:#26282f;color:#ffffff;}}"
        )
    }

    fn redraw(&mut self) {
        let (w, h) = match &self.window {
            Some(window) => {
                let s = window.inner_size();
                (s.width.max(1), s.height.max(1))
            }
            None => return,
        };
        let sw = SIDEBAR_W.min(w);
        // The assistant panel takes width from the content area, so the page reflows.
        let aw = if self.ai_open { AI_PANEL_W.min(w.saturating_sub(sw)) } else { 0 };
        let content_w = w.saturating_sub(sw + aw).max(1);
        let tb = TOOLBAR_H.min(h);
        let page_vh = h.saturating_sub(tb).max(1);

        // Re-render the active tab only when its page or layout size changed;
        // scrolling and tab switching just re-blit cached canvases.
        {
            let engine = &self.engine;
            let tab = &mut self.tabs[self.active];
            if tab.page_canvas.is_none() || tab.cache_w != content_w || tab.cache_h != page_vh {
                let loader = ShellLoader::new(tab.address.clone());
                let page = engine.render_document(
                    &mut tab.doc,
                    content_w as f32,
                    page_vh as f32,
                    &loader,
                );
                tab.blocked_count = loader.blocked.get();
                for line in &page.console {
                    eprintln!("[js] {line}");
                }
                tab.page_canvas = Some(page.canvas);
                tab.links = page.links;
                tab.element_rects = page.element_rects;
                tab.cache_w = content_w;
                tab.cache_h = page_vh;
            }
            // Clamp scroll to available overflow.
            let page_height = tab.page_canvas.as_ref().unwrap().height;
            let max_scroll = page_height.saturating_sub(page_vh as usize) as f32;
            tab.scroll_y = tab.scroll_y.clamp(0.0, max_scroll);
        }
        let scroll = self.tab().scroll_y as usize;

        // Chrome is cheap; render fresh each frame so typing/tab changes show.
        let sidebar =
            self.engine.render(&self.sidebar_html(), &Self::sidebar_css(h), sw as f32, h as f32);
        let toolbar =
            self.engine.render(&self.toolbar_html(), TOOLBAR_CSS, content_w as f32, tb as f32);
        let ai_panel = if aw > 0 {
            Some(self.engine.render(&self.ai_html(), AI_CSS, aw as f32, h as f32))
        } else {
            None
        };

        // Disjoint field borrows: tabs (shared) + surface (mut).
        let page = self.tabs[self.active].page_canvas.as_ref().unwrap();
        let surface = match self.surface.as_mut() {
            Some(s) => s,
            None => return,
        };
        surface
            .resize(NonZeroU32::new(w).unwrap(), NonZeroU32::new(h).unwrap())
            .expect("surface resize");
        let mut buffer = surface.buffer_mut().expect("surface buffer");

        let (w, sw, tb, aw) = (w as usize, sw as usize, tb as usize, aw as usize);
        let ai_x0 = w - aw; // panel occupies the right edge
        for y in 0..h as usize {
            for x in 0..w {
                let px = if x < sw {
                    sidebar.pixels[y * sidebar.width + x]
                } else if aw > 0 && x >= ai_x0 {
                    let panel = ai_panel.as_ref().unwrap();
                    panel.pixels[y * panel.width + (x - ai_x0)]
                } else if y < tb {
                    toolbar.pixels[y * toolbar.width + (x - sw)]
                } else {
                    let py = (y - tb) + scroll;
                    page.pixels[py * page.width + (x - sw)]
                };
                buffer[y * w + x] = (px.r as u32) << 16 | (px.g as u32) << 8 | px.b as u32;
            }
        }
        buffer.present().expect("buffer present");
    }
}

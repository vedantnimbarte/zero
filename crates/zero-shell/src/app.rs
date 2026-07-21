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
use crate::storage;
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
const SCROLLBAR_W: u32 = 12;

const TOOLBAR_CSS: &str = "body{background:#1f2127;color:#f2f3f5;font-size:15px;} \
     #bar{padding:9px;height:30px;} \
     .btn{display:inline-block;background:#2b2e37;color:#f2f3f5;width:30px;padding:7px;\
          border-radius:6px;} \
     .off{display:inline-block;background:#24262d;color:#5f636d;width:30px;padding:7px;\
          border-radius:6px;} \
     .on{display:inline-block;background:#f5a524;color:#241a00;width:30px;padding:7px;\
         border-radius:6px;} \
     .addr{display:inline-block;background:#141519;color:#f2f3f5;padding:7px;\
           border-radius:6px;}";

const AI_CSS: &str = "body{background:#141519;color:#c9ccd3;font-size:14px;}      #head{background:#26282f;color:#f2f3f5;padding:12px;height:20px;}      .line{padding:2px;} .src{color:#6b7280;padding:10px;}";

/// Where the scrollbar thumb sits within the page area, as (offset, height).
/// `None` when the content fits and no scrollbar is warranted.
fn scrollbar_thumb(content: f32, viewport: f32, scroll: f32) -> Option<(f32, f32)> {
    if content <= viewport || viewport <= 0.0 {
        return None;
    }
    // Thumb length reflects the visible fraction, with a floor so it stays grabbable.
    let thumb = (viewport * viewport / content).clamp(24.0_f32.min(viewport), viewport);
    let travel = (viewport - thumb).max(0.0);
    let progress = (scroll / (content - viewport)).clamp(0.0, 1.0);
    Some((travel * progress, thumb))
}

/// Scroll offset for a cursor at `y` within the page area, centring the thumb.
fn scroll_for_cursor(content: f32, viewport: f32, y: f32) -> f32 {
    let Some((_, thumb)) = scrollbar_thumb(content, viewport, 0.0) else { return 0.0 };
    let travel = (viewport - thumb).max(1.0);
    let ratio = ((y - thumb / 2.0) / travel).clamp(0.0, 1.0);
    ratio * (content - viewport)
}

/// Where a submitted form navigates to, given the page it was submitted from.
fn submission_url(address: &str, sent: &zero_engine::Submission) -> String {
    // An empty action means "this page", whose own query the new one replaces.
    let base = match sent.action.is_empty() {
        true => address.split('?').next().unwrap_or(address).to_string(),
        false => resolve_url(address, &sent.action),
    };
    match sent.query.is_empty() {
        true => base,
        // An action may already carry a query string of its own.
        false if base.contains('?') => format!("{base}&{}", sent.query),
        false => format!("{base}?{}", sent.query),
    }
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// The storage partition for a target: its site for URLs, the file name for local
/// pages, so two local examples don't share one bucket.
fn storage_site(address: &str) -> String {
    let site = crate::cookies::site_of(address);
    if site.is_empty() {
        tab_title(address)
    } else {
        site
    }
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
    /// Find-in-page match boxes from the last render, for jumping between them.
    matches: Vec<zero_engine::layout::Rect>,
    /// Shared with this tab's document so its subresource cache outlives a
    /// single render — the engine re-asks for images and stylesheets each time.
    loader: Rc<ShellLoader>,
    cache_w: u32,
    cache_h: u32,
}

impl Tab {
    fn new(address: String, html: String, css: String) -> Tab {
        let loader = Rc::new(ShellLoader::new(address.clone()));
        let doc = zero_engine::Document::load_hosted(
            &html,
            &css,
            Some(loader.clone()),
            Some(Rc::new(crate::localstore::SiteStore::for_site(&storage_site(&address)))),
        );
        Tab {
            loader,
            history: vec![address.clone()],
            address,
            doc,
            element_rects: Vec::new(),
            history_index: 0,
            scroll_y: 0.0,
            secure: true,
            blocked_count: 0,
            page_canvas: None,
            links: Vec::new(),
            matches: Vec::new(),
            cache_w: 0,
            cache_h: 0,
        }
    }

    fn blank() -> Tab {
        let address = "zero://newtab".to_string();
        let mut tab = Tab::new(address.clone(), crate::internal::page(&address), String::new());
        tab.address = address; // shown in the bar, and reloadable like any page
        tab
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
        toolbar_rects: Vec::new(),
        find: None,
        dragging_scrollbar: false,
        window: None,
        surface: None,
    };
    event_loop.run_app(&mut app).expect("event loop error");
}

/// Reopen the tabs from the previous session, if any were saved.
pub fn run_window_restoring_session(engine: Engine) -> bool {
    let Some((urls, active)) = storage::load_session() else { return false };
    let event_loop = EventLoop::new().expect("failed to create event loop");
    let tabs: Vec<Tab> = urls
        .iter()
        .map(|url| {
            let fetched = load_target(url);
            let loader = Rc::new(ShellLoader::new(fetched.url.clone()));
            let mut tab = Tab::new(fetched.url.clone(), String::new(), String::new());
            tab.doc = zero_engine::Document::load_with(&fetched.body, "", loader);
            tab.secure = fetched.secure;
            tab
        })
        .collect();
    let mut app = App {
        engine,
        active: active.min(tabs.len() - 1),
        tabs,
        modifiers: ModifiersState::default(),
        cursor: (0.0, 0.0),
        ai_open: false,
        ai_text: String::new(),
        toolbar_rects: Vec::new(),
        find: None,
        dragging_scrollbar: false,
        window: None,
        surface: None,
    };
    event_loop.run_app(&mut app).expect("event loop error");
    true
}

struct App {
    engine: Engine,
    tabs: Vec<Tab>,
    active: usize,
    modifiers: ModifiersState,
    cursor: (f32, f32),
    ai_open: bool,
    ai_text: String,
    /// Clickable regions of the toolbar, refreshed every frame.
    toolbar_rects: Vec<zero_engine::ElementRect>,
    /// The find-in-page query while the find bar is open. `None` when closed,
    /// so typing goes back to the address bar.
    find: Option<String>,
    dragging_scrollbar: bool,
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
                if self.dragging_scrollbar {
                    let (_, h) = self.window_size();
                    let page_top = TOOLBAR_H.min(h) as f32;
                    let page_height = (h as f32 - page_top).max(1.0);
                    self.scroll_to_cursor(self.cursor.1, page_top, page_height);
                    self.request_redraw();
                }
            }
            WindowEvent::MouseInput { state: ElementState::Released, .. } => {
                self.dragging_scrollbar = false;
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

    /// Write the open tabs to disk so the next launch can restore them.
    fn save_session(&self) {
        let urls: Vec<String> =
            self.tabs.iter().map(|t| t.address.clone()).filter(|a| !a.is_empty()).collect();
        storage::save_session(&urls, self.active.min(urls.len().saturating_sub(1)));
    }

    fn new_tab(&mut self) {
        self.tabs.push(Tab::blank());
        self.active = self.tabs.len() - 1;
        self.save_session();
    }

    fn close_tab(&mut self) {
        self.tabs.remove(self.active);
        if self.tabs.is_empty() {
            self.tabs.push(Tab::blank()); // always keep one tab open
        }
        self.active = self.active.min(self.tabs.len() - 1);
        self.save_session();
    }

    fn next_tab(&mut self) {
        self.active = (self.active + 1) % self.tabs.len();
    }

    // --- input ---

    fn handle_key(&mut self, event: KeyEvent) {
        let ctrl = self.modifiers.control_key();
        // The find bar owns typing while it is open, like the page field does.
        if !ctrl && self.find.is_some() {
            match event.logical_key {
                Key::Named(NamedKey::Escape) => self.close_find(),
                Key::Named(NamedKey::Backspace) => {
                    self.find.as_mut().expect("open").pop();
                    self.apply_find();
                }
                Key::Named(NamedKey::Enter) => self.jump_to_match(),
                _ => match &event.text {
                    Some(text) => {
                        let typed: String = text.chars().filter(|c| !c.is_control()).collect();
                        if typed.is_empty() {
                            return;
                        }
                        self.find.as_mut().expect("open").push_str(&typed);
                        self.apply_find();
                    }
                    None => return,
                },
            }
            self.request_redraw();
            return;
        }
        // A focused page field owns plain typing; chords still reach the browser.
        if !ctrl && self.tab().doc.is_focused() {
            let handled = match event.logical_key {
                Key::Named(NamedKey::Backspace) => self.tab_mut().doc.backspace(),
                Key::Named(NamedKey::Escape) => {
                    self.tab_mut().doc.blur();
                    true
                }
                Key::Named(NamedKey::Enter) => {
                    self.submit_focused_form();
                    true
                }
                _ => match &event.text {
                    Some(text) => {
                        let typed: String =
                            text.chars().filter(|c| !c.is_control()).collect();
                        !typed.is_empty() && self.tab_mut().doc.insert_text(&typed)
                    }
                    None => false,
                },
            };
            if handled {
                self.tab_mut().page_canvas = None; // field text changed
                return;
            }
        }
        match event.logical_key {
            Key::Character(ref c) if ctrl => match c.as_str() {
                "t" => self.new_tab(),
                "w" => self.close_tab(),
                "i" => self.toggle_assistant(),
                "d" => self.toggle_bookmark(),
                "f" => self.open_find(),
                "l" => self.tab_mut().address.clear(), // ready for a new address
                "h" => self.go_to("zero://history".into()),
                "b" => self.go_to("zero://bookmarks".into()),
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

    fn window_size(&self) -> (u32, u32) {
        match &self.window {
            Some(window) => {
                let s = window.inner_size();
                (s.width.max(1), s.height.max(1))
            }
            None => (1, 1),
        }
    }

    /// Route a click: sidebar switches tabs, toolbar buttons act, page follows links.
    fn handle_click(&mut self) {
        let (cx, cy) = self.cursor;
        let (w, h) = self.window_size();

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
            if self.handle_toolbar_click(cx, cy) {
                self.request_redraw();
            }
            return;
        }

        // The scrollbar sits at the right edge of the page area.
        let ai_width = if self.ai_open { AI_PANEL_W.min(w.saturating_sub(SIDEBAR_W)) } else { 0 };
        let page_right = w.saturating_sub(ai_width) as f32;
        if cx >= page_right - SCROLLBAR_W as f32 && cx < page_right {
            let page_top = TOOLBAR_H.min(h) as f32;
            self.dragging_scrollbar = true;
            self.scroll_to_cursor(cy, page_top, (h as f32 - page_top).max(1.0));
            self.request_redraw();
            return;
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
        self.tab_mut().doc.blur(); // clicking the page clears focus unless a field is hit
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
            // Focus a text field so typing goes to the page instead of the address bar.
            if tab.doc.focus(node_id) {
                tab.page_canvas = None;
                self.request_redraw();
                return;
            }
            tab.doc.blur();
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
        // A new page means a new document and a fresh JS runtime. The loader goes
        // in so page scripts can fetch relative to this URL.
        tab.loader = Rc::new(ShellLoader::new(tab.address.clone()));
        // localStorage is partitioned by site, like cookies.
        let store = Rc::new(crate::localstore::SiteStore::for_site(&storage_site(&tab.address)));
        tab.doc = zero_engine::Document::load_hosted(
            &fetched.body,
            "",
            Some(tab.loader.clone()),
            Some(store),
        );
        tab.matches.clear();
        tab.scroll_y = 0.0;
        tab.page_canvas = None; // force re-render of the new page
        let title = tab.address.clone();
        storage::record_visit(&title, &tab_title(&title));
        if let Some(window) = &self.window {
            window.set_title(&format!("Zero Browser — {title}"));
        }
        self.save_session();
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
        let lock = if tab.secure { "" } else { "Not secure - " };
        let shield = if tab.blocked_count > 0 {
            format!("  -  {} blocked", tab.blocked_count)
        } else {
            String::new()
        };
        // Disabled buttons get a dim class, so the chrome reflects real state.
        let back = if tab.history_index > 0 { "btn" } else { "off" };
        let fwd = if tab.history_index + 1 < tab.history.len() { "btn" } else { "off" };
        // A lit star means this page is already bookmarked.
        let star = if storage::is_bookmarked(&tab.address) { "on" } else { "btn" };
        // With the find bar open it replaces the address, since it owns typing.
        if let Some(query) = &self.find {
            let count = tab.matches.len();
            let hits = match (query.is_empty(), count) {
                (true, _) => String::new(),
                (false, 0) => "  -  no matches".to_string(),
                (false, n) => format!("  -  {n} matches  -  Enter for next"),
            };
            return format!(
                "<html><body><div id=\"bar\">\
                 <span id=\"back\" class=\"off\">/</span>\
                 <span id=\"addr\" class=\"addr\">Find: {}|{hits}</span>\
                 </div></body></html>",
                escape(query)
            );
        }
        format!(
            "<html><body><div id=\"bar\">\
             <span id=\"back\" class=\"{back}\">&lt;</span>\
             <span id=\"fwd\" class=\"{fwd}\">&gt;</span>\
             <span id=\"reload\" class=\"btn\">R</span>\
             <span id=\"star\" class=\"{star}\">*</span>\
             <span id=\"marks\" class=\"btn\">B</span>\
             <span id=\"addr\" class=\"addr\">{lock}{}|{shield}</span>\
             </div></body></html>",
            escape(&tab.address)
        )
    }

    /// Act on a toolbar button, if the cursor is over one.
    /// Enter in a field submits its form, if it is in one; otherwise it just
    /// leaves the field, which is what a lone input does.
    fn submit_focused_form(&mut self) {
        let sent = self.tab().doc.focused_node().and_then(|id| self.tab().doc.submit(id));
        self.tab_mut().doc.blur();
        let Some(sent) = sent else { return };
        let target = submission_url(&self.tab().address, &sent);
        self.go_to(target);
    }

    // --- find in page ---

    fn open_find(&mut self) {
        self.find = Some(String::new());
        self.tab_mut().doc.blur(); // typing belongs to the find bar now
        self.request_redraw();
    }

    fn close_find(&mut self) {
        self.find = None;
        self.tab_mut().doc.set_find(None);
        self.tab_mut().page_canvas = None; // drop the highlights
    }

    fn apply_find(&mut self) {
        let query = self.find.clone();
        let tab = self.tab_mut();
        tab.doc.set_find(query);
        tab.page_canvas = None; // highlights are painted, so re-render
    }

    /// Scroll to the first match below the current position, wrapping at the end.
    fn jump_to_match(&mut self) {
        let (_, h) = self.window_size();
        let viewport = (h.saturating_sub(TOOLBAR_H)) as f32;
        let tab = self.tab_mut();
        // Matches are in document order, so "next" is the first one past the top
        // of the viewport; a small margin stops the current match re-matching.
        let next = tab
            .matches
            .iter()
            .find(|r| r.y > tab.scroll_y + 4.0)
            .or_else(|| tab.matches.first());
        if let Some(rect) = next {
            // Land the match a third of the way down rather than at the very top.
            tab.scroll_y = (rect.y - viewport / 3.0).max(0.0);
        }
    }

    /// Save the current page, or unsave it if it is already bookmarked.
    fn toggle_bookmark(&mut self) {
        let url = self.tab().address.clone();
        if url.is_empty() || crate::internal::is_internal(&url) {
            return; // nothing worth saving
        }
        if !storage::remove_bookmark(&url) {
            let title = tab_title(&url);
            storage::add_bookmark(&url, &title);
        }
        self.request_redraw(); // the star changed
    }

    fn handle_toolbar_click(&mut self, x: f32, y: f32) -> bool {
        let local_x = x - SIDEBAR_W as f32;
        let hit = self
            .toolbar_rects
            .iter()
            .filter(|r| {
                !r.id.is_empty()
                    && local_x >= r.x
                    && local_x <= r.x + r.width
                    && y >= r.y
                    && y <= r.y + r.height
            })
            .map(|r| r.id.clone())
            .next_back();
        match hit.as_deref() {
            Some("back") => self.back(),
            Some("fwd") => self.forward(),
            Some("reload") => {
                let target = self.tab().address.clone();
                self.load(target);
            }
            Some("star") => self.toggle_bookmark(),
            Some("marks") => self.go_to("zero://bookmarks".into()),
            _ => return false,
        }
        true
    }

    /// Scroll from a click or drag on the scrollbar track.
    fn scroll_to_cursor(&mut self, y: f32, page_top: f32, page_height: f32) {
        let tab = self.tab_mut();
        let content = tab.page_canvas.as_ref().map_or(0, |c| c.height) as f32;
        tab.scroll_y = scroll_for_cursor(content, page_height, y - page_top);
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
                let loader = tab.loader.clone();
                let page = engine.render_document(
                    &mut tab.doc,
                    content_w as f32,
                    page_vh as f32,
                    loader.as_ref(),
                );
                tab.blocked_count = loader.blocked.get();
                for line in &page.console {
                    eprintln!("[js] {line}");
                }
                tab.page_canvas = Some(page.canvas);
                tab.links = page.links;
                tab.matches = page.find_matches;
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
        let toolbar_page = self.engine.render_page(
            &self.toolbar_html(),
            TOOLBAR_CSS,
            content_w as f32,
            tb as f32,
            &crate::net::ShellLoader::new(String::new()),
        );
        self.toolbar_rects = toolbar_page.element_rects;
        let toolbar = toolbar_page.canvas;
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

        // Scrollbar: a track down the right edge of the page, with a thumb sized
        // to the visible fraction. Only shown when the page actually overflows.
        let content_h = page.height as f32;
        let viewport_h = (h as usize - tb) as f32;
        if let Some((offset, thumb_h)) = scrollbar_thumb(content_h, viewport_h, scroll as f32) {
            let bar_w = SCROLLBAR_W as usize;
            let x0 = (w - aw).saturating_sub(bar_w);
            let thumb = thumb_h as usize;
            let thumb_top = tb + offset as usize;
            for y in tb..h as usize {
                for x in x0..(x0 + bar_w).min(w) {
                    let on_thumb = y >= thumb_top && y < thumb_top + thumb;
                    let shade: u32 = if on_thumb { 0x5f636d } else { 0x1a1c21 };
                    buffer[y * w + x] = shade;
                }
            }
        }
        buffer.present().expect("buffer present");
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn form_submission_targets() {
        let sent = |action: &str, query: &str| zero_engine::Submission {
            action: action.into(),
            query: query.into(),
        };
        // Relative action against the page's directory.
        assert_eq!(
            submission_url("https://a.com/docs/x.html", &sent("/find", "q=hi")),
            "https://a.com/find?q=hi"
        );
        // An action with its own query keeps it and appends.
        assert_eq!(
            submission_url("https://a.com/", &sent("/s?lang=hi", "q=zero")),
            "https://a.com/s?lang=hi&q=zero"
        );
        // No action: back to this page, replacing the query it already had.
        assert_eq!(
            submission_url("https://a.com/s?q=old", &sent("", "q=new")),
            "https://a.com/s?q=new"
        );
    }

    #[test]
    fn no_scrollbar_when_content_fits() {
        assert!(scrollbar_thumb(500.0, 600.0, 0.0).is_none());
        assert!(scrollbar_thumb(600.0, 600.0, 0.0).is_none());
    }

    #[test]
    fn thumb_shrinks_with_content_and_tracks_scroll() {
        // Twice the viewport of content -> half-height thumb.
        let (top, height) = scrollbar_thumb(1200.0, 600.0, 0.0).unwrap();
        assert_eq!(height, 300.0);
        assert_eq!(top, 0.0);

        // Fully scrolled puts the thumb at the bottom of its travel.
        let (top, height) = scrollbar_thumb(1200.0, 600.0, 600.0).unwrap();
        assert_eq!(top + height, 600.0);

        // Halfway down sits halfway along the travel.
        let (top, _) = scrollbar_thumb(1200.0, 600.0, 300.0).unwrap();
        assert_eq!(top, 150.0);
    }

    #[test]
    fn dragging_maps_cursor_back_to_scroll_offset() {
        let (content, viewport) = (1200.0, 600.0);
        // Cursor at the track top clamps to the start.
        assert_eq!(scroll_for_cursor(content, viewport, 0.0), 0.0);
        // Cursor at the bottom clamps to full overflow.
        assert_eq!(scroll_for_cursor(content, viewport, viewport), 600.0);
        // Round-trip: drag to a position, and the thumb lands back under the cursor.
        let scroll = scroll_for_cursor(content, viewport, 300.0);
        let (top, thumb) = scrollbar_thumb(content, viewport, scroll).unwrap();
        assert!((top + thumb / 2.0 - 300.0).abs() < 1.0, "thumb centre should follow cursor");
    }
}

//! Zero reference shell — a minimal embedder of `zero-engine`.
//!
//! Window mode has a live address bar: type a URL, press Enter to navigate.
//! The toolbar is itself rendered *by the engine* (as a tiny HTML doc) and
//! composited above the page — so the shell needs no text code of its own.
//!
//! Two modes, both thin wrappers over `engine.render(...)`:
//!   * default: open a live OS window with an address bar.
//!   * `--png [out]`: headless — render once and write a PNG.
//!
//! The target may be a local file OR an http(s) URL (fetched over TLS).
//!
//! Usage:
//!   zero [target] [css]              # window   (target = file or URL)
//!   zero --png [target] [css] [out]  # headless PNG

use std::fs;
use std::io::Read;
use std::num::NonZeroU32;
use std::rc::Rc;
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, KeyEvent, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::{Window, WindowId};
use zero_engine::Engine;

const TOOLBAR_H: u32 = 48;
const TOOLBAR_CSS: &str =
    "body{background:#1f2127;color:#f2f3f5;font-size:16px;} #bar{padding:14px;height:20px;}";

/// Font *sourcing* is a platform/shell concern — the engine only wants bytes.
fn load_system_font() -> Option<Vec<u8>> {
    const CANDIDATES: &[&str] = &[
        "C:/Windows/Fonts/segoeui.ttf",
        "C:/Windows/Fonts/arial.ttf",
        "/System/Library/Fonts/Supplemental/Arial.ttf",
        "/Library/Fonts/Arial.ttf",
        "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
        "/usr/share/fonts/TTF/DejaVuSans.ttf",
    ];
    CANDIDATES.iter().find_map(|p| fs::read(p).ok())
}

fn build_engine() -> Engine {
    match load_system_font() {
        Some(bytes) => Engine::new(&bytes).unwrap_or_else(|e| {
            eprintln!("font load failed ({e}); rendering shapes only");
            Engine::shapes_only()
        }),
        None => {
            eprintln!("no system font found; rendering shapes only");
            Engine::shapes_only()
        }
    }
}

fn is_url(s: &str) -> bool {
    s.starts_with("http://") || s.starts_with("https://")
}

/// Loads `<img>` bytes for the engine, resolving relative URLs against the page.
/// Networking + filesystem are shell concerns; the engine only decodes bytes.
struct ShellLoader {
    base: String,
}

impl zero_engine::ResourceLoader for ShellLoader {
    fn load(&self, url: &str) -> Option<Vec<u8>> {
        let resolved = resolve_url(&self.base, url);
        if resolved.starts_with("http://") || resolved.starts_with("https://") {
            let mut buf = Vec::new();
            ureq::get(&resolved).call().ok()?.into_reader().read_to_end(&mut buf).ok()?;
            Some(buf)
        } else if resolved.starts_with("data:") {
            None // ponytail: data: URIs unsupported; add base64 decode when needed
        } else {
            fs::read(&resolved).ok()
        }
    }
}

/// Resolve a possibly-relative resource URL against a base page URL or file path.
fn resolve_url(base: &str, src: &str) -> String {
    let src = src.trim();
    if is_url(src) || src.starts_with("data:") {
        return src.to_string();
    }
    if let Some(rest) = src.strip_prefix("//") {
        let scheme = base.split("://").next().unwrap_or("https");
        return format!("{scheme}://{rest}");
    }
    if base.starts_with("http") {
        let scheme = base.split("://").next().unwrap_or("https");
        let host = base.split("://").nth(1).unwrap_or("").split('/').next().unwrap_or("");
        let origin = format!("{scheme}://{host}");
        if let Some(abs) = src.strip_prefix('/') {
            return format!("{origin}/{abs}");
        }
        let path = &base[origin.len().min(base.len())..];
        let dir = path.rfind('/').map(|i| &path[..=i]).unwrap_or("/");
        return format!("{origin}{dir}{src}");
    }
    // Local file base: resolve relative to its directory.
    std::path::Path::new(base)
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join(src)
        .to_string_lossy()
        .into_owned()
}

/// Turn address-bar text into something loadable: keep URLs/paths, else assume https.
fn normalize_target(s: &str) -> String {
    let s = s.trim();
    if is_url(s) || std::path::Path::new(s).exists() {
        s.to_string()
    } else if s.contains('.') && !s.contains(' ') {
        format!("https://{s}")
    } else {
        s.to_string()
    }
}

/// Fetch a URL over HTTP(S). Networking is a shell concern.
/// ponytail: blocking call on the UI thread — fine for now; move off-thread if it stalls.
fn fetch_url(url: &str) -> String {
    eprintln!("fetching {url} ...");
    ureq::get(url)
        .call()
        .and_then(|r| r.into_string().map_err(Into::into))
        .unwrap_or_else(|e| {
            eprintln!("fetch failed: {e}");
            format!("<html><body><h1>Failed to load</h1><p>{url}</p></body></html>")
        })
}

fn load_target(target: &str) -> String {
    if is_url(target) {
        fetch_url(target)
    } else {
        fs::read_to_string(target)
            .unwrap_or_else(|e| format!("<html><body><h1>Cannot open</h1><p>{target}: {e}</p></body></html>"))
    }
}

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let png_mode = args.first().map(|a| a == "--png").unwrap_or(false);
    if png_mode {
        args.remove(0);
    }
    let mut args = args.into_iter().peekable();

    let (html, css, address) = match args.next() {
        None => {
            let html = fs::read_to_string("examples/test.html").expect("read html");
            let css = fs::read_to_string("examples/test.css").expect("read css");
            (html, css, "examples/test.html".to_string())
        }
        Some(target) if is_url(&target) => (fetch_url(&target), String::new(), target),
        Some(target) => {
            let html = fs::read_to_string(&target).expect("could not read HTML file");
            let css = if args.peek().map(|a| a.ends_with(".css")).unwrap_or(false) {
                fs::read_to_string(args.next().unwrap()).expect("read css")
            } else {
                String::new()
            };
            (html, css, target)
        }
    };

    let engine = build_engine();

    if png_mode {
        let out_path = args.next().unwrap_or_else(|| "output.png".into());
        render_to_png(&engine, &html, &css, &out_path, &address);
    } else {
        run_window(engine, html, css, address);
    }
}

fn render_to_png(engine: &Engine, html: &str, css: &str, out_path: &str, base: &str) {
    let loader = ShellLoader { base: base.to_string() };
    let canvas = engine.render_page(html, css, 800.0, 600.0, &loader).canvas;
    let buffer: Vec<u8> = canvas.pixels.iter().flat_map(|c| [c.r, c.g, c.b, c.a]).collect();
    let img = image::RgbaImage::from_raw(canvas.width as u32, canvas.height as u32, buffer)
        .expect("pixel buffer size mismatch");
    img.save(out_path).expect("could not write PNG");
    println!("Rendered -> {out_path} ({}x{})", canvas.width, canvas.height);
}

fn run_window(engine: Engine, html: String, css: String, address: String) {
    let event_loop = EventLoop::new().expect("failed to create event loop");
    let mut app = App {
        engine,
        page_html: html,
        page_css: css,
        address: address.clone(),
        scroll_y: 0.0,
        history: vec![address],
        history_index: 0,
        modifiers: ModifiersState::default(),
        page_canvas: None,
        links: Vec::new(),
        cursor: (0.0, 0.0),
        cache_w: 0,
        cache_h: 0,
        window: None,
        surface: None,
    };
    event_loop.run_app(&mut app).expect("event loop error");
}

struct App {
    engine: Engine,
    page_html: String,
    page_css: String,
    address: String,
    scroll_y: f32,
    history: Vec<String>,
    history_index: usize,
    modifiers: ModifiersState,
    // Cached full-height page render; re-rendered only on navigate/resize, not per scroll frame.
    page_canvas: Option<zero_engine::Canvas>,
    links: Vec<zero_engine::LinkArea>,
    cursor: (f32, f32),
    cache_w: u32,
    cache_h: u32,
    window: Option<Rc<Window>>,
    surface: Option<softbuffer::Surface<Rc<Window>, Rc<Window>>>,
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        let attrs = Window::default_attributes()
            .with_title("Zero Browser")
            .with_inner_size(LogicalSize::new(800.0, 600.0));
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
                self.scroll_y = (self.scroll_y - dy).max(0.0); // clamped to content in redraw
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
    fn request_redraw(&self) {
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    fn handle_key(&mut self, event: KeyEvent) {
        match event.logical_key {
            Key::Named(NamedKey::ArrowLeft) if self.modifiers.alt_key() => self.back(),
            Key::Named(NamedKey::ArrowRight) if self.modifiers.alt_key() => self.forward(),
            Key::Named(NamedKey::Enter) => self.navigate(),
            Key::Named(NamedKey::Backspace) => {
                self.address.pop();
            }
            Key::Named(NamedKey::ArrowDown) => self.scroll_y += 48.0,
            Key::Named(NamedKey::ArrowUp) => self.scroll_y = (self.scroll_y - 48.0).max(0.0),
            Key::Named(NamedKey::PageDown) => self.scroll_y += 400.0,
            Key::Named(NamedKey::PageUp) => self.scroll_y = (self.scroll_y - 400.0).max(0.0),
            Key::Named(NamedKey::Home) => self.scroll_y = 0.0,
            _ => {
                if let Some(text) = &event.text {
                    for c in text.chars() {
                        if !c.is_control() {
                            self.address.push(c);
                        }
                    }
                }
            }
        }
    }

    /// Hit-test the last click against the page's link rects and navigate if one matches.
    fn handle_click(&mut self) {
        let (cx, cy) = self.cursor;
        let tb = TOOLBAR_H as f32;
        if cy < tb {
            return; // clicks in the toolbar don't navigate
        }
        // Window coords -> page coords (undo toolbar offset, add scroll).
        let (px, py) = (cx, cy - tb + self.scroll_y);
        let href = self
            .links
            .iter()
            .find(|l| px >= l.x && px <= l.x + l.width && py >= l.y && py <= l.y + l.height)
            .map(|l| l.href.clone());
        if let Some(href) = href {
            let target = resolve_url(&self.address, &href);
            self.go_to(target);
            self.request_redraw();
        }
    }

    /// Navigate to a target typed in the address bar (pushes onto history).
    fn navigate(&mut self) {
        let target = normalize_target(&self.address);
        self.go_to(target);
    }

    /// Navigate to a new target and record it in history (dropping any forward entries).
    fn go_to(&mut self, target: String) {
        self.history.truncate(self.history_index + 1);
        if self.history.last() != Some(&target) {
            self.history.push(target.clone());
            self.history_index = self.history.len() - 1;
        }
        self.load(target);
    }

    fn back(&mut self) {
        if self.history_index > 0 {
            self.history_index -= 1;
            self.load(self.history[self.history_index].clone());
        }
    }

    fn forward(&mut self) {
        if self.history_index + 1 < self.history.len() {
            self.history_index += 1;
            self.load(self.history[self.history_index].clone());
        }
    }

    /// Load a target into the page without touching history.
    fn load(&mut self, target: String) {
        self.address = target.clone();
        self.page_html = load_target(&target);
        self.page_css = String::new(); // rely on the page's own <style>
        self.scroll_y = 0.0;
        self.page_canvas = None; // force re-render of the new page
        if let Some(window) = &self.window {
            window.set_title(&format!("Zero Browser — {target}"));
        }
    }

    /// A blinking-free cursor: just show the caret at the end of the address.
    fn toolbar_html(&self) -> String {
        let safe = self
            .address
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;");
        format!("<html><body><div id=\"bar\">{safe}|</div></body></html>")
    }

    fn redraw(&mut self) {
        let (w, h) = match &self.window {
            Some(window) => {
                let s = window.inner_size();
                (s.width.max(1), s.height.max(1))
            }
            None => return,
        };
        let tb = TOOLBAR_H.min(h);
        let page_vh = h.saturating_sub(tb).max(1);

        // Re-render the page only when the page or the layout size changed; scrolling
        // just re-blits the cached (full-height) canvas.
        if self.page_canvas.is_none() || self.cache_w != w || self.cache_h != page_vh {
            let loader = ShellLoader { base: self.address.clone() };
            let page =
                self.engine.render_page(&self.page_html, &self.page_css, w as f32, page_vh as f32, &loader);
            self.page_canvas = Some(page.canvas);
            self.links = page.links;
            self.cache_w = w;
            self.cache_h = page_vh;
        }
        // Clamp scroll to available overflow (read height into a local first).
        let page_height = self.page_canvas.as_ref().unwrap().height;
        let max_scroll = page_height.saturating_sub(page_vh as usize) as f32;
        self.scroll_y = self.scroll_y.clamp(0.0, max_scroll);
        let scroll = self.scroll_y as usize;

        // Toolbar is cheap; render it fresh each frame (reflects typing).
        let toolbar = self.engine.render(&self.toolbar_html(), TOOLBAR_CSS, w as f32, tb as f32);

        // Disjoint field borrows: page_canvas (shared) + surface (mut).
        let page = self.page_canvas.as_ref().unwrap();
        let surface = match self.surface.as_mut() {
            Some(s) => s,
            None => return,
        };
        surface
            .resize(NonZeroU32::new(w).unwrap(), NonZeroU32::new(h).unwrap())
            .expect("surface resize");
        let mut buffer = surface.buffer_mut().expect("surface buffer");
        let w = w as usize;
        let tb = tb as usize;
        for y in 0..h as usize {
            let src = if y < tb {
                &toolbar.pixels[y * toolbar.width..]
            } else {
                let py = (y - tb) + scroll;
                &page.pixels[py * page.width..]
            };
            for x in 0..w {
                let px = src[x];
                buffer[y * w + x] = (px.r as u32) << 16 | (px.g as u32) << 8 | px.b as u32;
            }
        }
        buffer.present().expect("buffer present");
    }
}

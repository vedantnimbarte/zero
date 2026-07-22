//! The windowed browser app: window, input, tabs, navigation, and compositing.
//!
//! The chrome (tab rail, toolbar, menu, tooltips) is itself rendered *by the
//! engine* as small HTML documents and composited around the page, so the shell
//! needs no text-drawing or widget code of its own. Each of those documents gives
//! its controls an `id`, and the engine hands back the box it painted them in —
//! so hit-testing is one lookup shared by every surface rather than per-surface
//! arithmetic that has to be kept in step with the markup by hand.
//!
//! Window regions, vertical layout (the default):
//! ```text
//!   +----------+---------------------------+
//!   | rail     |         toolbar           |
//!   | (tabs)   +---------------------------+
//!   |          |          page             |
//!   | foot     |                           |
//!   +----------+---------------------------+
//! ```
//! Horizontal layout puts a tab strip across the top instead, and the rail goes away.

use crate::ai::{Assistant, LocalAssistant, PageContext};
use crate::net::{load_target, normalize_target, resolve_url, ShellLoader};
use crate::i18n::{t, t_tip};
use crate::settings::{self, Rail, Settings, TabLayout, ZOOM_STEPS};
use crate::storage;
use std::num::NonZeroU32;
use std::rc::Rc;
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, Ime, KeyEvent, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::{Window, WindowId};
use zero_engine::{Canvas, ElementRect, Engine};

const RAIL_W: u32 = 236;
/// Wide enough for one initial plus breathing room, per docs/02-UI-UX-SPEC.md §3.4.
const RAIL_ICON_W: u32 = 52;
/// The rail's footer is its own surface so it can sit at the bottom of the window
/// without the tab list having to know how tall the window is.
const RAIL_FOOT_H: u32 = 44;
const TABSTRIP_H: u32 = 38;
const TOOLBAR_H: u32 = 48;
const AI_PANEL_W: u32 = 320;
const SCROLLBAR_W: u32 = 12;
const MENU_W: u32 = 252;
/// How much horizontal room one toolbar button takes: glyph box plus padding.
const BUTTON_SPAN: u32 = 40;
/// One tab in the horizontal strip, including its close affordance.
const STRIP_TAB_W: u32 = 176;

/// One palette for the whole browser, so the chrome and the built-in pages are
/// recognisably the same product. Named rather than repeated hex, so a change
/// lands everywhere at once. Values follow docs/02-UI-UX-SPEC.md §3.1.
pub mod theme {
    pub const CANVAS: &str = "#0e0f12"; // the deepest layer, behind pages
    pub const CHROME: &str = "#121317"; // the tab rail
    pub const BAR: &str = "#16181d"; // toolbar, menus
    pub const SURFACE: &str = "#1e2027"; // buttons, cards, the address pill
    pub const HOVER: &str = "#282b34";
    /// Hairline rules. The engine has one font weight, so structure has to come
    /// from ruled lines and spacing rather than from bolder type.
    pub const LINE: &str = "#262931";
    pub const TEXT: &str = "#e8eaed";
    pub const MUTED: &str = "#8b919b";
    pub const FAINT: &str = "#5f646e";
    /// The mark and the active tab, in the current space's colour — which is
    /// how you can tell at a glance which profile you are typing into.
    pub fn accent() -> &'static str {
        crate::spaces::accent_of(&crate::spaces::current())
    }

    /// The accent at roughly 12% over [`CHROME`] — the active tab's wash.
    /// Mixed rather than listed, so a new accent needs no second constant.
    pub fn accent_soft() -> String {
        let mix = |i: usize| {
            let hex = |text: &str, at: usize| {
                u8::from_str_radix(&text[at..at + 2], 16).unwrap_or(0) as f32
            };
            let (accent, chrome) = (hex(accent(), 1 + i * 2), hex(CHROME, 1 + i * 2));
            (chrome + (accent - chrome) * 0.12).round() as u8
        };
        format!("#{:02x}{:02x}{:02x}", mix(0), mix(1), mix(2))
    }
    pub const SAVED: &str = "#f5a524"; // a bookmarked page
    pub const OK: &str = "#30a46c"; // a secure connection
    pub const LINK: &str = "#66ccff";
}

/// [`theme::CANVAS`] as a packed pixel, for the parts of the frame that are
/// filled directly rather than through the engine. A test keeps the two equal.
const CANVAS_RGB: u32 = 0x0e_0f_12;

/// The glyphs the chrome draws with. Kept in one place because each one has to
/// exist somewhere in the font chain — see `fonts::load_system_fonts`.
pub mod icon {
    pub const BACK: &str = "\u{2190}"; // ←
    pub const FORWARD: &str = "\u{2192}"; // →
    pub const RELOAD: &str = "\u{21bb}"; // ↻
    pub const STAR_EMPTY: &str = "\u{2606}"; // ☆
    pub const STAR_FULL: &str = "\u{2605}"; // ★
    pub const BOOKMARKS: &str = "\u{25a4}"; // ▤
    pub const FIND: &str = "\u{2315}"; // ⌕
    pub const CLOSE: &str = "\u{00d7}"; // ×
    pub const ADD: &str = "\u{ff0b}"; // ＋
    pub const MINUS: &str = "\u{2212}"; // −
    pub const SECURE: &str = "\u{1f512}"; // 🔒
    pub const INSECURE: &str = "\u{26a0}"; // ⚠
    pub const SHIELD: &str = "\u{25c6}"; // ◆
    pub const MENU: &str = "\u{22ee}"; // ⋮
    pub const COLLAPSE: &str = "\u{00ab}"; // «
    pub const EXPAND: &str = "\u{00bb}"; // »
    pub const SETTINGS: &str = "\u{2699}"; // ⚙
    pub const DOWNLOAD: &str = "\u{2193}"; // ↓
    pub const PINNED: &str = "\u{25cf}"; // ●
    pub const ASSISTANT: &str = "\u{25c7}"; // ◇
}

/// What each control says when the cursor rests on it: the action, then the key
/// that does the same thing. Named by the verb the control performs, so the
/// tooltip and the menu entry can never drift apart.
pub const TIPS: &[(&str, &str)] = &[
    ("rail", "Tab rail  ·  Ctrl+\\"),
    ("back", "Back  ·  Alt+←"),
    ("fwd", "Forward  ·  Alt+→"),
    ("reload", "Reload  ·  Ctrl+R"),
    ("star", "Bookmark this page  ·  Ctrl+D"),
    ("marks", "Bookmarks  ·  Ctrl+B"),
    ("ai", "Ask about this page  ·  Ctrl+I"),
    ("overflow", "More"),
    ("new", "New tab  ·  Ctrl+T"),
    ("search", "Search your tabs  ·  Ctrl+Shift+A"),
    ("shield", "Trackers blocked here"),
    ("zoom", "Page zoom  ·  Ctrl+0 to reset"),
    ("go:settings", "Settings  ·  Ctrl+,"),
    ("go:downloads", "Downloads  ·  Ctrl+J"),
];

/// The overflow menu, in the order it is drawn. `(id, label, shortcut)`.
const MENU_ITEMS: &[(&str, &str, &str)] = &[
    ("menu:new", "New tab", "Ctrl+T"),
    ("menu:reopen", "Reopen closed tab", "Ctrl+Shift+T"),
    ("menu:pin", "Pin this tab", ""),
    ("", "", ""), // a rule
    ("menu:zoom", "Zoom", ""),
    ("", "", ""),
    ("menu:split", "Split view", ""),
    ("menu:find", "Find on page", "Ctrl+F"),
    ("menu:save", "Save page", "Ctrl+S"),
    ("menu:source", "View source", "Ctrl+U"),
    ("menu:handoff", "Open in your other browser", "Ctrl+Shift+O"),
    ("", "", ""),
    ("go:history", "History", "Ctrl+H"),
    ("go:bookmarks", "Bookmarks", "Ctrl+B"),
    ("go:downloads", "Downloads", "Ctrl+J"),
    ("", "", ""),
    ("go:settings", "Settings", "Ctrl+,"),
];

/// What owns typed characters. Only one thing can at a time, and the address bar
/// is what typing falls back to.
enum Focus {
    Address,
    /// Find-in-page, holding the live query.
    Find(String),
    /// Filtering the tab rail, holding the live query.
    TabSearch(String),
}

impl Focus {
    fn query(&self) -> Option<&str> {
        match self {
            Focus::Find(q) | Focus::TabSearch(q) => Some(q),
            Focus::Address => None,
        }
    }

    fn push(&mut self, text: &str) {
        if let Focus::Find(q) | Focus::TabSearch(q) = self {
            q.push_str(text);
        }
    }

    fn pop(&mut self) {
        if let Focus::Find(q) | Focus::TabSearch(q) = self {
            q.pop();
        }
    }
}

/// A chrome control's painted box, in window coordinates, and what it does.
struct Hit {
    id: String,
    x: f32,
    y: f32,
    width: f32,
    height: f32,
}

impl Hit {
    fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x && x <= self.x + self.width && y >= self.y && y <= self.y + self.height
    }
}

/// Where every region of the window sits, given its size and the user's layout
/// preference. Computed once per frame so compositing, hit-testing and
/// page-coordinate maths cannot disagree about the geometry.
struct Regions {
    rail_w: u32,
    strip_h: u32,
    ai_w: u32,
    content_x: u32,
    content_y: u32,
    content_w: u32,
    content_h: u32,
    /// The other pane in a split, as `(x, width)`. Zero-width when not split.
    ///
    /// `content_*` always describes the *focused* pane, so everything that
    /// works on the page — clicks, hover, scrolling, the scrollbar — keeps
    /// addressing the tab whose address is in the bar, split or not.
    other_x: u32,
    other_w: u32,
    width: u32,
    height: u32,
}

/// The rail's settled width for these settings — where an animation is heading.
fn rail_target(settings: Settings) -> u32 {
    match settings.layout {
        TabLayout::Horizontal => 0,
        TabLayout::Vertical => match settings.rail {
            Rail::Expanded => RAIL_W,
            Rail::Icons => RAIL_ICON_W,
            Rail::Hidden => 0,
        },
    }
}

/// The grabbable gap between two split panes.
const DIVIDER_W: u32 = 6;

/// Below this width the rail shows initials instead of titles.
///
/// Decided from the width the rail actually has rather than from the setting, so
/// a rail caught mid-collapse switches over on the way past instead of holding a
/// layout that no longer fits.
const RAIL_ICON_MAX: u32 = 150;

/// Move an animated value toward its target, independently of frame rate.
///
/// Exponential smoothing: quick to start and easing out, which is the calm
/// motion docs/02-UI-UX-SPEC.md §3.5 asks for. It also retargets mid-flight with
/// no special case, so collapsing the rail while it is still opening just works.
fn ease_toward(current: f32, target: f32, dt: f32) -> f32 {
    // Chosen so a full-width collapse lands in about 260ms — the spec asks for
    // roughly 300ms, and smoothing has a long tail, so the constant is well
    // under the figure it is aiming at. The test pins the resulting duration.
    const TAU: f32 = 0.045;
    // Snap once the remaining distance is under a pixel, so the animation ends
    // rather than approaching forever and redrawing every frame.
    if (target - current).abs() <= 0.5 {
        return target;
    }
    current + (target - current) * (1.0 - (-dt / TAU).exp())
}

impl Regions {
    /// `rail_w` is passed in rather than derived from `settings`, because it
    /// animates: mid-collapse the rail sits between two states, and every other
    /// region has to be laid out against where it actually is right now.
    #[cfg(test)]
    fn of(width: u32, height: u32, settings: Settings, ai_open: bool, rail_w: u32) -> Regions {
        Regions::split(width, height, settings, ai_open, rail_w, None, 0.5)
    }

    /// `split` is which side the focused pane is on (`0` left, `1` right), or
    /// `None` for a single pane. `ratio` is where the divider sits.
    fn split(
        width: u32,
        height: u32,
        settings: Settings,
        ai_open: bool,
        rail_w: u32,
        split: Option<usize>,
        ratio: f32,
    ) -> Regions {
        let rail_w = rail_w.min(width / 2);
        let strip_h = match settings.layout {
            TabLayout::Horizontal => TABSTRIP_H.min(height / 4),
            TabLayout::Vertical => 0,
        };
        let ai_w = match ai_open {
            true => AI_PANEL_W.min(width.saturating_sub(rail_w) / 2),
            false => 0,
        };
        let content_y = (strip_h + TOOLBAR_H).min(height);
        let full_w = width.saturating_sub(rail_w + ai_w).max(1);
        // The divider is drawn in the gap, so each pane keeps its own edges.
        let (left_w, right_x, right_w) = match full_w.checked_sub(DIVIDER_W) {
            Some(usable) if split.is_some() && usable > 2 => {
                let left = ((usable as f32 * ratio) as u32).clamp(1, usable - 1);
                (left, rail_w + left + DIVIDER_W, usable - left)
            }
            _ => (full_w, 0, 0),
        };
        // Pane 1 focused: the bar and every page interaction follow the right.
        let focused_right = split == Some(1);
        Regions {
            rail_w,
            strip_h,
            ai_w,
            content_x: if focused_right { right_x } else { rail_w },
            content_y,
            content_w: if focused_right { right_w } else { left_w },
            content_h: height.saturating_sub(content_y).max(1),
            other_x: if focused_right { rail_w } else { right_x },
            other_w: if focused_right { left_w } else { right_w },
            width,
            height,
        }
    }

    /// The regions once any rail animation has finished.
    #[cfg(test)]
    fn settled(width: u32, height: u32, settings: Settings, ai_open: bool) -> Regions {
        Regions::of(width, height, settings, ai_open, rail_target(settings))
    }

    /// Where the divider between two panes starts, or `None` when not split.
    fn divider_x(&self) -> Option<u32> {
        if self.other_w == 0 {
            return None;
        }
        let left_w = match self.content_x < self.other_x {
            true => self.content_w,
            false => self.other_w,
        };
        Some(self.content_x.min(self.other_x) + left_w)
    }

    /// The toolbar spans everything right of the rail, below any tab strip.
    fn toolbar_w(&self) -> u32 {
        self.width.saturating_sub(self.rail_w).max(1)
    }

    /// How tall the rail's tab list is, above its footer.
    fn rail_list_h(&self) -> u32 {
        self.height.saturating_sub(RAIL_FOOT_H).max(1)
    }
}

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

/// The size a page is laid out at: the content area divided by the tab's zoom.
///
/// Dividing rather than scaling afterwards is what makes zoom *reflow* — a
/// zoomed page gets a narrower viewport, so its media queries and wrapping
/// behave as they would in a smaller window, instead of the page being cropped.
fn layout_size(content_w: u32, content_h: u32, zoom: f32) -> (f32, f32) {
    ((content_w as f32 / zoom).max(1.0), (content_h as f32 / zoom).max(1.0))
}

/// The next zoom step in `direction`, clamped at the ends of the scale.
fn zoom_step(current: u32, direction: i32) -> u32 {
    let at = ZOOM_STEPS.iter().position(|z| *z == current).unwrap_or_else(|| {
        // An unlisted value (an old settings file) snaps to the nearest step.
        ZOOM_STEPS
            .iter()
            .enumerate()
            .min_by_key(|(_, z)| z.abs_diff(current))
            .map(|(i, _)| i)
            .expect("the scale is not empty")
    });
    let next = (at as i32 + direction).clamp(0, ZOOM_STEPS.len() as i32 - 1);
    ZOOM_STEPS[next as usize]
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

/// Ctrl+Shift+T and Ctrl+T arrive as different characters, so chords are matched
/// on the lowercased key with Shift read separately.
fn lower(key: &str) -> String {
    key.to_lowercase()
}

/// The storage partition for a target: its site for URLs, the file name for local
/// pages, so two local examples don't share one bucket.
fn storage_site(address: &str) -> String {
    let site = crate::cookies::site_of(address);
    if site.is_empty() {
        address_label(address)
    } else {
        site
    }
}

/// The label for a tab: the page's own title when it has one, else its address.
///
/// Truncated to `max` characters, because a real title is longer than any tab is
/// wide and the engine has no `text-overflow` — so a title that does not fit
/// wraps onto a second line and is clipped, rather than trailing off politely.
fn label_for(title: &str, address: &str, max: usize) -> String {
    let text = match title.trim() {
        "" => address_label(address),
        title => title.to_string(),
    };
    match text.chars().count() > max {
        true => text.chars().take(max - 1).chain(['\u{2026}']).collect(),
        false => text,
    }
}

/// A short label for an address: the host for URLs, the file name for paths.
fn address_label(address: &str) -> String {
    if address.is_empty() {
        return "New Tab".to_string();
    }
    match address.split("://").nth(1) {
        Some(rest) => rest.split('/').next().unwrap_or(rest).to_string(),
        None => std::path::Path::new(address)
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| address.to_string()),
    }
}

/// How wide a tab's title may be in a rail of this width: the rail less the
/// body's padding, the row's accent edge and inset, and the close affordance.
fn rail_name_width(rail_w: u32) -> u32 {
    rail_w.saturating_sub(16 + 3 + 12 + 8 + 32)
}

/// How many characters of a title fit in a rail of this width.
///
/// ponytail: characters, not pixels — the engine cannot measure a string, so
/// this assumes a generous 8px average advance at 13px. Erring wide would wrap
/// the row, so it errs narrow.
fn rail_label_room(rail_w: u32) -> usize {
    (rail_name_width(rail_w) / 8).max(3) as usize
}

/// The single character that stands for a tab in the icon rail.
fn initial(label: &str) -> String {
    label
        .chars()
        .find(|c| c.is_alphanumeric())
        .map(|c| c.to_uppercase().to_string())
        .unwrap_or_else(|| "\u{2022}".to_string())
}

/// Everything that belongs to one tab, including its own history and render cache.
struct Tab {
    address: String,
    /// Owns the parsed DOM and a live JS runtime, so handlers survive between frames.
    doc: zero_engine::Document,
    element_rects: Vec<ElementRect>,
    history: Vec<String>,
    history_index: usize,
    scroll_y: f32,
    secure: bool,
    blocked_count: usize,
    /// Percent. Per tab, so zooming one page doesn't resize every other.
    zoom: u32,
    /// Pinned tabs lead the rail and survive "close others" reasoning.
    pinned: bool,
    page_canvas: Option<Canvas>,
    links: Vec<zero_engine::LinkArea>,
    /// Find-in-page match boxes from the last render, for jumping between them.
    matches: Vec<zero_engine::layout::Rect>,
    /// Whether the last render's stylesheet reacted to the cursor at all.
    uses_hover: bool,
    /// The markup this page was built from, kept for view-source and saving.
    source: String,
    /// Shared with this tab's document so its subresource cache outlives a
    /// single render — the engine re-asks for images and stylesheets each time.
    loader: Rc<ShellLoader>,
    cache_w: u32,
    cache_h: u32,
}

impl Tab {
    fn new(address: String, html: String, css: String) -> Tab {
        let loader = Rc::new(ShellLoader::new(address.clone()));
        let source = html.clone();
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
            zoom: settings::current().zoom,
            pinned: false,
            page_canvas: None,
            links: Vec::new(),
            matches: Vec::new(),
            uses_hover: false,
            source,
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

    /// How the tab names itself in a space `max` characters wide.
    fn label_capped(&self, max: usize) -> String {
        label_for(&self.doc.title(), &self.address, max)
    }

    /// The rail's width, which is what most of the chrome is sized against.
    fn label(&self) -> String {
        self.label_capped(22)
    }

    fn zoom_factor(&self) -> f32 {
        self.zoom as f32 / 100.0
    }
}

/// Compose the browser window — chrome and all — without opening one.
///
/// The UI is worth looking at while developing it, and this goes through the
/// same [`App::frame`] the window does, so a screenshot cannot drift from what
/// the user actually sees.
///
/// `poses` put the chrome into a state a still image cannot otherwise reach —
/// an open menu, a hovered control — so every surface stays reviewable without
/// a person having to hold the mouse in the right place.
pub fn screenshot(
    engine: Engine,
    html: String,
    css: String,
    address: String,
    width: u32,
    height: u32,
    poses: &[String],
) -> (Vec<u32>, u32, u32) {
    let mut app = App::new(engine, vec![Tab::new(address, html, css)], 0);
    for pose in poses {
        // `menu`, `hover:star` and `layout=horizontal` are all poses, so both
        // separators are accepted rather than making the caller remember which.
        match pose.split_once([':', '=']).unwrap_or((pose.as_str(), "")) {
            ("menu", _) => app.menu_open = true,
            ("ai", _) => {
                app.ai_open = true;
                app.run_assistant();
            }
            ("hover", id) => app.hovered = Some(id.to_string()),
            ("railpx", _) => {} // applied after the loop, once settings are known
            ("search", query) => app.focus = Focus::TabSearch(query.to_string()),
            ("split", _) => app.toggle_split(),
            ("space", name) => app.switch_space(name),
            ("tabs", n) => {
                // Extra tabs, so the rail and the strip can be seen carrying more
                // than one thing.
                for i in 1..n.parse::<usize>().unwrap_or(3) {
                    let mut tab = Tab::blank();
                    tab.address = format!("https://example{i}.org");
                    tab.pinned = i == 1;
                    app.tabs.push(tab);
                }
            }
            (key, value) => {
                let mut settings = app.settings;
                if settings.set(key, value) {
                    app.settings = settings;
                    settings::preview(settings);
                    // Zoom is a per-tab value, and the tab was built before this
                    // pose was read — so hand it down, or the shot ignores it.
                    for tab in &mut app.tabs {
                        tab.zoom = settings.zoom;
                    }
                } else {
                    eprintln!("unknown pose: {pose}");
                }
            }
        }
    }
    // A built-in page is drawn from the settings, so it has to be rebuilt after
    // a pose changes them — otherwise `--shot zero://settings lang=hi` would
    // pose the chrome in Hindi around a page still written in English.
    if crate::internal::is_internal(&app.tabs[0].address) {
        let address = app.tabs[0].address.clone();
        app.tabs[0] = Tab::new(address.clone(), crate::internal::page(&address), String::new());
    }
    // A still image has no time to animate in, so the rail starts where it
    // lands — unless a pose asked for a particular point mid-slide.
    if let Some(px) = poses.iter().find_map(|p| p.strip_prefix("railpx:")) {
        app.rail_px = px.parse().unwrap_or_else(|_| rail_target(app.settings) as f32);
    } else {
        app.rail_px = rail_target(app.settings) as f32;
    }
    (app.frame(width, height), width, height)
}

pub fn run_window(engine: Engine, html: String, css: String, address: String) {
    App::new(engine, vec![Tab::new(address, html, css)], 0).run();
}

/// Reopen the tabs from the previous session, if any were saved and the user
/// asked for them back.
pub fn run_window_restoring_session(engine: Engine) -> bool {
    if !settings::current().restore {
        return false;
    }
    let Some((saved, active)) = storage::load_session() else { return false };
    let tabs: Vec<Tab> = saved
        .iter()
        .map(|(url, pinned)| {
            let fetched = load_target(url);
            let loader = Rc::new(ShellLoader::new(fetched.url.clone()));
            let mut tab = Tab::new(fetched.url.clone(), String::new(), String::new());
            tab.doc = zero_engine::Document::load_with(&fetched.body, "", loader);
            tab.secure = fetched.secure;
            tab.pinned = *pinned;
            tab
        })
        .collect();
    let active = active.min(tabs.len().saturating_sub(1));
    App::new(engine, tabs, active).run();
    true
}

struct App {
    engine: Engine,
    tabs: Vec<Tab>,
    active: usize,
    settings: Settings,
    modifiers: ModifiersState,
    cursor: (f32, f32),
    ai_open: bool,
    ai_text: String,
    menu_open: bool,
    /// Addresses of recently closed tabs, most recent last.
    closed: Vec<String>,
    /// What typed characters go to.
    focus: Focus,
    /// Every chrome control's box from the last frame, in paint order — so the
    /// topmost surface wins a click. Rebuilt each frame.
    hits: Vec<Hit>,
    /// Which control the cursor is resting on, for highlighting and tooltips.
    hovered: Option<String>,
    /// The rail's width right now, which eases toward [`rail_target`].
    rail_px: f32,
    /// When the last animated frame was drawn, for a frame-rate-independent step.
    /// `None` when nothing is moving, so the first frame of an animation starts
    /// from rest instead of jumping by however long the window sat idle.
    last_frame: Option<std::time::Instant>,
    /// When this window opened. Page transitions run against the time since,
    /// which is monotonic and shared by every tab.
    started: std::time::Instant,
    /// Whether a page transition is still running and wants another frame.
    page_animating: bool,
    /// Whether the last frame left something mid-animation and so owes another.
    animating: bool,
    dragging_scrollbar: bool,
    /// The tab sharing the window, if the view is split. Always a different tab
    /// from the active one; the pair is drawn in tab order, so which side each
    /// sits on does not change when focus moves between them.
    split: Option<usize>,
    /// Where the divider sits, as a fraction of the content area.
    split_ratio: f32,
    dragging_divider: bool,
    window: Option<Rc<Window>>,
    surface: Option<softbuffer::Surface<Rc<Window>, Rc<Window>>>,
}

impl App {
    fn new(engine: Engine, tabs: Vec<Tab>, active: usize) -> App {
        let settings = settings::current();
        App {
            engine,
            tabs,
            active,
            settings,
            rail_px: rail_target(settings) as f32,
            last_frame: None,
            started: std::time::Instant::now(),
            page_animating: false,
            animating: false,
            modifiers: ModifiersState::default(),
            cursor: (0.0, 0.0),
            ai_open: false,
            ai_text: String::new(),
            menu_open: false,
            closed: Vec::new(),
            focus: Focus::Address,
            hits: Vec::new(),
            hovered: None,
            dragging_scrollbar: false,
            split: None,
            split_ratio: 0.5,
            dragging_divider: false,
            window: None,
            surface: None,
        }
    }

    fn run(mut self) {
        let event_loop = EventLoop::new().expect("failed to create event loop");
        event_loop.run_app(&mut self).expect("event loop error");
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        let attrs = Window::default_attributes()
            .with_title("Zero Browser")
            .with_inner_size(LogicalSize::new(1180.0, 760.0));
        let window = Rc::new(event_loop.create_window(attrs).expect("failed to create window"));
        // Devanagari, Tamil and CJK are typed through an input method, which
        // sends composed text as its own event and never as a key press.
        window.set_ime_allowed(true);
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
            WindowEvent::RedrawRequested => self.render(),
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
                self.handle_key(event);
                self.request_redraw();
            }
            // A finished composition is exactly typing, however many keys it
            // took to produce. The preedit (what the IME is still deciding) is
            // ignored: showing it needs a caret the fields do not have yet.
            WindowEvent::Ime(Ime::Commit(text)) => {
                self.type_text(&text);
                self.request_redraw();
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let dy = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y * 48.0,
                    MouseScrollDelta::PixelDelta(pos) => pos.y as f32,
                };
                // Ctrl+wheel zooms, as it does everywhere else.
                if self.modifiers.control_key() {
                    self.zoom_by(if dy > 0.0 { 1 } else { -1 });
                } else {
                    // The wheel scrolls whichever pane the cursor is over, which
                    // is the whole point of having two of them side by side.
                    let index = self.pane_under_cursor();
                    let tab = &mut self.tabs[index];
                    tab.scroll_y = (tab.scroll_y - dy).max(0.0); // clamped to content in redraw
                }
                self.request_redraw();
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor = (position.x as f32, position.y as f32);
                if self.dragging_divider {
                    let regions = self.regions();
                    let span = regions.content_w + regions.other_w + DIVIDER_W;
                    let left = regions.content_x.min(regions.other_x) as f32;
                    self.split_ratio = ((self.cursor.0 - left) / span as f32).clamp(0.15, 0.85);
                    self.invalidate_panes();
                    self.request_redraw();
                } else if self.dragging_scrollbar {
                    let regions = self.regions();
                    self.scroll_to_cursor(self.cursor.1, &regions);
                    self.request_redraw();
                } else {
                    self.update_hover();
                }
            }
            WindowEvent::MouseInput { state: ElementState::Released, .. } => {
                self.dragging_scrollbar = false;
                self.dragging_divider = false;
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

    fn window_size(&self) -> (u32, u32) {
        match &self.window {
            Some(window) => {
                let s = window.inner_size();
                (s.width.max(1), s.height.max(1))
            }
            None => (1, 1),
        }
    }

    /// The regions as last drawn. Uses the rail's animated width, not its target,
    /// so a click during a collapse hits what is actually on screen.
    fn regions(&self) -> Regions {
        let (w, h) = self.window_size();
        Regions::split(
            w,
            h,
            self.settings,
            self.ai_open,
            self.rail_px.round() as u32,
            self.focused_pane(),
            self.split_ratio,
        )
    }

    /// Which side the focused pane is on, or `None` when the view is not split.
    fn focused_pane(&self) -> Option<usize> {
        let other = self.split?;
        match other < self.tabs.len() && other != self.active {
            // Drawn in tab order, so focusing the other pane does not move it.
            true => Some((self.active > other) as usize),
            false => None,
        }
    }

    /// Open this page in whatever browser the system already has.
    ///
    /// The specified answer to "Zero cannot render this yet" is a compat bridge
    /// — another engine embedded behind a flag. This is the same promise kept
    /// at a fraction of the weight: the machine already has a browser that
    /// renders the whole web, and one keystroke reaches it. No 200 MB
    /// dependency, no second engine to keep current, and nothing pretending the
    /// page rendered here.
    ///
    /// ponytail: a handoff, not a bridge — the page opens *there*, outside this
    /// browser's cookie jar and tracker blocking, which is exactly what handing
    /// a page to another browser means and worth knowing before pressing it.
    fn hand_off(&mut self) {
        let url = self.tab().address.clone();
        // Only ever a web address: anything else would be handing the system a
        // path or a scheme of the page's choosing.
        if !is_web_url(&url) {
            return;
        }
        let launcher = if cfg!(windows) {
            // Not `cmd /c start`: that would parse `&` in the URL as a command
            // separator. `explorer` takes the target as one argument.
            ("explorer", vec![url])
        } else if cfg!(target_os = "macos") {
            ("open", vec![url])
        } else {
            ("xdg-open", vec![url])
        };
        let _ = std::process::Command::new(launcher.0).args(launcher.1).spawn();
    }

    /// Move to another space: a different profile, and so a different session,
    /// history, cookie jar and set of preferences.
    fn switch_space(&mut self, name: &str) {
        if name == crate::spaces::current() {
            return;
        }
        self.save_session(); // the space being left keeps its tabs
        let Some(_) = crate::spaces::switch(name) else { return };
        settings::reload();
        self.settings = settings::current();
        // Nothing from the old space may stay on screen: its tabs are its own.
        self.tabs = vec![Tab::blank()];
        self.active = 0;
        self.split = None;
        self.closed.clear();
        self.rail_px = rail_target(self.settings) as f32;
        self.request_redraw();
    }

    /// Show `index` beside the active tab, or close the split if it is already
    /// showing. Splitting with nothing else open opens a new tab to fill it.
    fn toggle_split(&mut self) {
        if self.split.is_some() {
            self.split = None;
        } else {
            if self.tabs.len() < 2 {
                self.new_tab(); // which makes the new tab active
            }
            let other = (0..self.tabs.len()).find(|i| *i != self.active);
            self.split = other;
            self.split_ratio = 0.5;
        }
        self.invalidate_panes();
        self.request_redraw();
    }

    /// Both panes have to lay out again when the space they share changes.
    fn invalidate_panes(&mut self) {
        for tab in &mut self.tabs {
            tab.page_canvas = None;
        }
    }

    /// Step the rail toward its target. Returns whether it is still moving.
    fn advance_rail(&mut self) -> bool {
        let target = rail_target(self.settings) as f32;
        if !self.settings.motion {
            self.rail_px = target;
            self.last_frame = None;
            return false;
        }
        if self.rail_px == target {
            self.last_frame = None;
            return false;
        }
        let now = std::time::Instant::now();
        // A long gap since the last frame means the window was idle, not that a
        // huge step is owed — so the clock starts fresh rather than jumping.
        let dt = match self.last_frame.replace(now) {
            Some(then) => (now - then).as_secs_f32().min(0.05),
            None => 0.0,
        };
        self.rail_px = ease_toward(self.rail_px, target, dt);
        self.rail_px != target
    }

    // --- tab management ---

    /// Write the open tabs to disk so the next launch can restore them.
    fn save_session(&self) {
        let tabs: Vec<(String, bool)> = self
            .tabs
            .iter()
            .filter(|t| !t.address.is_empty())
            .map(|t| (t.address.clone(), t.pinned))
            .collect();
        storage::save_session(&tabs, self.active.min(tabs.len().saturating_sub(1)));
    }

    fn new_tab(&mut self) {
        self.tabs.push(Tab::blank());
        self.active = self.tabs.len() - 1;
        self.focus = Focus::Address;
        self.save_session();
    }

    fn close_tab_at(&mut self, index: usize) {
        if index >= self.tabs.len() {
            return;
        }
        let gone = self.tabs.remove(index);
        // Closing is undoable, so remember where it pointed.
        if !gone.address.is_empty() && gone.address != "zero://newtab" {
            self.closed.push(gone.address);
        }
        // Keep the same tab in front where possible: closing one before the
        // active tab would otherwise shift the selection sideways.
        if index < self.active {
            self.active -= 1;
        }
        // The split points at a tab by index, and indices shift underneath it.
        self.split = match self.split {
            Some(i) if i == index => None, // the pane's own tab is gone
            Some(i) if i > index => Some(i - 1),
            other => other,
        };
        if self.tabs.is_empty() {
            self.tabs.push(Tab::blank()); // always keep one tab open
        }
        self.active = self.active.min(self.tabs.len() - 1);
        self.save_session();
    }

    /// Reopen the most recently closed tab, in front.
    fn reopen_closed(&mut self) {
        let Some(address) = self.closed.pop() else { return };
        self.tabs.push(Tab::blank());
        self.active = self.tabs.len() - 1;
        self.go_to(address);
    }

    fn next_tab(&mut self) {
        self.active = (self.active + 1) % self.tabs.len();
    }

    fn toggle_pin(&mut self) {
        let tab = self.tab_mut();
        tab.pinned = !tab.pinned;
        self.save_session();
    }

    /// The tabs to show, in rail order: pinned first, then the rest, each with
    /// its index into `self.tabs` so ids stay stable however the rail sorts them.
    fn rail_order(&self) -> Vec<usize> {
        let query = match &self.focus {
            Focus::TabSearch(q) => q.to_lowercase(),
            _ => String::new(),
        };
        let mut order: Vec<usize> = (0..self.tabs.len())
            .filter(|i| {
                query.is_empty() || {
                    let tab = &self.tabs[*i];
                    tab.label().to_lowercase().contains(&query)
                        || tab.address.to_lowercase().contains(&query)
                }
            })
            .collect();
        order.sort_by_key(|i| !self.tabs[*i].pinned); // pinned first, order otherwise kept
        order
    }

    // --- settings ---

    /// Adopt a changed preference: persist it, and drop every cached page render
    /// because the content area has almost certainly changed width.
    fn store_settings(&mut self, settings: Settings) {
        self.settings = settings;
        settings::store(settings);
        for tab in &mut self.tabs {
            tab.page_canvas = None;
        }
    }

    fn cycle_rail(&mut self) {
        let mut settings = self.settings;
        // In horizontal layout the rail is not on screen, so the control that
        // would collapse it brings the rail layout back instead.
        match settings.layout {
            TabLayout::Horizontal => settings.layout = TabLayout::Vertical,
            TabLayout::Vertical => settings.rail = settings.rail.next(),
        }
        self.store_settings(settings);
    }

    // --- zoom ---

    fn zoom_by(&mut self, direction: i32) {
        let tab = self.tab_mut();
        tab.zoom = zoom_step(tab.zoom, direction);
        tab.page_canvas = None;
    }

    fn zoom_reset(&mut self) {
        let default = self.settings.zoom;
        let tab = self.tab_mut();
        tab.zoom = default;
        tab.page_canvas = None;
    }

    // --- input ---

    /// Put typed text wherever focus is: an open chrome field, a focused page
    /// field, or the address bar. Key presses and input-method commits are the
    /// same act and must land in the same place.
    fn type_text(&mut self, text: &str) -> bool {
        let typed: String = text.chars().filter(|c| !c.is_control()).collect();
        if typed.is_empty() {
            return false;
        }
        if self.focus.query().is_some() {
            self.focus.push(&typed);
            self.apply_chrome_field();
            return true;
        }
        if self.tab().doc.is_focused() && self.tab_mut().doc.insert_text(&typed) {
            self.tab_mut().page_canvas = None; // field text changed
            return true;
        }
        self.tab_mut().address.push_str(&typed);
        true
    }

    fn handle_key(&mut self, event: KeyEvent) {
        let ctrl = self.modifiers.control_key();
        let shift = self.modifiers.shift_key();
        // A chrome field owns typing while it is open, like a page field does.
        if !ctrl && self.focus.query().is_some() {
            match event.logical_key {
                Key::Named(NamedKey::Escape) => self.close_chrome_field(),
                Key::Named(NamedKey::Backspace) => {
                    self.focus.pop();
                    self.apply_chrome_field();
                }
                Key::Named(NamedKey::Enter) => self.accept_chrome_field(),
                _ => match &event.text {
                    Some(text) => {
                        if !self.type_text(&text.to_string()) {
                            return;
                        }
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
                    Some(text) => self.type_text(&text.to_string()),
                    None => false,
                },
            };
            if handled {
                self.tab_mut().page_canvas = None; // field text changed
                return;
            }
        }
        match event.logical_key {
            // Shift changes what a chord means, so match on the lowercased key
            // and read Shift separately rather than on the character's case.
            Key::Character(ref c) if ctrl && shift && lower(c) == "o" => self.hand_off(),
            Key::Character(ref c) if ctrl => match (lower(c).as_str(), shift) {
                ("t", false) => self.new_tab(),
                ("t", true) => self.reopen_closed(),
                ("a", true) => self.open_tab_search(),
                ("w", _) => {
                    let active = self.active;
                    self.close_tab_at(active);
                }
                ("i", _) => self.toggle_assistant(),
                ("d", _) => self.toggle_bookmark(),
                ("f", _) => self.open_find(),
                ("u", _) => self.view_source(),
                ("r", _) => self.reload(),
                ("s", _) => self.save_page(),
                ("l", _) => {
                    self.tab_mut().address.clear(); // ready for a new address
                    self.focus = Focus::Address;
                }
                ("h", _) => self.go_to("zero://history".into()),
                ("b", _) => self.go_to("zero://bookmarks".into()),
                ("j", _) => self.go_to("zero://downloads".into()),
                (",", _) => self.go_to("zero://settings".into()),
                ("\\", _) => self.cycle_rail(),
                ("=", _) | ("+", _) => self.zoom_by(1),
                ("-", _) => self.zoom_by(-1),
                ("0", _) => self.zoom_reset(),
                _ => {}
            },
            Key::Named(NamedKey::Tab) if ctrl => self.next_tab(),
            Key::Named(NamedKey::Escape) => self.menu_open = false,
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

    /// Route a click: chrome controls act, everything else goes to the page.
    fn handle_click(&mut self) {
        let (cx, cy) = self.cursor;
        let regions = self.regions();

        // Topmost surface first, so a menu covering the page wins the click.
        if let Some(id) = self.hit_at(cx, cy).map(str::to_string) {
            if self.act_on_menu(&id) || self.act_on(&id) {
                // Acting on a menu entry closes the menu, so the two can never
                // disagree about whether it is still up. Zoom is the exception:
                // it is a value you nudge, so the stepper stays under the cursor.
                if id != "overflow" && !id.starts_with("zoom") {
                    self.menu_open = false;
                }
                self.request_redraw();
            }
            return;
        }
        // A click anywhere else dismisses the menu rather than reaching the page:
        // the first click closes, the second acts, which is what a menu should do.
        if self.menu_open {
            self.menu_open = false;
            self.request_redraw();
            return;
        }
        if cx < regions.content_x as f32 || cy < regions.content_y as f32 {
            return; // chrome, but not a control
        }

        // The scrollbar sits at the right edge of the page area.
        let page_right = (regions.content_x + regions.content_w) as f32;
        if cx >= page_right - SCROLLBAR_W as f32 && cx < page_right {
            self.dragging_scrollbar = true;
            self.scroll_to_cursor(cy, &regions);
            self.request_redraw();
            return;
        }
        // The other pane takes focus on a click, without either pane moving.
        if regions.other_w > 0
            && cx >= regions.other_x as f32
            && cx < (regions.other_x + regions.other_w) as f32
        {
            if let Some(other) = self.split {
                self.split = Some(self.active);
                self.active = other;
                self.request_redraw();
            }
            return;
        }
        // The divider between two panes drags to resize them.
        if regions.other_w > 0 && self.on_divider(cx, &regions) {
            self.dragging_divider = true;
            return;
        }
        if cx >= page_right {
            return; // the assistant panel is not the page
        }

        let Some((px, py)) = self.page_coords((cx, cy), &regions) else { return };
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

    /// The chrome control under a window position, topmost and innermost first.
    fn hit_at(&self, x: f32, y: f32) -> Option<&str> {
        self.hits.iter().rev().find(|hit| hit.contains(x, y)).map(|hit| hit.id.as_str())
    }

    /// Perform a chrome control's action. Returns whether anything happened.
    fn act_on(&mut self, id: &str) -> bool {
        let (verb, arg) = id.split_once(':').unwrap_or((id, ""));
        match verb {
            "tab" => {
                if let Ok(index) = arg.parse::<usize>() {
                    self.active = index.min(self.tabs.len() - 1);
                }
            }
            "close" => {
                if let Ok(index) = arg.parse::<usize>() {
                    self.close_tab_at(index);
                }
            }
            "go" => self.go_to(format!("zero://{arg}")),
            "new" => self.new_tab(),
            "rail" => self.cycle_rail(),
            "back" => self.back(),
            "fwd" => self.forward(),
            "reload" => self.reload(),
            "star" => self.toggle_bookmark(),
            "marks" => self.go_to("zero://bookmarks".into()),
            "ai" => self.toggle_assistant(),
            "search" => self.open_tab_search(),
            "overflow" => self.menu_open = !self.menu_open,
            "zoom" => match arg {
                "in" => self.zoom_by(1),
                "out" => self.zoom_by(-1),
                _ => self.zoom_reset(),
            },
            "shield" => self.go_to("zero://settings".into()),
            _ => return false,
        }
        true
    }

    /// Menu entries that have no equivalent toolbar control. Everything else in
    /// the menu shares an id — and therefore an action — with the toolbar.
    fn act_on_menu(&mut self, id: &str) -> bool {
        match id {
            "menu:new" => self.new_tab(),
            "menu:reopen" => self.reopen_closed(),
            "menu:pin" => self.toggle_pin(),
            "menu:find" => self.open_find(),
            "menu:save" => self.save_page(),
            "menu:source" => self.view_source(),
            "menu:split" => self.toggle_split(),
            "menu:handoff" => self.hand_off(),
            _ => return false,
        }
        true
    }

    // --- navigation (per tab) ---

    fn navigate(&mut self) {
        let target = normalize_target(&self.tab().address);
        self.go_to(target);
    }

    fn reload(&mut self) {
        let target = self.tab().address.clone();
        self.load(target);
    }

    fn go_to(&mut self, target: String) {
        // A settings link carries its new value in the query; applying it here
        // keeps the address that lands in history clean.
        let target = self.apply_setting_link(target);
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

    /// Apply `zero://settings?key=value`, returning the address to actually open.
    fn apply_setting_link(&mut self, target: String) -> String {
        let Some(query) = target.strip_prefix("zero://settings?") else { return target };
        // A space is not a preference — it decides which preferences file the
        // rest of this query would even be written to, so it goes first and
        // alone.
        if let Some(name) = query.strip_prefix("space=") {
            self.switch_space(&name.replace("+", " ").replace("%20", " "));
            return "zero://settings".to_string();
        }
        let mut settings = self.settings;
        settings.apply_query(query);
        self.store_settings(settings);
        "zero://settings".to_string()
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
        tab.source = fetched.body;
        tab.matches.clear();
        tab.scroll_y = 0.0;
        tab.page_canvas = None; // force re-render of the new page
        let address = tab.address.clone();
        let title = tab.doc.title();
        // Built-in pages are the browser's own furniture, not places you visited.
        if !crate::internal::is_internal(&address) {
            // History has room for a full title, unlike a tab.
            storage::record_visit(&address, &label_for(&title, &address, 120));
        }
        if let Some(window) = &self.window {
            let shown = if title.is_empty() { address } else { title };
            window.set_title(&format!("Zero Browser — {shown}"));
        }
        self.save_session();
        if self.ai_open {
            self.run_assistant(); // keep the panel in sync with the new page
        }
    }

    /// Keep a copy of the page in the Downloads folder.
    fn save_page(&mut self) {
        let (url, title, source) = {
            let tab = self.tab();
            (tab.address.clone(), tab.doc.title(), tab.source.clone())
        };
        if source.is_empty() {
            return;
        }
        match storage::save_page(&url, &title, &source) {
            Some(name) => eprintln!("saved {name}"),
            None => eprintln!("could not save this page"),
        }
    }

    // --- assistant ---

    fn toggle_assistant(&mut self) {
        self.ai_open = !self.ai_open;
        for tab in &mut self.tabs {
            tab.page_canvas = None; // the content area changed width
        }
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
        self.ai_text = format!("{}\n\n[{}]", assistant.respond(&ctx), assistant.provenance());
    }

    fn ai_html(&self) -> String {
        let body: String = self
            .ai_text
            .lines()
            .map(|line| format!("<div class=\"line\">{}</div>", escape(line)))
            .collect();
        format!("<html><body><div id=\"head\">Assistant</div>{body}</body></html>")
    }

    /// Enter in a field submits its form, if it is in one; otherwise it just
    /// leaves the field, which is what a lone input does.
    fn submit_focused_form(&mut self) {
        let sent = self.tab().doc.focused_node().and_then(|id| self.tab().doc.submit(id));
        self.tab_mut().doc.blur();
        let Some(sent) = sent else { return };
        let target = submission_url(&self.tab().address, &sent);
        self.go_to(target);
    }

    /// Tell the page which element the cursor is over, for `:hover`, and track
    /// which chrome control it is on, for highlighting and tooltips.
    fn update_hover(&mut self) {
        let (cx, cy) = self.cursor;
        let over = self.hit_at(cx, cy).map(str::to_string);
        if over != self.hovered {
            self.hovered = over;
            self.request_redraw();
        }
        if !self.tab().uses_hover {
            return;
        }
        let regions = self.regions();
        let hit = self.page_coords((cx, cy), &regions).and_then(|(px, py)| {
            self.tab()
                .element_rects
                .iter()
                .filter(|r| px >= r.x && px <= r.x + r.width && py >= r.y && py <= r.y + r.height)
                .map(|r| r.node_id)
                .next_back()
        });
        let tab = self.tab_mut();
        if tab.doc.set_hover(hit) {
            tab.page_canvas = None;
            self.request_redraw();
        }
    }

    /// Window coordinates to page coordinates, undoing the chrome offset, the
    /// scroll position and the tab's zoom. `None` when the point is not on the page.
    fn page_coords(&self, (cx, cy): (f32, f32), regions: &Regions) -> Option<(f32, f32)> {
        let page_right = (regions.content_x + regions.content_w) as f32;
        if cx < regions.content_x as f32 || cy < regions.content_y as f32 || cx >= page_right {
            return None;
        }
        let zoom = self.tab().zoom_factor();
        Some((
            (cx - regions.content_x as f32) / zoom,
            (cy - regions.content_y as f32 + self.tab().scroll_y) / zoom,
        ))
    }

    /// Open the current page's markup in a new tab.
    ///
    /// It goes through the engine like any other page, so what you read is what
    /// Zero was actually served — useful when a site renders unexpectedly.
    fn view_source(&mut self) {
        let (address, source) = {
            let tab = self.tab();
            (tab.address.clone(), tab.source.clone())
        };
        if source.is_empty() {
            return;
        }
        let html = crate::internal::source_page(&address, &source);
        self.tabs.push(Tab::new(format!("view-source:{address}"), html, String::new()));
        self.active = self.tabs.len() - 1;
        self.request_redraw();
    }

    // --- chrome fields (find in page, tab search) ---

    fn open_find(&mut self) {
        self.focus = Focus::Find(String::new());
        self.menu_open = false;
        self.tab_mut().doc.blur(); // typing belongs to the find bar now
        self.request_redraw();
    }

    fn open_tab_search(&mut self) {
        // Searching tabs you cannot see is no help, so open the rail with it.
        if self.settings.rail != Rail::Expanded || self.settings.layout != TabLayout::Vertical {
            let settings = Settings {
                layout: TabLayout::Vertical,
                rail: Rail::Expanded,
                ..self.settings
            };
            self.store_settings(settings);
        }
        self.focus = Focus::TabSearch(String::new());
        self.menu_open = false;
        self.tab_mut().doc.blur();
        self.request_redraw();
    }

    fn close_chrome_field(&mut self) {
        if matches!(self.focus, Focus::Find(_)) {
            self.tab_mut().doc.set_find(None);
            self.tab_mut().page_canvas = None; // drop the highlights
        }
        self.focus = Focus::Address;
    }

    /// Push the field's live text at whatever it filters.
    fn apply_chrome_field(&mut self) {
        if let Focus::Find(query) = &self.focus {
            let query = query.clone();
            let tab = self.tab_mut();
            tab.doc.set_find(Some(query));
            tab.page_canvas = None; // highlights are painted, so re-render
        }
        // Tab search needs nothing: the rail is rebuilt from the query each frame.
    }

    /// Enter: the next match for find, the first matching tab for tab search.
    fn accept_chrome_field(&mut self) {
        match self.focus {
            Focus::Find(_) => self.jump_to_match(),
            Focus::TabSearch(_) => {
                if let Some(index) = self.rail_order().first().copied() {
                    self.active = index;
                    self.focus = Focus::Address;
                }
            }
            Focus::Address => {}
        }
    }

    /// Scroll to the first match below the current position, wrapping at the end.
    fn jump_to_match(&mut self) {
        let regions = self.regions();
        let viewport = regions.content_h as f32;
        let tab = self.tab_mut();
        // Matches are in document order, so "next" is the first one past the top
        // of the viewport; a small margin stops the current match re-matching.
        let top = tab.scroll_y / tab.zoom_factor();
        let next =
            tab.matches.iter().find(|r| r.y > top + 4.0).or_else(|| tab.matches.first()).copied();
        if let Some(rect) = next {
            // Land the match a third of the way down rather than at the very top.
            tab.scroll_y = (rect.y * tab.zoom_factor() - viewport / 3.0).max(0.0);
        }
    }

    /// Save the current page, or unsave it if it is already bookmarked.
    fn toggle_bookmark(&mut self) {
        let url = self.tab().address.clone();
        if url.is_empty() || crate::internal::is_internal(&url) {
            return; // nothing worth saving
        }
        if !storage::remove_bookmark(&url) {
            let title = self.tab().label();
            storage::add_bookmark(&url, &title);
        }
        self.request_redraw(); // the star changed
    }

    /// Scroll from a click or drag on the scrollbar track.
    fn scroll_to_cursor(&mut self, y: f32, regions: &Regions) {
        let top = regions.content_y as f32;
        let viewport = regions.content_h as f32;
        let tab = self.tab_mut();
        let content = tab.page_canvas.as_ref().map_or(0.0, |c| c.height as f32) * tab.zoom_factor();
        tab.scroll_y = scroll_for_cursor(content, viewport, y - top);
    }

    // --- chrome markup ---

    /// `hot` when the cursor is on this control, so the chrome reflects what the
    /// cursor is on. The chrome is a separate document from the page with no
    /// hover state of its own, so the class is decided here and baked in.
    fn lit(&self, id: &str, base: &str) -> String {
        match self.hovered.as_deref() == Some(id) {
            true => format!("{base} hot"),
            false => base.to_string(),
        }
    }

    fn toolbar_html(&self, regions: &Regions) -> String {
        let tab = self.tab();
        // With the find bar open it replaces the address, since it owns typing.
        if let Focus::Find(query) = &self.focus {
            let count = tab.matches.len();
            let hits = match (query.is_empty(), count) {
                (true, _) => "type to search this page".to_string(),
                (false, 0) => "no matches".to_string(),
                (false, n) => format!("{n} matches — Enter for next, Esc to close"),
            };
            return format!(
                "<html><body><div id=\"bar\">\
                 <span class=\"btn\">{}</span>\
                 <span class=\"addr\">{}| <span class=\"hint\">{hits}</span></span>\
                 </div></body></html>",
                icon::FIND,
                escape(query)
            );
        }
        // The padlock is the one claim the address bar makes, so it says plainly
        // when a page arrived over cleartext. A built-in page made no connection
        // at all, so it claims nothing.
        let lock = match (crate::internal::is_internal(&tab.address), tab.secure) {
            (true, _) => String::new(),
            (false, true) => format!("<span class=\"lock\">{} </span>", icon::SECURE),
            (false, false) => format!("<span class=\"warn\">{} not secure </span>", icon::INSECURE),
        };
        let shield = match tab.blocked_count {
            0 => String::new(),
            n => format!(" <span id=\"shield\" class=\"hint\">{} {n}</span>", icon::SHIELD),
        };
        // A zoom that is not 100% has to be visible, or a page just looks wrong.
        let zoom = match tab.zoom {
            100 => String::new(),
            z => format!("<span id=\"zoom\" class=\"badge\">{z}%</span>"),
        };
        // Only the buttons that are actually drawn get counted, so the address
        // pill fills exactly the width left over rather than wrapping out of the bar.
        let mut left = String::new();
        let mut buttons = 4; // back, forward, reload, menu
        if self.settings.layout == TabLayout::Vertical {
            let glyph = match self.settings.rail {
                Rail::Hidden => icon::EXPAND,
                _ => icon::COLLAPSE,
            };
            left.push_str(&format!(
                "<span id=\"rail\" class=\"{}\">{glyph}</span>",
                self.lit("rail", "btn")
            ));
            buttons += 1;
        }
        // With no rail on screen there is nowhere else to open a tab from.
        let hidden_rail = self.settings.layout == TabLayout::Vertical
            && self.settings.rail == Rail::Hidden;
        if hidden_rail {
            left.push_str(&format!(
                "<span id=\"new\" class=\"{}\">{}</span>",
                self.lit("new", "btn"),
                icon::ADD
            ));
            buttons += 1;
        }
        let bookmarked = storage::is_bookmarked(&tab.address);
        let mut right = format!(
            "<span id=\"star\" class=\"{}\">{}</span>\
             <span id=\"marks\" class=\"{}\">{}</span>\
             <span id=\"ai\" class=\"{}\">{}</span>",
            self.lit("star", if bookmarked { "on" } else { "btn" }),
            if bookmarked { icon::STAR_FULL } else { icon::STAR_EMPTY },
            self.lit("marks", "btn"),
            icon::BOOKMARKS,
            self.lit("ai", if self.ai_open { "on" } else { "btn" }),
            icon::ASSISTANT,
        );
        buttons += 3;
        right.push_str(&format!(
            "<span id=\"overflow\" class=\"{}\">{}</span>",
            self.lit("overflow", "btn"),
            icon::MENU
        ));
        let addr_width = regions
            .toolbar_w()
            .saturating_sub(BUTTON_SPAN * buttons + 44 + if zoom.is_empty() { 0 } else { 52 })
            .max(80);
        let back = self.lit("back", if tab.history_index > 0 { "btn" } else { "off" });
        let fwd =
            self.lit("fwd", if tab.history_index + 1 < tab.history.len() { "btn" } else { "off" });
        format!(
            "<html><head><style>.addr{{width:{addr_width}px;}}</style></head>\
             <body><div id=\"bar\">{left}\
             <span id=\"back\" class=\"{back}\">{}</span>\
             <span id=\"fwd\" class=\"{fwd}\">{}</span>\
             <span id=\"reload\" class=\"{}\">{}</span>\
             <span class=\"addr\">{lock}{}|{shield}</span>{zoom}{right}\
             </div></body></html>",
            icon::BACK,
            icon::FORWARD,
            self.lit("reload", "btn"),
            icon::RELOAD,
            escape(&tab.address),
        )
    }

    /// Toolbar styling. Buttons are a fixed square so their glyphs sit centred
    /// rather than lopsided; the pill's width is injected per frame because it
    /// depends on how many buttons the current layout draws.
    fn toolbar_css() -> String {
        format!(
            "body{{background:{bar};color:{text};font-size:14px;}} \
             #bar{{padding:8px;height:28px;\
                  border-bottom-width:1px;border-color:{line};}} \
             .btn{{display:inline-block;background:{surface};color:{text};width:24px;\
                  padding:6px;border-radius:8px;text-align:center;}} \
             .off{{display:inline-block;background:{bar};color:{faint};width:24px;\
                  padding:6px;border-radius:8px;text-align:center;}} \
             .on{{display:inline-block;background:{surface};color:{saved};width:24px;\
                 padding:6px;border-radius:8px;text-align:center;}} \
             .hot{{background:{hover};}} \
             .addr{{display:inline-block;background:{surface};color:{text};padding:7px;\
                   border-radius:9px;}} \
             .badge{{display:inline-block;background:{surface};color:{muted};font-size:12px;\
                    padding:7px;border-radius:8px;}} \
             .lock{{color:{ok};}} .warn{{color:{saved};}} .hint{{color:{faint};}}",
            bar = theme::BAR,
            text = theme::TEXT,
            surface = theme::SURFACE,
            hover = theme::HOVER,
            faint = theme::FAINT,
            saved = theme::SAVED,
            muted = theme::MUTED,
            line = theme::LINE,
            ok = theme::OK,
        )
    }

    /// The vertical rail: a wordmark, a tab search field, the tabs, and a way to
    /// open another. Pinned tabs lead.
    fn rail_html(&self, rail_w: u32) -> String {
        let icons = rail_w <= RAIL_ICON_MAX;
        let room = rail_label_room(rail_w);
        let order = self.rail_order();
        let rows: String = order
            .iter()
            .map(|i| {
                let tab = &self.tabs[*i];
                let base = match *i == self.active {
                    true => "tab active",
                    false => "tab",
                };
                let class = self.lit(&format!("tab:{i}"), base);
                if icons {
                    return format!(
                        "<div id=\"tab:{i}\" class=\"{class}\">{}</div>",
                        escape(&initial(&tab.label()))
                    );
                }
                let pin = match tab.pinned {
                    true => format!("<span class=\"pin\">{} </span>", icon::PINNED),
                    false => String::new(),
                };
                format!(
                    "<div id=\"tab:{i}\" class=\"{class}\">\
                     <span class=\"name\">{pin}{}</span>\
                     <span id=\"close:{i}\" class=\"{}\">{}</span></div>",
                    escape(&tab.label_capped(room)),
                    self.lit(&format!("close:{i}"), "x"),
                    icon::CLOSE,
                )
            })
            .collect();
        let new = format!(
            "<div id=\"new\" class=\"{}\">{}{}</div>",
            self.lit("new", "tab new"),
            icon::ADD,
            if icons { String::new() } else { "  New tab".to_string() },
        );
        if icons {
            return format!(
                "<html><body><div id=\"head\">0</div>{rows}{new}</body></html>"
            );
        }
        // The search field replaces the header's subtitle while it is open, so the
        // rail never grows a row it did not have a moment ago.
        let search = match &self.focus {
            Focus::TabSearch(query) => format!(
                "<div id=\"search\" class=\"find on\">{} {}|</div>",
                icon::FIND,
                escape(query)
            ),
            _ => format!(
                "<div id=\"search\" class=\"{}\">{} Search tabs</div>",
                self.lit("search", "find"),
                icon::FIND
            ),
        };
        let empty = match rows.is_empty() {
            true => "<div class=\"none\">No tab matches that.</div>",
            false => "",
        };
        format!(
            "<html><body><div id=\"head\">zero</div>{search}{rows}{empty}{new}</body></html>"
        )
    }

    /// Height is injected so the rail background fills the window, and the rail's
    /// own width because it animates — every inner measurement follows from it.
    fn rail_css(height: u32, rail_w: u32) -> String {
        let icons = rail_w <= RAIL_ICON_MAX;
        // The name gives up whatever the close affordance and the row's own
        // insets need, so a long title is truncated rather than wrapping the
        // close glyph onto a second line.
        let (row_pad, name_w) = match icons {
            true => (0, 0),
            false => (12, rail_name_width(rail_w)),
        };
        format!(
            "body{{background:{chrome};color:{muted};font-size:13px;height:{height}px;\
                  padding-left:8px;padding-right:8px;}} \
             #head{{color:{accent};padding-top:14px;padding-bottom:14px;padding-left:{row_pad}px;\
                   font-size:15px;text-align:{align};}} \
             .find{{color:{faint};font-size:12px;padding-top:8px;padding-bottom:8px;\
                   padding-left:{row_pad}px;border-radius:8px;margin-bottom:6px;}} \
             .on{{background:{surface};color:{text};}} \
             .tab{{padding-top:9px;padding-bottom:9px;padding-left:{row_pad}px;\
                  padding-right:8px;border-radius:9px;\
                  border-left-width:3px;border-color:{chrome};text-align:{align};}} \
             .hot{{background:{hover};color:{text};}} \
             .active{{background:{soft};color:{text};border-left-width:3px;border-color:{accent};}} \
             .new{{color:{faint};margin-top:4px;}} \
             .none{{color:{faint};font-size:12px;padding-top:10px;padding-left:{row_pad}px;}} \
             .pin{{color:{accent};font-size:9px;}} \
             .name{{display:inline-block;width:{name_w}px;}} \
             .x{{display:inline-block;width:24px;color:{faint};text-align:right;\
                border-radius:6px;}}",
            align = if icons { "center" } else { "left" },
            chrome = theme::CHROME,
            surface = theme::SURFACE,
            soft = theme::accent_soft(),
            hover = theme::HOVER,
            text = theme::TEXT,
            muted = theme::MUTED,
            faint = theme::FAINT,
            accent = theme::accent(),
        )
    }

    /// The rail's footer: a permanent home for settings, pinned to the bottom of
    /// the window by being its own surface rather than by padding arithmetic.
    fn rail_foot_html(&self, icons: bool) -> String {
        let settings = format!(
            "<span id=\"go:settings\" class=\"{}\">{}{}</span>",
            self.lit("go:settings", "foot"),
            icon::SETTINGS,
            if icons { String::new() } else { "  Settings".to_string() },
        );
        let downloads = match icons {
            true => String::new(),
            false => format!(
                "<span id=\"go:downloads\" class=\"{}\">{}</span>",
                self.lit("go:downloads", "foot"),
                icon::DOWNLOAD
            ),
        };
        format!("<html><body><div id=\"row\">{settings}{downloads}</div></body></html>")
    }

    fn rail_foot_css(icons: bool) -> String {
        format!(
            "body{{background:{chrome};color:{muted};font-size:13px;\
                  padding-left:8px;padding-right:8px;}} \
             #row{{display:flex;justify-content:{justify};align-items:center;\
                  padding-top:10px;border-top-width:1px;border-color:{line};height:24px;}} \
             .foot{{display:inline-block;color:{faint};padding-top:5px;padding-bottom:5px;\
                   padding-left:10px;padding-right:10px;border-radius:8px;}} \
             .hot{{background:{hover};color:{text};}}",
            justify = if icons { "center" } else { "space-between" },
            chrome = theme::CHROME,
            hover = theme::HOVER,
            text = theme::TEXT,
            muted = theme::MUTED,
            faint = theme::FAINT,
            line = theme::LINE,
        )
    }

    /// The horizontal tab strip. Tabs that do not fit are reachable from tab
    /// search and Ctrl+Tab.
    ///
    /// ponytail: no scrolling or overflow chevron — the count tells you how many
    /// are hidden. Add a scroll offset here if people start living past tab 8.
    fn strip_html(&self, regions: &Regions) -> String {
        let order = self.rail_order();
        let room = (regions.width.saturating_sub(140) / STRIP_TAB_W).max(1) as usize;
        let shown = order.len().min(room);
        let tabs: String = order[..shown]
            .iter()
            .map(|i| {
                let tab = &self.tabs[*i];
                let base = match *i == self.active {
                    true => "tab active",
                    false => "tab",
                };
                let pin = match tab.pinned {
                    true => format!("<span class=\"pin\">{} </span>", icon::PINNED),
                    false => String::new(),
                };
                // A strip tab is narrower than a rail row, so it names itself
                // more briefly. The tooltip still gives the full title.
                let room = if tab.pinned { 14 } else { 16 };
                format!(
                    "<span id=\"tab:{i}\" class=\"{}\"><span class=\"name\">{pin}{}</span>\
                     <span id=\"close:{i}\" class=\"{}\">{}</span></span>",
                    self.lit(&format!("tab:{i}"), base),
                    escape(&tab.label_capped(room)),
                    self.lit(&format!("close:{i}"), "x"),
                    icon::CLOSE,
                )
            })
            .collect();
        let more = match order.len() - shown {
            0 => String::new(),
            n => format!("<span class=\"more\">+{n}</span>"),
        };
        format!(
            "<html><body><div id=\"strip\"><span class=\"mark\">zero</span>{tabs}\
             <span id=\"new\" class=\"{}\">{}</span>{more}\
             <span id=\"rail\" class=\"{}\">{}</span></div></body></html>",
            self.lit("new", "add"),
            icon::ADD,
            self.lit("rail", "add"),
            icon::COLLAPSE,
        )
    }

    fn strip_css() -> String {
        format!(
            "body{{background:{chrome};color:{muted};font-size:13px;}} \
             #strip{{padding-left:10px;padding-top:6px;height:26px;\
                    border-bottom-width:1px;border-color:{line};}} \
             .mark{{display:inline-block;color:{accent};width:44px;font-size:15px;\
                   padding-top:5px;padding-bottom:5px;}} \
             .tab{{display:inline-block;width:{tab_w}px;padding-top:5px;padding-bottom:5px;\
                  padding-left:10px;padding-right:6px;border-radius:8px;\
                  border-bottom-width:2px;border-color:{chrome};}} \
             .active{{display:inline-block;width:{tab_w}px;background:{soft};color:{text};\
                     padding-top:5px;padding-bottom:5px;padding-left:10px;padding-right:6px;\
                     border-radius:8px;border-bottom-width:2px;border-color:{accent};}} \
             .hot{{background:{hover};color:{text};}} \
             .name{{display:inline-block;width:{name_w}px;}} \
             .x{{display:inline-block;width:18px;color:{faint};text-align:right;}} \
             .add{{display:inline-block;color:{muted};width:20px;padding:5px;\
                  border-radius:7px;text-align:center;}} \
             .more{{display:inline-block;color:{faint};font-size:12px;padding:6px;}} \
             .pin{{color:{accent};font-size:9px;}}",
            tab_w = STRIP_TAB_W - 32,
            name_w = STRIP_TAB_W - 32 - 34,
            chrome = theme::CHROME,
            soft = theme::accent_soft(),
            hover = theme::HOVER,
            text = theme::TEXT,
            muted = theme::MUTED,
            faint = theme::FAINT,
            accent = theme::accent(),
            line = theme::LINE,
        )
    }

    fn menu_html(&self) -> String {
        let items: String = MENU_ITEMS
            .iter()
            .map(|(id, label, key)| {
                if id.is_empty() {
                    return "<div class=\"rule\"></div>".to_string();
                }
                if *id == "menu:zoom" {
                    // Zoom is a value, not a destination, so it gets a stepper.
                    // Laid out in normal flow rather than as a flex row: the
                    // engine will not hold three small boxes on one flex line.
                    return format!(
                        "<div class=\"zoom\"><span class=\"zlabel\">{zoom_label}</span>\
                         <span id=\"zoom:out\" class=\"{}\">{}</span>\
                         <span id=\"zoom:reset\" class=\"{}\">{}%</span>\
                         <span id=\"zoom:in\" class=\"{}\">{}</span></div>",
                        self.lit("zoom:out", "step"),
                        icon::MINUS,
                        self.lit("zoom:reset", "level"),
                        self.tab().zoom,
                        self.lit("zoom:in", "step"),
                        icon::ADD,
                        zoom_label = escape(&t("Zoom")),
                    );
                }
                let label = match *id {
                    "menu:split" if self.focused_pane().is_some() => "Close split view",
                    "menu:pin" if self.tab().pinned => "Unpin this tab",
                    "menu:reopen" if self.closed.is_empty() => return String::new(),
                    _ => label,
                };
                let label = escape(&t(label));
                format!(
                    "<div id=\"{id}\" class=\"{}\"><span class=\"label\">{label}</span>\
                     <span class=\"key\">{key}</span></div>",
                    self.lit(id, "row item"),
                )
            })
            .collect();
        format!("<html><body>{items}</body></html>")
    }

    fn menu_css() -> String {
        format!(
            "body{{background:{bar};color:{text};font-size:13px;\
                  border-width:1px;border-color:{line};\
                  padding-top:6px;padding-bottom:6px;padding-left:6px;padding-right:6px;}} \
             .row{{display:flex;justify-content:space-between;align-items:center;\
                  padding-top:8px;padding-bottom:8px;padding-left:10px;padding-right:10px;\
                  border-radius:7px;}} \
             .item{{color:{text};}} \
             .hot{{background:{hover};}} \
             .rule{{height:1px;background:{line};margin-top:6px;margin-bottom:6px;}} \
             .label{{color:{text};}} \
             .key{{color:{faint};font-size:12px;}} \
             .zoom{{padding-top:8px;padding-bottom:8px;\
                   padding-left:10px;padding-right:10px;}} \
             .zlabel{{display:inline-block;color:{text};width:96px;}} \
             .step{{display:inline-block;background:{surface};color:{text};width:16px;\
                   padding:4px;border-radius:6px;text-align:center;}} \
             .level{{display:inline-block;color:{muted};width:44px;font-size:12px;\
                    padding:4px;text-align:center;border-radius:6px;}}",
            bar = theme::BAR,
            surface = theme::SURFACE,
            hover = theme::HOVER,
            text = theme::TEXT,
            muted = theme::MUTED,
            faint = theme::FAINT,
            line = theme::LINE,
        )
    }

    fn tooltip_css() -> String {
        format!(
            "body{{background:{surface};color:{text};font-size:12px;\
                  border-width:1px;border-color:{line};}} \
             #tip{{padding-top:6px;padding-bottom:6px;padding-left:10px;padding-right:10px;\
                  text-align:center;}}",
            surface = theme::SURFACE,
            text = theme::TEXT,
            line = theme::LINE,
        )
    }

    /// What the cursor is resting on, if it has something to say. Tab rows
    /// describe themselves, which is the whole point of the icon rail.
    fn tooltip_text(&self) -> Option<String> {
        if self.menu_open {
            return None; // an open menu already names everything it offers
        }
        let id = self.hovered.as_deref()?;
        if let Some(index) = id.strip_prefix("tab:").and_then(|i| i.parse::<usize>().ok()) {
            return self.tabs.get(index).map(|tab| tab.label());
        }
        if id.starts_with("close:") {
            return Some(t_tip("Close tab  ·  Ctrl+W"));
        }
        TIPS.iter().find(|(key, _)| *key == id).map(|(_, tip)| t_tip(tip))
    }

    // --- compositing ---

    /// Lay out and paint one tab at the given size, reusing its cached canvas
    /// when nothing about the page or the space it has changed.
    fn render_pane(&mut self, index: usize, w: f32, h: f32) {
        let now = self.started.elapsed().as_secs_f32() * 1000.0;
        let animating = self.page_animating;
        let engine = &self.engine;
        let tab = &mut self.tabs[index];
        let settled = tab.page_canvas.is_some()
            && tab.cache_w == w as u32
            && tab.cache_h == h as u32
            && !animating;
        if settled {
            return;
        }
        tab.doc.set_time(now);
        let loader = tab.loader.clone();
        let render_start = std::time::Instant::now();
        let page = engine.render_document(&mut tab.doc, w, h, loader.as_ref());
        if timing_wanted() {
            eprintln!("page render {:?}", render_start.elapsed());
        }
        tab.blocked_count = loader.blocked.get();
        for line in &page.console {
            eprintln!("[js] {line}");
        }
        let page_animating = page.animating;
        tab.page_canvas = Some(page.canvas);
        tab.links = page.links;
        tab.matches = page.find_matches;
        tab.uses_hover = page.uses_hover;
        tab.element_rects = page.element_rects;
        tab.cache_w = w as u32;
        tab.cache_h = h as u32;
        self.page_animating = page_animating;
    }

    /// Which tab the cursor is over: the other pane if it is in it, else the
    /// focused one. Used by the wheel, which should not need a click first.
    fn pane_under_cursor(&self) -> usize {
        let regions = self.regions();
        let cx = self.cursor.0;
        let in_other = regions.other_w > 0
            && cx >= regions.other_x as f32
            && cx < (regions.other_x + regions.other_w) as f32;
        match (in_other, self.split) {
            (true, Some(other)) => other,
            _ => self.active,
        }
    }

    /// Is the cursor in the gap between two panes?
    fn on_divider(&self, cx: f32, regions: &Regions) -> bool {
        match regions.divider_x() {
            Some(x) => cx >= x as f32 && cx < (x + DIVIDER_W) as f32,
            None => false,
        }
    }

    /// Render a chrome document and record where it painted each of its controls,
    /// in window coordinates. `interactive` documents contribute to hit-testing;
    /// tooltips do not, because you cannot click a tooltip.
    fn chrome(
        engine: &Engine,
        hits: &mut Vec<Hit>,
        html: &str,
        css: &str,
        (x, y): (u32, u32),
        (w, h): (u32, u32),
        interactive: bool,
    ) -> Canvas {
        let loader = ShellLoader::new(String::new());
        let page = engine.render_page(html, css, w as f32, h as f32, &loader);
        if interactive {
            hits.extend(page.element_rects.iter().filter(|r| !r.id.is_empty()).map(|r| Hit {
                id: r.id.clone(),
                x: r.x + x as f32,
                y: r.y + y as f32,
                width: r.width,
                height: r.height,
            }));
        }
        page.canvas
    }

    /// Render every region and compose them into one frame (0xRRGGBB per pixel).
    ///
    /// Independent of the window, so a headless screenshot goes through exactly
    /// the same path the user sees rather than a second, drifting copy.
    fn frame(&mut self, w: u32, h: u32) -> Vec<u32> {
        self.animating = self.advance_rail();
        // A page mid-transition wants frames for the same reason the rail does.
        self.page_animating = false;
        let regions = Regions::split(
            w,
            h,
            self.settings,
            self.ai_open,
            self.rail_px.round() as u32,
            self.focused_pane(),
            self.split_ratio,
        );
        let zoom = self.tab().zoom_factor();

        // Re-render the active tab only when its page or layout size changed;
        // scrolling and tab switching just re-blit cached canvases. The page is
        // laid out at the zoomed-down width, so zooming reflows rather than crops.
        let (layout_w, layout_h) = layout_size(regions.content_w, regions.content_h, zoom);
        // The other half of a split lays out at its own width and scrolls on its
        // own; only the focused pane's tab drives the toolbar.
        if regions.other_w > 0 {
            if let Some(other) = self.split {
                let (ow, oh) = layout_size(regions.other_w, regions.content_h, zoom);
                self.render_pane(other, ow, oh);
                let content = self.tabs[other]
                    .page_canvas
                    .as_ref()
                    .map_or(0.0, |c| c.height as f32)
                    * zoom;
                let max_scroll = (content - regions.content_h as f32).max(0.0);
                self.tabs[other].scroll_y = self.tabs[other].scroll_y.clamp(0.0, max_scroll);
            }
        }
        {
            // Mid-slide the content area changes width every frame, and
            // re-laying out a real page at 60fps would make the rail stutter.
            // The last layout is slid instead, and reflows once the rail lands —
            // which is what every other browser does with an animating panel.
            let tab = &self.tabs[self.active];
            let stale = tab.cache_w != layout_w as u32 || tab.cache_h != layout_h as u32;
            if tab.page_canvas.is_none() || self.page_animating || (stale && !self.animating) {
                self.render_pane(self.active, layout_w, layout_h);
            }
            let tab = &mut self.tabs[self.active];
            // Clamp scroll to available overflow, in screen pixels.
            let content = tab.page_canvas.as_ref().expect("just rendered").height as f32 * zoom;
            let max_scroll = (content - regions.content_h as f32).max(0.0);
            tab.scroll_y = tab.scroll_y.clamp(0.0, max_scroll);
        }
        let scroll = self.tab().scroll_y;

        // Chrome is cheap; render fresh each frame so typing and tab changes show.
        let mut hits = Vec::new();
        let engine = &self.engine;
        let mut surfaces: Vec<(Canvas, u32, u32)> = Vec::new();
        if regions.rail_w > 0 {
            let icons = regions.rail_w <= RAIL_ICON_MAX;
            let list_h = regions.rail_list_h();
            surfaces.push((
                Self::chrome(
                    engine,
                    &mut hits,
                    &self.rail_html(regions.rail_w),
                    &Self::rail_css(list_h, regions.rail_w),
                    (0, 0),
                    (regions.rail_w, list_h),
                    true,
                ),
                0,
                0,
            ));
            surfaces.push((
                Self::chrome(
                    engine,
                    &mut hits,
                    &self.rail_foot_html(icons),
                    &Self::rail_foot_css(icons),
                    (0, list_h),
                    (regions.rail_w, RAIL_FOOT_H),
                    true,
                ),
                0,
                list_h,
            ));
        }
        if regions.strip_h > 0 {
            surfaces.push((
                Self::chrome(
                    engine,
                    &mut hits,
                    &self.strip_html(&regions),
                    &Self::strip_css(),
                    (0, 0),
                    (regions.width, regions.strip_h),
                    true,
                ),
                0,
                0,
            ));
        }
        surfaces.push((
            Self::chrome(
                engine,
                &mut hits,
                &self.toolbar_html(&regions),
                &Self::toolbar_css(),
                (regions.rail_w, regions.strip_h),
                (regions.toolbar_w(), TOOLBAR_H),
                true,
            ),
            regions.rail_w,
            regions.strip_h,
        ));
        if regions.ai_w > 0 {
            let x = regions.width - regions.ai_w;
            surfaces.push((
                Self::chrome(
                    engine,
                    &mut hits,
                    &self.ai_html(),
                    Self::ai_css(),
                    (x, regions.content_y),
                    (regions.ai_w, regions.content_h),
                    false,
                ),
                x,
                regions.content_y,
            ));
        }

        // --- compose ---
        let compose_start = std::time::Instant::now();
        // Starts as the canvas colour rather than black, because mid-animation
        // the page can be narrower than the area it is being slid into.
        let mut buffer = vec![CANVAS_RGB; (w * h) as usize];
        let page = self.tabs[self.active].page_canvas.as_ref().expect("rendered above");
        blit_page(
            &mut buffer,
            w,
            h,
            page,
            (regions.content_x, regions.content_y, regions.content_w),
            scroll,
            zoom,
        );
        // The other pane, drawn the same way the focused one just was.
        if regions.other_w > 0 {
            if let Some(other) = self.split {
                let tab = &self.tabs[other];
                let zoom = tab.zoom_factor();
                let scroll = tab.scroll_y;
                if let Some(canvas) = tab.page_canvas.as_ref() {
                    blit_page(
                        &mut buffer,
                        w,
                        h,
                        canvas,
                        (regions.other_x, regions.content_y, regions.other_w),
                        scroll,
                        zoom,
                    );
                }
            }
            // The divider: a hairline in the gap, so the two pages read as two.
            let start = regions.divider_x().unwrap_or(0);
            for y in regions.content_y..h {
                for x in start..(start + DIVIDER_W).min(w) {
                    let edge = x == start + DIVIDER_W / 2;
                    buffer[(y * w + x) as usize] = if edge { 0x3a3d45 } else { CANVAS_RGB };
                }
            }
        }
        for (canvas, x, y) in &surfaces {
            blit(&mut buffer, w, h, canvas, *x, *y);
        }

        // Scrollbar: a track down the right edge of the page, with a thumb sized
        // to the visible fraction. Only shown when the page actually overflows.
        let content_h = page.height as f32 * zoom;
        if let Some((offset, thumb_h)) =
            scrollbar_thumb(content_h, regions.content_h as f32, scroll)
        {
            let bar_w = SCROLLBAR_W;
            let x0 = (regions.content_x + regions.content_w).saturating_sub(bar_w);
            let thumb_top = regions.content_y + offset as u32;
            for y in regions.content_y..h {
                for x in x0..(x0 + bar_w).min(w) {
                    let on_thumb = y >= thumb_top && y < thumb_top + thumb_h as u32;
                    buffer[(y * w + x) as usize] = if on_thumb { 0x5f636d } else { 0x1a1c21 };
                }
            }
        }

        // Overlays last, so they sit above the page and the chrome alike.
        if self.menu_open {
            let x = regions.width.saturating_sub(MENU_W + 10);
            let y = regions.content_y + 4;
            let menu = Self::chrome(
                engine,
                &mut hits,
                &self.menu_html(),
                &Self::menu_css(),
                (x, y),
                (MENU_W, 1), // height comes from the content
                true,
            );
            blit(&mut buffer, w, h, &menu, x, y);
        }
        if let Some(text) = self.tooltip_text() {
            if let Some((x, y, tw)) = self.tooltip_box(&text, &hits, &regions) {
                let tip = Self::chrome(
                    engine,
                    &mut hits,
                    &format!("<html><body><div id=\"tip\">{}</div></body></html>", escape(&text)),
                    &Self::tooltip_css(),
                    (x, y),
                    (tw, 1),
                    false,
                );
                blit(&mut buffer, w, h, &tip, x, y);
            }
        }

        self.hits = hits;
        if timing_wanted() {
            eprintln!("compose {:?}", compose_start.elapsed());
        }
        buffer
    }

    /// Where a tooltip goes: beside the rail so it does not cover the tab it
    /// names, below anything else, and always inside the window.
    ///
    /// ponytail: the width is estimated from the character count rather than
    /// measured, so a proportional font leaves a little slack — which centring
    /// spends symmetrically. Ask the engine to measure if it ever looks wrong.
    fn tooltip_box(&self, text: &str, hits: &[Hit], regions: &Regions) -> Option<(u32, u32, u32)> {
        let id = self.hovered.as_deref()?;
        let anchor = hits.iter().find(|hit| hit.id == id)?;
        let width = (text.chars().count() as u32 * 7 + 24).clamp(72, 280);
        let in_rail = regions.rail_w > 0 && anchor.x < regions.rail_w as f32;
        let (x, y) = match in_rail {
            true => (regions.rail_w + 6, anchor.y as u32),
            false => (
                (anchor.x + anchor.width / 2.0 - width as f32 / 2.0).max(6.0) as u32,
                (anchor.y + anchor.height) as u32 + 8,
            ),
        };
        // Keep the whole tip on screen, including when the control is at the edge.
        let x = x.min(regions.width.saturating_sub(width + 6));
        let y = y.min(regions.height.saturating_sub(40));
        Some((x, y, width))
    }

    fn ai_css() -> &'static str {
        static CSS: std::sync::OnceLock<String> = std::sync::OnceLock::new();
        CSS.get_or_init(|| {
            format!(
                "body{{background:{chrome};color:{muted};font-size:13px;}} \
                 #head{{background:{surface};color:{text};padding:12px;height:20px;}} \
                 .line{{padding:3px;color:{text};}} \
                 .src{{color:{faint};padding:12px;font-size:12px;}}",
                chrome = theme::CHROME,
                surface = theme::SURFACE,
                text = theme::TEXT,
                muted = theme::MUTED,
                faint = theme::FAINT,
            )
        })
    }

    /// Blit a composed frame to the window.
    fn render(&mut self) {
        let (w, h) = self.window_size();
        if w == 0 || h == 0 {
            return;
        }
        let frame = self.frame(w, h);
        let Some(surface) = self.surface.as_mut() else { return };
        surface
            .resize(NonZeroU32::new(w).unwrap(), NonZeroU32::new(h).unwrap())
            .expect("surface resize");
        let mut buffer = surface.buffer_mut().expect("surface buffer");
        buffer.copy_from_slice(&frame);
        buffer.present().expect("buffer present");
        // An unfinished animation asks for the next frame itself — the rail
        // sliding, or a page mid-transition. Nothing else drives a clock, so the
        // window goes back to sleep the moment they land.
        if self.animating || self.page_animating {
            self.request_redraw();
        }
    }
}

/// Copy a rendered surface into the window buffer, clipped at its edges.
/// Blit a page canvas into one pane: `(x, y, width)` in the window, scrolled and
/// zoomed. Two panes differ only in where they land and how far each is scrolled.
/// Whether to report where frame time goes (`ZERO_FRAME_TIMES=1`).
///
/// Read once: this is consulted every frame, and the answer cannot change.
fn timing_wanted() -> bool {
    static WANTED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *WANTED.get_or_init(|| std::env::var_os("ZERO_FRAME_TIMES").is_some())
}

/// Is this something a system browser should be handed — an `http(s)` address,
/// and not a local path, a `zero://` page, or a scheme a page chose?
fn is_web_url(url: &str) -> bool {
    let lowered = url.trim().to_ascii_lowercase();
    (lowered.starts_with("http://") || lowered.starts_with("https://"))
        && !lowered.contains(char::is_whitespace)
}

fn blit_page(
    buffer: &mut [u32],
    w: u32,
    h: u32,
    page: &Canvas,
    (x0, y0, pane_w): (u32, u32, u32),
    scroll: f32,
    zoom: f32,
) {
    let inv_zoom = 1.0 / zoom;
    let right = (x0 + pane_w).min(w);
    for y in y0..h {
        let sy = ((y - y0) as f32 + scroll) * inv_zoom;
        let sy = (sy as usize).min(page.height.saturating_sub(1));
        let row = sy * page.width;
        // Unzoomed, a row of the page is a row of the window: the per-pixel
        // coordinate arithmetic is the same answer as walking forwards, and it
        // was most of the time spent compositing a frame.
        if zoom == 1.0 {
            let span = (right - x0) as usize;
            let take = span.min(page.width);
            let source = &page.pixels[row..row + take];
            let start = (y * w + x0) as usize;
            for (slot, px) in buffer[start..start + take].iter_mut().zip(source) {
                *slot = (px.r as u32) << 16 | (px.g as u32) << 8 | px.b as u32;
            }
            continue;
        }
        for x in x0..right {
            let sx = ((x - x0) as f32 * inv_zoom) as usize;
            if sx >= page.width {
                break; // a stale layout narrower than the area it is sliding into
            }
            let px = page.pixels[row + sx];
            buffer[(y * w + x) as usize] = (px.r as u32) << 16 | (px.g as u32) << 8 | px.b as u32;
        }
    }
}

fn blit(buffer: &mut [u32], w: u32, h: u32, canvas: &Canvas, x0: u32, y0: u32) {
    for y in 0..canvas.height.min(h.saturating_sub(y0) as usize) {
        for x in 0..canvas.width.min(w.saturating_sub(x0) as usize) {
            let px = canvas.pixels[y * canvas.width + x];
            buffer[(y0 as usize + y) * w as usize + x0 as usize + x] =
                (px.r as u32) << 16 | (px.g as u32) << 8 | px.b as u32;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings_with(layout: TabLayout, rail: Rail) -> Settings {
        Settings { layout, rail, ..Settings::default() }
    }

    #[test]
    fn only_web_addresses_are_handed_to_another_browser() {
        assert!(is_web_url("https://example.org/a?b=1&c=2"));
        assert!(is_web_url("http://example.org"));
        // A built-in page, a local file, and anything with a scheme of the
        // page's choosing stay here.
        assert!(!is_web_url("zero://settings"));
        assert!(!is_web_url("file:///C:/secrets.txt"));
        assert!(!is_web_url("javascript:alert(1)"));
        assert!(!is_web_url(""));
        // Whitespace is how a second argument would be smuggled in.
        assert!(!is_web_url("https://example.org /x"));
    }

    #[test]
    fn a_split_divides_the_content_area_between_two_panes() {
        let settings = settings_with(TabLayout::Vertical, Rail::Hidden);
        let whole = Regions::settled(1000, 700, settings, false);
        let left = Regions::split(1000, 700, settings, false, 0, Some(0), 0.5);
        let right = Regions::split(1000, 700, settings, false, 0, Some(1), 0.5);

        // Every pixel of the content area is one pane, the other, or the divider.
        assert_eq!(left.content_w + left.other_w + DIVIDER_W, whole.content_w);
        // The focused side is the one `content_*` describes, and the two views
        // of the same split agree about where each pane sits.
        assert_eq!(left.content_x, right.other_x);
        assert_eq!(left.other_x, right.content_x);
        assert_eq!(left.divider_x(), right.divider_x());
        assert!(left.content_x < left.other_x, "pane 0 is the left one");

        // The divider sits exactly between them, touching neither.
        let gap = left.divider_x().expect("split");
        assert_eq!(gap, left.content_x + left.content_w);
        assert_eq!(gap + DIVIDER_W, left.other_x);

        // A dragged divider moves the boundary without changing the total.
        let dragged = Regions::split(1000, 700, settings, false, 0, Some(0), 0.25);
        assert!(dragged.content_w < left.content_w);
        assert_eq!(dragged.content_w + dragged.other_w + DIVIDER_W, whole.content_w);
        // An unsplit window has no second pane at all.
        assert_eq!((whole.other_x, whole.other_w), (0, 0));
    }

    #[test]
    fn the_rail_takes_width_from_the_page_and_gives_it_back() {
        let expanded = Regions::settled(1000, 700, settings_with(TabLayout::Vertical, Rail::Expanded), false);
        assert_eq!(expanded.rail_w, RAIL_W);
        assert_eq!(expanded.content_x, RAIL_W);
        assert_eq!(expanded.content_w, 1000 - RAIL_W);
        assert_eq!(expanded.strip_h, 0);

        let icons = Regions::settled(1000, 700, settings_with(TabLayout::Vertical, Rail::Icons), false);
        assert_eq!(icons.rail_w, RAIL_ICON_W);

        // Hidden gives the page the whole window width.
        let hidden = Regions::settled(1000, 700, settings_with(TabLayout::Vertical, Rail::Hidden), false);
        assert_eq!(hidden.rail_w, 0);
        assert_eq!(hidden.content_w, 1000);
        assert_eq!(hidden.content_y, TOOLBAR_H);
    }

    #[test]
    fn horizontal_layout_trades_the_rail_for_a_strip() {
        let regions = Regions::settled(1000, 700, settings_with(TabLayout::Horizontal, Rail::Expanded), false);
        assert_eq!(regions.rail_w, 0, "no rail when tabs are on top");
        assert_eq!(regions.strip_h, TABSTRIP_H);
        // The page starts below both the strip and the toolbar.
        assert_eq!(regions.content_y, TABSTRIP_H + TOOLBAR_H);
        assert_eq!(regions.content_w, 1000);
    }

    #[test]
    fn the_assistant_panel_never_squeezes_the_page_away() {
        let regions = Regions::settled(400, 700, settings_with(TabLayout::Vertical, Rail::Expanded), true);
        assert!(regions.content_w >= 1);
        assert!(regions.rail_w + regions.ai_w <= 400);
        // A window narrower than the chrome still produces a usable page area.
        let tiny = Regions::settled(60, 40, settings_with(TabLayout::Vertical, Rail::Expanded), true);
        assert!(tiny.content_w >= 1 && tiny.content_h >= 1);
    }

    #[test]
    fn the_rail_eases_to_its_target_and_then_stops() {
        let (mut px, target) = (RAIL_W as f32, RAIL_ICON_W as f32);
        let mut frames = 0;
        while px != target {
            let before = px;
            px = ease_toward(px, target, 1.0 / 60.0);
            assert!(px < before, "the rail must keep closing, not stall at {px}");
            frames += 1;
            assert!(frames < 120, "the rail never settled");
        }
        // Between 100ms and 330ms at 60fps: fast enough not to be in the way,
        // slow enough to read as motion. docs/02-UI-UX-SPEC.md §3.5.
        assert!((6..=20).contains(&frames), "settled in {frames} frames");
        // Once there it stays there, so the window can stop redrawing.
        assert_eq!(ease_toward(target, target, 1.0 / 60.0), target);
    }

    #[test]
    fn the_rail_animation_retargets_mid_flight() {
        // Collapse halfway, then change your mind: it must turn around from
        // where it is rather than snapping or restarting.
        let mut px = ease_toward(RAIL_W as f32, 0.0, 0.05);
        assert!(px < RAIL_W as f32 && px > 0.0);
        let turning = ease_toward(px, RAIL_W as f32, 1.0 / 60.0);
        assert!(turning > px, "it should head back out from where it got to");
        px = turning;
        for _ in 0..120 {
            px = ease_toward(px, RAIL_W as f32, 1.0 / 60.0);
        }
        assert_eq!(px, RAIL_W as f32);
    }

    #[test]
    fn turning_motion_off_moves_the_rail_at_once() {
        let mut app = App::new(Engine::shapes_only(), vec![Tab::blank()], 0);
        app.settings = Settings { motion: false, ..Settings::default() };
        app.rail_px = RAIL_W as f32;
        app.settings.rail = Rail::Hidden;
        assert!(!app.advance_rail(), "nothing should be left to animate");
        assert_eq!(app.rail_px, 0.0);
    }

    #[test]
    fn the_rail_squeezes_its_labels_as_it_narrows() {
        // Every width the animation passes through has to produce a layout that
        // fits, not just the two settled ones.
        let mut room = usize::MAX;
        for rail_w in (RAIL_ICON_W..=RAIL_W).rev() {
            let next = rail_label_room(rail_w);
            assert!(next <= room, "room grew as the rail narrowed at {rail_w}");
            assert!(next >= 3, "a label needs some room at {rail_w}");
            room = next;
            // The name can never claim more than the rail has.
            assert!(rail_name_width(rail_w) < rail_w);
        }
        // A rail narrower than its own insets asks for no title width at all,
        // rather than underflowing.
        assert_eq!(rail_name_width(4), 0);
    }

    #[test]
    fn the_canvas_colour_is_the_one_the_chrome_uses() {
        assert_eq!(format!("#{CANVAS_RGB:06x}"), theme::CANVAS);
    }

    #[test]
    fn zooming_in_narrows_the_layout_so_the_page_reflows() {
        let (w, h) = (1000, 800);
        assert_eq!(layout_size(w, h, 1.0), (1000.0, 800.0));
        // At 200% the page is laid out for half the room and then magnified,
        // which is what makes text bigger instead of the page being cropped.
        assert_eq!(layout_size(w, h, 2.0), (500.0, 400.0));
        // Zooming out gives the page more room than the window has.
        assert_eq!(layout_size(w, h, 0.5), (2000.0, 1600.0));
        // A page area of nothing still lays out, rather than dividing to zero.
        assert_eq!(layout_size(0, 0, 2.0), (1.0, 1.0));
    }

    #[test]
    fn zoom_walks_the_scale_and_stops_at_its_ends() {
        assert_eq!(zoom_step(100, 1), 110);
        assert_eq!(zoom_step(100, -1), 90);
        assert_eq!(zoom_step(*ZOOM_STEPS.last().unwrap(), 1), *ZOOM_STEPS.last().unwrap());
        assert_eq!(zoom_step(ZOOM_STEPS[0], -1), ZOOM_STEPS[0]);
        // A value that is not a step snaps to the nearest one before moving.
        assert_eq!(zoom_step(103, 1), 110);
    }

    #[test]
    fn a_click_lands_on_the_topmost_control_that_covers_it() {
        let mut app = App::new(Engine::shapes_only(), vec![Tab::blank()], 0);
        app.hits = vec![
            Hit { id: "back".into(), x: 0.0, y: 0.0, width: 40.0, height: 40.0 },
            // A menu drawn later covers the same pixels and must win.
            Hit { id: "menu:new".into(), x: 20.0, y: 20.0, width: 40.0, height: 40.0 },
        ];
        assert_eq!(app.hit_at(5.0, 5.0), Some("back"));
        assert_eq!(app.hit_at(25.0, 25.0), Some("menu:new"));
        assert_eq!(app.hit_at(500.0, 500.0), None);
    }

    #[test]
    fn tab_ids_survive_the_rail_reordering_pinned_tabs_to_the_top() {
        let mut app = App::new(Engine::shapes_only(), vec![Tab::blank(), Tab::blank(), Tab::blank()], 0);
        app.tabs[2].pinned = true;
        // The pinned tab leads, but every row still carries its real index.
        assert_eq!(app.rail_order(), vec![2, 0, 1]);
        app.act_on("tab:1");
        assert_eq!(app.active, 1, "ids address tabs, not rail positions");
    }

    #[test]
    fn tab_search_filters_by_title_and_address() {
        let mut app = App::new(
            Engine::shapes_only(),
            vec![Tab::new("https://news.ycombinator.com".into(), String::new(), String::new()),
                 Tab::new("https://en.wikipedia.org".into(), String::new(), String::new())],
            0,
        );
        app.focus = Focus::TabSearch("wiki".into());
        assert_eq!(app.rail_order(), vec![1]);
        app.focus = Focus::TabSearch("nothing here".into());
        assert!(app.rail_order().is_empty());
    }

    #[test]
    fn closing_a_tab_remembers_it_so_it_can_come_back() {
        let mut app = App::new(
            Engine::shapes_only(),
            vec![Tab::new("https://a.com".into(), String::new(), String::new()), Tab::blank()],
            0,
        );
        app.close_tab_at(0);
        assert_eq!(app.closed, vec!["https://a.com".to_string()]);
        // Closing the last tab leaves a blank one rather than an empty window.
        app.close_tab_at(0);
        assert_eq!(app.tabs.len(), 1);
        // A new tab was never anywhere, so it is not worth reopening.
        assert_eq!(app.closed, vec!["https://a.com".to_string()]);
    }

    #[test]
    fn the_collapse_control_brings_the_rail_back_from_horizontal_layout() {
        let mut app = App::new(Engine::shapes_only(), vec![Tab::blank()], 0);
        app.settings = settings_with(TabLayout::Horizontal, Rail::Expanded);
        app.cycle_rail();
        assert_eq!(app.settings.layout, TabLayout::Vertical);
        // From there it cycles through the rail's own states.
        app.cycle_rail();
        assert_eq!(app.settings.rail, Rail::Icons);
    }

    /// The href of the first link on the settings page whose value is `value`.
    fn settings_link(value: &str) -> String {
        let page = crate::internal::page("zero://settings");
        let needle = format!("href=\"zero://settings?{value}\"");
        assert!(page.contains(&needle), "no control on the page sets {value}");
        format!("zero://settings?{value}")
    }

    #[test]
    fn clicking_a_control_on_the_settings_page_changes_the_setting() {
        // The whole path a click takes: the href the page actually renders,
        // through link resolution, into the browser's live settings. Resolution
        // is in the middle because that is where it broke — `zero://` was not
        // treated as absolute, so every control on the page did nothing.
        let mut app = App::new(Engine::shapes_only(), vec![Tab::blank()], 0);
        assert_eq!(app.settings.layout, TabLayout::Vertical);

        let href = settings_link("layout=horizontal");
        let target = crate::net::resolve_url("zero://settings", &href);
        assert_eq!(target, href, "the link must survive resolution intact");
        app.go_to(target);
        assert_eq!(app.settings.layout, TabLayout::Horizontal, "the click did not land");
        // And the address that lands in the tab is clean.
        assert_eq!(app.tab().address, "zero://settings");

        // Every other control on the page reaches its setting too.
        for value in ["rail=icons", "zoom=125", "engine=brave", "blocking=off", "restore=off"] {
            let href = settings_link(value);
            app.go_to(crate::net::resolve_url("zero://settings", &href));
        }
        assert_eq!(app.settings.rail, Rail::Icons);
        assert_eq!(app.settings.zoom, 125);
        assert_eq!(app.settings.engine().0, "brave");
        assert!(!app.settings.blocking);
        assert!(!app.settings.restore);
    }

    #[test]
    fn every_control_on_the_settings_page_is_actually_clickable() {
        // The half the test above cannot see: a click only reaches a href if
        // layout produced a link area for it. The controls are `<a>` elements
        // styled `display:inline-block`, and an `<a>` that is its own box used
        // to carry no href down to its text — so the whole page rendered as
        // links you could not click.
        let engine = crate::fonts::build_engine();
        let html = crate::internal::page("zero://settings");
        let loader = ShellLoader::new("zero://settings".to_string());
        let page = engine.render_page(&html, "", 1000.0, 700.0, &loader);
        for value in ["layout=horizontal", "rail=icons", "zoom=125", "engine=brave"] {
            let href = format!("zero://settings?{value}");
            let area = page.links.iter().find(|l| l.href == href);
            let area = area.unwrap_or_else(|| panic!("nothing to click for {value}"));
            assert!(area.width > 0.0 && area.height > 0.0, "{value} has an empty target");
        }
    }

    #[test]
    fn a_settings_link_is_applied_and_then_forgotten() {
        let mut app = App::new(Engine::shapes_only(), vec![Tab::blank()], 0);
        let landed = app.apply_setting_link("zero://settings?rail=hidden".into());
        assert_eq!(landed, "zero://settings", "the query does not belong in history");
        assert_eq!(app.settings.rail, Rail::Hidden);
        // Anything else passes straight through.
        assert_eq!(app.apply_setting_link("https://a.com".into()), "https://a.com");
    }

    #[test]
    fn every_control_with_a_tooltip_is_one_the_chrome_can_act_on() {
        let mut app = App::new(Engine::shapes_only(), vec![Tab::blank()], 0);
        for (id, tip) in TIPS {
            assert!(!tip.is_empty(), "{id} has an empty tooltip");
            assert!(app.act_on(id), "{id} has a tooltip but nothing happens when clicked");
        }
    }

    #[test]
    fn every_menu_entry_does_something() {
        let mut app = App::new(Engine::shapes_only(), vec![Tab::blank()], 0);
        for (id, label, _) in MENU_ITEMS {
            if id.is_empty() {
                continue; // a rule, not an entry
            }
            assert!(!label.is_empty(), "{id} has no label");
            // The zoom row is a label with its own stepper rather than one control.
            let ids: &[&str] = match *id {
                "menu:zoom" => &["zoom:out", "zoom:reset", "zoom:in"],
                other => &[other],
            };
            for id in ids {
                assert!(
                    app.act_on_menu(id) || app.act_on(id),
                    "{id} is in the menu but nothing happens when clicked"
                );
            }
        }
    }

    #[test]
    fn tooltips_stay_inside_the_window() {
        let mut app = App::new(Engine::shapes_only(), vec![Tab::blank()], 0);
        let regions = Regions::settled(800, 600, Settings::default(), false);
        // A control hard against the right edge.
        app.hovered = Some("menu".into());
        let hits = vec![Hit { id: "menu".into(), x: 780.0, y: 10.0, width: 20.0, height: 20.0 }];
        let (x, _, width) = app.tooltip_box("More", &hits, &regions).expect("a box");
        assert!(x + width <= 800, "tip runs off the right edge");
    }

    #[test]
    fn a_rail_tooltip_sits_beside_the_rail_rather_than_over_it() {
        let mut app = App::new(Engine::shapes_only(), vec![Tab::blank()], 0);
        let regions = Regions::settled(1000, 700, Settings::default(), false);
        app.hovered = Some("tab:0".into());
        let hits = vec![Hit { id: "tab:0".into(), x: 8.0, y: 90.0, width: 200.0, height: 34.0 }];
        let (x, y, _) = app.tooltip_box("Hacker News", &hits, &regions).expect("a box");
        assert!(x >= regions.rail_w, "a rail tooltip must not cover the rail");
        assert_eq!(y, 90, "it lines up with the row it names");
    }

    #[test]
    fn tab_labels_prefer_the_page_title_and_fit_the_space_given() {
        assert_eq!(label_for("Hacker News", "https://news.ycombinator.com", 22), "Hacker News");
        // No title: fall back to the host, not the whole URL.
        assert_eq!(
            label_for("", "https://news.ycombinator.com/item?id=1", 22),
            "news.ycombinator.com"
        );
        assert_eq!(label_for("   ", "https://example.com", 22), "example.com");
        // A title too long for the space never exceeds it — the engine has no
        // text-overflow, so anything over would wrap and be clipped instead.
        let title = "Rust (programming language) - Wikipedia";
        for max in [8, 16, 22] {
            let short = label_for(title, "https://x.com", max);
            assert_eq!(short.chars().count(), max, "max {max}");
            assert!(short.ends_with('\u{2026}'), "max {max}");
        }
    }

    #[test]
    fn the_icon_rail_labels_a_tab_with_one_character() {
        assert_eq!(initial("Hacker News"), "H");
        assert_eq!(initial("  wikipedia.org"), "W");
        assert_eq!(initial("...."), "\u{2022}"); // nothing to letter it with
    }

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

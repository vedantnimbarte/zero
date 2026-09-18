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
use crate::renderer;

/// A real renderer spawns a child process, which a unit test's binary can't
/// point at (see [`renderer::FakeRenderer`] for why) — so tests get an
/// in-process stand-in with the same inherent methods instead. Every real
/// run of the browser, and every integration test driving the compiled
/// binary from outside, uses the real one.
#[cfg(not(test))]
type TabRenderer = renderer::TabRenderer;
#[cfg(test)]
type TabRenderer = renderer::FakeRenderer;
use crate::storage;
use std::num::NonZeroU32;
use std::rc::Rc;
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, Ime, KeyEvent, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::{Window, WindowId};
use zero_engine::{Canvas, ElementRect, Engine, TextRun};

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
/// How far the mouse travels with the button down before it is a drag and not
/// a click. Without it, a hand that moves a pixel while clicking a link would
/// select instead of following it.
const DRAG_SLOP: f32 = 3.0;
/// How long after a press a second one still counts as a double-click, and how
/// far it may land from the first. Windows' own default is 500 ms; matching it
/// is what makes the browser feel like the rest of the desktop.
const MULTI_CLICK: std::time::Duration = std::time::Duration::from_millis(500);
const MULTI_CLICK_SLOP: f32 = 4.0;
/// The selection wash, drawn over the painted page rather than into it: the
/// text it covers has to stay readable, so this is a tint, not a fill.
const SELECTION_TINT: (u32, u32, u32) = (56, 110, 226);
const SELECTION_ALPHA: u32 = 90; // out of 255
/// A placeholder viewport for a tab's very first render, before its real
/// size is known — `render_pane` asks again at the window's actual size on
/// the next frame regardless, since `cache_w`/`cache_h` start unset.
const DEFAULT_VIEWPORT: (f32, f32) = (800.0, 600.0);
const MENU_W: u32 = 274;
/// How much page is painted above and below the visible one, as a fraction of
/// the window, so small scrolls move within what the renderer already sent
/// instead of asking for more.
///
/// Half a screen each way absorbs several wheel notches before a round trip,
/// and keeps a band at twice the window — which is what bounds a frame. Raising
/// it buys smoother scrolling and costs pixels in every frame, including on a
/// display far larger than the one this was tuned on.
pub const BAND_MARGIN: f32 = 0.5;
/// The page lies on the window as a card rather than filling a hole in it: this
/// much window shows past its right and bottom edges, and its corners are
/// rounded by [`PAGE_RADIUS`]. Straight from the site, whose `.page` is the same
/// surface (`web/app/globals.css`).
const PAGE_GAP: u32 = 8;
const PAGE_RADIUS: u32 = 12;
/// Menus and tooltips float over the page, so their corners are cut by the
/// compositor rather than by the engine — see [`blit_rounded`].
const POPOVER_RADIUS: u32 = 11;
/// How much horizontal room one toolbar button takes: glyph box plus padding.
const BUTTON_SPAN: u32 = 40;
/// One tab in the horizontal strip, including its close affordance.
const STRIP_TAB_W: u32 = 176;

/// One palette for the whole browser, so the chrome and the built-in pages are
/// recognisably the same product. Named rather than repeated hex, so a change
/// lands everywhere at once. Values follow docs/02-UI-UX-SPEC.md §3.1–3.2 and
/// the website's own chrome (`web/app/globals.css`), which is the same design.
pub mod theme {
    use crate::settings::Theme;

    /// Every colour the chrome draws with. A second theme is a second set of
    /// these, not a second set of call sites.
    pub struct Palette {
        /// The deepest layer, behind pages.
        pub canvas: &'static str,
        /// The tab rail and the toolbar — the window's own surface.
        pub chrome: &'static str,
        /// Menus, tooltips and anything else that floats above the window.
        pub elevated: &'static str,
        /// Buttons, cards, the address pill.
        pub surface: &'static str,
        pub hover: &'static str,
        /// Hairline rules. The engine has one font weight, so structure has to
        /// come from ruled lines and spacing rather than from bolder type.
        pub line: &'static str,
        pub text: &'static str,
        pub muted: &'static str,
        pub faint: &'static str,
        /// A bookmarked page.
        pub saved: &'static str,
        /// A secure connection.
        pub ok: &'static str,
        pub link: &'static str,
        /// The scrollbar thumb, and the hairline between two split panes.
        pub edge: &'static str,
    }

    /// Light: the website's palette, which takes its colours from the logo —
    /// the slate ink of the ring, the white it sits on, and its blue light.
    static LIGHT: Palette = Palette {
        canvas: "#ffffff",
        chrome: "#f3f5f7",
        elevated: "#ffffff",
        surface: "#ffffff",
        hover: "#e7eaee",
        line: "#e3e7eb",
        text: "#1a222b",
        muted: "#5b6570",
        faint: "#8a939c",
        saved: "#d08700",
        ok: "#2a8f5e",
        link: "#2f7fd8",
        edge: "#c8cfd6",
    };

    static DARK: Palette = Palette {
        canvas: "#0e0f12",
        chrome: "#121317",
        elevated: "#16181d",
        surface: "#1e2027",
        hover: "#282b34",
        line: "#262931",
        text: "#e8eaed",
        muted: "#8b919b",
        faint: "#5f646e",
        saved: "#f5a524",
        ok: "#30a46c",
        link: "#66ccff",
        edge: "#5f636d",
    };

    /// The palette in force, which the `theme` preference chooses.
    pub fn palette() -> &'static Palette {
        match crate::settings::current().theme {
            Theme::Light => &LIGHT,
            Theme::Dark => &DARK,
            Theme::System => match system_prefers_dark() {
                true => &DARK,
                false => &LIGHT,
            },
        }
    }

    /// Whether the desktop is set to a dark appearance.
    ///
    /// Read once: an app that restyles itself mid-session because the setting
    /// changed is a nice touch, but reading the registry on every CSS string is
    /// not the way to get it.
    fn system_prefers_dark() -> bool {
        static DARK_DESKTOP: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *DARK_DESKTOP.get_or_init(crate::settings::desktop_prefers_dark)
    }

    pub fn canvas() -> &'static str {
        palette().canvas
    }
    pub fn chrome() -> &'static str {
        palette().chrome
    }
    pub fn elevated() -> &'static str {
        palette().elevated
    }
    pub fn surface() -> &'static str {
        palette().surface
    }
    pub fn hover() -> &'static str {
        palette().hover
    }
    pub fn line() -> &'static str {
        palette().line
    }
    pub fn text() -> &'static str {
        palette().text
    }
    pub fn muted() -> &'static str {
        palette().muted
    }
    pub fn faint() -> &'static str {
        palette().faint
    }
    pub fn saved() -> &'static str {
        palette().saved
    }
    pub fn ok() -> &'static str {
        palette().ok
    }
    pub fn link() -> &'static str {
        palette().link
    }
    pub fn edge() -> &'static str {
        palette().edge
    }

    /// The active tab and the space dot, in the current space's colour — which
    /// is how you can tell at a glance which profile you are typing into.
    pub fn accent() -> &'static str {
        crate::spaces::accent_of(&crate::spaces::current())
    }

    /// `ratio` of `over` blended onto `onto`. Both must be `#rrggbb`.
    pub fn mix(over: &str, onto: &str, ratio: f32) -> String {
        let channel = |at: usize| {
            let hex = |text: &str| u8::from_str_radix(&text[at..at + 2], 16).unwrap_or(0) as f32;
            (hex(onto) + (hex(over) - hex(onto)) * ratio).round() as u8
        };
        format!("#{:02x}{:02x}{:02x}", channel(1), channel(3), channel(5))
    }

    /// A colour as a packed pixel, for the parts of the frame that are filled
    /// directly rather than through the engine.
    pub fn packed(hex: &str) -> u32 {
        u32::from_str_radix(hex.trim_start_matches('#'), 16).unwrap_or(0)
    }
}

/// The icons the chrome draws with: 16px on a 16px grid, a 1.5px stroke, round
/// geometry — one family, the website's (`web/app/chrome.tsx`).
///
/// Each is a function of its colour rather than a constant, because the engine
/// resolves `currentColor` to black: an icon has to be told what it is. They are
/// inline `<svg>`, so the browser's own buttons go through the same SVG
/// rasterizer a page's icons do — if these look wrong, so does the web.
pub mod icon {
    /// The icon size everything here is drawn at.
    pub const SIZE: u32 = 16;

    /// Geometry in, icon out. `fill:none` and the stroke are stated once on the
    /// root, the way an icon set states them.
    fn drawn(color: &str, size: u32, body: &str) -> String {
        format!(
            "<svg width='{size}' height='{size}' viewBox='0 0 16 16' fill='none' \
             stroke='{color}' stroke-width='1.5'>{body}</svg>"
        )
    }

    fn line(color: &str, body: &str) -> String {
        drawn(color, SIZE, body)
    }

    pub fn back(color: &str) -> String {
        line(color, "<path d='M10 3 5 8l5 5'/>")
    }
    pub fn forward(color: &str) -> String {
        line(color, "<path d='m6 3 5 5-5 5'/>")
    }
    pub fn reload(color: &str) -> String {
        line(color, "<path d='M13 8a5 5 0 1 1-1.46-3.54M13.25 2.25v3h-3'/>")
    }
    pub fn star(color: &str, filled: bool) -> String {
        let fill = if filled { color } else { "none" };
        drawn(
            color,
            SIZE,
            &format!(
                "<path fill='{fill}' d='M8 2.4 9.73 5.9l3.87.56-2.8 2.73.66 3.85L8 11.23 \
                 4.54 13.04l.66-3.85-2.8-2.73L6.27 5.9z'/>"
            ),
        )
    }
    /// A page with a corner turned — bookmarks, and the documents they are.
    pub fn bookmarks(color: &str) -> String {
        line(
            color,
            "<path d='M4 1.75h5.5L12.25 4.5v9.75H4z'/>\
             <path d='M9.25 1.75V4.75h3M6.5 8h3.5M6.5 10.75h3.5'/>",
        )
    }
    pub fn history(color: &str) -> String {
        line(
            color,
            "<circle cx='8' cy='8' r='5.75'/><path d='M8 4.75V8.25l2.4 1.4'/>",
        )
    }
    pub fn find(color: &str) -> String {
        line(color, "<circle cx='7.2' cy='7.2' r='4.45'/><path d='M10.6 10.6 13.6 13.6'/>")
    }
    pub fn close(color: &str, size: u32) -> String {
        drawn(color, size, "<path d='m4.75 4.75 6.5 6.5m0-6.5-6.5 6.5'/>")
    }
    pub fn add(color: &str) -> String {
        line(color, "<path d='M8 3.25v9.5M3.25 8h9.5'/>")
    }
    pub fn minus(color: &str) -> String {
        line(color, "<path d='M3.25 8h9.5'/>")
    }
    /// A closed padlock — the one claim the address bar makes.
    pub fn secure(color: &str) -> String {
        line(
            color,
            "<rect x='3.25' y='7' width='9.5' height='6.75' rx='1.75'/>\
             <path d='M5.6 7V5.1a2.4 2.4 0 0 1 4.8 0V7'/>",
        )
    }
    pub fn insecure(color: &str) -> String {
        line(color, "<path d='M8 2.6 14 13.2H2z'/><path d='M8 6.6v2.9'/><path d='M8 11.4v.1'/>")
    }
    /// The shield the site marks a clean page with.
    pub fn shield(color: &str) -> String {
        line(color, "<path d='M8 1.8 13 3.6v4.1c0 3-2.1 5.4-5 6.5-2.9-1.1-5-3.5-5-6.5V3.6z'/>")
    }
    /// The assistant's speech bubble.
    pub fn assistant(color: &str) -> String {
        line(
            color,
            "<path d='M3 3.5h10a1 1 0 0 1 1 1v6a1 1 0 0 1-1 1H7.5L4.5 14v-2.5H3a1 1 0 0 1-1-1v-6a1 1 0 0 1 1-1z'/>",
        )
    }
    pub fn menu(color: &str) -> String {
        drawn(
            color,
            SIZE,
            &format!(
                "<circle fill='{color}' stroke='none' cx='8' cy='3.4' r='1.15'/>\
                 <circle fill='{color}' stroke='none' cx='8' cy='8' r='1.15'/>\
                 <circle fill='{color}' stroke='none' cx='8' cy='12.6' r='1.15'/>"
            ),
        )
    }
    /// The rail control: the rail itself, with the way it will move next.
    pub fn rail(color: &str, expanding: bool) -> String {
        let chevron = match expanding {
            true => "<path d='m8.4 6.2 1.8 1.8-1.8 1.8'/>",
            false => "<path d='M10.2 6.2 8.4 8l1.8 1.8'/>",
        };
        line(color, &format!("<rect x='2' y='3' width='12' height='10' rx='2'/>{chevron}"))
    }
    /// Settings as two sliders rather than a gear: a gear at 16px with a 1.5px
    /// stroke is a smudge, and this says the same thing.
    pub fn settings(color: &str) -> String {
        line(
            color,
            "<path d='M2.5 5.5h11M2.5 10.5h11'/>\
             <circle fill='none' cx='6' cy='5.5' r='1.8'/><circle fill='none' cx='10.5' cy='10.5' r='1.8'/>",
        )
    }
    pub fn download(color: &str) -> String {
        line(color, "<path d='M8 2.5v7.75M4.9 7.4 8 10.5l3.1-3.1M3 13.25h10'/>")
    }
    /// A pinned tab's dot, small enough to sit inside a line of text.
    pub fn pinned(color: &str) -> String {
        drawn(color, 8, &format!("<circle fill='{color}' stroke='none' cx='8' cy='8' r='4'/>"))
    }
    /// The space's dot in the rail's footer.
    pub fn dot(color: &str, size: u32) -> String {
        drawn(color, size, &format!("<circle fill='{color}' stroke='none' cx='8' cy='8' r='4.5'/>"))
    }

    /// The mark: a ring with a gap near one o'clock, and the blue light where
    /// its stroke ends.
    ///
    /// The website draws the light as a gradient. The engine has no gradients in
    /// SVG, so it is three arcs stepping from pale to full blue — at the sizes a
    /// browser draws a logo, the step is the fade.
    pub fn ring(color: &str, size: u32) -> String {
        // 324° of the circle from -58°, which is the site's dash, ending at the
        // gap. The light is painted back along the last 57° from that end, so it
        // reads as the stroke running out rather than as a mark of its own — and
        // its far end blends into the ring, standing in for the fade to nothing.
        let tail = crate::app::theme::mix("#559ff3", color, 0.45);
        let light = [
            ("#b9d8fb", "M47.4 12.1A38 38 0 0 0 35.2 15"),
            ("#559ff3", "M35.2 15A38 38 0 0 0 24.6 21.8"),
            (tail.as_str(), "M24.6 21.8A38 38 0 0 0 17.1 31"),
        ]
        .iter()
        .map(|(shade, arc)| format!("<path stroke='{shade}' d='{arc}'/>"))
        .collect::<String>();
        format!(
            "<svg width='{size}' height='{size}' viewBox='0 0 100 100' fill='none'              stroke='{color}' stroke-width='17'>             <path d='M70.1 17.8A38 38 0 1 1 47.4 12.1'/>{light}</svg>"
        )
    }
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
    ("go:bookmarks", "Bookmarks  ·  Ctrl+B"),
    ("go:history", "History  ·  Ctrl+H"),
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
        // The card's own margin comes off the content area, so everything that
        // reads a region — clicks, scrolling, layout width — already knows the
        // page is smaller than the space below the toolbar.
        let full_w = width.saturating_sub(rail_w + ai_w + PAGE_GAP).max(1);
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
            content_h: height.saturating_sub(content_y + PAGE_GAP).max(1),
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
    // Deliberately not `cookies::site_of`, which drops the scheme and the
    // port. That is right for cookies — they are scoped by host and a cookie
    // set over https is readable over http — but a storage area is scoped by
    // *origin*. Sharing one between `http://example.com` and
    // `https://example.com` would let anything able to tamper with the plain
    // page read and rewrite what the secure one stored, which is the whole
    // reason the web draws this boundary where it does.
    let host = crate::cookies::site_of(address);
    if host.is_empty() {
        return address_label(address);
    }
    let scheme = address.split("://").next().unwrap_or("").to_ascii_lowercase();
    if scheme.is_empty() || scheme == address {
        return host;
    }
    // The port is part of the origin, so it stays. Its absence is itself a
    // distinct origin from any explicit port, which is what the web says.
    let port = address
        .split("://")
        .nth(1)
        .and_then(|rest| rest.split('/').next())
        .and_then(|hostport| hostport.rsplit('@').next())
        .and_then(|hostport| hostport.split(':').nth(1))
        .filter(|port| !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()));
    match port {
        Some(port) => format!("{scheme}://{host}:{port}"),
        None => format!("{scheme}://{host}"),
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

/// What is selected on a page, in page coordinates.
///
/// Kept as the *points* the mouse was at rather than as a list of words,
/// because the words move: a resize, a zoom or a script re-lays the page out
/// under a selection that is still meant to be the same sentence. Resolving it
/// against the current runs each frame is also what lets select-all be one
/// variant rather than a special case threaded through everything.
#[derive(Clone, Copy)]
enum Selection {
    /// Pressed at one point and dragged to another — or extended with Shift,
    /// which moves `focus` and leaves `anchor` where it was.
    Span { anchor: (f32, f32), focus: (f32, f32) },
    /// Double-click: the word under the point.
    Word((f32, f32)),
    /// Triple-click: the line under the point.
    Line((f32, f32)),
    /// Ctrl+A.
    All,
}

/// Whether `point` falls inside a run's box.
fn run_holds(run: &TextRun, (x, y): (f32, f32)) -> bool {
    x >= run.x && x < run.x + run.width && y >= run.y && y < run.y + run.height
}

/// Where a run sits in reading order: down the page first, then across it.
/// The run's centre, so which side of a word a boundary falls on is decided by
/// whether the cursor passed its middle.
fn run_key(run: &TextRun) -> (f32, f32) {
    (run.y + run.height / 2.0, run.x + run.width / 2.0)
}

/// Where a *point* sits in reading order.
///
/// Its row is snapped to the middle of whatever line it landed in, so two
/// points on the same line compare by x alone — which is what dragging along a
/// line has to mean. A point in the gutter between lines keeps its own y and so
/// sorts between them, which is what dragging past the end of a line has to mean.
fn point_key(runs: &[TextRun], (x, y): (f32, f32)) -> (f32, f32) {
    let row = runs
        .iter()
        .find(|r| y >= r.y && y < r.y + r.height)
        .map_or(y, |r| r.y + r.height / 2.0);
    (row, x)
}

/// The runs a selection covers, in reading order.
fn selected_runs(runs: &[TextRun], selection: Selection) -> Vec<&TextRun> {
    match selection {
        Selection::All => runs.iter().collect(),
        Selection::Word(point) => runs.iter().filter(|r| run_holds(r, point)).take(1).collect(),
        // Runs on one line all carry that line's top as their `y`, set once by
        // inline layout — so "the same line" is an exact comparison, not a
        // tolerance.
        Selection::Line(point) => match runs.iter().find(|r| run_holds(r, point)) {
            Some(line) => runs.iter().filter(|r| r.y == line.y).collect(),
            None => Vec::new(),
        },
        Selection::Span { anchor, focus } => {
            let (a, b) = (point_key(runs, anchor), point_key(runs, focus));
            let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
            runs.iter().filter(|r| (lo..=hi).contains(&run_key(r))).collect()
        }
    }
}

/// The selected words as text: a space between words, a newline between lines.
///
/// ponytail: word-granular. Inline layout keeps one fragment per word and
/// throws the original spacing away, so this rebuilds it — a run of two spaces
/// cannot come back, and half a word cannot be selected. Character granularity
/// needs glyph-to-character mapping through shaping, which is the same thing
/// find-in-page's own highlight is waiting on.
fn selection_text(runs: &[&TextRun]) -> String {
    let mut out = String::new();
    let mut row: Option<f32> = None;
    for run in runs {
        match row {
            Some(y) if y == run.y => out.push(' '),
            Some(_) => out.push('\n'),
            None => {}
        }
        out.push_str(&run.text);
        row = Some(run.y);
    }
    out
}

/// Everything that belongs to one tab, including its own history and render cache.
struct Tab {
    /// Identifies this tab for the life of the process, so a `localStorage`
    /// write can name the one tab that must *not* be told about it. Not an
    /// index: tabs get reordered and closed, and an index would quietly start
    /// pointing at a different tab.
    id: usize,
    address: String,
    /// A process of its own, holding the parsed DOM and a live JS runtime so
    /// handlers survive between frames — see `docs/03-ROADMAP.md`'s note on
    /// why page content no longer runs in this one.
    renderer: TabRenderer,
    /// This tab's own title and focus state, cached from the last frame its
    /// renderer sent back rather than asked for fresh each time.
    title: String,
    is_focused: bool,
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
    /// Every painted word of the last render, in reading order — what a
    /// selection is resolved against. Chrome-side, so dragging across a
    /// paragraph costs no round trip to the renderer and no re-layout.
    text_runs: Vec<TextRun>,
    /// What the mouse has selected on this page, if anything. Per tab, so the
    /// two panes of a split each keep their own.
    selection: Option<Selection>,
    /// Whether the last render's stylesheet reacted to the cursor at all.
    uses_hover: bool,
    /// The page element the cursor was over last, so `update_hover` only
    /// sends a `hover` message — and pays for a round trip and a repaint —
    /// when it has actually changed, not on every mouse-move pixel.
    hovered_node: Option<usize>,
    /// This tab's `sessionStorage`, one map per site. It lives here, above
    /// the renderer, because a web navigation replaces the renderer process
    /// outright — and surviving exactly that is the whole point of it.
    sessions: Rc<crate::localstore::TabSessions>,
    /// The markup this page was built from, kept for view-source and saving.
    source: String,
    /// Shared with this tab's document so its subresource cache outlives a
    /// single render — the engine re-asks for images and stylesheets each time.
    loader: Rc<ShellLoader>,
    cache_w: u32,
    cache_h: u32,
    /// The first document row `page_canvas` holds, and the height of the whole
    /// page it came out of — the canvas is one band, not the document.
    band_top: f32,
    doc_height: f32,
}

/// A tab's own `localStorage`, partitioned by site like cookies — shared by
/// [`Tab::new`] and [`App::load`], the two places a renderer is spawned.
/// Whether a navigation can stay in the renderer process it is already in.
///
/// Only between the browser's own screens. Every preference is a link, so a
/// settings toggle is a navigation — and killing a renderer to start another
/// one, which then reads the whole font chain again, is a great deal of
/// machinery to move a radio button. Two `zero://` pages are both ours and
/// trust each other by definition.
///
/// Anything involving the web gets its own process, including one web page
/// following another: a fresh process per navigation is the isolation this
/// browser has, and it is not being spent to save milliseconds.
fn can_share_a_renderer(from: &str, to: &str) -> bool {
    crate::internal::is_internal(from) && crate::internal::is_internal(to)
}

/// Both of a page's stores, partitioned by the same site rule: its site's
/// `localStorage` off disk, and its tab's `sessionStorage` out of `sessions`.
/// Navigating away to another site and back finds the first site's keys still
/// there in both, and the second site's kept apart from them.
fn stores_for(
    address: &str,
    sessions: &Rc<crate::localstore::TabSessions>,
    tab: usize,
) -> crate::localstore::Stores {
    let site = storage_site(address);
    crate::localstore::Stores {
        local: crate::localstore::site_store(&site),
        session: sessions.for_site(&site),
        site,
        tab,
    }
}

/// Whether a tab should be told about someone else's `localStorage` write.
///
/// Two rules, and both matter: the tab that made the write never hears its own
/// (the web fires `storage` only on *other* documents, and a page that heard
/// its own writes would loop if its handler wrote anything), and a tab on a
/// different site shares no storage area to be told about.
fn hears_storage(tab: usize, address: &str, notice: &crate::localstore::StorageNotice) -> bool {
    tab != notice.tab && storage_site(address) == notice.site
}

/// The next tab identity. Monotonic, never reused, so a notice queued against
/// a tab that has since closed matches nothing rather than the wrong tab.
fn next_tab_id() -> usize {
    thread_local! {
        static NEXT: std::cell::Cell<usize> = const { std::cell::Cell::new(1) };
    }
    NEXT.with(|next| {
        let id = next.get();
        next.set(id + 1);
        id
    })
}

/// Undo `renderer::write_frame`'s RGBA packing.
fn canvas_from_frame(frame: &renderer::Frame) -> Canvas {
    let mut pixels = Vec::with_capacity(frame.width * frame.height);
    for p in frame.pixels.chunks_exact(4) {
        pixels.push(zero_engine::Color { r: p[0], g: p[1], b: p[2], a: p[3] });
    }
    Canvas { pixels, width: frame.width, height: frame.height }
}

impl Tab {
    fn new(address: String, html: String, css: String) -> Tab {
        let loader = Rc::new(ShellLoader::new(address.clone()));
        let sessions = Rc::new(crate::localstore::TabSessions::default());
        let id = next_tab_id();
        let draw = |html: &str, css: &str| {
            TabRenderer::spawn(
                html,
                css,
                DEFAULT_VIEWPORT.0,
                DEFAULT_VIEWPORT.1,
                loader.clone(),
                stores_for(&address, &sessions, id),
            )
        };
        // A page the renderer cannot draw costs this tab, not the window: the
        // tab says so instead, and reloading is one keystroke away. Only a
        // renderer that cannot draw even that is fatal — at which point the
        // browser cannot show anything at all, and saying so beats a window
        // full of blank tabs.
        let (source, (renderer, frame)) = match draw(&html, &css) {
            Some(drawn) => (html, drawn),
            None => {
                let failed = crate::internal::render_failed_page(&address);
                let drawn = draw(&failed, "")
                    .expect("the renderer could not draw even the page explaining itself");
                (failed, drawn)
            }
        };
        let mut tab = Tab {
            id,
            loader,
            history: vec![address.clone()],
            address,
            renderer,
            title: String::new(),
            is_focused: false,
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
            text_runs: Vec::new(),
            selection: None,
            uses_hover: false,
            hovered_node: None,
            sessions,
            source,
            cache_w: 0,
            cache_h: 0,
            band_top: 0.0,
            doc_height: 0.0,
        };
        // `cache_w`/`cache_h` stay 0, so the first real `render_pane` call —
        // at the window's actual size, not this placeholder one — still
        // finds itself unsettled and asks for a fresh frame.
        tab.apply_frame(frame, DEFAULT_VIEWPORT.0 as u32, DEFAULT_VIEWPORT.1 as u32);
        tab
    }

    fn blank() -> Tab {
        let address = "zero://newtab".to_string();
        let mut tab = Tab::new(address.clone(), crate::internal::page(&address), String::new());
        tab.address = address; // shown in the bar, and reloadable like any page
        tab
    }

    /// Adopt a frame from this tab's own renderer — every cache the rest of
    /// `app.rs` reads (compositing, hit-testing, the window title) comes
    /// from here, so a click, a resize and a navigation all keep them in
    /// sync the same way rather than each updating a subset by hand.
    fn apply_frame(&mut self, frame: renderer::Frame, w: u32, h: u32) {
        self.title = frame.title.clone();
        self.is_focused = frame.is_focused;
        self.uses_hover = frame.uses_hover;
        self.element_rects = frame.element_rects.clone();
        self.links = frame.links.clone();
        self.matches = frame.find_matches.clone();
        self.page_canvas = Some(canvas_from_frame(&frame));
        self.band_top = frame.band_top;
        self.doc_height = frame.doc_height;
        self.cache_w = w;
        self.cache_h = h;
        self.text_runs = frame.text_runs; // moved out, so last: `frame` is borrowed above
    }

    /// Replace a dead renderer with a fresh one, reloading the same page the
    /// tab was already showing — not *recovered* state (typed text, focus,
    /// running scripts), which a crashed process has no way to hand over,
    /// but the same navigation, retried silently instead of leaving the tab
    /// permanently blank after one hung script.
    fn respawn(&mut self, w: f32, h: f32, band_top: f32) -> Option<renderer::Frame> {
        if self.source.is_empty() {
            return None; // nothing loaded yet to reload
        }
        let (mut renderer, frame) = TabRenderer::spawn(
            &self.source,
            "",
            w,
            h,
            self.loader.clone(),
            stores_for(&self.address, &self.sessions, self.id),
        )?;
        // A fresh renderer starts at the top of the page; this tab may not be.
        let frame = match band_top > 0.0 {
            true => renderer.resize(w, h, band_top).unwrap_or(frame),
            false => frame,
        };
        self.renderer = renderer;
        Some(frame)
    }

    /// Whether the band this tab is holding covers `height` rows from `top`.
    fn band_covers(&self, top: f32, height: f32) -> bool {
        let Some(canvas) = self.page_canvas.as_ref() else { return false };
        let held = canvas.height as f32;
        // The bottom of the page is covered by a band that reaches the end of
        // the document, even when it is shorter than a screenful.
        let reaches_end = self.band_top + held >= self.doc_height - 0.5;
        top >= self.band_top && (top + height <= self.band_top + held || reaches_end)
    }

    /// How the tab names itself in a space `max` characters wide.
    fn label_capped(&self, max: usize) -> String {
        label_for(&self.title, &self.address, max)
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
            // Down the page, so a long article can be reviewed past its first
            // screenful — which is also the only way to see a band boundary.
            ("scroll", px) => app.tab_mut().scroll_y = px.parse().unwrap_or(0.0),
            ("railpx", _) => {} // applied after the loop, once settings are known
            ("search", query) => app.focus = Focus::TabSearch(query.to_string()),
            ("find", query) => {
                // Run the search rather than only opening the bar, so the shot
                // shows a real match count instead of an empty field.
                app.focus = Focus::Find(query.to_string());
                app.apply_chrome_field();
            }
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
            let mut tab = Tab::new(fetched.url.clone(), fetched.body, String::new());
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
    /// Whether a page transition is still running and wants another frame.
    page_animating: bool,
    /// Whether the last frame left something mid-animation and so owes another.
    animating: bool,
    dragging_scrollbar: bool,
    /// Where on the page the left button went down, and whether the mouse has
    /// moved far enough since for this to be a drag rather than a click. A
    /// press over the page starts a selection; a release that never dragged is
    /// the click, which is why the page's click routing waits for the release.
    pressed: Option<(f32, f32)>,
    dragged: bool,
    /// When and where the last press landed, and how many presses have landed
    /// in the same spot in a row — two is a word, three is a line.
    click_run: Option<(std::time::Instant, (f32, f32), u32)>,
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
            pressed: None,
            dragged: false,
            click_run: None,
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
        // The spare renderer belongs to the window, not to the process.
        renderer::drop_warm();
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        let attrs = Window::default_attributes()
            .with_title("Zero Browser")
            .with_window_icon(window_icon())
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
        // Get a renderer ready while the first frame is being drawn, so the
        // first Ctrl+T does not pay for one.
        renderer::warm();
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
                } else if self.pressed.is_some() {
                    self.extend_selection();
                } else {
                    self.update_hover();
                }
            }
            WindowEvent::MouseInput { state: ElementState::Released, button, .. } => {
                self.dragging_scrollbar = false;
                self.dragging_divider = false;
                // A press that never became a drag was a click, and the page
                // finds out about it now: pressing on a link and dragging off
                // it is how you select its text rather than follow it.
                if button == MouseButton::Left {
                    if let Some(point) = self.pressed.take() {
                        if !std::mem::take(&mut self.dragged) {
                            self.click_page(point);
                            self.request_redraw();
                        }
                    }
                }
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
        // Whatever just ran may have written to localStorage. The other tabs
        // on that site are told here, at the end of the event that caused it,
        // because a write arrives deep inside a renderer reply loop that is
        // already talking to one tab and cannot start talking to another.
        self.deliver_storage_events();
    }
}

/// The mark, for the taskbar and the window's own corner.
///
/// Rasterized from the same ring the chrome draws rather than loaded from a
/// file: there is then one logo in the codebase, and it cannot fall out of step
/// with itself. `None` costs the icon, never the window.
fn window_icon() -> Option<winit::window::Icon> {
    const SIZE: u32 = 64;
    // The taskbar is the desktop's surface, not ours: an ink ring on a dark
    // taskbar is an invisible ring, whichever theme the browser itself is in.
    let ring = match crate::settings::desktop_prefers_dark() {
        true => "#e8eaed",
        false => "#1a222b",
    };
    let source = icon::ring(ring, SIZE);
    let drawn = zero_engine::svg::rasterize(&source, SIZE as usize, SIZE as usize)?;
    let rgba: Vec<u8> =
        drawn.pixels.iter().flat_map(|p| [p.r, p.g, p.b, p.a]).collect();
    winit::window::Icon::from_rgba(rgba, SIZE, SIZE).ok()
}

impl App {
    fn tab(&self) -> &Tab {
        &self.tabs[self.active]
    }

    fn tab_mut(&mut self) -> &mut Tab {
        &mut self.tabs[self.active]
    }

    /// Adopt whatever frame the active tab's renderer sent back after one
    /// interaction — the single place every input handler goes through, so
    /// hit-testing caches, the window's animation flag and the tab's own
    /// state all stay in step the same way, however small the interaction.
    ///
    /// ponytail: `None` (the renderer died or the pipe broke) is silently
    /// ignored — the tab keeps showing its last good frame rather than
    /// anything worse. Noticing the death and respawning is its own step,
    /// not this one.
    fn adopt(&mut self, frame: Option<renderer::Frame>) -> bool {
        let Some(frame) = frame else {
            // A failed call is exactly how a renderer's death is first
            // noticed (a timeout, or a closed pipe — see `TabRenderer`).
            // `render_pane` is where recovery actually happens (it checks
            // `is_dead` and respawns), but only when it runs — so ask for a
            // redraw here too, or a tab that died on an interaction with no
            // other reason to repaint would just sit on its last good frame
            // until something unrelated happened to trigger one.
            self.request_redraw();
            return false;
        };
        self.page_animating = frame.animating;
        let tab = self.tab_mut();
        let (w, h) = (tab.cache_w, tab.cache_h);
        tab.apply_frame(frame, w, h);
        true
    }

    /// Hand every queued `localStorage` write to the other tabs on that site.
    ///
    /// Called once per window event rather than at the write itself: a write
    /// arrives deep inside a renderer reply loop, which is talking to one tab
    /// and cannot start talking to another without reentering itself.
    ///
    /// ponytail: every same-site tab pays a round trip whether or not it has a
    /// `storage` listener, because only its own renderer knows whether it
    /// does. Usually that is no tabs at all; a per-renderer "is anyone
    /// listening" flag on the frame would cut it if a site ever opens many.
    fn deliver_storage_events(&mut self) {
        let notices = crate::localstore::take_writes();
        if notices.is_empty() {
            return;
        }
        let mut repaint = false;
        for notice in notices {
            for i in 0..self.tabs.len() {
                if !hears_storage(self.tabs[i].id, &self.tabs[i].address, &notice) {
                    continue;
                }
                let url = self.tabs[i].address.clone();
                let (w, h) = (self.tabs[i].cache_w, self.tabs[i].cache_h);
                if let Some(frame) = self.tabs[i].renderer.storage_event(&notice, &url) {
                    self.tabs[i].apply_frame(frame, w, h);
                    repaint = true;
                }
            }
        }
        if repaint {
            self.request_redraw();
        }
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
        if self.tab().is_focused {
            let frame = self.tab_mut().renderer.insert_text(&typed);
            if self.adopt(frame) {
                return true;
            }
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
        if !ctrl && self.tab().is_focused {
            let handled = match event.logical_key {
                Key::Named(NamedKey::Backspace) => {
                    let frame = self.tab_mut().renderer.backspace();
                    self.adopt(frame)
                }
                Key::Named(NamedKey::Escape) => {
                    let frame = self.tab_mut().renderer.blur();
                    self.adopt(frame);
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
                ("a", false) => self.tab_mut().selection = Some(Selection::All),
                ("c", _) => self.copy_selection(),
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

        let Some(point) = self.page_coords((cx, cy), &regions) else { return };
        self.press_page(point);
    }

    /// The left button went down on the page. This starts a selection rather
    /// than acting: what the press *means* is only known on release (a click)
    /// or on the first movement (a drag), and a double or triple press means
    /// something else again.
    fn press_page(&mut self, point: (f32, f32)) {
        let now = std::time::Instant::now();
        // Three presses close together and close by are one gesture. The window
        // never sees a double-click event of its own — winit reports presses —
        // so the run is counted here.
        let run = match self.click_run {
            Some((at, (x, y), n))
                if now.duration_since(at) < MULTI_CLICK
                    && (point.0 - x).abs() < MULTI_CLICK_SLOP
                    && (point.1 - y).abs() < MULTI_CLICK_SLOP =>
            {
                n + 1
            }
            _ => 1,
        };
        self.click_run = Some((now, point, run));
        self.pressed = Some(point);
        self.dragged = run > 1; // a double or triple press has already selected
        let extend = self.modifiers.shift_key();
        let tab = self.tab_mut();
        tab.selection = match (run, tab.selection) {
            (2, _) => Some(Selection::Word(point)),
            (3.., _) => Some(Selection::Line(point)),
            // Shift keeps the anchor and moves the far end, so a selection can
            // be grown after the fact without dragging it again.
            (_, Some(Selection::Span { anchor, .. })) if extend => {
                Some(Selection::Span { anchor, focus: point })
            }
            // Shift after a double- or triple-click has no anchor of its own to
            // keep, so the selection's own first word stands in for one.
            (_, Some(existing)) if extend => {
                let anchor = selected_runs(&tab.text_runs, existing)
                    .first()
                    .map_or(point, |r| (r.x, r.y + r.height / 2.0));
                Some(Selection::Span { anchor, focus: point })
            }
            // A plain press clears what was selected; the drag, if there is
            // one, puts a new selection back.
            _ => None,
        };
        self.request_redraw();
    }

    /// The mouse moved with the button down: select from the press to here.
    fn extend_selection(&mut self) {
        let Some(anchor) = self.pressed else { return };
        let regions = self.regions();
        let Some(focus) = self.page_coords(self.cursor, &regions) else { return };
        // A few pixels of travel is a shaky click, not a drag — without this a
        // hand that moves while clicking would never reach `click_page`, and
        // links would stop working.
        if !self.dragged
            && (focus.0 - anchor.0).abs() < DRAG_SLOP
            && (focus.1 - anchor.1).abs() < DRAG_SLOP
        {
            return;
        }
        self.dragged = true;
        self.tab_mut().selection = Some(Selection::Span { anchor, focus });
        self.request_redraw();
    }

    /// Put the selected text on the system clipboard.
    fn copy_selection(&mut self) {
        let tab = self.tab();
        let Some(selection) = tab.selection else { return };
        crate::clipboard::set(&selection_text(&selected_runs(&tab.text_runs, selection)));
    }

    /// A press on the page that turned out to be a click: links, focus and
    /// script handlers, exactly as it was decided when this ran on the press.
    fn click_page(&mut self, (px, py): (f32, f32)) {
        // Innermost element wins, so a handler on a child beats one on its parent.
        let hit = self
            .tab()
            .element_rects
            .iter()
            .filter(|r| px >= r.x && px <= r.x + r.width && py >= r.y && py <= r.y + r.height)
            .map(|r| r.node_id)
            .next_back();
        let href = self
            .tab()
            .links
            .iter()
            .find(|l| px >= l.x && px <= l.x + l.width && py >= l.y && py <= l.y + l.height)
            .map(|l| l.href.clone());
        // A link on one of the browser's own screens can be followed straight
        // away. Those pages run no scripts, so nothing there can intercept a
        // click — and asking the renderer first would draw the whole page again
        // only to find that out, then draw the page being navigated to as well.
        // Every preference is a link, so this is the whole cost of a settings
        // toggle. Anything from the web is still asked first.
        if let Some(href) = href.clone() {
            if crate::internal::is_internal(&self.tab().address) {
                let target = resolve_url(&self.tab().address, &href);
                self.go_to(target);
                self.request_redraw();
                return;
            }
        }
        // Sent even when nothing was hit: `click` always blurs first —
        // clicking the page clears focus unless the click lands on a field —
        // which is why this always reaches the renderer rather than being
        // skipped. `usize::MAX` never names a real node, so the renderer's
        // own focus/click attempts on it are a harmless no-op and only the
        // blur takes effect.
        let frame = self.tab_mut().renderer.click(hit.unwrap_or(usize::MAX));
        let handled = frame.as_ref().is_some_and(|f| f.click_handled);
        if self.adopt(frame) {
            self.request_redraw();
        }
        if handled {
            return;
        }

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
        // Render at the size this tab is already showing, so the new page's
        // first frame is the real one rather than `DEFAULT_VIEWPORT`
        // (unknown only on a brand new tab, where `Tab::new` already used it).
        let (w, h) = {
            let tab = self.tab();
            match (tab.cache_w, tab.cache_h) {
                (0, _) | (_, 0) => DEFAULT_VIEWPORT,
                (w, h) => (w as f32, h as f32),
            }
        };
        let tab = self.tab_mut();
        let stay = can_share_a_renderer(&tab.address, &fetched.url) && !tab.renderer.is_dead();
        // An HTTPS upgrade can change the URL, so adopt whatever actually loaded.
        tab.address = fetched.url;
        tab.secure = fetched.secure;
        // A new page means a new document and a fresh JS runtime. The loader goes
        // in so page scripts can fetch relative to this URL.
        tab.loader = Rc::new(ShellLoader::new(tab.address.clone()));
        let frame = match stay {
            true => tab.renderer.replace_page(
                &fetched.body,
                "",
                w,
                h,
                tab.loader.clone(),
                stores_for(&tab.address, &tab.sessions, tab.id),
            ),
            false => TabRenderer::spawn(
                &fetched.body,
                "",
                w,
                h,
                tab.loader.clone(),
                stores_for(&tab.address, &tab.sessions, tab.id),
            )
            .map(|(renderer, frame)| {
                tab.renderer = renderer; // dropping the old one kills its process
                frame
            }),
        };
        // The same rule as opening a tab: a page that cannot be drawn costs this
        // tab, not the window. The address stays, so Ctrl+R retries it.
        let (frame, body) = match frame {
            Some(frame) => (frame, fetched.body),
            None => {
                let failed = crate::internal::render_failed_page(&tab.address);
                match TabRenderer::spawn(
                    &failed,
                    "",
                    w,
                    h,
                    tab.loader.clone(),
                    stores_for(&tab.address, &tab.sessions, tab.id),
                ) {
                    Some((renderer, frame)) => {
                        tab.renderer = renderer;
                        (frame, failed)
                    }
                    // Not even that could be drawn: leave the tab showing what
                    // it already had rather than blanking it.
                    None => return,
                }
            }
        };
        tab.source = body;
        tab.matches.clear();
        tab.selection = None; // it pointed at words on the page being replaced
        tab.scroll_y = 0.0;
        tab.apply_frame(frame, w as u32, h as u32);
        let address = tab.address.clone();
        let title = tab.title.clone();
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
            (tab.address.clone(), tab.title.clone(), tab.source.clone())
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
        let (url, blocked_trackers, secure) = {
            let tab = self.tab();
            (tab.address.clone(), tab.blocked_count, tab.secure)
        };
        let Some((text, headings)) = self.tab_mut().renderer.page_text() else { return };
        let ctx = PageContext { url, text, headings, blocked_trackers, secure };
        let assistant = LocalAssistant;
        // The provenance is a plain sentence, not an aside in brackets: where
        // the summary came from is the most reassuring thing on the panel.
        self.ai_text = format!("{}\n\n{}", assistant.respond(&ctx), assistant.provenance());
    }

    fn ai_html(&self) -> String {
        let body: String = self
            .ai_text
            .lines()
            .map(|line| {
                // A section label reads as one, rather than as another sentence.
                let class = if crate::ai::is_section(line) { "sec" } else { "line" };
                format!("<div class=\"{class}\">{}</div>", escape(line))
            })
            .collect();
        format!("<html><body><div id=\"head\">Assistant</div>{body}</body></html>")
    }

    /// Enter in a field submits its form, if it is in one; otherwise it just
    /// leaves the field, which is what a lone input does.
    fn submit_focused_form(&mut self) {
        let frame = self.tab_mut().renderer.submit();
        let sent = frame.as_ref().and_then(|f| f.submission.clone());
        self.adopt(frame);
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
        if hit == self.tab().hovered_node {
            return; // nothing changed; not worth a round trip and a repaint
        }
        self.tab_mut().hovered_node = hit;
        let frame = self.tab_mut().renderer.hover(hit);
        if self.adopt(frame) {
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
        let frame = self.tab_mut().renderer.blur(); // typing belongs to the find bar now
        self.adopt(frame);
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
        let frame = self.tab_mut().renderer.blur();
        self.adopt(frame);
        self.request_redraw();
    }

    fn close_chrome_field(&mut self) {
        if matches!(self.focus, Focus::Find(_)) {
            let frame = self.tab_mut().renderer.find(None); // drop the highlights
            self.adopt(frame);
        }
        self.focus = Focus::Address;
    }

    /// Push the field's live text at whatever it filters.
    fn apply_chrome_field(&mut self) {
        if let Focus::Find(query) = &self.focus {
            let query = query.clone();
            let frame = self.tab_mut().renderer.find(Some(&query)); // highlights are painted, so re-render
            self.adopt(frame);
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
        let content = tab.doc_height * tab.zoom_factor();
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

    /// A control's icon colour. Under the cursor it brightens to the text
    /// colour, which is the whole of the hover state on a control whose
    /// background is already doing the other half.
    fn ink(&self, id: &str, base: &'static str) -> &'static str {
        match self.hovered.as_deref() == Some(id) {
            true => theme::text(),
            false => base,
        }
    }

    /// A colour dimmed halfway to the surface behind it: a control that is
    /// there but cannot be used.
    fn dim() -> String {
        theme::mix(theme::faint(), theme::chrome(), 0.45)
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
                "<html><body><div id=\"bar\"><div class=\"omni\">\
                 <span class=\"ico\">{}</span>\
                 <span class=\"addr\">{}|</span>\
                 <span class=\"hint\">{hits}</span></div></div></body></html>",
                icon::find(theme::muted()),
                escape(query)
            );
        }
        // The padlock is the one claim the address bar makes, so it says plainly
        // when a page arrived over cleartext. A built-in page made no connection
        // at all, so it claims nothing.
        let lock = match (crate::internal::is_internal(&tab.address), tab.secure) {
            (true, _) => String::new(),
            (false, true) => format!("<span class=\"ico\">{}</span>", icon::secure(theme::ok())),
            (false, false) => format!(
                "<span class=\"ico\">{}</span><span class=\"warn\">not secure</span>",
                icon::insecure(theme::saved())
            ),
        };
        let shield = match tab.blocked_count {
            0 => String::new(),
            n => format!(
                "<span id=\"shield\" class=\"{}\">{} {n}</span>",
                self.lit("shield", "chip"),
                icon::shield(self.ink("shield", theme::muted())),
            ),
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
            left.push_str(&format!(
                "<span id=\"rail\" class=\"{}\">{}</span>",
                self.lit("rail", "btn"),
                icon::rail(self.ink("rail", theme::muted()), self.settings.rail == Rail::Hidden),
            ));
            buttons += 1;
        }
        // With no rail on screen there is nowhere else to open a tab from.
        let hidden_rail =
            self.settings.layout == TabLayout::Vertical && self.settings.rail == Rail::Hidden;
        if hidden_rail {
            left.push_str(&format!(
                "<span id=\"new\" class=\"{}\">{}</span>",
                self.lit("new", "btn"),
                icon::add(self.ink("new", theme::muted())),
            ));
            buttons += 1;
        }
        let bookmarked = storage::is_bookmarked(&tab.address);
        // State is a colour, not a fill: a saved page and an open assistant say
        // so by lighting their own icon, the way the site marks a current tab.
        let star_ink = match bookmarked {
            true => theme::saved(),
            false => self.ink("star", theme::muted()),
        };
        let ai_ink = match self.ai_open {
            true => theme::accent(),
            false => self.ink("ai", theme::muted()),
        };
        let right = format!(
            "<span id=\"star\" class=\"{}\">{}</span>\
             <span id=\"marks\" class=\"{}\">{}</span>\
             <span id=\"ai\" class=\"{}\">{}</span>\
             <span id=\"overflow\" class=\"{}\">{}</span>",
            self.lit("star", "btn"),
            icon::star(star_ink, bookmarked),
            self.lit("marks", "btn"),
            icon::bookmarks(self.ink("marks", theme::muted())),
            self.lit("ai", "btn"),
            icon::assistant(ai_ink),
            self.lit("overflow", "btn"),
            icon::menu(self.ink("overflow", theme::muted())),
        );
        buttons += 3;
        // Capped as well as fitted: past a point a wider window should give its
        // room to the page, not stretch one field across the whole screen.
        let omni_width = regions
            .toolbar_w()
            .saturating_sub(BUTTON_SPAN * buttons + 40 + if zoom.is_empty() { 0 } else { 56 })
            .clamp(120, 640);
        // The scheme is dimmed rather than hidden: it is the part of an address
        // that matters least to read and most to be able to check.
        let address = match tab.address.split_once("://") {
            Some((scheme, rest)) => format!(
                "<span class=\"scheme\">{}://</span>{}",
                escape(scheme),
                escape(rest)
            ),
            None => escape(&tab.address),
        };
        let back = self.lit("back", if tab.history_index > 0 { "btn" } else { "off" });
        let back_ink = match tab.history_index > 0 {
            true => self.ink("back", theme::muted()).to_string(),
            false => Self::dim(),
        };
        let can_forward = tab.history_index + 1 < tab.history.len();
        let fwd = self.lit("fwd", if can_forward { "btn" } else { "off" });
        let fwd_ink = match can_forward {
            true => self.ink("fwd", theme::muted()).to_string(),
            false => Self::dim(),
        };
        format!(
            "<html><head><style>.omni{{width:{omni_width}px;}}</style></head>\
             <body><div id=\"bar\">\
             <div class=\"cluster\">{left}\
             <span id=\"back\" class=\"{back}\">{}</span>\
             <span id=\"fwd\" class=\"{fwd}\">{}</span>\
             <span id=\"reload\" class=\"{}\">{}</span></div>\
             <div class=\"omni\">{lock}<span class=\"addr\">{address}|</span>{shield}</div>\
             <div class=\"cluster\">{zoom}{right}</div>\
             </div></body></html>",
            icon::back(&back_ink),
            icon::forward(&fwd_ink),
            self.lit("reload", "btn"),
            icon::reload(self.ink("reload", theme::muted())),
        )
    }

    /// Toolbar styling. Buttons are a fixed square so their icons sit centred
    /// rather than lopsided; the pill's width is injected per frame because it
    /// depends on how many buttons the current layout draws.
    fn toolbar_css() -> String {
        format!(
            "body{{background:{chrome};color:{text};font-size:14px;}} \
             #bar{{display:flex;align-items:center;justify-content:space-between;\
                  height:38px;padding:9px;}} \
             .cluster{{display:flex;align-items:center;gap:2px;}} \
             .btn{{display:inline-flex;flex-shrink:0;align-items:center;justify-content:center;\
                  width:34px;height:34px;border-radius:9px;}} \
             .off{{display:inline-flex;flex-shrink:0;align-items:center;justify-content:center;\
                  width:34px;height:34px;border-radius:9px;}} \
             .hot{{background:{hover};}} \
             .omni{{display:flex;align-items:center;height:36px;border-radius:11px;\
                   background:{surface};border-width:1px;border-color:{line};\
                   padding-left:4px;padding-right:5px;}} \
             .addr{{flex-grow:1;padding-left:8px;color:{text};font-size:14.5px;\
                   white-space:nowrap;}} \
             .scheme{{color:{faint};}} \
             .ico{{display:inline-flex;flex-shrink:0;align-items:center;padding-left:7px;}} \
             .warn{{color:{saved};font-size:13px;padding-left:6px;}} \
             .chip{{display:inline-flex;flex-shrink:0;align-items:center;height:26px;border-radius:7px;\
                   background:{chrome};color:{muted};font-size:13px;\
                   padding-left:8px;padding-right:9px;}} \
             .badge{{display:inline-flex;flex-shrink:0;align-items:center;height:26px;border-radius:7px;\
                    background:{surface};border-width:1px;border-color:{line};\
                    color:{muted};font-size:12px;padding-left:9px;padding-right:9px;\
                    margin-right:6px;}} \
             .hint{{color:{faint};font-size:13px;padding-left:10px;padding-right:8px;}}",
            chrome = theme::chrome(),
            text = theme::text(),
            surface = theme::surface(),
            hover = theme::hover(),
            faint = theme::faint(),
            saved = theme::saved(),
            muted = theme::muted(),
            line = theme::line(),
        )
    }

    /// The vertical rail: the mark, two pinned destinations, a tab search field,
    /// the tabs, and a way to open another. Pinned tabs lead.
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
                    true => format!("<span class=\"pin\">{}</span>", icon::pinned(theme::accent())),
                    false => String::new(),
                };
                format!(
                    "<div id=\"tab:{i}\" class=\"{class}\">{pin}\
                     <span class=\"name\">{}</span>\
                     <span id=\"close:{i}\" class=\"{}\">{}</span></div>",
                    escape(&tab.label_capped(room)),
                    self.lit(&format!("close:{i}"), "x"),
                    icon::close(self.ink(&format!("close:{i}"), theme::faint()), 13),
                )
            })
            .collect();
        let new = format!(
            "<div id=\"new\" class=\"{}\"><span class=\"plus\">{}</span>{}</div>",
            self.lit("new", "tab new"),
            icon::add(self.ink("new", theme::faint())),
            if icons { String::new() } else { escape(&t("New tab")) },
        );
        if icons {
            // Narrow: the mark alone, and a column of initials under it.
            return format!(
                "<html><body><div id=\"head\">{}</div>{rows}{new}</body></html>",
                icon::ring(theme::text(), 20),
            );
        }
        // The search field replaces the header's subtitle while it is open, so the
        // rail never grows a row it did not have a moment ago.
        let search = match &self.focus {
            Focus::TabSearch(query) => format!(
                "<div id=\"search\" class=\"find on\"><span class=\"ico\">{}</span>{}|</div>",
                icon::find(theme::text()),
                escape(query)
            ),
            _ => format!(
                "<div id=\"search\" class=\"{}\"><span class=\"ico\">{}</span>{}</div>",
                self.lit("search", "find"),
                icon::find(self.ink("search", theme::faint())),
                escape(&t("Search tabs")),
            ),
        };
        let empty = match rows.is_empty() {
            true => format!("<div class=\"none\">{}</div>", escape(&t("No tab matches that."))),
            false => String::new(),
        };
        // Two places worth keeping one click away, as the site's rail does.
        let pinned = format!(
            "<div class=\"pinned\">\
             <span id=\"go:bookmarks\" class=\"{}\">{}</span>\
             <span id=\"go:history\" class=\"{}\">{}</span></div>",
            self.lit("go:bookmarks", "quick"),
            icon::bookmarks(self.ink("go:bookmarks", theme::muted())),
            self.lit("go:history", "quick"),
            icon::history(self.ink("go:history", theme::muted())),
        );
        format!(
            "<html><body><div id=\"head\">{}<span class=\"word\">zero</span></div>\
             {pinned}{search}{rows}{empty}{new}</body></html>",
            icon::ring(theme::text(), 20),
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
            "body{{background:{chrome};color:{muted};font-size:14.5px;height:{height}px;\
                  padding-left:12px;padding-right:10px;padding-top:16px;}} \
             #head{{display:flex;align-items:center;padding-left:{head_pad}px;\
                   padding-bottom:18px;justify-content:{align};}} \
             .word{{color:{text};font-size:22px;letter-spacing:-0.03em;padding-left:9px;}} \
             .pinned{{display:flex;gap:6px;padding-bottom:16px;}} \
             .quick{{display:inline-flex;flex-shrink:0;flex-grow:1;align-items:center;justify-content:center;\
                    height:38px;border-radius:10px;background:{quiet};}} \
             .find{{display:flex;align-items:center;color:{faint};font-size:13px;\
                   height:34px;padding-left:{row_pad}px;border-radius:10px;margin-bottom:8px;}} \
             .ico{{display:inline-flex;flex-shrink:0;align-items:center;padding-right:9px;}} \
             .on{{background:{surface};color:{text};}} \
             .tab{{display:flex;align-items:center;height:38px;padding-left:{row_pad}px;\
                  padding-right:8px;border-radius:10px;\
                  border-left-width:3px;border-color:{chrome};text-align:{text_align};}} \
             .hot{{background:{hover};color:{text};}} \
             .active{{background:{surface};color:{text};border-left-width:3px;\
                     border-color:{accent};}} \
             .new{{color:{faint};margin-top:2px;}} \
             .plus{{display:inline-flex;flex-shrink:0;align-items:center;padding-right:9px;}} \
             .none{{color:{faint};font-size:13px;padding-top:10px;padding-left:{row_pad}px;}} \
             .pin{{display:inline-flex;flex-shrink:0;align-items:center;padding-right:7px;}} \
             .name{{display:inline-block;flex-grow:1;width:{name_w}px;white-space:nowrap;}} \
             .x{{display:inline-flex;flex-shrink:0;align-items:center;justify-content:center;\
                width:22px;height:22px;border-radius:6px;}}",
            head_pad = if icons { 0 } else { 8 },
            align = if icons { "center" } else { "flex-start" },
            text_align = if icons { "center" } else { "left" },
            quiet = theme::mix(theme::text(), theme::chrome(), 0.05),
            chrome = theme::chrome(),
            surface = theme::surface(),
            hover = theme::hover(),
            text = theme::text(),
            muted = theme::muted(),
            faint = theme::faint(),
            accent = theme::accent(),
        )
    }

    /// The rail's footer: which space you are in, and a permanent home for
    /// settings. Pinned to the bottom of the window by being its own surface
    /// rather than by padding arithmetic.
    fn rail_foot_html(&self, icons: bool) -> String {
        let settings = format!(
            "<span id=\"go:settings\" class=\"{}\">{}</span>",
            self.lit("go:settings", "foot"),
            icon::settings(self.ink("go:settings", theme::faint())),
        );
        if icons {
            return format!("<html><body><div id=\"row\">{settings}</div></body></html>");
        }
        let downloads = format!(
            "<span id=\"go:downloads\" class=\"{}\">{}</span>",
            self.lit("go:downloads", "foot"),
            icon::download(self.ink("go:downloads", theme::faint())),
        );
        // The space's name and colour, which is what tells you whose history and
        // cookies the next click lands in.
        let space = crate::spaces::current();
        format!(
            "<html><body><div id=\"row\">\
             <span class=\"space\"><span class=\"dot\">{}</span>{}</span>\
             <span class=\"tools\">{settings}{downloads}</span></div></body></html>",
            icon::dot(theme::accent(), 8),
            escape(&space),
        )
    }

    fn rail_foot_css(icons: bool) -> String {
        format!(
            "body{{background:{chrome};color:{muted};font-size:13px;\
                  padding-left:12px;padding-right:10px;}} \
             #row{{display:flex;justify-content:{justify};align-items:center;\
                  padding-top:10px;border-top-width:1px;border-color:{line};height:30px;}} \
             .space{{display:inline-flex;flex-shrink:0;align-items:center;color:{faint};padding-left:8px;}} \
             .dot{{display:inline-flex;flex-shrink:0;align-items:center;padding-right:9px;}} \
             .tools{{display:inline-flex;flex-shrink:0;align-items:center;gap:2px;}} \
             .foot{{display:inline-flex;flex-shrink:0;align-items:center;justify-content:center;\
                   width:30px;height:30px;border-radius:8px;}} \
             .hot{{background:{hover};color:{text};}}",
            justify = if icons { "center" } else { "space-between" },
            chrome = theme::chrome(),
            hover = theme::hover(),
            text = theme::text(),
            muted = theme::muted(),
            faint = theme::faint(),
            line = theme::line(),
        )
    }

    /// The horizontal tab strip. Tabs that do not fit are reachable from tab
    /// search and Ctrl+Tab.
    fn strip_html(&self, regions: &Regions) -> String {
        let order = self.rail_order();
        let room = (regions.width.saturating_sub(150) / STRIP_TAB_W).max(1) as usize;
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
                    true => format!("<span class=\"pin\">{}</span>", icon::pinned(theme::accent())),
                    false => String::new(),
                };
                // A strip tab is narrower than a rail row, so it names itself
                // more briefly. The tooltip still gives the full title.
                let room = if tab.pinned { 14 } else { 16 };
                format!(
                    "<span id=\"tab:{i}\" class=\"{}\">{pin}<span class=\"name\">{}</span>\
                     <span id=\"close:{i}\" class=\"{}\">{}</span></span>",
                    self.lit(&format!("tab:{i}"), base),
                    escape(&tab.label_capped(room)),
                    self.lit(&format!("close:{i}"), "x"),
                    icon::close(self.ink(&format!("close:{i}"), theme::faint()), 13),
                )
            })
            .collect();
        let more = match order.len() - shown {
            0 => String::new(),
            n => format!("<span class=\"more\">+{n}</span>"),
        };
        format!(
            "<html><body><div id=\"strip\"><span class=\"mark\">{}</span>{tabs}\
             <span id=\"new\" class=\"{}\">{}</span>{more}\
             <span id=\"rail\" class=\"{}\">{}</span></div></body></html>",
            icon::ring(theme::text(), 18),
            self.lit("new", "add"),
            icon::add(self.ink("new", theme::muted())),
            self.lit("rail", "add"),
            icon::rail(self.ink("rail", theme::muted()), true),
        )
    }

    fn strip_css() -> String {
        format!(
            "body{{background:{chrome};color:{muted};font-size:13px;}} \
             #strip{{display:flex;align-items:center;gap:2px;height:30px;\
                    padding-left:12px;padding-right:8px;padding-top:4px;\
                    border-bottom-width:1px;border-color:{line};}} \
             .mark{{display:inline-flex;flex-shrink:0;align-items:center;padding-right:10px;}} \
             .tab{{display:inline-flex;flex-shrink:0;align-items:center;width:{tab_w}px;height:28px;\
                  padding-left:10px;padding-right:5px;border-radius:9px;}} \
             .active{{background:{surface};color:{text};border-left-width:3px;\
                     border-color:{accent};}} \
             .hot{{background:{hover};color:{text};}} \
             .name{{display:inline-block;flex-grow:1;width:{name_w}px;white-space:nowrap;}} \
             .pin{{display:inline-flex;flex-shrink:0;align-items:center;padding-right:6px;}} \
             .x{{display:inline-flex;flex-shrink:0;align-items:center;justify-content:center;\
                width:20px;height:20px;border-radius:6px;}} \
             .add{{display:inline-flex;flex-shrink:0;align-items:center;justify-content:center;\
                  width:28px;height:28px;border-radius:8px;}} \
             .more{{display:inline-flex;flex-shrink:0;align-items:center;color:{faint};font-size:12px;\
                   padding-left:6px;padding-right:6px;}}",
            tab_w = STRIP_TAB_W - 32,
            name_w = STRIP_TAB_W - 32 - 40,
            chrome = theme::chrome(),
            surface = theme::surface(),
            hover = theme::hover(),
            text = theme::text(),
            muted = theme::muted(),
            faint = theme::faint(),
            accent = theme::accent(),
            line = theme::line(),
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
                    return format!(
                        "<div class=\"zoom\"><span class=\"zlabel\">{zoom_label}</span>\
                         <span id=\"zoom:out\" class=\"{}\">{}</span>\
                         <span id=\"zoom:reset\" class=\"{}\">{}%</span>\
                         <span id=\"zoom:in\" class=\"{}\">{}</span></div>",
                        self.lit("zoom:out", "step"),
                        icon::minus(self.ink("zoom:out", theme::text())),
                        self.lit("zoom:reset", "level"),
                        self.tab().zoom,
                        self.lit("zoom:in", "step"),
                        icon::add(self.ink("zoom:in", theme::text())),
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
            "body{{background:{elevated};color:{text};font-size:13.5px;\
                  border-width:1px;border-color:{line};padding:6px;}} \
             .row{{display:flex;justify-content:space-between;align-items:center;\
                  height:32px;padding-left:10px;padding-right:10px;border-radius:8px;}} \
             .item{{color:{text};}} \
             .hot{{background:{hover};}} \
             .rule{{height:1px;background:{line};margin-top:6px;margin-bottom:6px;}} \
             .label{{color:{text};}} \
             .key{{color:{faint};font-size:12px;}} \
             .zoom{{display:flex;align-items:center;height:32px;\
                   padding-left:10px;padding-right:6px;}} \
             .zlabel{{display:inline-block;flex-grow:1;color:{text};}} \
             .step{{display:inline-flex;flex-shrink:0;align-items:center;justify-content:center;\
                   width:26px;height:26px;border-radius:7px;background:{surface};\
                   border-width:1px;border-color:{line};}} \
             .level{{display:inline-flex;flex-shrink:0;align-items:center;justify-content:center;\
                    color:{muted};width:46px;font-size:12px;border-radius:7px;}}",
            elevated = theme::elevated(),
            surface = theme::surface(),
            hover = theme::hover(),
            text = theme::text(),
            muted = theme::muted(),
            faint = theme::faint(),
            line = theme::line(),
        )
    }

    fn tooltip_css() -> String {
        format!(
            "body{{background:{elevated};color:{text};font-size:12.5px;\
                  border-width:1px;border-color:{line};}} \
             #tip{{padding-top:7px;padding-bottom:7px;padding-left:10px;padding-right:10px;\
                  text-align:center;}}",
            elevated = theme::elevated(),
            text = theme::text(),
            line = theme::line(),
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
        let animating = self.page_animating;
        let visible = self.tabs[index].scroll_y / self.tabs[index].zoom_factor();
        let tab = &mut self.tabs[index];
        let settled = tab.page_canvas.is_some()
            && tab.cache_w == w as u32
            && tab.cache_h == h as u32
            && tab.band_covers(visible, h)
            && !animating
            // A renderer found dead by some other call (a click, typing, ...)
            // must not stay "settled" on its last good frame forever — this
            // is the one call site every unsettled tab passes through, so it
            // has to be where a dead renderer gets noticed and retried.
            && !tab.renderer.is_dead();
        if settled {
            return;
        }
        let render_start = std::time::Instant::now();
        // `resize` doubles as "give me an updated frame at this size" even
        // when the size hasn't changed — the renderer's own clock (see
        // `renderer::Session::created`) is what actually advances a
        // transition between calls, not anything this message carries.
        //
        // A renderer a previous call already gave up on (`is_dead`) is
        // reloaded from scratch here rather than asked again — this is the
        // one call site every unsettled tab passes through on its own, no
        // matter which interaction found the renderer dead, so it is the
        // natural place to notice and recover rather than every call site
        // trying to.
        // The band to ask for: what is on screen, with a screenful of slack each
        // way so scrolling moves inside it rather than through the pipe.
        let band_top = (visible - h * BAND_MARGIN).max(0.0);
        let band_height = h * (1.0 + 2.0 * BAND_MARGIN);
        let frame = match tab.renderer.is_dead() {
            true => tab.respawn(w, band_height, band_top),
            false => tab.renderer.resize(w, band_height, band_top),
        };
        let Some(frame) = frame else { return };
        if timing_wanted() {
            eprintln!("page render {:?}", render_start.elapsed());
        }
        tab.blocked_count = tab.loader.blocked.get();
        let page_animating = frame.animating;
        tab.apply_frame(frame, w as u32, h as u32);
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
            let resized = tab.cache_w != layout_w as u32 || tab.cache_h != layout_h as u32;
            // Scrolling off the end of the band the renderer sent is the other
            // way a frame goes stale: the pixels for where we are looking now
            // were never painted, so they have to be asked for.
            let scrolled_out = !tab.band_covers(tab.scroll_y / zoom, layout_h);
            let stale = resized || scrolled_out;
            if tab.page_canvas.is_none() || self.page_animating || (stale && !self.animating) {
                self.render_pane(self.active, layout_w, layout_h);
            }
            let tab = &mut self.tabs[self.active];
            // Clamp scroll to available overflow, in screen pixels. The whole
            // document's height, not the band's — the band is one screenful.
            let content = tab.doc_height * zoom;
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
                    &Self::ai_css(),
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
        let canvas_rgb = theme::packed(theme::canvas());
        // The window shows past the page card on two edges, and that showing
        // part is chrome — filling it with the page's own colour would make the
        // card's margin look like a rendering gap.
        let mut buffer = vec![theme::packed(theme::chrome()); (w * h) as usize];
        let page = self.tabs[self.active].page_canvas.as_ref().expect("rendered above");
        blit_page(
            &mut buffer,
            w,
            h,
            page,
            (regions.content_x, regions.content_y, regions.content_w, regions.content_h),
            scroll,
            zoom,
            self.tabs[self.active].band_top,
        );
        if let Some(selection) = self.tabs[self.active].selection {
            let tab = &self.tabs[self.active];
            tint_selection(
                &mut buffer,
                w,
                h,
                (regions.content_x, regions.content_y, regions.content_w, regions.content_h),
                scroll,
                zoom,
                &selected_runs(&tab.text_runs, selection),
            );
        }
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
                        (
                            regions.other_x,
                            regions.content_y,
                            regions.other_w,
                            regions.content_h,
                        ),
                        scroll,
                        zoom,
                        tab.band_top,
                    );
                    if let Some(selection) = tab.selection {
                        tint_selection(
                            &mut buffer,
                            w,
                            h,
                            (
                                regions.other_x,
                                regions.content_y,
                                regions.other_w,
                                regions.content_h,
                            ),
                            scroll,
                            zoom,
                            &selected_runs(&tab.text_runs, selection),
                        );
                    }
                }
            }
            // The divider: a hairline in the gap, so the two pages read as two.
            let start = regions.divider_x().unwrap_or(0);
            for y in regions.content_y..h {
                for x in start..(start + DIVIDER_W).min(w) {
                    let edge = x == start + DIVIDER_W / 2;
                    let hairline = theme::packed(theme::line());
                    buffer[(y * w + x) as usize] = if edge { hairline } else { canvas_rgb };
                }
            }
        }
        for (canvas, x, y) in &surfaces {
            blit(&mut buffer, w, h, canvas, *x, *y);
        }

        // Scrollbar: a track down the right edge of the page, with a thumb sized
        // to the visible fraction. Only shown when the page actually overflows.
        let content_h = self.tabs[self.active].doc_height * zoom;
        if let Some((offset, thumb_h)) =
            scrollbar_thumb(content_h, regions.content_h as f32, scroll)
        {
            let bar_w = SCROLLBAR_W;
            let x0 = (regions.content_x + regions.content_w).saturating_sub(bar_w);
            let thumb_top = regions.content_y + offset as u32;
            // The track is barely there and the thumb is the only real mark —
            // a scrollbar says where you are, it does not need to be furniture.
            let thumb = theme::packed(theme::edge());
            let track = theme::packed(&theme::mix(theme::edge(), theme::canvas(), 0.14));
            for y in regions.content_y..h {
                for x in x0..(x0 + bar_w).min(w) {
                    let on_thumb = y >= thumb_top && y < thumb_top + thumb_h as u32;
                    buffer[(y * w + x) as usize] = if on_thumb { thumb } else { track };
                }
            }
        }

        // The page is a card on the window, so its corners are rounded and its
        // edge is ruled — after the scrollbar, which draws inside it.
        let hairline = theme::packed(theme::line());
        let behind = theme::packed(theme::chrome());
        let pane = (regions.content_x, regions.content_y, regions.content_w, regions.content_h);
        frame_pane(&mut buffer, w, h, pane, hairline, behind);
        if regions.other_w > 0 {
            let other =
                (regions.other_x, regions.content_y, regions.other_w, regions.content_h);
            frame_pane(&mut buffer, w, h, other, hairline, behind);
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
            blit_rounded(&mut buffer, w, h, &menu, x, y, POPOVER_RADIUS);
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
                blit_rounded(&mut buffer, w, h, &tip, x, y, 8);
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

    /// The assistant panel. Built per call rather than cached for the process:
    /// the palette can change under it, and a stylesheet held in a `OnceLock`
    /// would keep drawing in whichever theme happened to be in force first.
    fn ai_css() -> String {
        format!(
            "body{{background:{chrome};color:{muted};font-size:13.5px;\
                  border-left-width:1px;border-color:{line};}} \
             #head{{display:flex;align-items:center;background:{chrome};color:{text};\
                   height:38px;padding-left:16px;padding-right:16px;\
                   border-bottom-width:1px;border-color:{line};}} \
             .line{{padding-left:16px;padding-right:16px;padding-top:3px;\
                   padding-bottom:3px;color:{text};}} \
             .sec{{color:{muted};font-size:12px;padding-left:16px;padding-right:16px;\
                  padding-top:18px;padding-bottom:5px;}} \
             .src{{color:{faint};padding:16px;font-size:12px;}}",
            chrome = theme::chrome(),
            text = theme::text(),
            muted = theme::muted(),
            faint = theme::faint(),
            line = theme::line(),
        )
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
    (x0, y0, pane_w, pane_h): (u32, u32, u32, u32),
    scroll: f32,
    zoom: f32,
    band_top: f32,
) {
    let inv_zoom = 1.0 / zoom;
    let right = (x0 + pane_w).min(w);
    let bottom = (y0 + pane_h).min(h);
    for y in y0..bottom {
        // Window row -> document row -> row of the band actually held.
        let document_row = ((y - y0) as f32 + scroll) * inv_zoom;
        let sy = document_row - band_top;
        if sy < 0.0 || sy as usize >= page.height {
            continue; // outside the band: leave the window's own surface showing
        }
        let sy = sy as usize;
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

/// Wash the selected words with a tint, over the page the compositor has just
/// blitted.
///
/// Drawn here rather than by the engine because the selection belongs to the
/// browser, not to the page — and because going through the engine would mean a
/// re-layout and a re-render for every pixel the mouse moves while dragging.
fn tint_selection(
    buffer: &mut [u32],
    w: u32,
    h: u32,
    (x0, y0, pane_w, pane_h): (u32, u32, u32, u32),
    scroll: f32,
    zoom: f32,
    runs: &[&TextRun],
) {
    let right = (x0 + pane_w).min(w);
    let bottom = (y0 + pane_h).min(h);
    for (i, run) in runs.iter().enumerate() {
        // The space between two selected words on the same line belongs to the
        // selection. Inline layout keeps a fragment per word and none for the
        // gaps, so without this the wash comes out striped. Only a gap narrow
        // enough to be a space is bridged — two columns that happen to share a
        // row are not one run of text.
        let width = match runs.get(i + 1) {
            Some(next)
                if next.y == run.y
                    && next.x > run.x
                    && next.x - (run.x + run.width) <= run.height =>
            {
                next.x - run.x
            }
            _ => run.width,
        };
        // The inverse of `blit_page`'s window row -> document row.
        let left = x0 as f32 + run.x * zoom;
        let top = y0 as f32 + run.y * zoom - scroll;
        // Float-to-int casts saturate, so a run scrolled off the top clamps to
        // zero and its range comes out empty rather than wrapping.
        let x_from = (left.max(x0 as f32) as u32).min(right);
        let x_to = ((left + width * zoom).max(0.0) as u32).min(right);
        let y_from = (top.max(y0 as f32) as u32).min(bottom);
        let y_to = ((top + run.height * zoom).max(0.0) as u32).min(bottom);
        for y in y_from..y_to {
            for x in x_from..x_to {
                let slot = &mut buffer[(y * w + x) as usize];
                *slot = tinted(*slot);
            }
        }
    }
}

/// Blend [`SELECTION_TINT`] over one packed pixel, so the words underneath stay
/// readable instead of being painted out.
fn tinted(pixel: u32) -> u32 {
    let (r, g, b) = SELECTION_TINT;
    let mix = |shift: u32, tint: u32| {
        let base = (pixel >> shift) & 0xff;
        ((base * (255 - SELECTION_ALPHA) + tint * SELECTION_ALPHA) / 255) << shift
    };
    mix(16, r) | mix(8, g) | mix(0, b)
}

/// Round a pane's corners and rule its edge.
///
/// The compositor copies rectangles, so a card with soft corners is finished
/// here rather than by the engine: well inside the radius nothing changes,
/// across it the page gives way to the window behind, and the last pixel or so
/// is the rule that separates the two.
fn frame_pane(
    buffer: &mut [u32],
    w: u32,
    h: u32,
    (x0, y0, pane_w, pane_h): (u32, u32, u32, u32),
    line: u32,
    behind: u32,
) {
    let (right, bottom) = ((x0 + pane_w).min(w), (y0 + pane_h).min(h));
    // A pane too small to round is left alone rather than drawn wrong.
    if right <= x0 + 2 * PAGE_RADIUS || bottom <= y0 + 2 * PAGE_RADIUS {
        return;
    }
    for x in (x0 + PAGE_RADIUS)..(right - PAGE_RADIUS) {
        buffer[(y0 * w + x) as usize] = line;
        buffer[((bottom - 1) * w + x) as usize] = line;
    }
    for y in (y0 + PAGE_RADIUS)..(bottom - PAGE_RADIUS) {
        buffer[(y * w + x0) as usize] = line;
        buffer[(y * w + right - 1) as usize] = line;
    }
    let radius = PAGE_RADIUS as f32;
    for dy in 0..PAGE_RADIUS {
        for dx in 0..PAGE_RADIUS {
            // How far this pixel's centre is from the corner's own centre.
            let (ox, oy) = (radius - dx as f32 - 0.5, radius - dy as f32 - 0.5);
            let distance = (ox * ox + oy * oy).sqrt();
            let paint = match distance {
                d if d > radius + 0.5 => behind,
                d if d > radius - 0.5 => mix_rgb(behind, line, radius + 0.5 - d),
                d if d > radius - 1.5 => line,
                _ => continue, // still page
            };
            for (x, y) in [
                (x0 + dx, y0 + dy),
                (right - 1 - dx, y0 + dy),
                (x0 + dx, bottom - 1 - dy),
                (right - 1 - dx, bottom - 1 - dy),
            ] {
                buffer[(y * w + x) as usize] = paint;
            }
        }
    }
}

/// `ratio` of `over` blended onto `under`, both packed `0xRRGGBB`.
fn mix_rgb(under: u32, over: u32, ratio: f32) -> u32 {
    let ratio = ratio.clamp(0.0, 1.0);
    let channel = |shift: u32| {
        let (a, b) = ((under >> shift & 255) as f32, (over >> shift & 255) as f32);
        ((a + (b - a) * ratio).round() as u32) << shift
    };
    channel(16) | channel(8) | channel(0)
}

/// Blit a surface with its corners cut, for something that floats over the page.
///
/// A rounded box the engine painted would still arrive as a rectangle, because
/// the canvas it paints on has no transparency — so the corner is taken off
/// here, by simply not copying the pixels outside it. What was underneath stays
/// where it is, which is what makes the corner look cut rather than filled.
fn blit_rounded(
    buffer: &mut [u32],
    w: u32,
    h: u32,
    canvas: &Canvas,
    x0: u32,
    y0: u32,
    radius: u32,
) {
    let (cw, ch) = (canvas.width as u32, canvas.height as u32);
    let radius = radius.min(cw / 2).min(ch / 2);
    for y in 0..ch.min(h.saturating_sub(y0)) {
        for x in 0..cw.min(w.saturating_sub(x0)) {
            // How far into a corner this pixel is, if it is in one at all.
            let dx = radius as f32 - x.min(cw - 1 - x).min(radius) as f32;
            let dy = radius as f32 - y.min(ch - 1 - y).min(radius) as f32;
            let distance = (dx * dx + dy * dy).sqrt();
            if distance > radius as f32 {
                continue; // outside the corner: leave what is under it
            }
            let px = canvas.pixels[(y * cw + x) as usize];
            let paint = (px.r as u32) << 16 | (px.g as u32) << 8 | px.b as u32;
            let slot = ((y0 + y) * w + x0 + x) as usize;
            // The last half pixel of the arc fades, or the curve reads as steps.
            buffer[slot] = match radius as f32 - distance {
                edge if edge < 1.0 => mix_rgb(buffer[slot], paint, edge.max(0.0)),
                _ => paint,
            };
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
    /// A storage area belongs to an origin. If this ever collapses back to a
    /// bare host, a page served over http silently gains read and write access
    /// to whatever the https site of the same name stored — a boundary that
    /// fails open and shows no symptom until it is being exploited.
    #[test]
    fn a_storage_area_is_scoped_to_an_origin_not_just_a_host() {
        let site = super::storage_site;
        assert_ne!(
            site("http://example.com/a"),
            site("https://example.com/a"),
            "http and https must not share a storage area"
        );
        assert_ne!(
            site("https://example.com/a"),
            site("https://example.com:8443/a"),
            "an explicit port is a different origin"
        );
        // The things that must NOT split an area.
        assert_eq!(site("https://example.com/a"), site("https://example.com/b/c?q=1"));
        assert_eq!(site("https://EXAMPLE.com/a"), site("https://example.com/a"));
        assert_eq!(site("https://user@example.com/a"), site("https://example.com/a"));
        // A subdomain is its own origin, as on the web.
        assert_ne!(site("https://app.example.com/"), site("https://example.com/"));
    }

    /// The two rules that decide who hears a `localStorage` write. Getting
    /// either wrong is silent: a missed event looks like a page that just
    /// did not react, and a self-delivered one looks like a page that
    /// reacted twice — or, if its handler writes, that will not stop.
    #[test]
    fn a_storage_write_reaches_other_tabs_on_its_site_and_nobody_else() {
        let notice = crate::localstore::StorageNotice {
            // An origin, which is what `storage_site` yields.
            site: "https://example.com".into(),
            tab: 7,
            key: Some("k".into()),
            old: None,
            new: Some("v".into()),
        };

        assert!(
            super::hears_storage(8, "https://example.com/page", &notice),
            "another tab on the same site should hear it"
        );
        assert!(
            !super::hears_storage(7, "https://example.com/page", &notice),
            "the tab that wrote it must never hear its own write"
        );
        assert!(
            !super::hears_storage(8, "https://other.example/page", &notice),
            "a tab on another site shares no storage area"
        );
    }

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
        // The page is a card with a margin, so it is that much narrower than
        // the space left over — everything downstream measures the card, not
        // the gap it sits in.
        assert_eq!(expanded.content_w, 1000 - RAIL_W - PAGE_GAP);
        assert_eq!(expanded.strip_h, 0);

        let icons = Regions::settled(1000, 700, settings_with(TabLayout::Vertical, Rail::Icons), false);
        assert_eq!(icons.rail_w, RAIL_ICON_W);

        // Hidden gives the page the whole window width.
        let hidden = Regions::settled(1000, 700, settings_with(TabLayout::Vertical, Rail::Hidden), false);
        assert_eq!(hidden.rail_w, 0);
        assert_eq!(hidden.content_w, 1000 - PAGE_GAP);
        assert_eq!(hidden.content_y, TOOLBAR_H);
        assert_eq!(hidden.content_h, 700 - TOOLBAR_H - PAGE_GAP);
    }

    #[test]
    fn horizontal_layout_trades_the_rail_for_a_strip() {
        let regions = Regions::settled(1000, 700, settings_with(TabLayout::Horizontal, Rail::Expanded), false);
        assert_eq!(regions.rail_w, 0, "no rail when tabs are on top");
        assert_eq!(regions.strip_h, TABSTRIP_H);
        // The page starts below both the strip and the toolbar.
        assert_eq!(regions.content_y, TABSTRIP_H + TOOLBAR_H);
        assert_eq!(regions.content_w, 1000 - PAGE_GAP);
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
    fn the_mark_rasterizes_for_the_taskbar() {
        // The window icon is drawn from the same ring the chrome draws, so a
        // broken path would cost the taskbar its icon silently.
        let icon = super::window_icon();
        assert!(icon.is_some(), "the mark did not rasterize");
        // ...and it is actually a picture, not an empty square.
        let drawn = zero_engine::svg::rasterize(&icon::ring(theme::text(), 64), 64, 64)
            .expect("rasterized");
        let painted = drawn.pixels.iter().filter(|p| p.a > 0).count();
        assert!(painted > 500, "only {painted} pixels of mark");
        // The ring has a gap, so its middle stays empty.
        assert_eq!(drawn.pixels[32 * 64 + 32].a, 0);
    }

    #[test]
    fn only_the_browsers_own_screens_share_a_renderer_process() {
        // Settings is a page whose controls are links, so changing a preference
        // is a navigation between two `zero://` pages — the one case worth
        // keeping the process for.
        assert!(can_share_a_renderer("zero://settings", "zero://settings"));
        assert!(can_share_a_renderer("zero://newtab", "zero://history"));
        // Everything the web touches keeps its own process, including one site
        // following another, and a site returning to a built-in page.
        assert!(!can_share_a_renderer("https://example.com", "zero://settings"));
        assert!(!can_share_a_renderer("zero://settings", "https://example.com"));
        assert!(!can_share_a_renderer("https://a.com", "https://b.com"));
        assert!(!can_share_a_renderer("https://a.com", "https://a.com"));
    }

    #[test]
    fn a_packed_colour_is_the_one_the_chrome_uses() {
        // The compositor fills whole rectangles itself rather than through the
        // engine, so it needs the palette as pixels. The two must not drift.
        assert_eq!(format!("#{:06x}", theme::packed(theme::canvas())), theme::canvas());
        assert_eq!(format!("#{:06x}", theme::packed("#1a222b")), "#1a222b");
    }

    #[test]
    fn each_theme_defines_every_colour_the_other_does() {
        // A palette with a hole in it is a surface that draws in the wrong
        // theme's colour, which is only ever noticed by eye. Mixing proves each
        // value parses as a colour rather than merely being present.
        for theme_choice in [crate::settings::Theme::Light, crate::settings::Theme::Dark] {
            let mut settings = Settings::default();
            settings.theme = theme_choice;
            crate::settings::preview(settings);
            let p = theme::palette();
            for colour in [
                p.canvas, p.chrome, p.elevated, p.surface, p.hover, p.line, p.text, p.muted,
                p.faint, p.saved, p.ok, p.link, p.edge, theme::accent(),
            ] {
                assert_eq!(colour.len(), 7, "{colour} is not #rrggbb");
                assert_eq!(theme::mix(colour, colour, 0.5), colour);
            }
        }
        crate::settings::preview(Settings::default());
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

    /// Two lines of three words each, laid out the way inline layout lays them
    /// out: every word on a line shares that line's top as its `y`.
    fn two_lines() -> Vec<TextRun> {
        let words = [
            ("The", 0.0, 0.0, 30.0),
            ("quick", 34.0, 0.0, 50.0),
            ("fox", 88.0, 0.0, 30.0),
            ("jumps", 0.0, 20.0, 50.0),
            ("over", 54.0, 20.0, 40.0),
            ("it", 98.0, 20.0, 16.0),
        ];
        words
            .iter()
            .map(|(text, x, y, width)| TextRun {
                text: text.to_string(),
                x: *x,
                y: *y,
                width: *width,
                height: 20.0,
            })
            .collect()
    }

    fn selected(runs: &[TextRun], selection: Selection) -> String {
        selection_text(&selected_runs(runs, selection))
    }

    #[test]
    fn a_drag_selects_from_where_it_started_to_where_it_ended() {
        let runs = two_lines();
        // Across one line: from inside "The" to past the middle of "fox".
        let span = Selection::Span { anchor: (5.0, 10.0), focus: (110.0, 10.0) };
        assert_eq!(selected(&runs, span), "The quick fox");
        // Dragged backwards is the same selection, not an empty one.
        let back = Selection::Span { anchor: (110.0, 10.0), focus: (5.0, 10.0) };
        assert_eq!(selected(&runs, back), "The quick fox");
        // Across lines, the line break comes back as one.
        let down = Selection::Span { anchor: (40.0, 10.0), focus: (60.0, 30.0) };
        assert_eq!(selected(&runs, down), "quick fox\njumps");
        // A click selects nothing: both ends land in the same place.
        let click = Selection::Span { anchor: (40.0, 10.0), focus: (40.0, 10.0) };
        assert_eq!(selected(&runs, click), "");
    }

    #[test]
    fn double_click_takes_a_word_triple_click_a_line_and_ctrl_a_the_page() {
        let runs = two_lines();
        assert_eq!(selected(&runs, Selection::Word((40.0, 10.0))), "quick");
        assert_eq!(selected(&runs, Selection::Line((40.0, 10.0))), "The quick fox");
        // The second line, not the first: a line is picked by where the click
        // landed, and the two share nothing but their words' height.
        assert_eq!(selected(&runs, Selection::Line((60.0, 30.0))), "jumps over it");
        assert_eq!(selected(&runs, Selection::All), "The quick fox\njumps over it");
        // A gesture that lands in the margin selects nothing rather than
        // guessing at the nearest word.
        assert_eq!(selected(&runs, Selection::Word((300.0, 10.0))), "");
        assert_eq!(selected(&runs, Selection::Line((300.0, 10.0))), "");
    }

    #[test]
    fn a_selection_boundary_falls_on_whichever_side_of_a_word_the_cursor_passed() {
        let runs = two_lines();
        // "quick" spans 34..84, so its middle is 59. Stopping before the middle
        // leaves it out; stopping past the middle takes it.
        let short = Selection::Span { anchor: (5.0, 10.0), focus: (50.0, 10.0) };
        assert_eq!(selected(&runs, short), "The");
        let long = Selection::Span { anchor: (5.0, 10.0), focus: (70.0, 10.0) };
        assert_eq!(selected(&runs, long), "The quick");
    }

    #[test]
    fn the_selection_tint_lightens_a_pixel_without_replacing_it() {
        // Black text under the wash stays darker than the white page around it
        // — the whole point of tinting rather than filling.
        let (ink, paper) = (tinted(0x000000), tinted(0xffffff));
        assert!(ink < paper, "tinted text must stay darker than tinted background");
        assert_ne!(paper, 0xffffff, "the wash has to be visible on a white page");
        // The blend is integer maths on bytes: nothing may carry past the top
        // of the pixel, and what comes out has to read as the blue it is.
        assert!(paper < 0x100_0000, "the blend must stay inside three channels");
        assert!(paper & 0xff > (paper >> 16) & 0xff, "more blue left than red");
    }
}

//! Built-in `zero://` pages, generated as HTML and rendered by our own engine.
//!
//! Dogfooding the engine for browser UI keeps these pages honest: if history
//! renders badly, the engine has a bug worth fixing.

use crate::i18n::t;
use crate::storage::{self, Visit};

use crate::app::theme;

/// One stylesheet for every built-in page, because they are one product.
///
/// Rows are separated by hairlines rather than filled cards: the engine has a
/// single font weight, so lines and space carry the structure that weight would
/// carry elsewhere — and a page of tinted boxes reads as decoration, not order.
fn page_style() -> String {
    format!(
        "<style>\
         body{{background:{canvas};color:{text};font-size:14.5px;\
              padding-left:56px;padding-right:56px;padding-top:44px;padding-bottom:64px;}}\
         h1{{color:{text};font-size:32px;letter-spacing:-0.02em;padding-bottom:6px;}}\
         .lede{{color:{muted};font-size:15px;padding-bottom:30px;}}\
         .sec{{color:{muted};font-size:13px;padding-top:34px;padding-bottom:6px;}}\
         .row{{display:flex;justify-content:space-between;align-items:center;\
              padding-top:14px;padding-bottom:14px;\
              border-bottom-width:1px;border-color:{line};}}\
         .name{{color:{text};font-size:14.5px;}}\
         .hint{{color:{faint};font-size:12.5px;padding-top:3px;}}\
         .when{{color:{faint};font-size:13px;}}\
         .fact{{color:{muted};font-size:13.5px;}}\
         .seg{{display:inline-block;text-align:right;\
              padding-top:3px;padding-bottom:3px;}}\
         .opt{{display:inline-block;color:{muted};font-size:13px;border-radius:8px;\
              padding-left:13px;padding-right:13px;padding-top:6px;padding-bottom:6px;\
              text-decoration:none;}}\
         .opton{{display:inline-block;background:{chrome};color:{text};font-size:13px;\
                border-radius:8px;padding-left:13px;padding-right:13px;\
                padding-top:6px;padding-bottom:4px;text-decoration:none;\
                border-bottom-width:2px;border-color:{accent};}}\
         .optoff{{display:inline-block;color:{faint};font-size:13px;text-decoration:none;\
                 padding-left:13px;padding-right:13px;padding-top:6px;padding-bottom:6px;}}\
         .empty{{background:{chrome};padding-left:22px;padding-right:22px;\
                padding-top:20px;padding-bottom:20px;border-radius:12px;color:{muted};}}\
         a{{color:{link};}}\
         </style>",
        canvas = theme::canvas(),
        chrome = theme::chrome(),
        text = theme::text(),
        muted = theme::muted(),
        faint = theme::faint(),
        accent = theme::accent(),
        line = theme::line(),
        link = theme::link(),
    )
}

/// The start page. Deliberately sparse: the mark, one field, and the sites you
/// actually use — the UI spec asks for space rather than density
/// (docs/02-UI-UX-SPEC.md §7).
fn newtab_style() -> String {
    // The field and the grid are the same width, so the page has one edge down
    // each side rather than two that nearly agree.
    const WIDTH: u32 = 586;
    format!(
        "<style>         body{{background:{canvas};color:{text};font-size:15px;text-align:center;}}         .hero{{padding-top:108px;padding-bottom:28px;}}         .ring{{display:flex;justify-content:center;padding-bottom:30px;}}         .mark{{color:{text};font-size:46px;letter-spacing:-0.04em;}}         .tag{{color:{muted};font-size:16px;padding-top:14px;padding-bottom:38px;}}         .field{{display:flex;justify-content:center;}}         .omni{{display:flex;align-items:center;width:{width}px;height:58px;               background:{surface};border-radius:14px;               border-width:1px;border-color:{line};               padding-left:10px;padding-right:18px;}}         .engine{{flex-shrink:0;background:{chrome};color:{muted};font-size:13px;                 border-radius:9px;padding-left:12px;padding-right:12px;                 padding-top:7px;padding-bottom:7px;}}         .q{{flex-grow:1;background:transparent;color:{text};font-size:16px;            padding-left:14px;padding-right:0px;padding-top:0px;padding-bottom:0px;            border-width:0px;text-align:left;}}         .tiles-head{{color:{faint};font-size:13px;padding-bottom:12px;}}         .tiles-wrap{{display:flex;justify-content:center;                     padding-left:20px;padding-right:20px;padding-bottom:56px;}}         .tiles{{display:grid;grid-template-columns:repeat(3, 1fr);gap:10px;                width:{width}px;}}         .tile{{background:{surface};border-radius:10px;               border-width:1px;border-color:{line};text-decoration:none;               padding-left:14px;padding-right:14px;padding-top:12px;padding-bottom:12px;               text-align:left;}}         .tile-host{{display:block;color:{text};font-size:13.5px;white-space:nowrap;}}         .tile-page{{display:block;color:{faint};font-size:12px;padding-top:3px;                    white-space:nowrap;}}         </style>",
        width = WIDTH,
        canvas = theme::canvas(),
        chrome = theme::chrome(),
        surface = theme::surface(),
        text = theme::text(),
        muted = theme::muted(),
        faint = theme::faint(),
        line = theme::line(),
    )
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// True for addresses this module serves.
pub fn is_internal(target: &str) -> bool {
    target.starts_with("zero://")
}

/// Render a built-in page, or a "not found" page for an unknown one.
///
/// The query string is stripped before matching: `zero://settings?rail=icons`
/// is the settings page, and the shell has already applied the preference.
pub fn page(target: &str) -> String {
    let path = target.split('?').next().unwrap_or(target);
    match path.trim_end_matches('/') {
        "zero://newtab" => newtab_page(),
        "zero://history" => history_page(),
        "zero://bookmarks" => bookmarks_page(),
        "zero://downloads" => downloads_page(),
        "zero://settings" => settings_page(),
        other => wrap(
            &t("Unknown page"),
            &format!(
                "<div class=\"empty\">{} {}.</div>",
                t("No built-in page at"),
                escape(other)
            ),
        ),
    }
}

fn wrap(title: &str, body: &str) -> String {
    // A built-in page carries a title like any other, so the tab and the history
    // entry read "History" rather than a fragment of the URL.
    format!(
        "<html><head><title>{title}</title>{}</head><body><h1>{title}</h1>{body}</body></html>",
        page_style()
    )
}

/// The page shown when a page arrived but could not be drawn.
///
/// The renderer is a separate process, and one that stops — killed, out of
/// memory, handed something it could not finish — must cost its own tab and
/// nothing else. The tab keeps its address so reloading is one keystroke, and
/// says plainly what happened: a blank tab that explains nothing is worse than
/// either the page or the error.
pub fn render_failed_page(url: &str) -> String {
    format!(
        "<html><head><title>{title}</title>{style}</head><body>         <h1>{title}</h1>         <div class=\"lede\">{lede}</div>         <div class=\"empty\">{}</div>         <div class=\"hint\">{hint}</div>         </body></html>",
        escape(url),
        style = page_style(),
        title = escape(&t("This page could not be drawn")),
        lede = escape(&t(
            "The renderer stopped before it finished this page. Your other tabs are unaffected."
        )),
        hint = escape(&t("Press Ctrl+R to try again.")),
    )
}

/// The page's own markup, as Zero received it.
///
/// Lines become separate blocks and leading spaces become non-breaking ones:
/// the engine collapses whitespace like any HTML renderer, so indentation has
/// to survive as content rather than as formatting.
pub fn source_page(url: &str, source: &str) -> String {
    let lines: String = source
        .lines()
        .map(|line| {
            let indent = line.len() - line.trim_start().len();
            let spaces = "\u{a0}".repeat(indent);
            format!(
                "<div class=\"ln\">{spaces}{}</div>",
                escape(line.trim_start())
            )
        })
        .collect();
    format!(
        "<html><head><title>Source of {}</title>{style}</head><body>\
         <div class=\"head\">Source of {}</div>{lines}</body></html>",
        escape(url),
        escape(url),
        style = source_style(),
    )
}

fn source_style() -> String {
    format!(
        "<style>\
         body{{background:{canvas};color:{text};padding:28px;font-size:13px;}}\
         .head{{color:{faint};font-size:13px;padding-bottom:14px;}}\
         .ln{{color:{text};}}\
         </style>",
        canvas = theme::canvas(),
        text = theme::text(),
        faint = theme::faint(),
    )
}

/// The start page: one search field, and where you were last.
///
/// The search box is an ordinary GET form, so it goes through the same
/// submission path as any site's search box rather than a special case.
fn newtab_page() -> String {
    let tiles: String = recent_sites(8)
        .iter()
        .map(|(url, title)| {
            // The host names the tile; the page's own title says which page it
            // was. A tile with only a title is a guessing game about where it
            // goes, and one with only a URL is unreadable.
            let host = host_of(url);
            // A title that is really just the address again says nothing the
            // host has not already said, so the tile stays one line.
            let says_more = !title.trim().is_empty() && title != url && title.trim() != host;
            let name = match says_more {
                true => format!(
                    "<span class=\"tile-page\">{}</span>",
                    escape(&short(title, 26))
                ),
                false => String::new(),
            };
            format!(
                "<a class=\"tile\" href=\"{}\"><span class=\"tile-host\">{}</span>{name}</a>",
                escape(url),
                escape(&short(&host, 24)),
            )
        })
        .collect();
    // Tiles only earn their heading once there is something to show.
    let tiles = match tiles.is_empty() {
        true => String::new(),
        false => format!(
            "<div class=\"tiles-head\">{}</div>             <div class=\"tiles-wrap\"><div class=\"tiles\">{tiles}</div></div>",
            escape(&t("Recently visited"))
        ),
    };
    // The field submits to whichever engine settings names, so the start page
    // and the address bar can never send a search to different places — and it
    // says which one that is, because "search" is not a promise you can keep
    // silently: the query is about to leave for somebody.
    let now = crate::settings::current();
    let (action, field) = now.search_form();
    // Everything here is centred by a flex row rather than by `text-align`,
    // which centres words but leaves a box where it started.
    format!(
        "<html><head><title>New Tab</title>{style}</head><body>         <div class=\"hero\">         <div class=\"ring\">{ring}</div>         <div class=\"mark\">zero</div>         <div class=\"tag\">{tag}</div>         <form class=\"field\" action=\"{action}\">         <div class=\"omni\">         <span class=\"engine\">{engine}</span>         <input name=\"{field}\" class=\"q\" placeholder=\"{ask}\">         </div></form>         </div>{tiles}</body></html>",
        style = newtab_style(),
        ring = crate::app::icon::ring(theme::text(), 76),
        tag = escape(&t("A browser built from scratch, in India.")),
        engine = escape(now.engine().1),
        ask = escape(&t("Search or enter an address")),
    )
}

/// The sites visited most recently, newest first and each one only once.
fn recent_sites(limit: usize) -> Vec<(String, String)> {
    let mut seen = std::collections::HashSet::new();
    let mut sites = Vec::new();
    for visit in storage::load_history().into_iter().rev() {
        if !seen.insert(visit.url.clone()) {
            continue;
        }
        sites.push((visit.url, visit.title));
        if sites.len() == limit {
            break;
        }
    }
    sites
}

/// The host on its own, which is what a tile is really identified by.
fn host_of(url: &str) -> String {
    url.split("://")
        .nth(1)
        .unwrap_or(url)
        .split('/')
        .next()
        .unwrap_or(url)
        .trim_start_matches("www.")
        .to_string()
}

/// A tile is small, and the engine has no `text-overflow` — so anything too
/// long for one is cut here, to a count that fits the column
/// (docs/02-UI-UX-SPEC.md §12).
fn short(text: &str, fits: usize) -> String {
    match text.chars().count() {
        n if n > fits => text.chars().take(fits - 1).chain(['…']).collect(),
        _ => text.to_string(),
    }
}

/// A row: what the setting is on the left, the control on the right.
fn setting(name: &str, hint: &str, control: &str) -> String {
    format!(
        "<div class=\"row\"><div><div class=\"name\">{name}</div>\
         <div class=\"hint\">{hint}</div></div>{control}</div>"
    )
}

/// How wide a control holding these labels needs to be.
///
/// ponytail: the engine has no shrink-to-fit for a flex item and no way to ask
/// how wide a string will be, so the width is estimated from the character count
/// with room to spare. Options wrap onto a second line if this comes out short,
/// so it errs high — and the control has no background of its own, which is what
/// makes the slack invisible. Measure properly if the engine grows an API for it.
fn control_width(labels: &[&str]) -> u32 {
    labels
        .iter()
        .map(|label| 30 + (label.chars().count() as u32 * 8))
        .sum::<u32>()
        + 10
}

/// A segmented control built from links. Each option is a `zero://settings` URL
/// carrying its own value, so choosing one is an ordinary navigation — the same
/// path any link on the web takes through this browser. The chosen option is
/// marked with an accent edge, the same way the active tab is.
fn segmented(key: &str, options: &[(&str, &str)], chosen: &str) -> String {
    // Widths are measured on the translated text, since that is what is drawn.
    let translated: Vec<String> = options.iter().map(|(_, label)| t(label)).collect();
    let labels: Vec<&str> = translated.iter().map(String::as_str).collect();
    let opts: String = options
        .iter()
        .map(|(value, label)| {
            let class = if *value == chosen { "opton" } else { "opt" };
            format!(
                "<a class=\"{class}\" href=\"zero://settings?{key}={value}\">{}</a>",
                escape(&t(label))
            )
        })
        .collect();
    wrap_control(&labels, &opts)
}

/// The engine has no inline `style` attribute, so a control that needs its own
/// width carries a one-rule stylesheet with it. `<style>` is `display:none` in
/// the user-agent sheet, so it can sit anywhere — including inside a flex row.
fn wrap_control(labels: &[&str], inner: &str) -> String {
    let width = control_width(labels);
    format!(
        "<style>.w{width}{{width:{width}px;}}</style>\
         <div class=\"seg w{width}\">{inner}</div>"
    )
}

fn on_off(key: &str, on: bool) -> String {
    segmented(
        key,
        &[("on", "On"), ("off", "Off")],
        if on { "on" } else { "off" },
    )
}

/// A settings row whose name and hint are translated.
fn setting_t(name: &str, hint: &str, control: &str) -> String {
    setting(&escape(&t(name)), &escape(&t(hint)), control)
}

fn settings_page() -> String {
    let now = crate::settings::current();
    let layout = segmented(
        "layout",
        &[("vertical", "Vertical"), ("horizontal", "Horizontal")],
        match now.layout {
            crate::settings::TabLayout::Vertical => "vertical",
            crate::settings::TabLayout::Horizontal => "horizontal",
        },
    );
    let rail = segmented(
        "rail",
        &[
            ("expanded", "Expanded"),
            ("icons", "Icons"),
            ("hidden", "Hidden"),
        ],
        match now.rail {
            crate::settings::Rail::Expanded => "expanded",
            crate::settings::Rail::Icons => "icons",
            crate::settings::Rail::Hidden => "hidden",
        },
    );
    let zoom_options: Vec<(String, String)> = [80, 100, 125, 150]
        .iter()
        .map(|z| (z.to_string(), format!("{z}%")))
        .collect();
    let zoom = segmented(
        "zoom",
        &zoom_options
            .iter()
            .map(|(v, l)| (v.as_str(), l.as_str()))
            .collect::<Vec<_>>(),
        &now.zoom.to_string(),
    );
    let engines: Vec<(&str, &str)> = crate::settings::ENGINES
        .iter()
        .map(|(key, label, _)| (*key, *label))
        .collect();
    let engine = segmented("engine", &engines, now.engine().0);
    let theme_control = segmented(
        "theme",
        &[("light", "Light"), ("dark", "Dark"), ("system", "System")],
        match now.theme {
            crate::settings::Theme::Light => "light",
            crate::settings::Theme::Dark => "dark",
            crate::settings::Theme::System => "system",
        },
    );
    // Each language is named in its own script, so these labels are not translated.
    let language = segmented("lang", crate::settings::LANGUAGES, now.language());
    // A space is named by whoever makes it, so its own name is its label.
    let here = crate::spaces::current();
    let names = crate::spaces::list();
    let spaces: Vec<(&str, &str)> = names.iter().map(|n| (n.as_str(), n.as_str())).collect();
    let spaces = segmented("space", &spaces, &here);
    let profile = crate::storage::profile_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| "unavailable".to_string());

    let body = format!(
        "<div class=\"lede\">{lede}</div>\
         <div class=\"sec\">{appearance}</div>{}{}{}{}{}{}{}\
         <div class=\"sec\">{search}</div>{}\
         <div class=\"sec\">{privacy}</div>{}{}\
         <div class=\"sec\">{about}</div>{}{}{}{}{}{}",
        setting_t("Tab layout", "A rail down the side, or a strip across the top", &layout),
        setting_t("Tab rail", "How much of the vertical rail stays open", &rail),
        setting_t("Page zoom", "The size new tabs open at. Ctrl+= and Ctrl+- change one tab", &zoom),
        setting_t("Language", "What the browser's own screens are written in", &language),
        setting_t(
            "Space",
            "Separate profiles: their own tabs, history, cookies and colour.              Open zero://settings?space=work to make one",
            &spaces,
        ),
        setting_t("Theme", "System follows whatever your desktop is set to", &theme_control),
        setting_t(
            "Animation",
            "Slides the tab rail open and closed. Turn off to change it instantly",
            &on_off("motion", now.motion),
        ),
        setting_t("Search engine", "Where the address bar sends anything that isn't a URL", &engine),
        setting_t(
            "Block trackers",
            "Drops requests to known tracking and ad hosts before they are made",
            &on_off("blocking", now.blocking),
        ),
        setting_t(
            "Reopen tabs at launch",
            "Restores the last session instead of starting on a new tab",
            &on_off("restore", now.restore),
        ),
        setting_t("Engine", "HTML, CSS and JavaScript, written from scratch in Rust",
            "<span class=\"fact\">Zero 0.1.0</span>"),
        setting_t(
            "Encryption at rest",
            "Where the key that protects this profile is kept",
            &format!(
                "<span class=\"fact\">{}</span>",
                escape(&match crate::crypto::is_available() {
                    true => crate::crypto::key_origin().to_string(),
                    false => t("Not available on this system").to_string(),
                })
            ),
        ),
        setting_t("Profile folder", "Where history, bookmarks and this file live",
            &format!("<span class=\"fact\">{}</span>", escape(&profile))),
        setting_t(
            "Process hardening",
            "Applied at startup, before any page is parsed. Not site isolation: page content still runs in this process",
            &format!(
                "<span class=\"fact\">{}</span>",
                escape(&t(crate::sandbox::describe()))
            ),
        ),
        setting_t(
            "Sync",
            "One sealed file and a code that opens it. Put the file wherever you              like — there is no server",
            "<span class=\"fact\">zero --export</span>",
        ),
        setting_t("Source", "Zero is open source, Apache-2.0",
            "<span class=\"fact\">github.com/zero-browser</span>"),
        lede = escape(&t("Every preference is stored on this device, as text you can read.")),
        appearance = escape(&t("Appearance")),
        search = escape(&t("Search")),
        privacy = escape(&t("Privacy")),
        about = escape(&t("About")),
    );
    console_wrap(&t("Settings"), &body)
}

fn downloads_page() -> String {
    let mut saved = crate::storage::load_downloads();
    saved.reverse(); // newest first, like history
    if saved.is_empty() {
        return console_wrap(
            &t("Downloads"),
            "<div class=\"empty\">Nothing saved yet. Press Ctrl+S to keep a copy of the \
             page you are reading.</div>",
        );
    }
    let rows: String = saved
        .iter()
        .map(|file| {
            setting(
                &escape(&file.name),
                &escape(&file.url),
                &format!(
                    "<span class=\"fact\">{}</span>",
                    escape(&date_of(file.when))
                ),
            )
        })
        .collect();
    console_wrap(
        &t("Downloads"),
        &format!(
            "<div class=\"lede\">{}</div>{rows}",
            t("Saved pages, newest first.")
        ),
    )
}

fn console_wrap(title: &str, body: &str) -> String {
    format!(
        "<html><head><title>{title}</title>{}</head><body><h1>{title}</h1>{body}</body></html>",
        page_style()
    )
}

/// Newest first, and only the most recent visit per URL so the list stays useful.
fn history_page() -> String {
    let mut visits = storage::load_history();
    visits.reverse();
    let mut seen = std::collections::HashSet::new();
    let rows: String = visits
        .iter()
        .filter(|v| seen.insert(v.url.clone()))
        .take(200)
        .map(|v| row(v.url.as_str(), &label(v), Some(v.when)))
        .collect();

    if rows.is_empty() {
        return wrap(
            &t("History"),
            &format!("<div class=\"empty\">{}</div>", t("Nothing visited yet.")),
        );
    }
    wrap(
        &t("History"),
        &format!(
            "<div class=\"lede\">{}</div>{rows}",
            t("Most recent first.")
        ),
    )
}

fn bookmarks_page() -> String {
    let marks = storage::load_bookmarks();
    let rows: String = marks.iter().map(|b| row(&b.url, &b.title, None)).collect();

    if rows.is_empty() {
        return wrap(
            &t("Bookmarks"),
            &format!(
                "<div class=\"empty\">{}</div>",
                t("No bookmarks yet. Press Ctrl+D on a page to add one.")
            ),
        );
    }
    wrap(&t("Bookmarks"), &rows)
}

/// A clickable entry: what it is on the left, when it was on the right, and a
/// hairline between it and the next — the same row every built-in page uses.
fn row(url: &str, title: &str, when: Option<u64>) -> String {
    let stamp = match when {
        Some(secs) => format!(" <span class=\"when\">{}</span>", escape(&date_of(secs))),
        None => String::new(),
    };
    format!(
        "<div class=\"row\"><a href=\"{}\">{}</a>{stamp}</div>",
        escape(url),
        escape(title)
    )
}

fn label(visit: &Visit) -> String {
    if visit.title.is_empty() {
        visit.url.clone()
    } else {
        visit.title.clone()
    }
}

/// Format a Unix timestamp as `YYYY-MM-DD`, the inverse of the cookie date maths.
fn date_of(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Howard Hinnant's civil-from-days algorithm.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_view_keeps_indentation_and_escapes_markup() {
        let page = source_page("https://a.com", "<div>\n  <p>hi</p>\n</div>");
        // One block per line, so whitespace collapsing cannot run them together.
        assert_eq!(page.matches("class=\"ln\"").count(), 3);
        // Markup is shown, not interpreted.
        assert!(page.contains("&lt;p&gt;hi&lt;/p&gt;"), "{page}");
        assert!(!page.contains("<p>hi</p>"));
        // Two leading spaces survive as non-breaking spaces.
        assert!(page.contains("\u{a0}\u{a0}&lt;p&gt;"), "{page}");
        // The URL is escaped too.
        assert!(source_page("https://a.com/<x>", "x").contains("&lt;x&gt;"));
    }

    #[test]
    fn the_start_page_names_the_engine_it_will_search_with() {
        // The field says where a query is about to be sent. If that ever drifts
        // from where the form actually posts, the page is lying about which
        // company is getting what you typed — so the two are checked together.
        for key in ["duckduckgo", "google", "brave", "startpage"] {
            let mut settings = crate::settings::current();
            assert!(settings.set("engine", key));
            crate::settings::preview(settings);

            let page = newtab_page();
            let now = crate::settings::current();
            assert!(page.contains(now.engine().1), "the field should name {key}");
            assert!(
                page.contains(now.search_form().0),
                "the form should post to {key}'s own address"
            );
        }
        crate::settings::preview(crate::settings::Settings::default());
    }

    #[test]
    fn a_tile_is_named_by_its_host_and_cut_to_fit() {
        assert_eq!(
            host_of("https://news.ycombinator.com/item?id=1"),
            "news.ycombinator.com"
        );
        // `www.` is noise on a tile this small.
        assert_eq!(
            host_of("https://www.wikipedia.org/wiki/Rust"),
            "wikipedia.org"
        );
        assert_eq!(short("github.com", 24), "github.com");
        // The engine has no `text-overflow`, so anything too long is cut here.
        assert_eq!(
            short("a-very-long-hostname.example.com", 12),
            "a-very-long…"
        );
    }

    #[test]
    fn recognises_internal_targets() {
        assert!(is_internal("zero://history"));
        assert!(!is_internal("https://zero.dev/history"));
    }

    #[test]
    fn unknown_pages_render_rather_than_fail() {
        let html = page("zero://nope");
        assert!(html.contains("No built-in page"));
        // The target is escaped, not injected raw.
        assert!(page("zero://<script>").contains("&lt;script&gt;"));
    }

    #[test]
    fn formats_dates_from_timestamps() {
        assert_eq!(date_of(0), "1970-01-01");
        assert_eq!(date_of(1_609_459_200), "2021-01-01");
        assert_eq!(date_of(1_582_977_600), "2020-02-29"); // leap day
    }
}

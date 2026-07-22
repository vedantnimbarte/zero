//! Built-in `zero://` pages, generated as HTML and rendered by our own engine.
//!
//! Dogfooding the engine for browser UI keeps these pages honest: if history
//! renders badly, the engine has a bug worth fixing.

use crate::i18n::t;
use crate::storage::{self, Visit};

use crate::app::theme;

/// Shared styling for the list pages, from the same palette as the window so a
/// built-in page does not look like a different program.
fn list_style() -> String {
    format!(
        "<style>\
         body{{background:{canvas};color:{text};padding:36px;font-size:15px;}}\
         h1{{color:{text};font-size:26px;padding:6px;}}\
         .sub{{color:{faint};font-size:13px;padding:6px;}}\
         .row{{background:{surface};padding:14px;border-radius:10px;margin:3px;}}\
         .alt{{background:{chrome};padding:14px;border-radius:10px;margin:3px;}}\
         a{{color:{link};}}\
         .when{{color:{faint};font-size:13px;}}\
         .empty{{background:{surface};padding:20px;border-radius:10px;color:{muted};}}\
         </style>",
        canvas = theme::CANVAS,
        chrome = theme::CHROME,
        surface = theme::SURFACE,
        text = theme::TEXT,
        muted = theme::MUTED,
        faint = theme::FAINT,
        link = theme::LINK,
    )
}

/// Deliberately sparse: one mark, one field, one row of tiles. The UI spec asks
/// for space rather than density (docs/02-UI-UX-SPEC.md).
fn newtab_style() -> String {
    format!(
        "<style>\
         body{{background:{canvas};color:{text};font-size:15px;}}\
         .hero{{padding:88px;}}\
         .mark{{color:{accent};font-size:46px;padding:10px;}}\
         .tag{{color:{faint};font-size:14px;padding:10px;}}\
         .q{{background:{surface};color:{text};width:64%;padding:18px;\
            border-radius:14px;font-size:17px;}}\
         .tiles{{display:flex;flex-wrap:wrap;padding:20px;}}\
         .tile{{display:inline-block;background:{surface};color:{muted};width:150px;\
               padding:16px;margin:8px;border-radius:12px;}}\
         .tiles-head{{color:{faint};font-size:13px;padding:10px;}}\
         </style>",
        canvas = theme::CANVAS,
        surface = theme::SURFACE,
        text = theme::TEXT,
        muted = theme::MUTED,
        faint = theme::FAINT,
        accent = theme::ACCENT,
    )
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
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
            &format!("<div class=\"empty\">{} {}.</div>", t("No built-in page at"), escape(other)),
        ),
    }
}

fn wrap(title: &str, body: &str) -> String {
    // A built-in page carries a title like any other, so the tab and the history
    // entry read "History" rather than a fragment of the URL.
    format!(
        "<html><head><title>{title}</title>{}</head><body><h1>{title}</h1>{body}</body></html>",
        list_style()
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
            format!("<div class=\"ln\">{spaces}{}</div>", escape(line.trim_start()))
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
         body{{background:{canvas};color:{muted};padding:24px;font-size:13px;}}\
         .head{{color:{faint};padding:10px;}}\
         .ln{{color:{text};}}\
         </style>",
        canvas = theme::CANVAS,
        text = theme::TEXT,
        muted = theme::MUTED,
        faint = theme::FAINT,
    )
}

/// The start page: one search field, and the sites actually used most.
///
/// The search box is an ordinary GET form, so it goes through the same
/// submission path as any site's search box rather than a special case.
fn newtab_page() -> String {
    let tiles: String = top_sites(8)
        .iter()
        .map(|(url, title)| {
            format!(
                "<a class=\"tile\" href=\"{}\">{}</a>",
                escape(url),
                escape(&short(title, url))
            )
        })
        .collect();
    // Tiles only earn their heading once there is something to show.
    let tiles = match tiles.is_empty() {
        true => String::new(),
        false => format!("<div class=\"tiles-head\">Frequently visited</div><div class=\"tiles\">{tiles}</div>"),
    };
    // The field submits to whichever engine settings names, so the start page
    // and the address bar can never send a search to different places.
    let (action, field) = crate::settings::current().search_form();
    format!(
        "<html><head><title>New Tab</title>{style}</head><body>\
         <div class=\"hero\">\
         <div class=\"mark\">zero</div>\
         <div class=\"tag\">A browser built from scratch, in India.</div>\
         <form action=\"{action}\">\
         <input name=\"{field}\" class=\"q\" placeholder=\"Search the web, privately\">\
         </form>\
         </div>{tiles}</body></html>",
        style = newtab_style(),
    )
}

/// Most-visited sites, by how often they appear in history.
fn top_sites(limit: usize) -> Vec<(String, String)> {
    let mut counts: std::collections::HashMap<String, (usize, String)> = Default::default();
    for visit in storage::load_history() {
        let entry = counts.entry(visit.url).or_insert((0, String::new()));
        entry.0 += 1;
        if !visit.title.is_empty() {
            entry.1 = visit.title; // keep the latest title we saw
        }
    }
    let mut sites: Vec<_> = counts.into_iter().collect();
    // Ties broken by URL so the grid does not reshuffle between renders.
    sites.sort_by(|a, b| b.1 .0.cmp(&a.1 .0).then_with(|| a.0.cmp(&b.0)));
    sites.into_iter().take(limit).map(|(url, (_, title))| (url, title)).collect()
}

/// Tiles are small, so prefer a short title and fall back to the host.
fn short(title: &str, url: &str) -> String {
    let host = url.split("://").nth(1).unwrap_or(url).split('/').next().unwrap_or(url);
    match title.chars().count() {
        0 => host.to_string(),
        n if n > 24 => title.chars().take(23).chain(['…']).collect(),
        _ => title.to_string(),
    }
}

/// Settings and downloads share a layout: a column of rows, each separated by a
/// hairline rather than a filled card. The engine has one font weight, so
/// hierarchy comes from size and colour alone — `.name` reads as bold because it
/// is the brightest thing in the row, `.hint` as light because it is the dimmest.
fn console_style() -> String {
    format!(
        "<style>\
         body{{background:{canvas};color:{text};padding-left:56px;padding-right:56px;\
              padding-top:44px;padding-bottom:64px;font-size:14px;}}\
         h1{{color:{text};font-size:26px;}}\
         .lede{{color:{faint};font-size:13px;padding-bottom:28px;}}\
         .sec{{color:{muted};font-size:11px;padding-top:32px;padding-bottom:10px;}}\
         .row{{display:flex;justify-content:space-between;align-items:center;\
              padding-top:14px;padding-bottom:14px;\
              border-bottom-width:1px;border-color:{line};}}\
         .name{{color:{text};font-size:14px;}}\
         .hint{{color:{faint};font-size:12px;padding-top:3px;}}\
         .seg{{display:inline-block;text-align:right;\
              padding-top:3px;padding-bottom:3px;}}\
         .opt{{display:inline-block;color:{muted};font-size:13px;border-radius:8px;\
              padding-left:13px;padding-right:13px;padding-top:6px;padding-bottom:6px;}}\
         .opton{{display:inline-block;background:{surface};color:{text};font-size:13px;\
                border-radius:8px;padding-left:13px;padding-right:13px;\
                padding-top:6px;padding-bottom:4px;\
                border-bottom-width:2px;border-color:{accent};}}\
         .optoff{{display:inline-block;color:{faint};font-size:13px;\
                 padding-left:13px;padding-right:13px;padding-top:6px;padding-bottom:6px;}}\
         .fact{{color:{muted};font-size:13px;}}\
         .empty{{background:{surface};padding-left:22px;padding-right:22px;\
                padding-top:20px;padding-bottom:20px;border-radius:12px;color:{muted};}}\
         a{{color:{link};}}\
         </style>",
        canvas = theme::CANVAS,
        surface = theme::SURFACE,
        text = theme::TEXT,
        muted = theme::MUTED,
        faint = theme::FAINT,
        accent = theme::ACCENT,
        line = theme::LINE,
        link = theme::LINK,
    )
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
    labels.iter().map(|label| 30 + (label.chars().count() as u32 * 8)).sum::<u32>() + 10
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

/// An option that exists in the design but not yet in the build. Shown rather
/// than hidden, so the page is honest about what is coming.
fn unavailable(label: &str) -> String {
    format!("<span class=\"optoff\">{}</span>", escape(&t(label)))
}

fn on_off(key: &str, on: bool) -> String {
    segmented(key, &[("on", "On"), ("off", "Off")], if on { "on" } else { "off" })
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
        &[("expanded", "Expanded"), ("icons", "Icons"), ("hidden", "Hidden")],
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
        &zoom_options.iter().map(|(v, l)| (v.as_str(), l.as_str())).collect::<Vec<_>>(),
        &now.zoom.to_string(),
    );
    let engines: Vec<(&str, &str)> =
        crate::settings::ENGINES.iter().map(|(key, label, _)| (*key, *label)).collect();
    let engine = segmented("engine", &engines, now.engine().0);
    let theme_labels = [t("Dark"), t("Light")];
    let theme_control = wrap_control(
        &theme_labels.iter().map(String::as_str).collect::<Vec<_>>(),
        &format!(
            "<span class=\"opton\">{}</span>{}",
            escape(&t("Dark")),
            unavailable("Light")
        ),
    );
    // Each language is named in its own script, so these labels are not translated.
    let language = segmented("lang", crate::settings::LANGUAGES, now.language());
    let profile = crate::storage::profile_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| "unavailable".to_string());

    let body = format!(
        "<div class=\"lede\">{lede}</div>\
         <div class=\"sec\">{appearance}</div>{}{}{}{}{}{}\
         <div class=\"sec\">{search}</div>{}\
         <div class=\"sec\">{privacy}</div>{}{}\
         <div class=\"sec\">{about}</div>{}{}{}",
        setting_t("Tab layout", "A rail down the side, or a strip across the top", &layout),
        setting_t("Tab rail", "How much of the vertical rail stays open", &rail),
        setting_t("Page zoom", "The size new tabs open at. Ctrl+= and Ctrl+- change one tab", &zoom),
        setting_t("Language", "What the browser's own screens are written in", &language),
        setting_t("Theme", "Light is designed but not built yet", &theme_control),
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
        setting_t("Profile folder", "Where history, bookmarks and this file live",
            &format!("<span class=\"fact\">{}</span>", escape(&profile))),
        setting_t("Source", "Zero is open source, Apache-2.0",
            "<span class=\"fact\">github.com/zero-browser</span>"),
        lede = escape(&t("Every preference is stored on this device, as text you can read.")),
        appearance = escape(&t("APPEARANCE")),
        search = escape(&t("SEARCH")),
        privacy = escape(&t("PRIVACY")),
        about = escape(&t("ABOUT")),
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
                &format!("<span class=\"fact\">{}</span>", escape(&date_of(file.when))),
            )
        })
        .collect();
    console_wrap(
        &t("Downloads"),
        &format!("<div class=\"lede\">{}</div>{rows}", t("Saved pages, newest first.")),
    )
}

fn console_wrap(title: &str, body: &str) -> String {
    format!(
        "<html><head><title>{title}</title>{}</head><body><h1>{title}</h1>{body}</body></html>",
        console_style()
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
        .enumerate()
        .map(|(i, v)| row(i, v.url.as_str(), &label(v), Some(v.when)))
        .collect();

    if rows.is_empty() {
        return wrap(
            &t("History"),
            &format!("<div class=\"empty\">{}</div>", t("Nothing visited yet.")),
        );
    }
    wrap(
        &t("History"),
        &format!("<div class=\"sub\">{}</div>{rows}", t("Most recent first")),
    )
}

fn bookmarks_page() -> String {
    let marks = storage::load_bookmarks();
    let rows: String = marks
        .iter()
        .enumerate()
        .map(|(i, b)| row(i, &b.url, &b.title, None))
        .collect();

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

/// A clickable entry. Alternating backgrounds make long lists readable.
fn row(index: usize, url: &str, title: &str, when: Option<u64>) -> String {
    let class = if index % 2 == 0 { "row" } else { "alt" };
    let stamp = match when {
        Some(secs) => format!(" <span class=\"when\">{}</span>", escape(&date_of(secs))),
        None => String::new(),
    };
    format!(
        "<div class=\"{class}\"><a href=\"{}\">{}</a>{stamp}</div>",
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

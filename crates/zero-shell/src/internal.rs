//! Built-in `zero://` pages, generated as HTML and rendered by our own engine.
//!
//! Dogfooding the engine for browser UI keeps these pages honest: if history
//! renders badly, the engine has a bug worth fixing.

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
pub fn page(target: &str) -> String {
    match target.trim_end_matches('/') {
        "zero://newtab" => newtab_page(),
        "zero://history" => history_page(),
        "zero://bookmarks" => bookmarks_page(),
        other => wrap(
            "Unknown page",
            &format!("<div class=\"empty\">No built-in page at {}.</div>", escape(other)),
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
    format!(
        "<html><head><title>New Tab</title>{style}</head><body>\
         <div class=\"hero\">\
         <div class=\"mark\">zero</div>\
         <div class=\"tag\">A browser built from scratch, in India.</div>\
         <form action=\"https://duckduckgo.com/html/\">\
         <input name=\"q\" class=\"q\" placeholder=\"Search the web, privately\">\
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
        return wrap("History", "<div class=\"empty\">Nothing visited yet.</div>");
    }
    wrap("History", &format!("<div class=\"sub\">Most recent first</div>{rows}"))
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
            "Bookmarks",
            "<div class=\"empty\">No bookmarks yet. Press Ctrl+D on a page to add one.</div>",
        );
    }
    wrap("Bookmarks", &rows)
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

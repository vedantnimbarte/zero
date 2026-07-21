//! Built-in `zero://` pages, generated as HTML and rendered by our own engine.
//!
//! Dogfooding the engine for browser UI keeps these pages honest: if history
//! renders badly, the engine has a bug worth fixing.

use crate::storage::{self, Visit};

const STYLE: &str = "<style>\
    body{background:#0e0f12;color:#f2f3f5;padding:28px;font-size:15px;}\
    h1{color:#e5484d;font-size:28px;}\
    .sub{color:#9a9da6;font-size:13px;padding:4px;}\
    .row{background:#17181c;padding:12px;border-radius:8px;}\
    .alt{background:#1f2127;padding:12px;border-radius:8px;}\
    a{color:#66ccff;}\
    .when{color:#6b7280;font-size:13px;}\
    .empty{background:#17181c;padding:16px;border-radius:8px;color:#9a9da6;}\
    </style>";

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
        "zero://history" => history_page(),
        "zero://bookmarks" => bookmarks_page(),
        other => wrap(
            "Unknown page",
            &format!("<div class=\"empty\">No built-in page at {}.</div>", escape(other)),
        ),
    }
}

fn wrap(title: &str, body: &str) -> String {
    format!("<html><head>{STYLE}</head><body><h1>{title}</h1>{body}</body></html>")
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

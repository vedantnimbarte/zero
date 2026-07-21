//! On-disk state: browsing history and the open-tab session.
//!
//! Everything lives in a per-user profile directory and never leaves the machine,
//! which is the point — see docs/04-SECURITY-PRIVACY.md §5.3.
//!
//! The format is tab-separated lines rather than JSON: it needs no dependency, is
//! trivially inspectable, and a corrupt line can be skipped instead of failing the
//! whole file.
//!
//! Files are encrypted at rest where the platform supports it — see
//! [`crate::crypto`], which also explains where it does not.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Where profile data lives, creating it on first use.
pub fn profile_dir() -> Option<PathBuf> {
    // APPDATA on Windows, XDG_CONFIG_HOME/HOME elsewhere.
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from))
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    let dir = base.join("zero-browser");
    fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

/// Tabs and newlines would break the line format, so collapse them.
fn sanitize(field: &str) -> String {
    field.replace(['\t', '\r', '\n'], " ").trim().to_string()
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

#[derive(Debug, PartialEq)]
pub struct Visit {
    pub when: u64,
    pub url: String,
    pub title: String,
}

/// Record a page visit. Failures are ignored: losing history must never break
/// browsing.
pub fn record_visit(url: &str, title: &str) {
    if let Some(dir) = profile_dir() {
        append_visit(&dir, url, title);
    }
}

pub fn load_history() -> Vec<Visit> {
    profile_dir().map(|dir| read_history(&dir)).unwrap_or_default()
}

fn append_visit(dir: &Path, url: &str, title: &str) {
    let (url, title) = (sanitize(url), sanitize(title));
    if url.is_empty() {
        return;
    }
    let file = dir.join("history.tsv");
    let line = format!("{}\t{}\t{}\n", now_secs(), url, title);
    let existing = crate::crypto::read_file(&file).unwrap_or_default();
    crate::crypto::write_file(&file, &(existing + &line));
}

fn read_history(dir: &Path) -> Vec<Visit> {
    let text = crate::crypto::read_file(&dir.join("history.tsv")).unwrap_or_default();
    text.lines().filter_map(parse_visit).collect()
}

/// Skip malformed lines rather than discarding the whole file.
fn parse_visit(line: &str) -> Option<Visit> {
    let mut parts = line.splitn(3, '\t');
    let when = parts.next()?.parse().ok()?;
    let url = parts.next()?.to_string();
    if url.is_empty() {
        return None;
    }
    Some(Visit { when, url, title: parts.next().unwrap_or("").to_string() })
}

/// Persist the open tabs so the next launch can restore them.
pub fn save_session(urls: &[String], active: usize) {
    if let Some(dir) = profile_dir() {
        write_session(&dir, urls, active);
    }
}

/// The previous session's tabs and which one was in front.
pub fn load_session() -> Option<(Vec<String>, usize)> {
    read_session(&profile_dir()?)
}

fn write_session(dir: &Path, urls: &[String], active: usize) {
    let mut out = format!("{active}\n");
    for url in urls {
        out.push_str(&sanitize(url));
        out.push('\n');
    }
    crate::crypto::write_file(&dir.join("session.tsv"), &out);
}

fn read_session(dir: &Path) -> Option<(Vec<String>, usize)> {
    let text = crate::crypto::read_file(&dir.join("session.tsv"))?;
    let mut lines = text.lines();
    let active: usize = lines.next()?.trim().parse().unwrap_or(0);
    let urls: Vec<String> = lines.map(str::to_string).filter(|u| !u.is_empty()).collect();
    if urls.is_empty() {
        return None;
    }
    let active = active.min(urls.len() - 1);
    Some((urls, active))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_well_formed_lines_and_skips_junk() {
        assert_eq!(
            parse_visit("1700000000\thttps://example.com\tExample"),
            Some(Visit {
                when: 1_700_000_000,
                url: "https://example.com".into(),
                title: "Example".into()
            })
        );
        // A title is optional.
        assert_eq!(parse_visit("5\thttps://a.com").map(|v| v.title), Some(String::new()));
        // Junk lines are dropped, not fatal.
        assert!(parse_visit("").is_none());
        assert!(parse_visit("not-a-timestamp\thttps://a.com").is_none());
        assert!(parse_visit("12\t").is_none());
    }

    /// A scratch directory for round-trip tests, unique per test name.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("zero-storage-{name}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    #[test]
    fn history_appends_and_reads_back_in_order() {
        let dir = scratch("history");
        append_visit(&dir, "https://a.com", "A");
        append_visit(&dir, "https://b.com", "B");
        let visits = read_history(&dir);
        assert_eq!(visits.len(), 2);
        assert_eq!(visits[0].url, "https://a.com");
        assert_eq!(visits[1].title, "B");
        // Blank URLs are not recorded.
        append_visit(&dir, "   ", "nothing");
        assert_eq!(read_history(&dir).len(), 2);
    }

    #[test]
    fn session_round_trips_and_clamps_the_active_index() {
        let dir = scratch("session");
        assert!(read_session(&dir).is_none()); // nothing saved yet

        let urls = vec!["https://a.com".to_string(), "https://b.com".to_string()];
        write_session(&dir, &urls, 1);
        assert_eq!(read_session(&dir), Some((urls.clone(), 1)));

        // An out-of-range active index is clamped rather than panicking later.
        write_session(&dir, &urls, 99);
        assert_eq!(read_session(&dir).unwrap().1, 1);
    }

    #[test]
    fn sanitize_keeps_fields_on_one_line() {
        assert_eq!(sanitize("a\tb\nc"), "a b c");
        assert_eq!(sanitize("  spaced  "), "spaced");
    }
}

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
///
/// `None` under `cfg(test)`: the tests drive the same code paths the UI does —
/// opening tabs, saving pages, changing settings — and none of that may touch
/// the real profile. Functions that take a directory explicitly are tested
/// against a scratch one instead.
pub fn profile_dir() -> Option<PathBuf> {
    if cfg!(test) {
        return None;
    }
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

#[derive(Debug, PartialEq)]
pub struct Bookmark {
    pub url: String,
    pub title: String,
}

/// Save a bookmark, replacing any existing one for the same URL.
pub fn add_bookmark(url: &str, title: &str) {
    let (url, title) = (sanitize(url), sanitize(title));
    if url.is_empty() {
        return;
    }
    let Some(dir) = profile_dir() else { return };
    let mut marks = read_bookmarks(&dir);
    marks.retain(|b| b.url != url);
    marks.push(Bookmark { url, title });
    write_bookmarks(&dir, &marks);
}

/// Remove a bookmark. Returns whether anything was removed.
pub fn remove_bookmark(url: &str) -> bool {
    let Some(dir) = profile_dir() else { return false };
    let mut marks = read_bookmarks(&dir);
    let before = marks.len();
    marks.retain(|b| b.url != url);
    let changed = marks.len() != before;
    if changed {
        write_bookmarks(&dir, &marks);
    }
    changed
}

pub fn load_bookmarks() -> Vec<Bookmark> {
    profile_dir().map(|dir| read_bookmarks(&dir)).unwrap_or_default()
}

pub fn is_bookmarked(url: &str) -> bool {
    load_bookmarks().iter().any(|b| b.url == url)
}

fn read_bookmarks(dir: &Path) -> Vec<Bookmark> {
    let text = crate::crypto::read_file(&dir.join("bookmarks.tsv")).unwrap_or_default();
    text.lines()
        .filter_map(|line| {
            let (url, title) = line.split_once('\t')?;
            (!url.is_empty()).then(|| Bookmark { url: url.into(), title: title.into() })
        })
        .collect()
}

fn write_bookmarks(dir: &Path, marks: &[Bookmark]) {
    let text: String = marks.iter().map(|b| format!("{}\t{}\n", b.url, b.title)).collect();
    crate::crypto::write_file(&dir.join("bookmarks.tsv"), &text);
}

/// A saved page, listed by `zero://downloads`.
#[derive(Debug, PartialEq)]
pub struct Download {
    pub when: u64,
    /// The file name on disk, which is what the user will look for.
    pub name: String,
    pub url: String,
    pub path: String,
}

/// Where saved pages go: the OS Downloads folder, or the profile if there
/// isn't one. Zero never writes outside a directory the user already expects —
/// and, per [`profile_dir`], never writes there at all from a test.
fn downloads_dir() -> Option<PathBuf> {
    if cfg!(test) {
        return None;
    }
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"));
    let dir = match home {
        Some(home) => PathBuf::from(home).join("Downloads"),
        None => profile_dir()?,
    };
    fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

/// Write a page to the Downloads folder and record it. Returns the file name.
pub fn save_page(url: &str, title: &str, body: &str) -> Option<String> {
    let dir = downloads_dir()?;
    let name = unique_name(&dir, &file_stem(title, url));
    fs::write(dir.join(&name), body).ok()?;
    let line = format!(
        "{}\t{}\t{}\t{}\n",
        now_secs(),
        sanitize(&name),
        sanitize(url),
        sanitize(&dir.join(&name).to_string_lossy())
    );
    let file = profile_dir()?.join("downloads.tsv");
    let existing = crate::crypto::read_file(&file).unwrap_or_default();
    crate::crypto::write_file(&file, &(existing + &line));
    Some(name)
}

/// A file name from a page's title, falling back to its host. Anything the
/// filesystem might object to becomes a dash — which includes both separators,
/// so a title can never steer the write out of the Downloads folder.
fn file_stem(title: &str, url: &str) -> String {
    let source = match title.trim() {
        "" => url.split("://").nth(1).unwrap_or(url).split('/').next().unwrap_or("page"),
        title => title,
    };
    let stem: String = source
        .chars()
        .map(|c| match c {
            c if c.is_alphanumeric() => c,
            // Dots survive so a host reads as a host, not as dashes.
            '-' | ' ' | '.' | '_' => c,
            _ => '-',
        })
        .collect();
    let stem = stem.trim().trim_matches(['-', '.']).trim();
    let stem = if stem.is_empty() { "page" } else { stem };
    format!("{}.html", stem.chars().take(60).collect::<String>())
}

/// Saving the same page twice should not silently overwrite the first copy.
fn unique_name(dir: &Path, name: &str) -> String {
    if !dir.join(name).exists() {
        return name.to_string();
    }
    let (stem, ext) = name.rsplit_once('.').unwrap_or((name, "html"));
    (2..)
        .map(|n| format!("{stem} ({n}).{ext}"))
        .find(|candidate| !dir.join(candidate).exists())
        .expect("an unused name exists")
}

pub fn load_downloads() -> Vec<Download> {
    let Some(dir) = profile_dir() else { return Vec::new() };
    let text = crate::crypto::read_file(&dir.join("downloads.tsv")).unwrap_or_default();
    text.lines().filter_map(parse_download).collect()
}

fn parse_download(line: &str) -> Option<Download> {
    let mut parts = line.splitn(4, '\t');
    let when = parts.next()?.parse().ok()?;
    let name = parts.next()?.to_string();
    if name.is_empty() {
        return None;
    }
    Some(Download {
        when,
        name,
        url: parts.next().unwrap_or("").to_string(),
        path: parts.next().unwrap_or("").to_string(),
    })
}

/// Persist the open tabs so the next launch can restore them, remembering
/// which were pinned.
pub fn save_session(tabs: &[(String, bool)], active: usize) {
    if let Some(dir) = profile_dir() {
        write_session(&dir, tabs, active);
    }
}

/// The previous session's tabs (URL and whether pinned) and which was in front.
pub fn load_session() -> Option<(Vec<(String, bool)>, usize)> {
    read_session(&profile_dir()?)
}

fn write_session(dir: &Path, tabs: &[(String, bool)], active: usize) {
    let mut out = format!("{active}\n");
    for (url, pinned) in tabs {
        out.push_str(if *pinned { "pin\t" } else { "tab\t" });
        out.push_str(&sanitize(url));
        out.push('\n');
    }
    crate::crypto::write_file(&dir.join("session.tsv"), &out);
}

fn read_session(dir: &Path) -> Option<(Vec<(String, bool)>, usize)> {
    let text = crate::crypto::read_file(&dir.join("session.tsv"))?;
    let mut lines = text.lines();
    let active: usize = lines.next()?.trim().parse().unwrap_or(0);
    let tabs: Vec<(String, bool)> = lines
        .filter_map(|line| match line.split_once('\t') {
            Some((kind, url)) => Some((url.to_string(), kind == "pin")),
            // A session written before pinning existed is one URL per line.
            None => Some((line.to_string(), false)),
        })
        .filter(|(url, _)| !url.is_empty())
        .collect();
    if tabs.is_empty() {
        return None;
    }
    let active = active.min(tabs.len() - 1);
    Some((tabs, active))
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
    fn session_round_trips_pinning_and_clamps_the_active_index() {
        let dir = scratch("session");
        assert!(read_session(&dir).is_none()); // nothing saved yet

        let tabs = vec![("https://a.com".to_string(), true), ("https://b.com".to_string(), false)];
        write_session(&dir, &tabs, 1);
        assert_eq!(read_session(&dir), Some((tabs.clone(), 1)));

        // An out-of-range active index is clamped rather than panicking later.
        write_session(&dir, &tabs, 99);
        assert_eq!(read_session(&dir).unwrap().1, 1);
    }

    #[test]
    fn a_session_written_before_pinning_still_opens() {
        let dir = scratch("legacy-session");
        crate::crypto::write_file(&dir.join("session.tsv"), "1\nhttps://a.com\nhttps://b.com\n");
        let (tabs, active) = read_session(&dir).expect("legacy session");
        assert_eq!(active, 1);
        assert_eq!(tabs, vec![("https://a.com".into(), false), ("https://b.com".into(), false)]);
    }

    #[test]
    fn saved_pages_are_named_after_the_page_and_never_collide() {
        let dir = scratch("downloads");
        // A title becomes the file name; path-hostile characters do not survive.
        assert_eq!(file_stem("Rust: a language/guide", "https://a.com"), "Rust- a language-guide.html");
        // No usable title falls back to the host.
        assert_eq!(file_stem("  ", "https://news.ycombinator.com/x"), "news.ycombinator.com.html");
        assert_eq!(file_stem("///", "https://a.com"), "page.html");

        // Saving twice keeps both copies.
        assert_eq!(unique_name(&dir, "page.html"), "page.html");
        fs::write(dir.join("page.html"), "x").expect("write");
        assert_eq!(unique_name(&dir, "page.html"), "page (2).html");
    }

    #[test]
    fn tests_never_reach_the_real_profile_or_downloads_folder() {
        // The suite exercises saving pages, opening tabs and changing settings.
        // If either of these ever returns a real path again, `cargo test` starts
        // rewriting the user's session and dropping files in their Downloads.
        assert!(profile_dir().is_none());
        assert!(downloads_dir().is_none());
        assert!(save_page("https://a.com", "A", "<html></html>").is_none());
    }

    #[test]
    fn download_lines_parse_and_junk_is_skipped() {
        assert_eq!(
            parse_download("42\tpage.html\thttps://a.com\tC:/Downloads/page.html"),
            Some(Download {
                when: 42,
                name: "page.html".into(),
                url: "https://a.com".into(),
                path: "C:/Downloads/page.html".into(),
            })
        );
        assert!(parse_download("42\t").is_none());
        assert!(parse_download("nope\tpage.html").is_none());
    }

    #[test]
    fn sanitize_keeps_fields_on_one_line() {
        assert_eq!(sanitize("a\tb\nc"), "a b c");
        assert_eq!(sanitize("  spaced  "), "spaced");
    }
}

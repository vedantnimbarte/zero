//! Networking and URL handling — the shell's job, not the engine's.
//!
//! Everything that reaches the network passes through here, which is also why
//! tracker blocking lives at this layer (see [`crate::blocker`]).

use crate::blocker;
use crate::cookies::{site_of, CookieJar};
use std::cell::{Cell, RefCell};
use std::fs;
use std::io::Read;
use std::rc::Rc;

pub fn is_url(s: &str) -> bool {
    s.starts_with("http://") || s.starts_with("https://")
}

thread_local! {
    /// One jar for the process, loaded once and written back after each response.
    static JAR: RefCell<CookieJar> = RefCell::new(
        crate::storage::profile_dir()
            .map(|dir| CookieJar::load(&dir))
            .unwrap_or_default(),
    );
    /// The site being visited, which partitions every cookie for this page.
    static PARTITION: RefCell<String> = const { RefCell::new(String::new()) };
}

/// Set the top-level site that subsequent requests belong to.
pub fn set_partition(url: &str) {
    PARTITION.with(|p| *p.borrow_mut() = site_of(url));
}

fn partition() -> String {
    PARTITION.with(|p| p.borrow().clone())
}

/// Attach any cookies this request should carry.
fn with_cookies(url: &str, request: ureq::Request) -> ureq::Request {
    match JAR.with(|jar| jar.borrow().header_for(&partition(), url)) {
        Some(header) => request.set("Cookie", &header),
        None => request,
    }
}

/// Record `Set-Cookie` headers from a response, then persist the jar.
fn absorb_cookies(url: &str, response: &ureq::Response) {
    let headers = response.all("set-cookie");
    if headers.is_empty() {
        return;
    }
    let site = partition();
    JAR.with(|jar| {
        let mut jar = jar.borrow_mut();
        for header in headers {
            jar.store(&site, url, header);
        }
        if let Some(dir) = crate::storage::profile_dir() {
            jar.save(&dir);
        }
    });
}

/// Loads `<img>` bytes for the engine, resolving relative URLs against the page.
/// Networking + filesystem are shell concerns; the engine only decodes bytes.
pub struct ShellLoader {
    base: String,
    /// Shared with the app so the toolbar can report what was blocked.
    pub blocked: Rc<Cell<usize>>,
}

impl ShellLoader {
    pub fn new(base: String) -> ShellLoader {
        ShellLoader { base, blocked: Rc::new(Cell::new(0)) }
    }
}

impl zero_engine::ResourceLoader for ShellLoader {
    fn load(&self, url: &str) -> Option<Vec<u8>> {
        let resolved = resolve_url(&self.base, url);
        // Drop tracker/ad requests before they are ever sent.
        if blocker::is_blocked(&resolved) {
            self.blocked.set(self.blocked.get() + 1);
            return None;
        }
        if is_url(&resolved) {
            // HTTPS-first for subresources too, falling back to cleartext only on failure.
            let mut buf = Vec::new();
            if let Some(rest) = resolved.strip_prefix("http://") {
                if let Some(mut r) = fetch_bytes(&format!("https://{rest}")) {
                    if r.read_to_end(&mut buf).is_ok() {
                        return Some(buf);
                    }
                    buf.clear();
                }
            }
            fetch_bytes(&resolved)?.read_to_end(&mut buf).ok()?;
            Some(buf)
        } else if resolved.starts_with("data:") {
            None // ponytail: data: URIs unsupported; add base64 decode when needed
        } else {
            fs::read(&resolved).ok()
        }
    }
}

fn fetch_bytes(url: &str) -> Option<impl Read> {
    let response = with_cookies(url, ureq::get(url)).call().ok()?;
    absorb_cookies(url, &response);
    Some(response.into_reader())
}

/// Resolve a possibly-relative resource URL against a base page URL or file path.
pub fn resolve_url(base: &str, src: &str) -> String {
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
pub fn normalize_target(s: &str) -> String {
    let s = s.trim();
    if crate::internal::is_internal(s) || is_url(s) || std::path::Path::new(s).exists() {
        s.to_string()
    } else if s.contains('.') && !s.contains(' ') {
        format!("https://{s}")
    } else if s.is_empty() {
        s.to_string()
    } else {
        search_url(s)
    }
}

/// Anything that isn't an address is a search.
///
/// DuckDuckGo's HTML endpoint is the default because it needs no JavaScript and
/// does not profile the user — the same reasoning as docs/04-SECURITY-PRIVACY.md.
pub fn search_url(query: &str) -> String {
    format!("https://duckduckgo.com/html/?q={}", zero_engine::percent_encode(query))
}

/// A loaded document, plus the URL it actually came from and whether the
/// connection was secure (an HTTPS upgrade may change the URL).
pub struct Fetched {
    pub url: String,
    pub body: String,
    pub secure: bool,
}

/// ponytail: blocking call on the UI thread — fine for now; move off-thread if it stalls.
fn try_fetch(url: &str) -> Option<String> {
    eprintln!("fetching {url} ...");
    let response = with_cookies(url, ureq::get(url)).call().ok()?;
    absorb_cookies(url, &response);
    response.into_string().ok()
}

/// Load a page, preferring HTTPS.
///
/// An explicit `http://` URL is retried over HTTPS first, and only falls back to
/// cleartext if the secure attempt actually fails — so users get encryption by
/// default without breaking sites that genuinely have no HTTPS.
pub fn load_target(target: &str) -> Fetched {
    // Built-in pages never touch the network.
    if crate::internal::is_internal(target) {
        return Fetched {
            url: target.to_string(),
            body: crate::internal::page(target),
            secure: true,
        };
    }
    // Cookies are keyed by the site being visited, so set that before fetching.
    set_partition(target);
    if !is_url(target) {
        let body = fs::read_to_string(target)
            .unwrap_or_else(|e| error_page("Cannot open", &format!("{target}: {e}")));
        return Fetched { url: target.to_string(), body, secure: true }; // local: no network
    }

    if let Some(rest) = target.strip_prefix("http://") {
        let upgraded = format!("https://{rest}");
        if let Some(body) = try_fetch(&upgraded) {
            return Fetched { url: upgraded, body, secure: true };
        }
        eprintln!("HTTPS upgrade failed for {target}; falling back to cleartext");
        let body = try_fetch(target)
            .unwrap_or_else(|| error_page("Failed to load", target));
        return Fetched { url: target.to_string(), body, secure: false };
    }

    let body = try_fetch(target).unwrap_or_else(|| error_page("Failed to load", target));
    Fetched { url: target.to_string(), body, secure: true }
}

fn error_page(title: &str, detail: &str) -> String {
    format!("<html><body><h1>{title}</h1><p>{detail}</p></body></html>")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_relative_absolute_and_scheme_relative() {
        let base = "https://example.com/docs/page.html";
        assert_eq!(resolve_url(base, "img.png"), "https://example.com/docs/img.png");
        assert_eq!(resolve_url(base, "/img.png"), "https://example.com/img.png");
        assert_eq!(resolve_url(base, "//cdn.net/i.png"), "https://cdn.net/i.png");
        assert_eq!(resolve_url(base, "https://x.com/i.png"), "https://x.com/i.png");
    }
}

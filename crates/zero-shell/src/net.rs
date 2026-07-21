//! Networking and URL handling — the shell's job, not the engine's.
//!
//! Everything that reaches the network passes through here, which is also why
//! tracker blocking lives at this layer (see [`crate::blocker`]).

use crate::blocker;
use std::cell::Cell;
use std::fs;
use std::io::Read;
use std::rc::Rc;

pub fn is_url(s: &str) -> bool {
    s.starts_with("http://") || s.starts_with("https://")
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
            let mut buf = Vec::new();
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
    ureq::get(url).call().ok().map(|r| r.into_reader())
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
    if is_url(s) || std::path::Path::new(s).exists() {
        s.to_string()
    } else if s.contains('.') && !s.contains(' ') {
        format!("https://{s}")
    } else {
        s.to_string()
    }
}

/// Fetch a URL over HTTP(S).
/// ponytail: blocking call on the UI thread — fine for now; move off-thread if it stalls.
pub fn fetch_url(url: &str) -> String {
    eprintln!("fetching {url} ...");
    ureq::get(url)
        .call()
        .and_then(|r| r.into_string().map_err(Into::into))
        .unwrap_or_else(|e| {
            eprintln!("fetch failed: {e}");
            error_page("Failed to load", &format!("{url}: {e}"))
        })
}

pub fn load_target(target: &str) -> String {
    if is_url(target) {
        fetch_url(target)
    } else {
        fs::read_to_string(target)
            .unwrap_or_else(|e| error_page("Cannot open", &format!("{target}: {e}")))
    }
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

//! Networking and URL handling — the shell's job, not the engine's.
//!
//! Everything that reaches the network passes through here, which is also why
//! tracker blocking lives at this layer (see [`crate::blocker`]).

use crate::blocker;
use crate::cookies::{site_of, CookieJar};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::rc::Rc;

/// How Zero identifies itself. Sites use this for rate limiting and for
/// serving the right markup, and some (Wikimedia among them) reject requests
/// that arrive with a library's default agent string.
///
/// It names Zero honestly rather than impersonating another browser.
const USER_AGENT: &str = concat!(
    "Mozilla/5.0 (compatible; ZeroBrowser/",
    env!("CARGO_PKG_VERSION"),
    "; +https://github.com/vedantnimbarte/zero)"
);

/// One agent for the process: it carries our identity and, more importantly,
/// pools connections, so a page's subresources reuse an open TLS session
/// instead of paying for a handshake each.
fn agent() -> &'static ureq::Agent {
    static AGENT: std::sync::OnceLock<ureq::Agent> = std::sync::OnceLock::new();
    AGENT.get_or_init(|| {
        ureq::AgentBuilder::new()
            .user_agent(USER_AGENT)
            .timeout_connect(std::time::Duration::from_secs(10))
            .build()
    })
}

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

/// The `Cookie` header a request should carry, read on the calling thread.
fn cookie_header(url: &str) -> Option<String> {
    JAR.with(|jar| jar.borrow().header_for(&partition(), url))
}

/// Record `Set-Cookie` headers from a response, then persist the jar.
fn absorb_cookies(url: &str, response: &ureq::Response) {
    store_cookies(url, response.all("set-cookie").iter().map(|h| h.to_string()).collect());
}

/// The jar half of [`absorb_cookies`], usable with headers collected elsewhere
/// — worker threads cannot touch the jar, since it is thread-local by design.
fn store_cookies(url: &str, headers: Vec<String>) {
    if headers.is_empty() {
        return;
    }
    let site = partition();
    JAR.with(|jar| {
        let mut jar = jar.borrow_mut();
        for header in headers {
            jar.store(&site, url, &header);
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
    /// Subresources already fetched for this page, misses included so a 404 is
    /// not retried. The engine re-collects images and stylesheets on every
    /// render, and a render happens on every keystroke — without this, typing
    /// one character refetches the whole page's assets over the network.
    ///
    /// ponytail: lives as long as the tab's document and has no size limit;
    /// a shared LRU across tabs is the upgrade if memory becomes a problem.
    cache: RefCell<HashMap<String, Option<Vec<u8>>>>,
}

impl ShellLoader {
    pub fn new(base: String) -> ShellLoader {
        ShellLoader { base, blocked: Rc::new(Cell::new(0)), cache: RefCell::new(HashMap::new()) }
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
        if let Some(hit) = self.cache.borrow().get(&resolved) {
            return hit.clone();
        }
        let fetched = self.fetch(&resolved);
        self.cache.borrow_mut().insert(resolved, fetched.clone());
        fetched
    }

    /// Fetch a page's subresources concurrently.
    ///
    /// Sequentially this dominates load time — a real article spends seconds
    /// waiting on round trips it could have overlapped. Blocking, caching and
    /// cookie work stay on this thread: the jar is thread-local on purpose, so
    /// workers get a prepared header and hand back `Set-Cookie` to be stored here.
    fn load_all(&self, urls: &[String]) -> Vec<Option<Vec<u8>>> {
        let mut out: Vec<Option<Vec<u8>>> = vec![None; urls.len()];
        let mut pending: Vec<(usize, String, Option<String>)> = Vec::new();
        for (i, url) in urls.iter().enumerate() {
            let resolved = resolve_url(&self.base, url);
            if blocker::is_blocked(&resolved) {
                self.blocked.set(self.blocked.get() + 1);
                continue;
            }
            if let Some(hit) = self.cache.borrow().get(&resolved) {
                out[i] = hit.clone();
                continue;
            }
            let cookies = is_url(&resolved).then(|| cookie_header(&resolved)).flatten();
            pending.push((i, resolved, cookies));
        }

        // Batches are built so no host appears more than PER_HOST times: hosts
        // rate-limit per client, and a burst of parallel requests to one of them
        // gets throttled (429) where the same requests spread out succeed.
        let results: Vec<(usize, String, Option<(Vec<u8>, Vec<String>)>)> = batches(pending)
            .into_iter()
            .flat_map(|batch| {
                std::thread::scope(|scope| {
                    let handles: Vec<_> = batch
                        .iter()
                        .map(|(i, url, cookies)| {
                            scope.spawn(move || (*i, url.clone(), fetch_detached(url, cookies)))
                        })
                        .collect();
                    handles.into_iter().filter_map(|h| h.join().ok()).collect::<Vec<_>>()
                })
            })
            .collect();

        for (i, url, result) in results {
            let bytes = match result {
                Some((bytes, set_cookie)) => {
                    store_cookies(&url, set_cookie);
                    Some(bytes)
                }
                None => None,
            };
            self.cache.borrow_mut().insert(url, bytes.clone());
            out[i] = bytes;
        }
        out
    }
}

/// How many subresource fetches run at once, and how many of those may target
/// the same host.
const WORKERS: usize = 6;
const PER_HOST: usize = 2;

/// Split pending fetches into batches that respect both limits.
///
/// Requests are taken in order but a host that already has PER_HOST entries in
/// the current batch is deferred to a later one, so a page of images from one
/// CDN trickles rather than arriving as a burst.
fn batches(
    pending: Vec<(usize, String, Option<String>)>,
) -> Vec<Vec<(usize, String, Option<String>)>> {
    let mut out: Vec<Vec<(usize, String, Option<String>)>> = Vec::new();
    for item in pending {
        let host = host_of(&item.1);
        let slot = out.iter().position(|batch| {
            batch.len() < WORKERS
                && batch.iter().filter(|(_, url, _)| host_of(url) == host).count() < PER_HOST
        });
        match slot {
            Some(i) => out[i].push(item),
            None => out.push(vec![item]),
        }
    }
    out
}

fn host_of(url: &str) -> &str {
    url.split("://").nth(1).unwrap_or(url).split('/').next().unwrap_or("")
}

/// A fetch with no access to shell state, so it can run on a worker thread.
/// Returns the body and any `Set-Cookie` headers for the caller to store.
fn fetch_detached(url: &str, cookies: &Option<String>) -> Option<(Vec<u8>, Vec<String>)> {
    if !is_url(url) {
        // Local files and unsupported schemes are cheap; no thread needed.
        return std::fs::read(url).ok().map(|bytes| (bytes, Vec::new()));
    }
    // HTTPS-first, like every other request the shell makes.
    let attempts = match url.strip_prefix("http://") {
        Some(rest) => vec![format!("https://{rest}"), url.to_string()],
        None => vec![url.to_string()],
    };
    for attempt in attempts {
        let send = || {
            let mut request = agent().get(&attempt);
            if let Some(header) = cookies {
                request = request.set("Cookie", header);
            }
            request.call()
        };
        let response = match send() {
            Ok(response) => response,
            // Fetching a page's images at once can trip a host's rate limit.
            // Backing off once and retrying is what the status asks for, and it
            // is the difference between a page with images and one without.
            Err(ureq::Error::Status(429 | 503, response)) => {
                std::thread::sleep(retry_after(&response));
                match send() {
                    Ok(response) => response,
                    Err(_) => continue,
                }
            }
            Err(_) => continue,
        };
        let set_cookie: Vec<String> =
            response.all("set-cookie").iter().map(|h| h.to_string()).collect();
        let mut buf = Vec::new();
        if response.into_reader().read_to_end(&mut buf).is_ok() {
            return Some((buf, set_cookie));
        }
    }
    None
}

/// How long to wait before retrying, honouring `Retry-After` but never stalling
/// a page load for long — an image is not worth a multi-second freeze, and a
/// page with many of them would otherwise add up to a hang.
fn retry_after(response: &ureq::Response) -> std::time::Duration {
    let secs = response.header("retry-after").and_then(|v| v.trim().parse::<u64>().ok());
    std::time::Duration::from_millis(secs.map_or(250, |s| (s * 1000).clamp(100, 500)))
}

impl ShellLoader {
    /// The uncached fetch, by scheme.
    fn fetch(&self, resolved: &str) -> Option<Vec<u8>> {
        let resolved = resolved.to_string();
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
    let response = with_cookies(url, agent().get(url)).call().ok()?;
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
/// Fetch a page, returning the reason on failure so the user can be told one.
///
/// A 4xx or 5xx still carries a body — sites serve real pages for "not found"
/// — so the server's page is shown rather than replaced with our own.
fn try_fetch(url: &str) -> Result<String, String> {
    eprintln!("fetching {url} ...");
    let response = match with_cookies(url, agent().get(url)).call() {
        Ok(response) => response,
        Err(ureq::Error::Status(code, response)) => {
            absorb_cookies(url, &response);
            let status = format!("{code} {}", response.status_text().to_string());
            return match response.into_string() {
                Ok(body) if !body.trim().is_empty() => Ok(body),
                _ => Err(format!("the server replied {status}")),
            };
        }
        Err(e) => return Err(describe(&e.to_string())),
    };
    absorb_cookies(url, &response);
    response.into_string().map_err(|e| format!("the reply could not be read ({e})"))
}

/// A transport failure in words a person can act on.
fn describe(text: &str) -> String {
    let lower = text.to_lowercase();
    if lower.contains("dns") || lower.contains("resolve") {
        "that address could not be found".to_string()
    } else if lower.contains("timed out") || lower.contains("timeout") {
        "the site took too long to reply".to_string()
    } else if lower.contains("certificate") || lower.contains("tls") {
        "the secure connection could not be established".to_string()
    } else {
        text.to_string()
    }
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
            .unwrap_or_else(|e| error_page(target, &e.to_string()));
        return Fetched { url: target.to_string(), body, secure: true }; // local: no network
    }

    if let Some(rest) = target.strip_prefix("http://") {
        let upgraded = format!("https://{rest}");
        if let Ok(body) = try_fetch(&upgraded) {
            return Fetched { url: upgraded, body, secure: true };
        }
        eprintln!("HTTPS upgrade failed for {target}; falling back to cleartext");
        let body = try_fetch(target).unwrap_or_else(|why| error_page(target, &why));
        return Fetched { url: target.to_string(), body, secure: false };
    }

    let body = try_fetch(target).unwrap_or_else(|why| error_page(target, &why));
    Fetched { url: target.to_string(), body, secure: true }
}

/// The page shown when a load fails. It says what went wrong and what the user
/// can do, rather than restating the URL they just typed.
fn error_page(target: &str, why: &str) -> String {
    let escape = |text: &str| text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    format!(
        "<html><head><style>         body{{background:#0e0f12;color:#f2f3f5;padding:64px;font-size:15px;}}         h1{{color:#e5484d;font-size:26px;}}         .why{{background:#17181c;padding:16px;border-radius:8px;color:#c9ccd3;}}         .url{{color:#6b7280;padding:8px;}}         .hint{{color:#9a9da6;padding:8px;}}         </style></head><body>         <h1>This page did not load</h1>         <div class=\"why\">Zero tried to reach it, but {}.</div>         <div class=\"url\">{}</div>         <div class=\"hint\">Check the address, or press R to try again.</div>         </body></html>",
        escape(why),
        escape(target)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failures_are_described_in_plain_words() {
        assert_eq!(describe("Dns Failed: resolve host"), "that address could not be found");
        assert_eq!(describe("io: timed out reading"), "the site took too long to reply");
        assert_eq!(
            describe("Invalid TLS certificate"),
            "the secure connection could not be established"
        );
        // Anything we cannot classify is passed through rather than invented.
        assert_eq!(describe("connection refused"), "connection refused");
    }

    #[test]
    fn batches_spread_requests_across_hosts() {
        let pending: Vec<(usize, String, Option<String>)> = [
            "https://cdn.com/1.png",
            "https://cdn.com/2.png",
            "https://cdn.com/3.png",
            "https://other.com/a.png",
        ]
        .iter()
        .enumerate()
        .map(|(i, u)| (i, u.to_string(), None))
        .collect();

        let batched = batches(pending);
        // The third cdn.com request cannot share a batch with the first two.
        assert_eq!(batched.len(), 2);
        assert_eq!(batched[0].len(), 3); // 2x cdn.com + 1x other.com
        assert_eq!(batched[1].len(), 1);
        for batch in &batched {
            assert!(batch.len() <= WORKERS);
            let cdn = batch.iter().filter(|(_, u, _)| host_of(u) == "cdn.com").count();
            assert!(cdn <= PER_HOST, "no host may exceed its share of a batch");
        }
    }

    #[test]
    fn host_of_ignores_scheme_and_path() {
        assert_eq!(host_of("https://a.com/x/y.png"), "a.com");
        assert_eq!(host_of("a.com/x"), "a.com");
    }

    /// Deleting the file between loads proves the second never hit the disk —
    /// the same reason the network is not hit again on every keystroke.
    #[test]
    fn subresources_are_fetched_once() {
        use zero_engine::ResourceLoader;
        let dir = std::env::temp_dir().join("zero-loader-cache");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("scratch dir");
        let file = dir.join("style.css");
        fs::write(&file, "p{color:red}").expect("write");

        let loader = ShellLoader::new(dir.join("page.html").to_string_lossy().into_owned());
        assert_eq!(loader.load("style.css").as_deref(), Some(&b"p{color:red}"[..]));
        fs::remove_file(&file).expect("remove");
        assert_eq!(loader.load("style.css").as_deref(), Some(&b"p{color:red}"[..]));

        // A miss is remembered too, so a 404 is not retried on every render.
        assert_eq!(loader.load("missing.css"), None);
        fs::write(dir.join("missing.css"), "p{}").expect("write");
        assert_eq!(loader.load("missing.css"), None);
    }

    #[test]
    fn resolves_relative_absolute_and_scheme_relative() {
        let base = "https://example.com/docs/page.html";
        assert_eq!(resolve_url(base, "img.png"), "https://example.com/docs/img.png");
        assert_eq!(resolve_url(base, "/img.png"), "https://example.com/img.png");
        assert_eq!(resolve_url(base, "//cdn.net/i.png"), "https://cdn.net/i.png");
        assert_eq!(resolve_url(base, "https://x.com/i.png"), "https://x.com/i.png");
    }
}

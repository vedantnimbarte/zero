//! Cookie storage, partitioned by top-level site.
//!
//! Every jar entry is keyed by the site you are *visiting*, not just the site that
//! set the cookie. So an ad network embedded on `a.com` and on `b.com` gets two
//! separate cookies and cannot join them into one identity — the state
//! partitioning called for in docs/04-SECURITY-PRIVACY.md §5.2.
//!
//! ponytail: no `SameSite` enforcement and no public-suffix list — a `Domain=`
//! attribute is accepted as long as the host ends with it, so a cookie could be
//! widened to a registry suffix like `co.uk`.

use std::collections::HashMap;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, PartialEq)]
pub struct Cookie {
    pub name: String,
    pub value: String,
    pub domain: String,
    pub path: String,
    pub secure: bool,
    /// Unix seconds, or `None` for a session cookie.
    pub expires: Option<u64>,
}

#[derive(Default)]
pub struct CookieJar {
    /// partition (top-level site) -> cookies set while visiting it.
    partitions: HashMap<String, Vec<Cookie>>,
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// The registrable-ish site of a URL: its host, lowercased.
pub fn site_of(url: &str) -> String {
    url.split("://")
        .nth(1)
        .and_then(|rest| rest.split('/').next())
        .map(|host| host.rsplit('@').next().unwrap_or(host))
        .and_then(|host| host.split(':').next())
        .unwrap_or("")
        .trim_end_matches('.')
        .to_ascii_lowercase()
}

fn path_of(url: &str) -> String {
    match url.split("://").nth(1).and_then(|rest| rest.find('/').map(|i| &rest[i..])) {
        Some(path) => path.split(['?', '#']).next().unwrap_or("/").to_string(),
        None => "/".to_string(),
    }
}

fn is_secure(url: &str) -> bool {
    url.starts_with("https://")
}

impl CookieJar {
    /// Record a `Set-Cookie` header seen while visiting `partition`.
    pub fn store(&mut self, partition: &str, url: &str, header: &str) {
        let Some(cookie) = parse_set_cookie(header, url) else { return };
        let jar = self.partitions.entry(partition.to_string()).or_default();
        // A repeat name/domain/path replaces the old value, as browsers do.
        jar.retain(|c| {
            !(c.name == cookie.name && c.domain == cookie.domain && c.path == cookie.path)
        });
        // A cookie already expired is a deletion.
        if cookie.expires.map(|e| e > now_secs()).unwrap_or(true) {
            jar.push(cookie);
        }
    }

    /// The `Cookie` header value for a request, or `None` if nothing applies.
    pub fn header_for(&self, partition: &str, url: &str) -> Option<String> {
        let jar = self.partitions.get(partition)?;
        let (host, path, secure, now) = (site_of(url), path_of(url), is_secure(url), now_secs());
        let pairs: Vec<String> = jar
            .iter()
            .filter(|c| c.expires.map(|e| e > now).unwrap_or(true))
            .filter(|c| domain_matches(&host, &c.domain))
            .filter(|c| path.starts_with(&c.path))
            .filter(|c| !c.secure || secure)
            .map(|c| format!("{}={}", c.name, c.value))
            .collect();
        if pairs.is_empty() {
            None
        } else {
            Some(pairs.join("; "))
        }
    }

    /// How many cookies are stored, across every partition. Only the tests ask,
    /// but they ask about the thing that matters: what survives a restart.
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.partitions.values().map(Vec::len).sum()
    }

    /// Persist non-session cookies as tab-separated lines.
    pub fn save(&self, dir: &Path) {
        let now = now_secs();
        let mut out = String::new();
        for (partition, jar) in &self.partitions {
            for c in jar.iter().filter(|c| c.expires.map(|e| e > now).unwrap_or(false)) {
                out.push_str(&format!(
                    "{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
                    partition,
                    c.domain,
                    c.path,
                    c.name,
                    c.value,
                    c.secure as u8,
                    c.expires.unwrap_or(0)
                ));
            }
        }
        // Cookies are credentials, so this is the file that most needs encrypting.
        crate::crypto::write_file(&dir.join("cookies.tsv"), &out);
    }

    pub fn load(dir: &Path) -> CookieJar {
        let text = crate::crypto::read_file(&dir.join("cookies.tsv")).unwrap_or_default();
        let mut jar = CookieJar::default();
        let now = now_secs();
        for line in text.lines() {
            let f: Vec<&str> = line.split('\t').collect();
            if f.len() != 7 {
                continue; // skip corrupt lines rather than dropping the file
            }
            let Ok(expires) = f[6].parse::<u64>() else { continue };
            if expires <= now {
                continue; // already stale
            }
            jar.partitions.entry(f[0].to_string()).or_default().push(Cookie {
                domain: f[1].to_string(),
                path: f[2].to_string(),
                name: f[3].to_string(),
                value: f[4].to_string(),
                secure: f[5] == "1",
                expires: Some(expires),
            });
        }
        jar
    }
}

/// Parse an HTTP date such as `Wed, 21 Jul 2027 11:53:18 GMT` to Unix seconds.
///
/// Most sites express cookie lifetimes this way rather than with `Max-Age`, so
/// without this almost nothing would survive a restart.
fn parse_http_date(text: &str) -> Option<u64> {
    // Drop the weekday, then expect: day month year hh:mm:ss
    let rest = text.split_once(',').map(|(_, r)| r).unwrap_or(text);
    let mut parts = rest.split_whitespace();
    let day: u64 = parts.next()?.parse().ok()?;
    let month = month_number(parts.next()?)?;
    let year: i64 = parts.next()?.parse().ok()?;
    let mut clock = parts.next()?.split(':');
    let (h, m, s): (u64, u64, u64) = (
        clock.next()?.parse().ok()?,
        clock.next()?.parse().ok()?,
        clock.next().unwrap_or("0").parse().ok()?,
    );
    if !(1..=31).contains(&day) || h > 23 || m > 59 || s > 60 {
        return None;
    }
    let days = days_from_civil(year, month, day as i64);
    if days < 0 {
        return None;
    }
    Some(days as u64 * 86_400 + h * 3600 + m * 60 + s)
}

fn month_number(name: &str) -> Option<i64> {
    const MONTHS: [&str; 12] = [
        "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
    ];
    let key = name.to_ascii_lowercase();
    MONTHS.iter().position(|m| key.starts_with(m)).map(|i| i as i64 + 1)
}

/// Days since 1970-01-01 (Howard Hinnant's civil-date algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Host matches the cookie's domain exactly, or is a subdomain of it.
fn domain_matches(host: &str, domain: &str) -> bool {
    host == domain || host.ends_with(&format!(".{domain}"))
}

/// Parse one `Set-Cookie` value against the URL that produced it.
fn parse_set_cookie(header: &str, url: &str) -> Option<Cookie> {
    let mut parts = header.split(';');
    let (name, value) = parts.next()?.split_once('=')?;
    let name = name.trim();
    if name.is_empty() {
        return None;
    }

    let mut cookie = Cookie {
        name: name.to_string(),
        value: value.trim().to_string(),
        domain: site_of(url),
        path: "/".to_string(),
        secure: false,
        expires: None,
    };

    for attr in parts {
        let attr = attr.trim();
        let (key, val) = match attr.split_once('=') {
            Some((k, v)) => (k.trim().to_ascii_lowercase(), v.trim().to_string()),
            None => (attr.to_ascii_lowercase(), String::new()),
        };
        match key.as_str() {
            "domain" => {
                let candidate = val.trim_start_matches('.').to_ascii_lowercase();
                // A site may only widen to a domain it actually belongs to.
                if !candidate.is_empty() && domain_matches(&cookie.domain, &candidate) {
                    cookie.domain = candidate;
                }
            }
            "path" if val.starts_with('/') => cookie.path = val,
            "secure" => cookie.secure = true,
            "max-age" => {
                if let Ok(secs) = val.parse::<i64>() {
                    cookie.expires =
                        Some(if secs <= 0 { 0 } else { now_secs().saturating_add(secs as u64) });
                }
            }
            // Max-Age wins over Expires when both are present, so only fill a gap.
            "expires" => {
                if cookie.expires.is_none() {
                    cookie.expires = parse_http_date(&val);
                }
            }
            _ => {} // SameSite, HttpOnly, Priority, ...
        }
    }
    Some(cookie)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn stores_and_returns_cookies_for_matching_requests() {
        let mut jar = CookieJar::default();
        jar.store("shop.test", "https://shop.test/login", "sid=abc; Path=/");
        assert_eq!(jar.header_for("shop.test", "https://shop.test/account"), Some("sid=abc".into()));
        // A different host in the same partition doesn't get it.
        assert_eq!(jar.header_for("shop.test", "https://other.test/"), None);
    }

    #[test]
    fn partitioning_stops_cross_site_tracking() {
        let mut jar = CookieJar::default();
        // The same third party sets a cookie on two different sites.
        jar.store("a.com", "https://tracker.test/px", "id=from-a");
        jar.store("b.com", "https://tracker.test/px", "id=from-b");
        // Each partition sees only its own value, so the two visits can't be joined.
        assert_eq!(jar.header_for("a.com", "https://tracker.test/px"), Some("id=from-a".into()));
        assert_eq!(jar.header_for("b.com", "https://tracker.test/px"), Some("id=from-b".into()));
        assert_eq!(jar.header_for("c.com", "https://tracker.test/px"), None);
    }

    #[test]
    fn honours_path_secure_and_expiry() {
        let mut jar = CookieJar::default();
        jar.store("x.test", "https://x.test/", "deep=1; Path=/admin");
        jar.store("x.test", "https://x.test/", "tls=1; Secure");
        jar.store("x.test", "https://x.test/", "gone=1; Max-Age=0");

        // Path-scoped cookies only go to matching paths.
        assert_eq!(jar.header_for("x.test", "https://x.test/admin/panel"), Some("deep=1; tls=1".into()));
        assert_eq!(jar.header_for("x.test", "https://x.test/other"), Some("tls=1".into()));
        // Secure cookies never travel over cleartext.
        assert_eq!(jar.header_for("x.test", "http://x.test/other"), None);
        // Max-Age=0 is a deletion.
        assert!(!jar.header_for("x.test", "https://x.test/").unwrap_or_default().contains("gone"));
    }

    #[test]
    fn a_site_cannot_claim_an_unrelated_domain() {
        let mut jar = CookieJar::default();
        jar.store("evil.test", "https://evil.test/", "a=1; Domain=example.com");
        // The Domain attribute is ignored, so the cookie stays on evil.test.
        assert_eq!(jar.header_for("evil.test", "https://example.com/"), None);
        assert_eq!(jar.header_for("evil.test", "https://evil.test/"), Some("a=1".into()));

        // Widening to a real parent domain is allowed.
        let mut jar = CookieJar::default();
        jar.store("app.example.com", "https://app.example.com/", "b=2; Domain=example.com");
        assert_eq!(jar.header_for("app.example.com", "https://api.example.com/"), Some("b=2".into()));
    }

    #[test]
    fn parses_http_dates_used_by_real_cookies() {
        // The epoch itself.
        assert_eq!(parse_http_date("Thu, 01 Jan 1970 00:00:00 GMT"), Some(0));
        // A known instant: 2021-01-01T00:00:00Z.
        assert_eq!(parse_http_date("Fri, 01 Jan 2021 00:00:00 GMT"), Some(1_609_459_200));
        // Leap day arithmetic.
        assert_eq!(parse_http_date("Sat, 29 Feb 2020 12:00:00 GMT"), Some(1_582_977_600));
        // Junk is rejected, not guessed at.
        assert!(parse_http_date("not a date").is_none());
        assert!(parse_http_date("Wed, 45 Xxx 2027 11:53:18 GMT").is_none());
    }

    #[test]
    fn expires_dates_make_cookies_persistent() {
        let mut jar = CookieJar::default();
        jar.store("g.test", "https://g.test/", "a=1; expires=Wed, 21 Jul 2100 11:53:18 GMT");
        assert_eq!(jar.header_for("g.test", "https://g.test/"), Some("a=1".into()));
        // A date in the past deletes instead.
        jar.store("g.test", "https://g.test/", "a=1; expires=Wed, 21 Jul 2000 11:53:18 GMT");
        assert_eq!(jar.header_for("g.test", "https://g.test/"), None);
    }

    #[test]
    fn persists_only_cookies_with_a_future_expiry() {
        let dir = std::env::temp_dir().join("zero-cookie-test");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("scratch dir");

        let mut jar = CookieJar::default();
        jar.store("s.test", "https://s.test/", "keep=1; Max-Age=3600");
        jar.store("s.test", "https://s.test/", "session=1"); // no expiry
        jar.save(&dir);

        let loaded = CookieJar::load(&dir);
        assert_eq!(loaded.len(), 1, "session cookies must not survive a restart");
        assert_eq!(loaded.header_for("s.test", "https://s.test/"), Some("keep=1".into()));
    }
}

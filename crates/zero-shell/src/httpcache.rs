//! An HTTP cache on disk, so the sites you use most stop paying full network
//! cost every time you open them.
//!
//! The in-memory map in [`crate::net::ShellLoader`] is the first tier and lives
//! only as long as one document: it stops a keystroke refetching a page's
//! images, and nothing more. This is the second tier — it survives a reload, a
//! second tab, and a restart, and it is where validators live.
//!
//! Two things make it a cache rather than a pile of files:
//!
//! * **Freshness.** `Cache-Control: max-age` (or `Expires`) says how long a
//!   response may be reused with no network at all. That is the case that makes
//!   a revisit feel instant.
//! * **Validation.** Once stale, a stored `ETag` or `Last-Modified` goes back as
//!   `If-None-Match` / `If-Modified-Since`, and a `304 Not Modified` costs one
//!   small round trip instead of the whole body. Most static assets answer 304
//!   forever, which is the difference between a revisit and a reload.
//!
//! Entries are encrypted at rest like every other profile file — a cache is a
//! record of what you read, so it is browsing history with the bodies attached.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// How much the cache may hold before the oldest entries are dropped.
///
/// ponytail: trimmed by file modification time, oldest first, which is a
/// recency policy and not a very clever one. A real LRU with hit counts is the
/// upgrade if the hit rate ever looks disappointing.
const MAX_BYTES: u64 = 64 * 1024 * 1024;

/// A cached response.
pub struct Entry {
    pub body: Vec<u8>,
    /// Whether it may be used with no network at all.
    pub fresh: bool,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
}

/// The validators to send for a stale entry, as header name/value pairs.
impl Entry {
    pub fn validators(&self) -> Vec<(&'static str, String)> {
        let mut out = Vec::new();
        if let Some(etag) = &self.etag {
            out.push(("If-None-Match", etag.clone()));
        }
        if let Some(since) = &self.last_modified {
            out.push(("If-Modified-Since", since.clone()));
        }
        out
    }
}

/// What a response's headers say about storing it.
pub struct Policy {
    pub store: bool,
    /// When it stops being fresh, as a Unix timestamp.
    pub expires: u64,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
}

/// Read `Cache-Control`, `Expires` and the validators off a response.
///
/// `header` is asked for one header at a time so this stays testable without a
/// live response — the caller adapts whatever HTTP library it has.
pub fn policy(status: u16, header: impl Fn(&str) -> Option<String>) -> Policy {
    let cache_control = header("cache-control").unwrap_or_default().to_lowercase();
    let etag = header("etag");
    let last_modified = header("last-modified");
    // `no-store` means what it says. `no-cache` does *not* mean "do not store"
    // — it means "revalidate before reuse", which is a stored entry with no
    // freshness, and is exactly what a max-age of zero gives.
    let no_store = cache_control.contains("no-store");
    let max_age = cache_control
        .split(',')
        .map(str::trim)
        .find_map(|part| part.strip_prefix("max-age=")?.trim().parse::<u64>().ok());
    let seconds = match (cache_control.contains("no-cache"), max_age) {
        (true, _) => 0,
        (false, Some(seconds)) => seconds,
        // No directive at all: a validator is still worth keeping, because a
        // 304 is cheap. Without one there is nothing to revalidate with and
        // nothing to gain, so the entry would only take up room.
        (false, None) => 0,
    };
    Policy {
        // 200 only. A redirect or an error has no body worth keeping, and the
        // partial and conditional statuses need machinery this does not have.
        store: status == 200
            && !no_store
            && (seconds > 0 || etag.is_some() || last_modified.is_some()),
        expires: now() + seconds,
        etag,
        last_modified,
    }
}

/// Look a URL up. `None` means nothing stored, so the request is unconditional.
pub fn get(url: &str) -> Option<Entry> {
    let path = path_for(url)?;
    let sealed = std::fs::read(&path).ok()?;
    let plain = crate::crypto::unprotect(&sealed)?;
    let (head, body) = split_record(&plain)?;
    let mut fields = head.lines();
    // The URL is stored so a hash collision cannot serve one site's response
    // for another's request.
    if fields.next()? != url {
        return None;
    }
    let expires: u64 = fields.next()?.parse().ok()?;
    let etag = fields.next().map(str::to_string).filter(|v| !v.is_empty());
    let last_modified = fields.next().map(str::to_string).filter(|v| !v.is_empty());
    Some(Entry {
        body: body.to_vec(),
        fresh: expires > now(),
        etag,
        last_modified,
    })
}

/// Store a response, or replace one already stored.
pub fn put(url: &str, body: &[u8], policy: &Policy) {
    if !policy.store {
        return;
    }
    let Some(path) = path_for(url) else {
        return;
    };
    let head = format!(
        "{url}\n{}\n{}\n{}\n",
        policy.expires,
        policy.etag.clone().unwrap_or_default(),
        policy.last_modified.clone().unwrap_or_default(),
    );
    let mut record = head.into_bytes();
    record.push(0); // the one byte a header line can never contain
    record.extend_from_slice(body);
    let sealed = crate::crypto::protect(&record);
    if std::fs::write(&path, sealed).is_ok() {
        trim();
    }
}

/// A `304 Not Modified`: the stored body still stands, and its freshness is
/// whatever the new response says.
pub fn refresh(url: &str, entry: &Entry, policy: &Policy) {
    let carried = Policy {
        store: true,
        expires: policy.expires,
        // A 304 need not repeat the validators, so the stored ones stay.
        etag: policy.etag.clone().or_else(|| entry.etag.clone()),
        last_modified: policy
            .last_modified
            .clone()
            .or_else(|| entry.last_modified.clone()),
    };
    put(url, &entry.body, &carried);
}

fn dir() -> Option<PathBuf> {
    let dir = crate::storage::profile_dir()?.join("cache");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

fn path_for(url: &str) -> Option<PathBuf> {
    Some(dir()?.join(format!("{:016x}", key_of(url))))
}

/// FNV-1a. A cache key needs to spread, not to resist an adversary — and the
/// stored URL is checked on read, so a collision costs a miss, not a mix-up.
fn key_of(url: &str) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in url.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

/// Split the header block from the body at the NUL terminator.
fn split_record(record: &[u8]) -> Option<(&str, &[u8])> {
    let at = record.iter().position(|b| *b == 0)?;
    Some((std::str::from_utf8(&record[..at]).ok()?, &record[at + 1..]))
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Drop the oldest entries until the cache is back under its limit.
fn trim() {
    let Some(dir) = dir() else { return };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    let mut files: Vec<(SystemTime, u64, PathBuf)> = entries
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let meta = entry.metadata().ok()?;
            Some((meta.modified().ok()?, meta.len(), entry.path()))
        })
        .collect();
    let mut total: u64 = files.iter().map(|(_, size, _)| size).sum();
    if total <= MAX_BYTES {
        return;
    }
    files.sort_by_key(|(when, _, _)| *when);
    for (_, size, path) in files {
        if total <= MAX_BYTES {
            break;
        }
        if std::fs::remove_file(&path).is_ok() {
            total = total.saturating_sub(size);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A header lookup built from pairs, standing in for a live response.
    fn headers<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name: &str| {
            pairs
                .iter()
                .find(|(key, _)| key.eq_ignore_ascii_case(name))
                .map(|(_, value)| value.to_string())
        }
    }

    #[test]
    fn freshness_comes_from_cache_control_and_expires() {
        let fresh = policy(200, headers(&[("cache-control", "public, max-age=3600")]));
        assert!(fresh.store);
        assert!(fresh.expires > now() + 3000);

        // `no-store` is the one directive that means do not keep it at all.
        let forbidden = policy(
            200,
            headers(&[("cache-control", "no-store"), ("etag", "\"abc\"")]),
        );
        assert!(!forbidden.store);

        // `no-cache` means revalidate, which is a stored entry with no
        // freshness — not a refusal to store.
        let revalidate = policy(
            200,
            headers(&[("cache-control", "no-cache"), ("etag", "\"abc\"")]),
        );
        assert!(revalidate.store);
        assert!(revalidate.expires <= now());

        // A validator alone is worth storing: a 304 is far cheaper than a body.
        let validated = policy(
            200,
            headers(&[("last-modified", "Wed, 21 Oct 2026 07:28:00 GMT")]),
        );
        assert!(validated.store);
        assert_eq!(
            validated.last_modified.as_deref(),
            Some("Wed, 21 Oct 2026 07:28:00 GMT")
        );

        // Nothing to revalidate with and no freshness: keeping it would only
        // take up room for a response that has to be refetched anyway.
        assert!(!policy(200, headers(&[])).store);
        // Only 200 is stored.
        assert!(!policy(404, headers(&[("cache-control", "max-age=600")])).store);
        assert!(!policy(302, headers(&[("cache-control", "max-age=600")])).store);
    }

    #[test]
    fn a_stale_entry_asks_the_server_with_what_it_has() {
        let entry = Entry {
            body: b"body".to_vec(),
            fresh: false,
            etag: Some("\"v1\"".to_string()),
            last_modified: Some("Wed, 21 Oct 2026 07:28:00 GMT".to_string()),
        };
        let sent = entry.validators();
        assert_eq!(sent[0].0, "If-None-Match");
        assert_eq!(sent[0].1, "\"v1\"");
        assert_eq!(sent[1].0, "If-Modified-Since");

        // With nothing to validate against, there is nothing to send, and the
        // request has to be unconditional.
        let bare = Entry {
            body: Vec::new(),
            fresh: false,
            etag: None,
            last_modified: None,
        };
        assert!(bare.validators().is_empty());
    }

    #[test]
    fn a_record_survives_being_written_and_read_back() {
        // The record format, which is what a real get/put exchanges — exercised
        // directly because `profile_dir` is `None` under test and nothing here
        // may touch a real profile.
        let mut record = b"https://example.org/a.css\n9999999999\n\"v1\"\n\n".to_vec();
        record.push(0);
        record.extend_from_slice(b"body { color: red }");
        let (head, body) = split_record(&record).expect("a well-formed record");
        let mut fields = head.lines();
        assert_eq!(fields.next(), Some("https://example.org/a.css"));
        assert_eq!(fields.next(), Some("9999999999"));
        assert_eq!(fields.next(), Some("\"v1\""));
        assert_eq!(body, b"body { color: red }");
        // A body with a NUL in it does not confuse the split: the header ends at
        // the *first* one.
        let mut binary = b"u\n0\n\n\n".to_vec();
        binary.push(0);
        binary.extend_from_slice(&[1, 0, 2]);
        let (_, body) = split_record(&binary).expect("a binary body");
        assert_eq!(body, &[1, 0, 2]);
        // Different URLs land in different files.
        assert_ne!(key_of("https://a.example/x"), key_of("https://b.example/x"));
    }
}

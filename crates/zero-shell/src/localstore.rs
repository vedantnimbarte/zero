//! `localStorage` and `sessionStorage` for pages, partitioned by origin.
//!
//! Each origin gets its own file, so one can never read another's keys.
//! Deliberately *not* the cookie jar's rule: a cookie is scoped by host and
//! one set over https is readable over http, but a storage area is scoped by
//! scheme, host and port together. Sharing an area across schemes would let
//! anything able to tamper with a plain-HTTP page read and rewrite what the
//! secure origin stored.
//!
//! Both areas are capped at [`QUOTA`]; a write past it is refused so the
//! caller can raise the `QuotaExceededError` the web throws, rather than
//! dropping data a page believes it saved. `localStorage` is written eagerly
//! on every `setItem` (fine at these sizes) and encrypted at rest via
//! [`crate::crypto`]; `sessionStorage` never reaches disk at all.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

/// What one area may hold, counted over its keys and values together.
///
/// 5 MiB is the figure every browser settled on, and sites are written
/// against it — a smaller cap would reject writes that work everywhere else,
/// and a larger one would let a page fill the disk.
const QUOTA: usize = 5 * 1024 * 1024;

/// Whether `key` = `value` still fits once whatever `key` already held is
/// given back. Shared by both areas: they hold the same kind of map and the
/// web gives them the same cap.
fn fits(entries: &BTreeMap<String, String>, key: &str, value: &str) -> bool {
    let used: usize = entries.iter().map(|(k, v)| k.len() + v.len()).sum();
    let replacing = entries.get(key).map_or(0, |old| key.len() + old.len());
    used.saturating_sub(replacing) + key.len() + value.len() <= QUOTA
}

pub struct SiteStore {
    file: Option<PathBuf>,
    entries: RefCell<BTreeMap<String, String>>,
}

/// A file name that cannot escape its directory, whatever the site is called.
fn safe_name(site: &str) -> String {
    site.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Keep a site's data in its own file, with unsafe path characters replaced.
fn file_for(site: &str) -> Option<PathBuf> {
    let dir = crate::storage::profile_dir()?.join("localstorage");
    fs::create_dir_all(&dir).ok()?;
    let safe = safe_name(site);
    if safe.is_empty() {
        return None;
    }
    Some(dir.join(format!("{safe}.tsv")))
}

/// The bare host whose pre-origin file this area may adopt, if any.
///
/// Storage used to be keyed by host, so moving to origins orphans whatever a
/// site had already saved. Exactly one origin per host is allowed to inherit
/// it: `https` on the default port. Letting `http://example.com` inherit
/// would hand the plaintext origin the very data splitting by origin just
/// protected — the upgrade would reopen the hole it was closing.
fn legacy_host(site: &str) -> Option<&str> {
    let host = site.strip_prefix("https://")?;
    // An explicit port was never part of the old key, so no file of that name
    // can be this origin's.
    (!host.contains(':') && !host.is_empty()).then_some(host)
}

/// That host's file, if it is actually still there.
fn legacy_file(site: &str) -> Option<PathBuf> {
    let host = legacy_host(site)?;
    let path = crate::storage::profile_dir()?
        .join("localstorage")
        .join(format!("{}.tsv", safe_name(host)));
    path.exists().then_some(path)
}

thread_local! {
    /// Every `SiteStore` currently open, keyed by the file behind it.
    ///
    /// Two tabs on one site must share *one* instance. `flush` rewrites the
    /// whole file from its own map, so two instances over one file means the
    /// last writer silently erases whatever the other added after it loaded —
    /// a page losing keys it had just written. Sharing also makes a write in
    /// one tab visible in the other immediately, which is what the web
    /// promises and what a `storage` event would otherwise be announcing
    /// about a value the other tab still could not see.
    ///
    /// Keyed by path, not by site, so switching space — which moves the
    /// profile directory — cannot hand back the previous space's store.
    ///
    /// ponytail: never evicted, so a store stays open for the life of the
    /// process once its site is visited. Bounded by sites visited, and each
    /// holds only what that site wrote.
    static OPEN: RefCell<std::collections::HashMap<String, std::rc::Rc<SiteStore>>> =
        RefCell::new(std::collections::HashMap::new());
}

/// The one open store for `site`, loaded from disk the first time it is asked
/// for and shared by every caller after that.
pub fn site_store(site: &str) -> std::rc::Rc<SiteStore> {
    // Keyed by the backing file where there is one. With no profile directory
    // at all — a test run, or a headless render — one memory-backed store per
    // site still has to be shared, or two tabs on a site would fail to see
    // each other for a different reason than the one just fixed.
    let key = match file_for(site) {
        Some(file) => file.to_string_lossy().into_owned(),
        None => format!("memory:{site}"),
    };
    OPEN.with(|open| {
        open.borrow_mut()
            .entry(key)
            .or_insert_with(|| std::rc::Rc::new(SiteStore::for_site(site)))
            .clone()
    })
}

impl SiteStore {
    pub fn for_site(site: &str) -> SiteStore {
        let file = file_for(site);
        let mut entries = BTreeMap::new();
        // Read this origin's own file, or — only the first time, before it has
        // one — the pre-origin file for its host. The next write lands in the
        // new file; the old one is left alone rather than deleted, so a
        // downgrade still finds it.
        let source = match file.as_ref().filter(|path| path.exists()) {
            Some(path) => Some(path.clone()),
            None => legacy_file(site),
        };
        if let Some(text) = source.as_ref().and_then(|f| crate::crypto::read_file(f)) {
            for line in text.lines() {
                if let Some((k, v)) = line.split_once('\t') {
                    entries.insert(k.to_string(), v.to_string());
                }
            }
        }
        SiteStore {
            file,
            entries: RefCell::new(entries),
        }
    }

    fn flush(&self) {
        let Some(file) = &self.file else { return };
        let text: String = self
            .entries
            .borrow()
            .iter()
            .map(|(k, v)| format!("{k}\t{v}\n"))
            .collect();
        crate::crypto::write_file(file, &text);
    }
}

impl zero_engine::KeyValueStore for SiteStore {
    fn get(&self, key: &str) -> Option<String> {
        self.entries.borrow().get(key).cloned()
    }

    fn set(&self, key: &str, value: &str) -> bool {
        // Tabs and newlines are the record separators, so they cannot survive.
        let clean = |s: &str| s.replace(['\t', '\r', '\n'], " ");
        let (key, value) = (clean(key), clean(value));
        // Measured after cleaning, since that is what actually gets stored.
        if !fits(&self.entries.borrow(), &key, &value) {
            return false;
        }
        self.entries.borrow_mut().insert(key, value);
        self.flush();
        true
    }

    fn remove(&self, key: &str) {
        self.entries.borrow_mut().remove(key);
        self.flush();
    }

    fn clear(&self) {
        self.entries.borrow_mut().clear();
        self.flush();
    }

    fn keys(&self) -> Vec<String> {
        self.entries.borrow().keys().cloned().collect()
    }
}

/// A store that keeps nothing, for a renderer that has not been given a page
/// yet: no site means no site's storage. Replaced the moment one is loaded.
pub struct NullStore;

impl zero_engine::KeyValueStore for NullStore {
    fn get(&self, _key: &str) -> Option<String> {
        None
    }
    fn set(&self, _key: &str, _value: &str) -> bool {
        // Nothing is kept, so nothing can overflow. Reporting success keeps a
        // page with no storage on its working path rather than making it
        // handle a quota error it cannot do anything about.
        true
    }
    fn remove(&self, _key: &str) {}
    fn clear(&self) {}
    fn keys(&self) -> Vec<String> {
        Vec::new()
    }
}

/// A `localStorage` write one tab made, waiting to be told to the others.
///
/// `key` is `None` when the write was a `clear()`, matching the null key the
/// web puts on that event.
pub struct StorageNotice {
    pub site: String,
    /// Which tab wrote it. That tab is the one document that must *not* hear
    /// about it: the web fires `storage` only on the others.
    pub tab: usize,
    pub key: Option<String>,
    pub old: Option<String>,
    pub new: Option<String>,
}

thread_local! {
    /// Writes made since the last drain. Filled by the renderer reply loop,
    /// which runs on this thread and has no view of the other tabs, and
    /// emptied by the app once it is back somewhere that does.
    static PENDING: RefCell<Vec<StorageNotice>> = const { RefCell::new(Vec::new()) };
}

/// Record a write for the other tabs on its site.
pub fn note_write(notice: StorageNotice) {
    PENDING.with(|pending| pending.borrow_mut().push(notice));
}

/// Take everything recorded since the last call.
pub fn take_writes() -> Vec<StorageNotice> {
    PENDING.with(|pending| std::mem::take(&mut *pending.borrow_mut()))
}

/// The two key/value stores a page is given. They always travel together, and
/// they are the same type — so they ride in named fields rather than as two
/// positional arguments, where swapping them would silently hand a page its
/// `sessionStorage` as `localStorage` with nothing to catch it.
#[derive(Clone)]
pub struct Stores {
    /// Persistent, partitioned by site. Outlives the tab.
    pub local: std::rc::Rc<dyn zero_engine::KeyValueStore>,
    /// In memory, partitioned by site within one tab. Dies with the tab.
    pub session: std::rc::Rc<dyn zero_engine::KeyValueStore>,
    /// Whose these are — the site they are partitioned under, and the tab that
    /// holds them. Carried so a write can say who to tell and who to skip.
    pub site: String,
    pub tab: usize,
}

impl Stores {
    /// Neither store: a headless one-shot render has no site to persist to and
    /// no tab for a session to belong to.
    pub fn none() -> Stores {
        Stores {
            local: std::rc::Rc::new(NullStore),
            session: std::rc::Rc::new(NullStore),
            site: String::new(),
            tab: 0,
        }
    }
}

/// `sessionStorage` for one tab: in memory, partitioned by site, and gone the
/// moment the tab closes.
///
/// It lives on the `Tab` rather than in the engine or the renderer child
/// because both of those are rebuilt on every navigation — a fresh `Interp`
/// per `Document`, and a fresh process per web page — and the whole point of
/// `sessionStorage` is that it survives exactly that and nothing more.
///
/// Capped at [`QUOTA`], like [`SiteStore`]. Never touches disk, so there is
/// nothing to encrypt and nothing to clean up.
#[derive(Default)]
pub struct TabSessions {
    sites: RefCell<std::collections::HashMap<String, Site>>,
}

type Site = std::rc::Rc<RefCell<BTreeMap<String, String>>>;

impl TabSessions {
    /// This tab's storage for one site, created empty on first use. Two sites
    /// in the same tab never see each other's keys, the same rule
    /// [`SiteStore`] follows on disk.
    pub fn for_site(&self, site: &str) -> std::rc::Rc<dyn zero_engine::KeyValueStore> {
        let entries = self
            .sites
            .borrow_mut()
            .entry(site.to_string())
            .or_default()
            .clone();
        std::rc::Rc::new(SessionStore { entries })
    }
}

struct SessionStore {
    entries: Site,
}

impl zero_engine::KeyValueStore for SessionStore {
    fn get(&self, key: &str) -> Option<String> {
        self.entries.borrow().get(key).cloned()
    }

    fn set(&self, key: &str, value: &str) -> bool {
        if !fits(&self.entries.borrow(), key, value) {
            return false;
        }
        self.entries
            .borrow_mut()
            .insert(key.to_string(), value.to_string());
        true
    }

    fn remove(&self, key: &str) {
        self.entries.borrow_mut().remove(key);
    }

    fn clear(&self) {
        self.entries.borrow_mut().clear();
    }

    fn keys(&self) -> Vec<String> {
        self.entries.borrow().keys().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zero_engine::KeyValueStore;

    #[test]
    fn only_the_secure_origin_inherits_the_pre_origin_file() {
        // The migration must not undo the split it is migrating for. If
        // `http://` were ever allowed to adopt the old host file, every
        // secret the https origin had saved would be readable by a page an
        // attacker on the network can rewrite at will.
        assert_eq!(legacy_host("https://example.com"), Some("example.com"));
        assert_eq!(
            legacy_host("http://example.com"),
            None,
            "plaintext must inherit nothing"
        );
        assert_eq!(
            legacy_host("https://example.com:8443"),
            None,
            "a port was never in the old key"
        );
        assert_eq!(
            legacy_host("example.com"),
            None,
            "a bare host is not an origin"
        );
        assert_eq!(legacy_host("zero://settings"), None);
    }

    #[test]
    fn a_name_on_disk_can_never_climb_out_of_its_directory() {
        // The site string reaches this from a page's address. Dots survive —
        // hosts are full of them — so what makes this safe is that nothing can
        // be a separator: a literal ".." inside one flat name is just a
        // filename, with no directory above it to climb into.
        let name = safe_name("https://example.com/../../etc/passwd");
        assert!(!name.contains('/'), "got {name}");
        assert!(!name.contains('\\'), "got {name}");
        assert_eq!(
            std::path::Path::new(&name).components().count(),
            1,
            "the name must be a single path component, got {name}"
        );
    }

    #[test]
    fn an_area_that_is_full_refuses_the_write_instead_of_losing_it() {
        let store = site_store("quota.example");
        assert!(store.set("small", "fine"), "an ordinary write fits");

        let huge = "x".repeat(QUOTA + 1);
        assert!(!store.set("huge", &huge), "a value past the cap is refused");
        assert_eq!(store.get("huge"), None, "and nothing of it is kept");
        assert_eq!(
            store.get("small").as_deref(),
            Some("fine"),
            "a refused write must not disturb what was already there"
        );

        // Right up to the cap still fits: the check is not off by one in the
        // direction that would reject writes other browsers accept.
        let store = site_store("quota2.example");
        let key = "k";
        assert!(store.set(key, &"y".repeat(QUOTA - key.len())));

        // Replacing a value counts only the difference, so rewriting the same
        // key at the same size is not suddenly over.
        assert!(store.set(key, &"z".repeat(QUOTA - key.len())));
    }

    #[test]
    fn a_session_area_is_capped_the_same_way() {
        let sessions = TabSessions::default();
        let store = sessions.for_site("quota.example");
        assert!(store.set("ok", "1"));
        assert!(!store.set("huge", &"x".repeat(QUOTA + 1)));
        assert_eq!(store.get("ok").as_deref(), Some("1"));
    }

    #[test]
    fn two_tabs_on_one_site_share_a_store_instead_of_clobbering_each_other() {
        // `flush` rewrites the whole file from one instance's map, so two
        // instances over one file lose whichever keys the loser added after
        // the winner loaded. Sharing one instance is what stops that, and it
        // is also what makes a write in one tab visible in the other at all.
        let a = site_store("shared.example");
        let b = site_store("shared.example");
        assert!(
            std::rc::Rc::ptr_eq(&a, &b),
            "one site must mean one open store"
        );

        a.set("from_a", "1");
        b.set("from_b", "2");
        assert_eq!(
            a.get("from_b").as_deref(),
            Some("2"),
            "a must see b's write"
        );
        assert_eq!(
            b.get("from_a").as_deref(),
            Some("1"),
            "b must see a's write"
        );
        assert_eq!(a.keys().len(), 2, "neither write may have erased the other");

        // A different site is a different store, as on disk.
        let other = site_store("elsewhere.example");
        assert!(!std::rc::Rc::ptr_eq(&a, &other));
        assert_eq!(
            other.get("from_a"),
            None,
            "one site must not see another's keys"
        );
    }

    #[test]
    fn a_tab_session_survives_navigation_but_not_a_change_of_site() {
        // This is the whole reason `TabSessions` lives on the `Tab` rather
        // than in the engine or the renderer: a web navigation throws away
        // both of those, and asks for a fresh handle afterwards. If
        // `for_site` handed back a new map each time instead of a shared one,
        // `sessionStorage` would silently empty on every click.
        let sessions = TabSessions::default();

        sessions.for_site("example.com").set("k", "kept");
        // A navigation: the renderer process is gone, and the new one asks
        // this same `TabSessions` for the same site again.
        assert_eq!(
            sessions.for_site("example.com").get("k").as_deref(),
            Some("kept"),
            "navigating within a site should not empty its sessionStorage"
        );

        // Another site in the same tab is a different store, the same rule
        // `SiteStore` follows on disk.
        assert_eq!(
            sessions.for_site("other.example").get("k"),
            None,
            "one site must not see another's session keys"
        );

        // Clearing one leaves the other alone.
        sessions.for_site("other.example").set("k", "theirs");
        sessions.for_site("example.com").clear();
        assert_eq!(sessions.for_site("example.com").get("k"), None);
        assert_eq!(
            sessions.for_site("other.example").get("k").as_deref(),
            Some("theirs"),
            "clearing one site's session should not touch another's"
        );
    }
}

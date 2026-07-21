//! `localStorage` for pages, partitioned by site.
//!
//! Each site gets its own file, so one origin can never read another's keys —
//! the same partitioning rule the cookie jar follows.
//!
//! ponytail: written eagerly on every `setItem` (fine at these sizes), stored in
//! the clear, and unbounded — no quota is enforced.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

pub struct SiteStore {
    file: Option<PathBuf>,
    entries: RefCell<BTreeMap<String, String>>,
}

/// Keep a site's data in its own file, with unsafe path characters replaced.
fn file_for(site: &str) -> Option<PathBuf> {
    let dir = crate::storage::profile_dir()?.join("localstorage");
    fs::create_dir_all(&dir).ok()?;
    let safe: String = site
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' { c } else { '_' })
        .collect();
    if safe.is_empty() {
        return None;
    }
    Some(dir.join(format!("{safe}.tsv")))
}

impl SiteStore {
    pub fn for_site(site: &str) -> SiteStore {
        let file = file_for(site);
        let mut entries = BTreeMap::new();
        if let Some(text) = file.as_ref().and_then(|f| fs::read_to_string(f).ok()) {
            for line in text.lines() {
                if let Some((k, v)) = line.split_once('\t') {
                    entries.insert(k.to_string(), v.to_string());
                }
            }
        }
        SiteStore { file, entries: RefCell::new(entries) }
    }

    fn flush(&self) {
        let Some(file) = &self.file else { return };
        let text: String =
            self.entries.borrow().iter().map(|(k, v)| format!("{k}\t{v}\n")).collect();
        let _ = fs::write(file, text);
    }
}

impl zero_engine::KeyValueStore for SiteStore {
    fn get(&self, key: &str) -> Option<String> {
        self.entries.borrow().get(key).cloned()
    }

    fn set(&self, key: &str, value: &str) {
        // Tabs and newlines are the record separators, so they cannot survive.
        let clean = |s: &str| s.replace(['\t', '\r', '\n'], " ");
        self.entries.borrow_mut().insert(clean(key), clean(value));
        self.flush();
    }

    fn remove(&self, key: &str) {
        self.entries.borrow_mut().remove(key);
        self.flush();
    }

    fn clear(&self) {
        self.entries.borrow_mut().clear();
        self.flush();
    }
}

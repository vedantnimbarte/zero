//! Spaces: separate profiles in one browser.
//!
//! A space owns everything a profile owns — history, bookmarks, cookies,
//! `localStorage`, downloads, session, settings and its own encryption key —
//! because a space *is* a profile directory. Nothing needs to be filtered or
//! partitioned at read time; work and personal simply never look at the same
//! files.
//!
//! Which space is current is the one thing that cannot live inside a space, so
//! it sits in the root as plain text. It is a name, not a secret, and the key
//! that would encrypt it is itself per-space — reading it must not require
//! knowing it first.

use std::path::PathBuf;

/// The space every profile starts in. It keeps the root directory itself, so
/// upgrading from a build without spaces moves nobody's data.
pub const DEFAULT: &str = "personal";

/// Accents a space can take, in the order new spaces are offered them. The
/// first is Zero's own red, which the default space keeps.
const ACCENTS: &[&str] = &[
    "#e5484d", // red
    "#3e63dd", // indigo
    "#30a46c", // green
    "#f5a524", // amber
    "#8e4ec6", // violet
    "#0d9488", // teal
];

/// The name of the space in use.
pub fn current() -> String {
    let Some(path) = marker() else { return DEFAULT.to_string() };
    match std::fs::read_to_string(path) {
        Ok(name) => match sanitize(&name) {
            Some(name) => name,
            None => DEFAULT.to_string(),
        },
        Err(_) => DEFAULT.to_string(),
    }
}

/// Switch to `name`, creating it if it does not exist yet. Returns the name
/// actually switched to, or `None` if it was not a usable name.
pub fn switch(name: &str) -> Option<String> {
    let name = sanitize(name)?;
    let path = marker()?;
    std::fs::write(path, &name).ok()?;
    // Creating the directory here means `list` sees the new space immediately,
    // even before anything has been saved in it.
    if let Some(dir) = dir_of(&name) {
        std::fs::create_dir_all(dir).ok()?;
    }
    Some(name)
}

/// Every space that exists, default first, then the rest alphabetically.
pub fn list() -> Vec<String> {
    let mut names = vec![DEFAULT.to_string()];
    if let Some(root) = crate::storage::root_dir() {
        if let Ok(entries) = std::fs::read_dir(root.join("spaces")) {
            let mut found: Vec<String> = entries
                .filter_map(Result::ok)
                .filter(|e| e.path().is_dir())
                .filter_map(|e| sanitize(&e.file_name().to_string_lossy()))
                .filter(|name| name != DEFAULT)
                .collect();
            found.sort();
            names.extend(found);
        }
    }
    names
}

/// Where this space's profile files live. The default space is the root itself.
pub fn dir_of(name: &str) -> Option<PathBuf> {
    let root = crate::storage::root_dir()?;
    match name == DEFAULT {
        true => Some(root),
        false => Some(root.join("spaces").join(name)),
    }
}

/// The colour that marks this space — the mark, the active tab, the settings
/// control that is switched on.
///
/// Derived from the name rather than stored: a space needs no configuration to
/// be recognisable, and the same name is always the same colour.
pub fn accent_of(name: &str) -> &'static str {
    if name == DEFAULT {
        return ACCENTS[0];
    }
    let hash = name.bytes().fold(0usize, |acc, b| acc.wrapping_mul(31).wrapping_add(b as usize));
    // The default's red is reserved, so other spaces are visibly not it.
    ACCENTS[1 + hash % (ACCENTS.len() - 1)]
}

/// A space name is a directory name: no separators, no surprises, and short
/// enough to fit the control that lists them.
fn sanitize(name: &str) -> Option<String> {
    let cleaned: String = name
        .trim()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_' || *c == ' ')
        .collect();
    let cleaned = cleaned.trim().to_lowercase();
    match cleaned.is_empty() || cleaned.chars().count() > 24 {
        true => None,
        false => Some(cleaned),
    }
}

/// The file naming the current space, in the root rather than in a space.
fn marker() -> Option<PathBuf> {
    Some(crate::storage::root_dir()?.join("space.txt"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_cleaned_or_refused() {
        assert_eq!(sanitize("Work"), Some("work".to_string()));
        assert_eq!(sanitize("  side project "), Some("side project".to_string()));
        // Path separators and dots cannot survive: a space name is a directory.
        assert_eq!(sanitize("../../etc"), Some("etc".to_string()));
        assert_eq!(sanitize("a/b"), Some("ab".to_string()));
        assert_eq!(sanitize(""), None);
        assert_eq!(sanitize("..."), None);
        assert_eq!(sanitize(&"x".repeat(25)), None);
    }

    #[test]
    fn each_space_gets_a_stable_accent_and_the_default_keeps_reds() {
        assert_eq!(accent_of(DEFAULT), ACCENTS[0]);
        assert_eq!(accent_of("work"), accent_of("work"));
        // No other space takes the default's colour, or the two would be
        // indistinguishable at a glance.
        for name in ["work", "study", "banking", "side project", "x"] {
            assert_ne!(accent_of(name), ACCENTS[0], "{name} looks like the default");
        }
    }
}

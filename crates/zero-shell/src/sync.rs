//! Sync: a profile, sealed into one file you can put anywhere.
//!
//! There is no server here, and deliberately so. A bundle is encrypted end to
//! end with a code that never leaves your hands, which makes "where does it
//! live" someone else's problem — a USB stick, a shared drive, whatever folder
//! your existing sync tool already watches. Self-hostable by having nothing to
//! host.
//!
//! The code *is* the key, printed once, rather than a passphrase run through a
//! derivation this file would have to get right. 32 random bytes beat any
//! password a person will type, and skipping the KDF skips the mistake.
//!
//! ponytail: a bundle is a snapshot, not a merge — importing replaces the
//! space's files rather than reconciling them. Merging history and bookmarks
//! per-record is the follow-up, and needs a record format that carries which
//! device wrote what.

use std::path::{Path, PathBuf};

/// The local encryption key is the one file that must never travel: it belongs
/// to this machine's keystore, and the other machine has its own.
const NEVER_EXPORT: &[&str] = &["profile.key"];

/// Pack this space's profile into `path`, returning the code needed to read it.
pub fn export(path: &Path) -> Result<String, String> {
    let dir = crate::storage::profile_dir().ok_or("no profile directory")?;
    let mut bundle = Vec::new();
    for entry in std::fs::read_dir(&dir).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if !entry.path().is_file() || NEVER_EXPORT.contains(&name.as_str()) {
            continue;
        }
        // Read through the at-rest layer: the other machine cannot undo this
        // machine's DPAPI or keystore, so what travels is the plain content.
        let Some(text) = crate::crypto::read_file(&entry.path()) else { continue };
        bundle.extend_from_slice(format!("{name}\n{}\n", text.len()).as_bytes());
        bundle.extend_from_slice(text.as_bytes());
        bundle.push(b'\n');
    }
    if bundle.is_empty() {
        return Err("nothing in this space to export".into());
    }
    let key = crate::crypto::fresh_key().ok_or("no source of randomness")?;
    let sealed = crate::crypto::seal_with(&key, &bundle).ok_or("could not encrypt")?;
    std::fs::write(path, sealed).map_err(|e| e.to_string())?;
    Ok(format_code(&key))
}

/// Unpack a bundle into this space, replacing the files it carries.
pub fn import(path: &Path, code: &str) -> Result<usize, String> {
    let key = parse_code(code).ok_or("that code is not 64 hex characters")?;
    let sealed = std::fs::read(path).map_err(|e| e.to_string())?;
    let bundle = crate::crypto::open_with(&key, &sealed)
        .ok_or("wrong code, or the bundle has been altered")?;
    let dir = crate::storage::profile_dir().ok_or("no profile directory")?;

    let mut rest = &bundle[..];
    let mut restored = 0;
    while !rest.is_empty() {
        let Some((name, after)) = split_line(rest) else { break };
        let Some((len, after)) = split_line(after) else { break };
        let Ok(len) = len.parse::<usize>() else { break };
        if after.len() < len {
            return Err("the bundle ends in the middle of a file".into());
        }
        let (body, after) = after.split_at(len);
        // A name from a file is untrusted input: it must land in this profile
        // and nowhere else.
        let name = Path::new(&name)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .ok_or("a bundle entry has no name")?;
        if !NEVER_EXPORT.contains(&name.as_str()) {
            crate::crypto::write_file(&dir.join(&name), &String::from_utf8_lossy(body));
            restored += 1;
        }
        rest = after.strip_prefix(b"\n").unwrap_or(after);
    }
    Ok(restored)
}

fn split_line(input: &[u8]) -> Option<(String, &[u8])> {
    let end = input.iter().position(|b| *b == b'\n')?;
    let line = String::from_utf8_lossy(&input[..end]).into_owned();
    Some((line, &input[end + 1..]))
}

/// The key as five-character groups, which is what makes it typeable at all.
fn format_code(key: &[u8; 32]) -> String {
    let hex: String = key.iter().map(|b| format!("{b:02x}")).collect();
    hex.as_bytes()
        .chunks(8)
        .map(|chunk| String::from_utf8_lossy(chunk).into_owned())
        .collect::<Vec<String>>()
        .join("-")
}

/// The inverse, forgiving about the grouping — nobody retypes dashes exactly.
fn parse_code(code: &str) -> Option<[u8; 32]> {
    let hex: String = code.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    if hex.len() != 64 {
        return None;
    }
    let bytes: Option<Vec<u8>> = (0..64)
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).ok())
        .collect();
    bytes?.try_into().ok()
}

/// Where a bundle goes when no path was given: beside the profile, so the
/// default case needs no argument at all.
pub fn default_path() -> PathBuf {
    crate::storage::profile_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("zero-sync.bundle")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_code_survives_being_written_down_and_typed_back() {
        let key = [0xab; 32];
        let code = format_code(&key);
        // Grouped for a human, and read back however they space it out.
        assert!(code.contains('-'));
        assert_eq!(parse_code(&code), Some(key));
        assert_eq!(parse_code(&code.replace('-', " ")), Some(key));
        assert_eq!(parse_code(&code.to_uppercase()), Some(key));
        // Anything that is not a whole key is refused rather than padded.
        assert_eq!(parse_code("abc"), None);
        assert_eq!(parse_code(&code[..code.len() - 1]), None);
    }

    #[test]
    fn a_bundle_round_trips_through_the_cipher() {
        // The container format, exercised without touching a real profile.
        let key = [9u8; 32];
        let mut bundle = Vec::new();
        for (name, body) in [("settings.tsv", "zoom=125\n"), ("bookmarks.tsv", "a\tb\n")] {
            bundle.extend_from_slice(format!("{name}\n{}\n", body.len()).as_bytes());
            bundle.extend_from_slice(body.as_bytes());
            bundle.push(b'\n');
        }
        let sealed = crate::crypto::seal_with(&key, &bundle).expect("sealed");
        assert_eq!(crate::crypto::open_with(&key, &sealed).as_deref(), Some(&bundle[..]));
        // The wrong code reads nothing at all, rather than half a profile.
        assert_eq!(crate::crypto::open_with(&[8u8; 32], &sealed), None);
    }
}

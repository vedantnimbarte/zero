//! Encryption at rest for profile data, as required by
//! docs/04-SECURITY-PRIVACY.md §5.3.
//!
//! Rather than inventing key management, this defers to the operating system's
//! data-protection facility, which ties the ciphertext to the logged-in user
//! account. On Windows that is DPAPI (`CryptProtectData`), the same mechanism
//! mainstream browsers use to protect their cookie keys.
//!
//! Files are self-describing: encrypted ones start with [`MAGIC`], so a profile
//! written before encryption existed still loads, and a profile that cannot be
//! decrypted (different user, corrupt file) fails closed to "no data" rather than
//! returning garbage.
//!
//! ponytail: only Windows has a backend. On other platforms [`protect`] is a
//! pass-through and data stays in the clear — [`is_available`] reports that
//! honestly instead of pretending otherwise. macOS Keychain and Linux Secret
//! Service are the follow-ups.

/// Marks a file as ciphertext. Chosen to be invalid UTF-8 so it can never
/// collide with the plaintext TSV formats.
const MAGIC: &[u8] = b"ZEROENC\x00";

/// Whether stored data is actually encrypted on this platform.
pub fn is_available() -> bool {
    cfg!(windows)
}

/// Encrypt `data` for storage. Falls back to the input unchanged where the
/// platform has no backend, so callers never have to branch.
pub fn protect(data: &[u8]) -> Vec<u8> {
    match backend::encrypt(data) {
        Some(cipher) => {
            let mut out = MAGIC.to_vec();
            out.extend_from_slice(&cipher);
            out
        }
        None => data.to_vec(),
    }
}

/// Decrypt data written by [`protect`].
///
/// Plaintext (pre-encryption) files pass through, so existing profiles keep
/// working. Ciphertext we cannot decrypt yields `None` — better to lose history
/// than to hand back rubbish.
pub fn unprotect(data: &[u8]) -> Option<Vec<u8>> {
    match data.strip_prefix(MAGIC) {
        Some(cipher) => backend::decrypt(cipher),
        None => Some(data.to_vec()),
    }
}

/// Read a profile file, decrypting it if needed.
pub fn read_file(path: &std::path::Path) -> Option<String> {
    let raw = std::fs::read(path).ok()?;
    let plain = unprotect(&raw)?;
    String::from_utf8(plain).ok()
}

/// Write a profile file, encrypting it where the platform allows.
pub fn write_file(path: &std::path::Path, contents: &str) {
    let _ = std::fs::write(path, protect(contents.as_bytes()));
}

#[cfg(windows)]
mod backend {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{
        CryptProtectData, CryptUnprotectData, CRYPTPROTECT_PROMPTSTRUCT, CRYPT_INTEGER_BLOB,
    };

    /// DPAPI wants a mutable pointer even though it only reads the input.
    fn blob_of(buffer: &mut [u8]) -> CRYPT_INTEGER_BLOB {
        CRYPT_INTEGER_BLOB { cbData: buffer.len() as u32, pbData: buffer.as_mut_ptr() }
    }

    /// Copy an output blob into owned memory and release what Windows allocated.
    fn take(blob: CRYPT_INTEGER_BLOB) -> Option<Vec<u8>> {
        if blob.pbData.is_null() {
            return None;
        }
        // SAFETY: on success DPAPI guarantees `pbData` points to `cbData` bytes,
        // which we copy before freeing the buffer exactly once.
        unsafe {
            let owned = core::slice::from_raw_parts(blob.pbData, blob.cbData as usize).to_vec();
            LocalFree(blob.pbData as _);
            Some(owned)
        }
    }

    // The two entry points take different argument types — `CryptUnprotectData`
    // returns the description string rather than accepting one — so they cannot
    // share a single wrapper.
    pub fn encrypt(data: &[u8]) -> Option<Vec<u8>> {
        let mut input = data.to_vec();
        let blob_in = blob_of(&mut input);
        let mut blob_out = CRYPT_INTEGER_BLOB { cbData: 0, pbData: core::ptr::null_mut() };
        // SAFETY: the input blob outlives the call; the output is taken below.
        let ok = unsafe {
            CryptProtectData(
                &blob_in,
                core::ptr::null(),
                core::ptr::null(),
                core::ptr::null(),
                core::ptr::null::<CRYPTPROTECT_PROMPTSTRUCT>(),
                0,
                &mut blob_out,
            )
        };
        (ok != 0).then(|| take(blob_out)).flatten()
    }

    pub fn decrypt(data: &[u8]) -> Option<Vec<u8>> {
        let mut input = data.to_vec();
        let blob_in = blob_of(&mut input);
        let mut blob_out = CRYPT_INTEGER_BLOB { cbData: 0, pbData: core::ptr::null_mut() };
        // SAFETY: as above; the description out-parameter is left null since we
        // never set one when encrypting.
        let ok = unsafe {
            CryptUnprotectData(
                &blob_in,
                core::ptr::null_mut(),
                core::ptr::null(),
                core::ptr::null(),
                core::ptr::null::<CRYPTPROTECT_PROMPTSTRUCT>(),
                0,
                &mut blob_out,
            )
        };
        (ok != 0).then(|| take(blob_out)).flatten()
    }
}

#[cfg(not(windows))]
mod backend {
    /// No data-protection backend here yet, so nothing is encrypted.
    pub fn encrypt(_data: &[u8]) -> Option<Vec<u8>> {
        None
    }

    pub fn decrypt(_data: &[u8]) -> Option<Vec<u8>> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protect_round_trips() {
        let secret = b"sid=super-secret-session-token";
        let stored = protect(secret);
        if is_available() {
            assert!(stored.starts_with(MAGIC), "stored data must be tagged");
            assert!(
                !stored.windows(secret.len()).any(|w| w == secret),
                "the plaintext must not survive in the file"
            );
        }
        assert_eq!(unprotect(&stored).as_deref(), Some(&secret[..]));
    }

    #[test]
    fn plaintext_profiles_still_load() {
        // A file written before encryption existed has no magic and passes through.
        let legacy = b"github.com\tgithub.com\t/\t_octo\tvalue\t1\t99\n";
        assert_eq!(unprotect(legacy).as_deref(), Some(&legacy[..]));
    }

    #[test]
    fn undecryptable_data_fails_closed() {
        if !is_available() {
            return; // without a backend there is nothing to fail on
        }
        let mut corrupt = MAGIC.to_vec();
        corrupt.extend_from_slice(b"not actually ciphertext");
        assert_eq!(unprotect(&corrupt), None, "must not return garbage as plaintext");
    }

    #[test]
    fn file_round_trip() {
        let dir = std::env::temp_dir().join("zero-crypto-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        let path = dir.join("secrets.tsv");

        write_file(&path, "a\tb\n");
        assert_eq!(read_file(&path).as_deref(), Some("a\tb\n"));
        if is_available() {
            let raw = std::fs::read(&path).expect("file");
            assert!(raw.starts_with(MAGIC));
        }
    }
}

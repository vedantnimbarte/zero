//! Encryption at rest for profile data, as required by
//! docs/04-SECURITY-PRIVACY.md §5.3.
//!
//! Key management defers to the operating system rather than being invented
//! here, and the cipher is a vetted implementation rather than a hand-rolled
//! one — the two mistakes this file exists to avoid.
//!
//! * **Windows** — DPAPI (`CryptProtectData`) protects the bytes directly, tied
//!   to the logged-in account. No key ever reaches this process.
//! * **macOS / Linux** — AES-256-GCM under a key the OS keystore holds
//!   (`security` on macOS, `secret-tool` on Linux). Neither OS offers DPAPI's
//!   protect-a-blob call, so this is the arrangement mainstream browsers use
//!   there too.
//!
//! Where no keystore answers, the key falls back to a file in the profile
//! directory readable only by its owner. That guards the profile from other
//! accounts on the machine, and not from someone who already has this one — so
//! [`key_origin`] says where the key came from rather than leaving it implied.
//!
//! Files are self-describing: encrypted ones start with [`MAGIC`], so a profile
//! written before encryption existed still loads, and a profile that cannot be
//! decrypted (different user, corrupt file) fails closed to "no data" rather than
//! returning garbage.

/// Marks a file as ciphertext. Chosen to be invalid UTF-8 so it can never
/// collide with the plaintext TSV formats.
const MAGIC: &[u8] = b"ZEROENC\x00";

/// Whether stored data is actually encrypted on this platform.
pub fn is_available() -> bool {
    backend::encrypt(b"probe").is_some()
}

/// Where the encryption key lives, so the settings page can be honest about it.
pub fn key_origin() -> &'static str {
    backend::ORIGIN
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
    pub const ORIGIN: &str = "Windows DPAPI";

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
    //! AES-256-GCM under a key from the OS keystore. macOS and Linux have no
    //! equivalent of DPAPI's "protect these bytes for this user" call, so the
    //! key is stored there and the cipher runs here.
    use super::cipher;
    use std::process::{Command, Stdio};

    pub const ORIGIN: &str = if cfg!(target_os = "macos") {
        "macOS Keychain"
    } else {
        "Secret Service, else a file only you can read"
    };

    pub fn encrypt(data: &[u8]) -> Option<Vec<u8>> {
        cipher::seal(&key()?, data)
    }

    pub fn decrypt(data: &[u8]) -> Option<Vec<u8>> {
        cipher::open(&key()?, data)
    }

    /// The profile key, created on first use and cached for the process: every
    /// read of every profile file would otherwise shell out to the keystore.
    fn key() -> Option<[u8; 32]> {
        thread_local! {
            static KEY: Option<[u8; 32]> = load_or_create();
        }
        KEY.with(|k| *k)
    }

    fn load_or_create() -> Option<[u8; 32]> {
        if let Some(key) = from_keystore().or_else(from_file) {
            return Some(key);
        }
        let fresh = random_bytes()?;
        // A key we cannot store is worse than no key at all: the next launch
        // would make another one and every profile file would read as corrupt.
        (to_keystore(&fresh) || to_file(&fresh)).then_some(fresh)
    }

    fn hex(key: &[u8; 32]) -> String {
        key.iter().map(|b| format!("{b:02x}")).collect()
    }

    fn unhex(text: &str) -> Option<[u8; 32]> {
        let text = text.trim();
        let bytes: Option<Vec<u8>> = (0..text.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok())
            .collect();
        bytes?.try_into().ok()
    }

    /// 32 bytes from the kernel's pool. Every unix has `/dev/urandom`, and a
    /// dependency for it would only wrap the same file.
    fn random_bytes() -> Option<[u8; 32]> {
        use std::io::Read;
        let mut bytes = [0u8; 32];
        std::fs::File::open("/dev/urandom")
            .ok()?
            .read_exact(&mut bytes)
            .ok()
            .map(|_| bytes)
    }

    const SERVICE: &str = "zero-browser";
    const ACCOUNT: &str = "profile-key";

    fn from_keystore() -> Option<[u8; 32]> {
        let out = match cfg!(target_os = "macos") {
            true => Command::new("security")
                .args(["find-generic-password", "-s", SERVICE, "-a", ACCOUNT, "-w"])
                .output()
                .ok()?,
            false => Command::new("secret-tool")
                .args(["lookup", "service", SERVICE, "account", ACCOUNT])
                .output()
                .ok()?,
        };
        out.status
            .success()
            .then(|| unhex(&String::from_utf8_lossy(&out.stdout)))
            .flatten()
    }

    fn to_keystore(key: &[u8; 32]) -> bool {
        let hex = hex(key);
        if cfg!(target_os = "macos") {
            return Command::new("security")
                .args(["add-generic-password", "-U", "-s", SERVICE, "-a", ACCOUNT, "-w", &hex])
                .status()
                .map(|status| status.success())
                .unwrap_or(false);
        }
        // secret-tool takes the secret on stdin, so it never appears in `ps`.
        let Ok(mut child) = Command::new("secret-tool")
            .args(["store", "--label=Zero Browser", "service", SERVICE, "account", ACCOUNT])
            .stdin(Stdio::piped())
            .spawn()
        else {
            return false;
        };
        if let Some(stdin) = child.stdin.as_mut() {
            use std::io::Write;
            let _ = stdin.write_all(hex.as_bytes());
        }
        child.wait().map(|status| status.success()).unwrap_or(false)
    }

    fn key_path() -> Option<std::path::PathBuf> {
        Some(crate::storage::profile_dir()?.join("profile.key"))
    }

    fn from_file() -> Option<[u8; 32]> {
        unhex(&std::fs::read_to_string(key_path()?).ok()?)
    }

    fn to_file(key: &[u8; 32]) -> bool {
        let Some(path) = key_path() else { return false };
        if std::fs::write(&path, hex(key)).is_err() {
            return false;
        }
        // Owner-only, or the fallback protects nothing on a shared machine.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }
        true
    }
}

/// AES-256-GCM, from the `aes-gcm` crate.
///
/// Compiled on every platform even though Windows seals with DPAPI instead: a
/// cipher only exercised on machines the author cannot test on is a cipher
/// nobody has tested.
#[cfg_attr(windows, allow(dead_code))] // DPAPI seals there; only the tests seal here
mod cipher {
    use aes_gcm::aead::{Aead, KeyInit};
    use aes_gcm::{Aes256Gcm, Nonce};

    /// GCM is safe only while a nonce is never reused under one key, so every
    /// message gets a fresh random one and carries it in front of the ciphertext.
    const NONCE_LEN: usize = 12;

    pub fn seal(key: &[u8; 32], data: &[u8]) -> Option<Vec<u8>> {
        let nonce = nonce()?;
        let sealed = Aes256Gcm::new(&(*key).into())
            .encrypt(&Nonce::from(nonce), data)
            .ok()?;
        Some(nonce.iter().copied().chain(sealed).collect())
    }

    pub fn open(key: &[u8; 32], data: &[u8]) -> Option<Vec<u8>> {
        let (nonce, sealed) = data.split_at_checked(NONCE_LEN)?;
        let nonce: [u8; NONCE_LEN] = nonce.try_into().ok()?;
        // Failure here is the authentication tag failing: wrong key, or tampering.
        Aes256Gcm::new(&(*key).into())
            .decrypt(&Nonce::from(nonce), sealed)
            .ok()
    }

    #[cfg(unix)]
    fn nonce() -> Option<[u8; NONCE_LEN]> {
        use std::io::Read;
        let mut bytes = [0u8; NONCE_LEN];
        std::fs::File::open("/dev/urandom")
            .ok()?
            .read_exact(&mut bytes)
            .ok()
            .map(|_| bytes)
    }

    /// Windows never seals with this cipher, but its tests do, and they need a
    /// nonce like any other caller.
    #[cfg(windows)]
    fn nonce() -> Option<[u8; NONCE_LEN]> {
        use windows_sys::Win32::Security::Cryptography::{
            BCryptGenRandom, BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        };
        let mut bytes = [0u8; NONCE_LEN];
        // SAFETY: the buffer is exactly the length passed, and the call only
        // writes into it.
        let status = unsafe {
            BCryptGenRandom(
                std::ptr::null_mut(),
                bytes.as_mut_ptr(),
                bytes.len() as u32,
                BCRYPT_USE_SYSTEM_PREFERRED_RNG,
            )
        };
        (status == 0).then_some(bytes)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn sealed_data_round_trips_and_refuses_to_be_altered() {
            let key = [7u8; 32];
            let secret = b"sid=super-secret-session-token";
            let sealed = seal(&key, secret).expect("sealed");
            assert_ne!(&sealed[NONCE_LEN..], &secret[..], "plaintext must not survive");
            assert_eq!(open(&key, &sealed).as_deref(), Some(&secret[..]));

            // A different key cannot read it.
            assert_eq!(open(&[8u8; 32], &sealed), None);
            // Nor can a flipped bit be read as if nothing happened: that is what
            // the authentication tag is for.
            let mut tampered = sealed.clone();
            *tampered.last_mut().expect("non-empty") ^= 1;
            assert_eq!(open(&key, &tampered), None);
            let mut moved = sealed.clone();
            moved[NONCE_LEN] ^= 1;
            assert_eq!(open(&key, &moved), None);
            // Truncation is not a decryptable message either.
            assert_eq!(open(&key, &sealed[..4]), None);
        }

        #[test]
        fn every_message_gets_its_own_nonce() {
            // Two seals of the same plaintext under the same key must differ, or
            // GCM's security argument is gone.
            let key = [3u8; 32];
            let a = seal(&key, b"same").expect("sealed");
            let b = seal(&key, b"same").expect("sealed");
            assert_ne!(a, b);
            assert_eq!(open(&key, &a), open(&key, &b));
        }
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

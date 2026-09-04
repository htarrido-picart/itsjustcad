// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Transparent, keychain-backed AES-256-GCM encryption for the app-local chat
//! store (`chat_store.rs`). Chat sessions are PRIVATE and live beside — not
//! inside — the shared document, so they should not sit on disk in plaintext.
//!
//! Design:
//! * **Key**: a single random 256-bit data key kept in the OS keychain (macOS
//!   Keychain / Windows Credential Manager / Linux Secret Service) via the
//!   `keyring` crate. Generated on first use, fetched thereafter. The key never
//!   touches disk in plaintext. The key source is a [`KeyStore`] trait so tests
//!   inject an in-memory key and NEVER hit the real OS keychain.
//! * **Format**: `magic(4) ‖ version(1) ‖ nonce(12) ‖ ciphertext‖tag`. A fresh
//!   random 96-bit nonce per write; GCM authenticates everything. Plaintext
//!   stores written before this feature (or when no keychain is available) start
//!   with `{` (JSON), which never collides with the magic, so [`open`] tells the
//!   two apart on read and old sessions load + transparently upgrade on save.
//! * **Fallback**: if the keychain is unavailable (headless CI, Linux without a
//!   Secret Service, any keyring error) we fall back to PLAINTEXT rather than
//!   lose data or hard-fail — with a clear one-time log note. A file marker lets
//!   old plaintext load; a healthy keychain re-encrypts on the next save.
//!
//! This is deliberately independent of the crash-recovery journal / op-log:
//! chat sessions are not part of deterministic replay, so a random nonce per
//! write is fine.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

use aes_gcm::{
    aead::{generic_array::GenericArray, Aead, AeadCore, KeyInit, OsRng},
    Aes256Gcm, Key,
};

/// File magic marking an encrypted chat store. Chosen so it never collides with
/// a plaintext JSON store (which begins with `{` / whitespace).
const MAGIC: &[u8; 4] = b"ICEC"; // "ItsjustCad Encrypted Chat"
const VERSION: u8 = 1;
const NONCE_LEN: usize = 12; // 96-bit GCM nonce
const KEY_LEN: usize = 32; // 256-bit key
const HEADER_LEN: usize = 4 + 1 + NONCE_LEN;

/// keyring identifiers for the single per-user data key.
const KEYRING_SERVICE: &str = "itsjustcad";
const KEYRING_USER: &str = "chat-store-key-v1";

/// Errors from the crypto layer. Callers treat every variant as "could not
/// decrypt" and fall back / surface an error — never panic.
#[derive(Debug)]
pub enum CryptoError {
    /// The keychain was reachable but the key was malformed (wrong length /
    /// bad base64). Effectively a missing key.
    BadKey,
    /// AEAD open failed: wrong key, truncated, or tampered ciphertext.
    Decrypt,
    /// The blob was too short / not a recognised encrypted container.
    BadFormat,
}

impl std::fmt::Display for CryptoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CryptoError::BadKey => write!(f, "chat key malformed"),
            CryptoError::Decrypt => write!(f, "chat decrypt failed (wrong key or corrupted)"),
            CryptoError::BadFormat => write!(f, "not an encrypted chat container"),
        }
    }
}
impl std::error::Error for CryptoError {}

/// A source of the 256-bit data key. Abstracted so unit tests inject an
/// in-memory key and the real OS keychain is only touched at runtime.
pub trait KeyStore {
    /// Fetch the existing key, generating + persisting a fresh random one on
    /// first use. `Err` means the store is unavailable (fall back to plaintext).
    fn get_or_create_key(&self) -> Result<[u8; KEY_LEN], ()>;
}

/// The real OS-keychain-backed store used at runtime.
pub struct OsKeyStore;

/// Process-wide cache of the resolved data key. The keychain is consulted at
/// most ONCE per app run: macOS ties a keychain item's access ACL to the exact
/// signed binary, so an unsigned/ad-hoc build re-prompts on every access — and
/// `seal_for_write`/`open` run on every chat save/load. Caching collapses that
/// to a single prompt per launch. `Some(key)` = available; `None` = keychain
/// unavailable this run (fall back to plaintext without retrying + re-prompting).
static KEY_CACHE: OnceLock<Option<[u8; KEY_LEN]>> = OnceLock::new();

impl KeyStore for OsKeyStore {
    fn get_or_create_key(&self) -> Result<[u8; KEY_LEN], ()> {
        (*KEY_CACHE.get_or_init(|| Self::fetch_key().ok())).ok_or(())
    }
}

impl OsKeyStore {
    /// Uncached keychain fetch: read the per-user key, minting + storing one on
    /// first run. Called at most once via [`KEY_CACHE`].
    fn fetch_key() -> Result<[u8; KEY_LEN], ()> {
        let entry = keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER).map_err(|_| ())?;
        match entry.get_secret() {
            Ok(bytes) if bytes.len() == KEY_LEN => {
                let mut key = [0u8; KEY_LEN];
                key.copy_from_slice(&bytes);
                Ok(key)
            }
            Ok(_) => Err(()), // corrupt/wrong-length secret: treat as unavailable
            Err(keyring::Error::NoEntry) => {
                // First run: mint a random key and store it.
                let key = Aes256Gcm::generate_key(OsRng);
                entry.set_secret(key.as_slice()).map_err(|_| ())?;
                let mut out = [0u8; KEY_LEN];
                out.copy_from_slice(key.as_slice());
                Ok(out)
            }
            Err(_) => Err(()), // Secret Service down, locked keychain, etc.
        }
    }
}

/// One-time "encryption unavailable" warning latch so the log isn't spammed on
/// every save when there is no keychain (headless / CI / bare Linux).
static WARNED_PLAINTEXT: AtomicBool = AtomicBool::new(false);

fn warn_plaintext_once() {
    if !WARNED_PLAINTEXT.swap(true, Ordering::Relaxed) {
        tracing::warn!(
            "chat encryption unavailable (no OS keychain) — chat sessions stored in plaintext"
        );
    }
}

/// Whether `blob` is one of our encrypted containers (magic + version match).
pub fn is_encrypted(blob: &[u8]) -> bool {
    blob.len() >= HEADER_LEN && &blob[..4] == MAGIC && blob[4] == VERSION
}

/// Encrypt `plaintext` under `key` into a self-describing container.
/// Nondeterministic: a fresh random nonce per call.
pub fn encrypt_bytes(key: &[u8; KEY_LEN], plaintext: &[u8]) -> Vec<u8> {
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));
    let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
    // Infallible for in-memory buffers of sane size.
    let ct = cipher
        .encrypt(&nonce, plaintext)
        .expect("aes-gcm encrypt of an in-memory buffer");
    let mut out = Vec::with_capacity(HEADER_LEN + ct.len());
    out.extend_from_slice(MAGIC);
    out.push(VERSION);
    out.extend_from_slice(nonce.as_slice());
    out.extend_from_slice(&ct);
    out
}

/// Decrypt a container produced by [`encrypt_bytes`]. Returns an error (never
/// panics) on a bad header, wrong key, or corrupted/truncated ciphertext.
pub fn decrypt_bytes(key: &[u8; KEY_LEN], blob: &[u8]) -> Result<Vec<u8>, CryptoError> {
    if !is_encrypted(blob) {
        return Err(CryptoError::BadFormat);
    }
    // The 96-bit GCM nonce sits between the version byte and the ciphertext.
    let nonce = GenericArray::from_slice(&blob[5..HEADER_LEN]);
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));
    cipher
        .decrypt(nonce, &blob[HEADER_LEN..])
        .map_err(|_| CryptoError::Decrypt)
}

/// Serialize for WRITE: encrypt when the key store yields a key, otherwise fall
/// back to the plaintext `json` bytes (with a one-time warning). The chosen
/// format is self-describing so [`open`] always reads it back.
pub fn seal_for_write<K: KeyStore>(store: &K, json: &[u8]) -> Vec<u8> {
    match store.get_or_create_key() {
        Ok(key) => encrypt_bytes(&key, json),
        Err(()) => {
            warn_plaintext_once();
            json.to_vec()
        }
    }
}

/// Parse for READ: if `blob` is an encrypted container, decrypt it (needs the
/// key); otherwise return it as-is (legacy/plaintext store). Returns the inner
/// JSON bytes. `Err` only on a genuinely corrupt encrypted container — a
/// plaintext blob always succeeds so old sessions never fail to load.
pub fn open<K: KeyStore>(store: &K, blob: &[u8]) -> Result<Vec<u8>, CryptoError> {
    if is_encrypted(blob) {
        let key = store.get_or_create_key().map_err(|_| CryptoError::BadKey)?;
        decrypt_bytes(&key, blob)
    } else {
        Ok(blob.to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// In-memory key store for tests — NEVER touches the OS keychain.
    struct MemKeyStore {
        key: [u8; KEY_LEN],
    }
    impl MemKeyStore {
        fn with(byte: u8) -> Self {
            Self { key: [byte; KEY_LEN] }
        }
    }
    impl KeyStore for MemKeyStore {
        fn get_or_create_key(&self) -> Result<[u8; KEY_LEN], ()> {
            Ok(self.key)
        }
    }

    /// A store that is always unavailable (simulates a missing keychain).
    struct AbsentKeyStore;
    impl KeyStore for AbsentKeyStore {
        fn get_or_create_key(&self) -> Result<[u8; KEY_LEN], ()> {
            Err(())
        }
    }

    #[test]
    fn round_trip_identical_plaintext() {
        let store = MemKeyStore::with(7);
        let msg = br#"{"doc_uuid":"x","sessions":[]}"#;
        let sealed = seal_for_write(&store, msg);
        assert!(is_encrypted(&sealed), "seal must produce an encrypted container");
        let opened = open(&store, &sealed).expect("open");
        assert_eq!(opened, msg, "round-trip must be byte-identical");
    }

    #[test]
    fn wrong_key_fails_cleanly() {
        let good = MemKeyStore::with(1);
        let bad = MemKeyStore::with(2);
        let sealed = seal_for_write(&good, b"secret chat");
        let err = open(&bad, &sealed).unwrap_err();
        assert!(matches!(err, CryptoError::Decrypt), "wrong key => auth error, not panic");
    }

    #[test]
    fn corrupted_ciphertext_errors_not_panics() {
        let store = MemKeyStore::with(3);
        let mut sealed = seal_for_write(&store, b"hello world");
        // Flip a byte in the ciphertext region (after the header).
        let last = sealed.len() - 1;
        sealed[last] ^= 0xFF;
        let err = open(&store, &sealed).unwrap_err();
        assert!(matches!(err, CryptoError::Decrypt));
        // Truncated container is a clean error too.
        let short = sealed[..HEADER_LEN - 1].to_vec();
        assert!(!is_encrypted(&short));
    }

    #[test]
    fn plaintext_fallback_when_key_store_absent() {
        let store = AbsentKeyStore;
        let msg = br#"{"doc_uuid":"y"}"#;
        let sealed = seal_for_write(&store, msg);
        assert!(!is_encrypted(&sealed), "no key => plaintext passthrough");
        assert_eq!(sealed, msg);
        // And it reads straight back (as plaintext) with any store.
        let opened = open(&store, &sealed).expect("plaintext open");
        assert_eq!(opened, msg);
    }

    #[test]
    fn old_plaintext_loads_and_upgrades_on_write() {
        // A legacy plaintext store on disk.
        let legacy = br#"{"doc_uuid":"z","sessions":[]}"#;
        let store = MemKeyStore::with(9);
        // Load path: plaintext passes through untouched.
        let loaded = open(&store, legacy).expect("legacy load");
        assert_eq!(loaded, legacy);
        // Next save with a healthy key upgrades it to an encrypted container.
        let resaved = seal_for_write(&store, &loaded);
        assert!(is_encrypted(&resaved), "healthy key upgrades plaintext to encrypted");
        assert_eq!(open(&store, &resaved).unwrap(), legacy);
    }

    #[test]
    fn nonce_is_unique_across_writes() {
        let store = MemKeyStore::with(5);
        let a = seal_for_write(&store, b"same message");
        let b = seal_for_write(&store, b"same message");
        // Same plaintext + same key, but different nonce => different bytes.
        assert_ne!(a, b, "each write must use a fresh nonce");
        let nonce_a = &a[5..HEADER_LEN];
        let nonce_b = &b[5..HEADER_LEN];
        assert_ne!(nonce_a, nonce_b, "nonces must differ across writes");
        // Both still decrypt to the same plaintext.
        assert_eq!(open(&store, &a).unwrap(), open(&store, &b).unwrap());
    }

    #[test]
    fn is_encrypted_rejects_plaintext_and_short_blobs() {
        assert!(!is_encrypted(b""));
        assert!(!is_encrypted(b"{"));
        assert!(!is_encrypted(b"ICE")); // too short, no version/nonce
        assert!(!is_encrypted(br#"{"doc_uuid":"a"}"#));
    }

    #[test]
    fn empty_plaintext_round_trips() {
        let store = MemKeyStore::with(4);
        let sealed = seal_for_write(&store, b"");
        assert!(is_encrypted(&sealed));
        assert_eq!(open(&store, &sealed).unwrap(), b"");
    }
}

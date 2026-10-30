//! Keys, text encodings and the signed-message format (docs/architecture.md, "Reaching a
//! server"), for both sides: the server re-exports this as `flicks_server::keys`.
//!
//! Public keys are SEC1 compressed P-256 points (33 bytes), signatures are `r ‖ s` (64 bytes), and
//! both travel as unpadded base64url. Every signed message starts with its own `flicks-…-v1\0`
//! label, and every variable-length field in it is prefixed with its length as a big-endian u32.

use std::fs::OpenOptions;
use std::io::{ErrorKind, Read, Write};
use std::path::Path;

use data_encoding::BASE64URL_NOPAD;
use p256::ecdsa::signature::{Signer, Verifier};
use p256::ecdsa::{Signature, SigningKey, VerifyingKey};

use crate::Error;

/// Length of a compressed public key.
pub const PUBLIC_KEY_LEN: usize = 33;

/// Unpadded base64url, the text form of every key, code, nonce and signature.
pub fn encode(bytes: &[u8]) -> String {
    BASE64URL_NOPAD.encode(bytes)
}

/// Decodes unpadded base64url, refusing input longer than `max` decoded bytes.
pub fn decode(text: &str, max: usize) -> Option<Vec<u8>> {
    let len = BASE64URL_NOPAD.decode_len(text.len()).ok()?;
    if len > max {
        return None;
    }
    BASE64URL_NOPAD.decode(text.as_bytes()).ok()
}

/// The 33-byte compressed form of a public key.
pub fn public_bytes(key: &VerifyingKey) -> Vec<u8> {
    key.to_sec1_point(true).as_bytes().to_vec()
}

/// Parses a 33-byte compressed public key.
pub fn parse_public(bytes: &[u8]) -> Option<VerifyingKey> {
    if bytes.len() != PUBLIC_KEY_LEN {
        return None;
    }
    VerifyingKey::from_sec1_bytes(bytes).ok()
}

/// Signs `message` and returns the 64-byte `r ‖ s` signature.
pub fn sign(key: &SigningKey, message: &[u8]) -> [u8; 64] {
    let signature: Signature = key.sign(message);
    signature.to_bytes().into()
}

/// Checks a 64-byte `r ‖ s` signature.
pub fn verify(key: &VerifyingKey, message: &[u8], signature: &[u8]) -> bool {
    match Signature::from_slice(signature) {
        Ok(signature) => key.verify(message, &signature).is_ok(),
        Err(_) => false,
    }
}

/// Fills a buffer from the OS random source.
pub fn random<const N: usize>() -> Result<[u8; N], Error> {
    let mut bytes = [0u8; N];
    getrandom::fill(&mut bytes).map_err(|_| Error::Random)?;
    Ok(bytes)
}

/// Makes a new random key.
pub fn generate() -> Result<SigningKey, Error> {
    // A random 32-byte string is a valid scalar except with probability ~2^-128.
    for _ in 0..8 {
        if let Ok(key) = SigningKey::from_slice(&random::<32>()?) {
            return Ok(key);
        }
    }
    Err(Error::Random)
}

/// Loads a key file (the raw 32-byte scalar).
pub fn load(path: &Path) -> Result<SigningKey, Error> {
    let bad = || Error::KeyFile(path.to_path_buf());
    let mut bytes = Vec::with_capacity(32);
    std::fs::File::open(path)?.take(33).read_to_end(&mut bytes)?;
    if bytes.len() != 32 {
        return Err(bad());
    }
    SigningKey::from_slice(&bytes).map_err(|_| bad())
}

/// Loads a key file, creating it with mode 0600 (owner only) if it doesn't exist.
pub fn load_or_create(path: &Path) -> Result<SigningKey, Error> {
    match load(path) {
        Err(Error::Io(e)) if e.kind() == ErrorKind::NotFound => {
            let key = generate()?;
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
            let mut file = options.open(path)?;
            file.write_all(&key.to_bytes())?;
            file.sync_all()?;
            Ok(key)
        }
        other => other,
    }
}

/// A signed message: a label, then fields.
pub struct Message(Vec<u8>);

impl Message {
    /// Starts a message with `label` and its terminating NUL.
    pub fn new(label: &str) -> Self {
        let mut bytes = Vec::with_capacity(256);
        bytes.extend_from_slice(label.as_bytes());
        bytes.push(0);
        Message(bytes)
    }

    /// Appends a length-prefixed field.
    pub fn field(mut self, bytes: &[u8]) -> Self {
        // Every field is bounded by HTTP header and body limits far below 4 GiB.
        let len = u32::try_from(bytes.len()).unwrap_or(u32::MAX);
        self.0.extend_from_slice(&len.to_be_bytes());
        self.0.extend_from_slice(bytes);
        self
    }

    /// Appends a fixed-length field (a hash or an integer) as is.
    pub fn fixed(mut self, bytes: &[u8]) -> Self {
        self.0.extend_from_slice(bytes);
        self
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.0
    }
}

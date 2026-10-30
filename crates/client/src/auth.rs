//! Signed requests (docs/architecture.md, "Every request"): the message and its signature, made
//! here for clients and the local CLI and checked by `flicks_server::auth`.
//!
//! `Authorization: Flicks <device key> <timestamp> <nonce> <signature>`: the device's compressed
//! public key, Unix seconds in decimal, a 16-byte random nonce and the `r ‖ s` signature, each
//! binary field unpadded base64url. The signature covers
//!
//! ```text
//! "flicks-req-v1\0" ‖ lp(server key) ‖ lp(method) ‖ lp(path and query) ‖ lp(Range)
//!   ‖ SHA-256(body) ‖ timestamp (u64 BE) ‖ nonce (16 bytes)
//! ```
//!
//! where `lp(x)` is `x` behind its length as a big-endian u32, and Range is the header's value or
//! empty.

use p256::ecdsa::SigningKey;
use sha2::{Digest, Sha256};

use crate::{Error, keys};

pub const LABEL: &str = "flicks-req-v1";

pub const NONCE_LEN: usize = 16;

/// The bytes a request's signature covers.
pub fn message(
    server_key: &[u8],
    method: &str,
    path_and_query: &str,
    range: &[u8],
    body: &[u8],
    timestamp: u64,
    nonce: &[u8; NONCE_LEN],
) -> Vec<u8> {
    keys::Message::new(LABEL)
        .field(server_key)
        .field(method.as_bytes())
        .field(path_and_query.as_bytes())
        .field(range)
        .fixed(&Sha256::digest(body))
        .fixed(&timestamp.to_be_bytes())
        .fixed(nonce)
        .into_bytes()
}

/// The `Authorization` header value for a request signed by `device` at `timestamp`.
pub fn sign(
    device: &SigningKey,
    server_key: &[u8],
    method: &str,
    path_and_query: &str,
    range: &[u8],
    body: &[u8],
    timestamp: u64,
) -> Result<String, Error> {
    let nonce = keys::random::<NONCE_LEN>()?;
    let message = message(server_key, method, path_and_query, range, body, timestamp, &nonce);
    Ok(header(&keys::public_bytes(device.verifying_key()), timestamp, &nonce, &keys::sign(device, &message)))
}

/// The `Authorization` header value from its parts: the device's compressed public key, and the
/// signature over [`message`] (made elsewhere by the web app, whose key lives in WebCrypto).
pub fn header(device_key: &[u8], timestamp: u64, nonce: &[u8; NONCE_LEN], signature: &[u8]) -> String {
    format!("Flicks {} {timestamp} {} {}", keys::encode(device_key), keys::encode(nonce), keys::encode(signature))
}

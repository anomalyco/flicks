//! Signing requests exactly as the server checks them (docs/architecture.md, "Every request";
//! the server's side is `flicks_server::auth`).
//!
//! `Authorization: Flicks <device key> <timestamp> <nonce> <signature>`, signing
//!
//! ```text
//! "flicks-req-v1\0" ‖ lp(server key) ‖ lp(method) ‖ lp(path and query) ‖ lp(Range)
//!   ‖ SHA-256(body) ‖ timestamp (u64 BE) ‖ nonce (16 bytes)
//! ```

use p256::ecdsa::SigningKey;
use sha2::{Digest, Sha256};

use crate::{Error, keys};

/// The `Authorization` header value for a request signed by `device` at `timestamp`.
pub fn authorization(
    device: &SigningKey,
    server_key: &[u8],
    method: &str,
    path_and_query: &str,
    range: &str,
    body: &[u8],
    timestamp: u64,
) -> Result<String, Error> {
    let nonce = keys::random::<16>()?;
    let message = keys::Message::new("flicks-req-v1")
        .field(server_key)
        .field(method.as_bytes())
        .field(path_and_query.as_bytes())
        .field(range.as_bytes())
        .fixed(&Sha256::digest(body))
        .fixed(&timestamp.to_be_bytes())
        .fixed(&nonce)
        .into_bytes();
    Ok(format!(
        "Flicks {} {timestamp} {} {}",
        keys::encode(&keys::public_bytes(device.verifying_key())),
        keys::encode(&nonce),
        keys::encode(&keys::sign(device, &message)),
    ))
}

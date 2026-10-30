//! Signed requests (docs/architecture.md, "Every request").
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

use std::collections::{HashSet, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use p256::ecdsa::SigningKey;
use sha2::{Digest, Sha256};

use crate::{Error, keys};

pub const LABEL: &str = "flicks-req-v1";

/// How far a request's timestamp may be from the server's clock, in seconds.
pub const MAX_SKEW: u64 = 60;

/// The largest body a signed request may carry.
pub const MAX_BODY: usize = 64 * 1024;

pub const NONCE_LEN: usize = 16;
pub const SIGNATURE_LEN: usize = 64;

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
    let signature = keys::sign(device, &message);
    Ok(format!(
        "Flicks {} {timestamp} {} {}",
        keys::encode(&keys::public_bytes(device.verifying_key())),
        keys::encode(&nonce),
        keys::encode(&signature),
    ))
}

/// A parsed `Authorization` header. Nothing in it is checked yet.
#[derive(Debug, PartialEq, Eq)]
pub struct Header {
    pub key: Vec<u8>,
    pub timestamp: u64,
    pub nonce: [u8; NONCE_LEN],
    pub signature: Vec<u8>,
}

/// Parses an `Authorization` header value, refusing anything malformed or oversized.
pub fn parse(value: &[u8]) -> Option<Header> {
    if value.len() > 256 {
        return None;
    }
    let text = std::str::from_utf8(value).ok()?;
    let mut parts = text.split(' ');
    let (Some("Flicks"), Some(key), Some(timestamp), Some(nonce), Some(signature), None) =
        (parts.next(), parts.next(), parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return None;
    };
    let key = keys::decode(key, keys::PUBLIC_KEY_LEN)?;
    if key.len() != keys::PUBLIC_KEY_LEN
        || timestamp.is_empty()
        || timestamp.len() > 20
        || !timestamp.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let signature = keys::decode(signature, SIGNATURE_LEN)?;
    if signature.len() != SIGNATURE_LEN {
        return None;
    }
    Some(Header {
        key,
        timestamp: timestamp.parse().ok()?,
        nonce: keys::decode(nonce, NONCE_LEN)?.try_into().ok()?,
        signature,
    })
}

/// Whether a timestamp is within [`MAX_SKEW`] of `now`.
pub fn fresh(timestamp: u64, now: u64) -> bool {
    timestamp.abs_diff(now) <= MAX_SKEW
}

/// How long a nonce is remembered. A request is only fresh for `MAX_SKEW` either side of the
/// server's clock, so twice that (plus a second) covers every request that could still pass.
const NONCE_TTL: Duration = Duration::from_secs(2 * MAX_SKEW + 1);

/// The most nonces remembered at once. When full, new requests are refused rather than letting
/// an unexpired nonce be forgotten (and so replayable).
pub const MAX_NONCES: usize = 65_536;

/// Why a nonce can't be used.
#[derive(Debug, PartialEq, Eq)]
pub enum NonceRefusal {
    /// It was used before.
    Replayed,
    /// The cache is full of unexpired nonces.
    Full,
}

/// Nonces of accepted requests, each forgotten [`NONCE_TTL`] after it was recorded (by the
/// monotonic clock, so a wall-clock jump can't expire them early).
#[derive(Default)]
pub struct Nonces(Mutex<NonceSet>);

#[derive(Default)]
struct NonceSet {
    seen: HashSet<[u8; NONCE_LEN]>,
    order: VecDeque<(Instant, [u8; NONCE_LEN])>,
}

impl NonceSet {
    fn check(&mut self, nonce: &[u8; NONCE_LEN], now: Instant) -> Result<(), NonceRefusal> {
        while let Some(&(at, old)) = self.order.front() {
            if now.saturating_duration_since(at) <= NONCE_TTL {
                break;
            }
            self.seen.remove(&old);
            self.order.pop_front();
        }
        if self.seen.contains(nonce) {
            return Err(NonceRefusal::Replayed);
        }
        if self.seen.len() >= MAX_NONCES {
            return Err(NonceRefusal::Full);
        }
        Ok(())
    }
}

impl Nonces {
    /// Whether a nonce could be recorded at `now`, without recording it.
    pub fn check(&self, nonce: &[u8; NONCE_LEN], now: Instant) -> Result<(), NonceRefusal> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).check(nonce, now)
    }

    /// Records a nonce at `now`.
    pub fn insert(&self, nonce: [u8; NONCE_LEN], now: Instant) -> Result<(), NonceRefusal> {
        let mut set = self.0.lock().unwrap_or_else(|e| e.into_inner());
        set.check(&nonce, now)?;
        set.seen.insert(nonce);
        set.order.push_back((now, nonce));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_round_trips_and_rejects_mutations() {
        let key = keys::generate().unwrap();
        let value = sign(&key, &[2; 33], "GET", "/x?y=1", b"", b"", 1_800_000_000).unwrap();
        let header = parse(value.as_bytes()).unwrap();
        assert_eq!(header.key, keys::public_bytes(key.verifying_key()));
        assert_eq!(header.timestamp, 1_800_000_000);
        // Truncations, byte flips and junk never panic; most are refused outright.
        for len in 0..value.len() {
            let _ = parse(&value.as_bytes()[..len]);
        }
        for i in 0..value.len() {
            for bit in 0..8 {
                let mut bytes = value.clone().into_bytes();
                bytes[i] ^= 1 << bit;
                if let Some(h) = parse(&bytes) {
                    assert_ne!(h, header);
                }
            }
        }
        assert!(parse(format!("{value} extra").as_bytes()).is_none());
        assert!(parse(&[b'a'; 4096]).is_none());
    }

    #[test]
    fn nonces_are_single_use_expire_and_fill_up() {
        let nonces = Nonces::default();
        let t = Instant::now();
        let at = |secs| t + Duration::from_secs(secs);
        assert_eq!(nonces.insert([1; 16], t), Ok(()));
        assert_eq!(nonces.check(&[1; 16], t), Err(NonceRefusal::Replayed));
        assert_eq!(nonces.insert([1; 16], at(100)), Err(NonceRefusal::Replayed));
        assert_eq!(nonces.insert([2; 16], at(121)), Ok(()));
        assert_eq!(nonces.check(&[1; 16], at(121)), Err(NonceRefusal::Replayed));
        assert_eq!(nonces.insert([3; 16], at(122)), Ok(()));
        assert_eq!(nonces.check(&[1; 16], at(122)), Ok(()));
        assert_eq!(nonces.check(&[2; 16], at(122)), Err(NonceRefusal::Replayed));

        // Full of unexpired nonces: new ones are refused until the old ones expire.
        let full = Nonces::default();
        for i in 0..MAX_NONCES as u32 {
            let mut nonce = [0; 16];
            nonce[..4].copy_from_slice(&i.to_be_bytes());
            assert_eq!(full.insert(nonce, t), Ok(()));
        }
        assert_eq!(full.insert([9; 16], at(121)), Err(NonceRefusal::Full));
        assert_eq!(full.insert([9; 16], at(122)), Ok(()));
    }
}

//! The device's side of pairing (docs/architecture.md, "Pairing"), shared with the server, which
//! re-exports it from `flicks_server::pair`.
//!
//! The link carries the server key, a 128-bit one-time code and the server's `host:port`
//! addresses. The device never sends the code; it proves it knows it:
//!
//! ```text
//! transcript = "flicks-pair-v1\0" ‖ lp(server key) ‖ lp(device key) ‖ lp(device name)
//! proof      = HMAC-SHA256(HKDF-SHA256(salt = "flicks pair v1" ‖ server key, ikm = code), transcript)
//! signature  = Sign(device key, "flicks-pair-req-v1\0" ‖ lp(transcript) ‖ proof)
//! reply      = Sign(server key, "flicks-pair-ok-v1\0" ‖ lp(transcript) ‖ lp(user) ‖ admin (1 byte))
//! ```
//!
//! The transcript holds the device key, so a relayed request can only ever pair that key.

use hkdf::Hkdf;
use hmac::{Hmac, KeyInit, Mac};
use p256::ecdsa::{SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

use crate::{Error, keys};

pub const CODE_LEN: usize = 16;
pub const PROOF_LEN: usize = 32;

/// Longest address in a link, in bytes.
pub const MAX_ADDR: usize = 256;

/// What a device sends to `POST /pair/redeem`, every binary field base64url.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub key: String,
    pub name: String,
    pub proof: String,
    pub signature: String,
}

/// The server's answer to a redemption.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Reply {
    pub user: String,
    pub admin: bool,
    /// `Sign(server key, "flicks-pair-ok-v1\0" ‖ lp(transcript) ‖ lp(user) ‖ admin)`, base64url.
    pub signature: String,
}

/// A link address: `host:port` (IPv6 must be bracketed), not a URL or query, and nothing that
/// would break the single-quoted `flicks connect '<link>'` the server prints.
pub fn valid_addr(addr: &str) -> bool {
    let Some((host, port)) = addr.rsplit_once(':') else {
        return false;
    };
    addr.len() <= MAX_ADDR
        && !host.is_empty()
        && (!host.contains(':')
            || host
                .strip_prefix('[')
                .and_then(|h| h.strip_suffix(']'))
                .is_some_and(|h| h.parse::<std::net::Ipv6Addr>().is_ok()))
        && port.parse::<u16>().is_ok()
        && !addr.chars().any(|c| c.is_whitespace() || c.is_control() || matches!(c, '&' | '=' | '#' | '/' | '?' | '\''))
}

pub fn transcript(server_key: &[u8], device_key: &[u8], name: &str) -> Vec<u8> {
    keys::Message::new("flicks-pair-v1").field(server_key).field(device_key).field(name.as_bytes()).into_bytes()
}

fn mac(server_key: &[u8], code: &[u8]) -> Result<Hmac<Sha256>, Error> {
    let mut salt = b"flicks pair v1".to_vec();
    salt.extend_from_slice(server_key);
    let mut key = [0u8; 32];
    Hkdf::<Sha256>::new(Some(&salt), code).expand(&[], &mut key).map_err(|_| Error::KeyEncoding)?;
    Hmac::<Sha256>::new_from_slice(&key).map_err(|_| Error::KeyEncoding)
}

pub fn proof(server_key: &[u8], code: &[u8], transcript: &[u8]) -> Result<[u8; PROOF_LEN], Error> {
    let mut mac = mac(server_key, code)?;
    mac.update(transcript);
    Ok(mac.finalize().into_bytes().into())
}

/// Whether `proof` is right for this code and transcript, compared in constant time.
pub fn proof_matches(server_key: &[u8], code: &[u8], transcript: &[u8], proof: &[u8]) -> bool {
    mac(server_key, code).is_ok_and(|mut mac| {
        mac.update(transcript);
        mac.verify_slice(proof).is_ok()
    })
}

pub fn request_message(transcript: &[u8], proof: &[u8]) -> Vec<u8> {
    keys::Message::new("flicks-pair-req-v1").field(transcript).fixed(proof).into_bytes()
}

pub fn ok_message(transcript: &[u8], user: &str, admin: bool) -> Vec<u8> {
    keys::Message::new("flicks-pair-ok-v1")
        .field(transcript)
        .field(user.as_bytes())
        .fixed(&[u8::from(admin)])
        .into_bytes()
}

impl Request {
    /// The request a device with key `device` sends to redeem `code`.
    pub fn new(device: &SigningKey, server_key: &[u8], code: &[u8], name: &str) -> Result<Request, Error> {
        let key = keys::public_bytes(device.verifying_key());
        let transcript = transcript(server_key, &key, name);
        let proof = proof(server_key, code, &transcript)?;
        let signature = keys::sign(device, &request_message(&transcript, &proof));
        Ok(Request {
            key: keys::encode(&key),
            name: name.to_string(),
            proof: keys::encode(&proof),
            signature: keys::encode(&signature),
        })
    }
}

/// Whether a reply, its user and admin flag included, was signed by the server for this
/// transcript.
pub fn verify_reply(server_key: &VerifyingKey, transcript: &[u8], reply: &Reply) -> bool {
    let message = ok_message(transcript, &reply.user, reply.admin);
    keys::decode(&reply.signature, 64).is_some_and(|sig| keys::verify(server_key, &message, &sig))
}

/// A parsed pairing link: `flicks://pair?key=<server key>&code=<code>&addr=<host:port>…`.
#[derive(Debug)]
pub struct Link {
    pub server_key: VerifyingKey,
    pub code: Vec<u8>,
    /// `host:port` each.
    pub addrs: Vec<String>,
}

pub fn parse_link(link: &str) -> Option<Link> {
    let query = link.strip_prefix("flicks://pair?")?;
    let (mut key, mut code, mut addrs) = (None, None, Vec::new());
    for pair in query.split('&').take(64) {
        match pair.split_once('=')? {
            ("key", v) => key = keys::parse_public(&keys::decode(v, keys::PUBLIC_KEY_LEN)?),
            ("code", v) => code = keys::decode(v, CODE_LEN).filter(|c| c.len() == CODE_LEN),
            ("addr", v) if valid_addr(v) => addrs.push(v.to_string()),
            ("addr", _) => return None,
            _ => {}
        }
    }
    // A link with nowhere to go is no link.
    if addrs.is_empty() {
        return None;
    }
    Some(Link { server_key: key?, code: code?, addrs })
}

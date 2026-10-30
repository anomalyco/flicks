//! Pairing a device (docs/architecture.md, "Pairing").
//!
//! The link carries the server key, a 128-bit one-time code and the server's addresses. The device
//! never sends the code; it proves it knows it:
//!
//! ```text
//! transcript = "flicks-pair-v1\0" ‖ lp(server key) ‖ lp(device key) ‖ lp(device name)
//! proof      = HMAC-SHA256(HKDF-SHA256(salt = "flicks pair v1" ‖ server key, ikm = code), transcript)
//! signature  = Sign(device key, "flicks-pair-req-v1\0" ‖ lp(transcript) ‖ proof)
//! reply      = Sign(server key, "flicks-pair-ok-v1\0" ‖ lp(transcript))
//! ```
//!
//! The transcript holds the device key, so a relayed request can only ever pair that key.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use hkdf::Hkdf;
use hmac::{Hmac, KeyInit, Mac};
use p256::ecdsa::{SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

use crate::{Error, db, keys};

/// How long a pairing window stays open.
pub const WINDOW: Duration = Duration::from_secs(5 * 60);

pub const CODE_LEN: usize = 16;
pub const PROOF_LEN: usize = 32;

/// Longest user or device name, in bytes.
pub const MAX_NAME: usize = 64;

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
    /// `Sign(server key, "flicks-pair-ok-v1\0" ‖ lp(transcript))`, base64url.
    pub signature: String,
}

/// An open pairing window. One device key redeems it; that key may redeem it again.
pub struct Window {
    pub code: [u8; CODE_LEN],
    pub user: db::User,
    pub expires: Instant,
    /// The device key that redeemed the window, and the reply it got.
    pub redeemed: Option<(Vec<u8>, Vec<u8>)>,
}

impl Window {
    pub fn new(user: db::User) -> Result<Window, Error> {
        Ok(Window { code: keys::random()?, user, expires: Instant::now() + WINDOW, redeemed: None })
    }

    pub fn is_open(&self) -> bool {
        Instant::now() < self.expires
    }
}

/// A user or device name: 1 to 64 bytes, no control characters, no surrounding spaces.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= MAX_NAME && name.trim() == name && !name.chars().any(char::is_control)
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

pub fn request_message(transcript: &[u8], proof: &[u8]) -> Vec<u8> {
    keys::Message::new("flicks-pair-req-v1").field(transcript).fixed(proof).into_bytes()
}

pub fn ok_message(transcript: &[u8]) -> Vec<u8> {
    keys::Message::new("flicks-pair-ok-v1").field(transcript).into_bytes()
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

/// Why a redemption was refused.
#[derive(Debug, PartialEq, Eq)]
pub enum Refusal {
    /// Not a well-formed request.
    Malformed,
    /// Well-formed, but the proof or signature doesn't check out.
    Forbidden,
}

/// A redemption that checked out.
pub struct Checked {
    pub key: Vec<u8>,
    pub name: String,
    pub transcript: Vec<u8>,
}

/// Checks a redemption body against this server's key and the window's code.
pub fn check(server_key: &[u8], code: &[u8], body: &[u8]) -> Result<Checked, Refusal> {
    let request: Request = serde_json::from_slice(body).map_err(|_| Refusal::Malformed)?;
    let key = keys::decode(&request.key, keys::PUBLIC_KEY_LEN).ok_or(Refusal::Malformed)?;
    let device = keys::parse_public(&key).ok_or(Refusal::Malformed)?;
    let proof = keys::decode(&request.proof, PROOF_LEN).ok_or(Refusal::Malformed)?;
    let signature = keys::decode(&request.signature, 64).ok_or(Refusal::Malformed)?;
    if !valid_name(&request.name) || proof.len() != PROOF_LEN || signature.len() != 64 {
        return Err(Refusal::Malformed);
    }
    let transcript = transcript(server_key, &key, &request.name);
    let mut mac = mac(server_key, code).map_err(|_| Refusal::Forbidden)?;
    mac.update(&transcript);
    if mac.verify_slice(&proof).is_err() || !keys::verify(&device, &request_message(&transcript, &proof), &signature) {
        return Err(Refusal::Forbidden);
    }
    Ok(Checked { key, name: request.name, transcript })
}

/// Whether a reply was signed by the server for this transcript.
pub fn verify_reply(server_key: &VerifyingKey, transcript: &[u8], reply: &Reply) -> bool {
    keys::decode(&reply.signature, 64).is_some_and(|sig| keys::verify(server_key, &ok_message(transcript), &sig))
}

/// The pairing link: `flicks://pair?key=<server key>&code=<code>&addr=<host:port>…`.
pub fn link(server_key: &[u8], code: &[u8], addrs: &[SocketAddr]) -> String {
    let mut link = format!("flicks://pair?key={}&code={}", keys::encode(server_key), keys::encode(code));
    for addr in addrs {
        link.push_str("&addr=");
        link.push_str(&addr.to_string());
    }
    link
}

/// A parsed pairing link.
#[derive(Debug)]
pub struct Link {
    pub server_key: VerifyingKey,
    pub code: Vec<u8>,
    pub addrs: Vec<SocketAddr>,
}

pub fn parse_link(link: &str) -> Option<Link> {
    let query = link.strip_prefix("flicks://pair?")?;
    let (mut key, mut code, mut addrs) = (None, None, Vec::new());
    for pair in query.split('&').take(64) {
        match pair.split_once('=')? {
            ("key", v) => key = keys::parse_public(&keys::decode(v, keys::PUBLIC_KEY_LEN)?),
            ("code", v) => code = keys::decode(v, CODE_LEN).filter(|c| c.len() == CODE_LEN),
            ("addr", v) => addrs.push(v.parse().ok()?),
            _ => {}
        }
    }
    Some(Link { server_key: key?, code: code?, addrs })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_proof_binds_the_device_key() {
        let server = keys::public_bytes(keys::generate().unwrap().verifying_key());
        let code = [7u8; CODE_LEN];
        let device = keys::generate().unwrap();
        let body = serde_json::to_vec(&Request::new(&device, &server, &code, "Mac").unwrap()).unwrap();
        let checked = check(&server, &code, &body).unwrap();
        assert_eq!(checked.key, keys::public_bytes(device.verifying_key()));

        // Another key, signing the same proof, fails.
        let thief = keys::generate().unwrap();
        let mut request: Request = serde_json::from_slice(&body).unwrap();
        let thief_key = keys::public_bytes(thief.verifying_key());
        let transcript = transcript(&server, &thief_key, "Mac");
        let proof = keys::decode(&request.proof, PROOF_LEN).unwrap();
        request.key = keys::encode(&thief_key);
        request.signature = keys::encode(&keys::sign(&thief, &request_message(&transcript, &proof)));
        let stolen = serde_json::to_vec(&request).unwrap();
        assert_eq!(check(&server, &code, &stolen).err(), Some(Refusal::Forbidden));

        // A wrong code fails.
        assert_eq!(check(&server, &[8; CODE_LEN], &body).err(), Some(Refusal::Forbidden));
    }

    #[test]
    fn links_round_trip() {
        let key = keys::generate().unwrap();
        let server = keys::public_bytes(key.verifying_key());
        let addrs = ["192.168.1.5:7686".parse().unwrap(), "[fd7a::1]:7686".parse().unwrap()];
        let link = parse_link(&link(&server, &[3; CODE_LEN], &addrs)).unwrap();
        assert_eq!(&link.server_key, key.verifying_key());
        assert_eq!(link.code, [3; CODE_LEN]);
        assert_eq!(link.addrs, addrs);
    }
}

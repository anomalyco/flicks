//! Pairing a device (docs/architecture.md, "Pairing"): the server's side. The message formats,
//! the device's request and reply check, and the link parser live in `flicks_client::pair` and
//! are re-exported here, so both sides share one copy.

use std::time::{Duration, Instant};

pub use flicks_client::pair::{
    CODE_LEN, Link, PROOF_LEN, Reply, Request, ok_message, parse_link, proof_matches, request_message, transcript,
    valid_addr, valid_name, verify_reply,
};

use crate::{Error, db, keys};

/// How long a pairing window stays open.
pub const WINDOW: Duration = Duration::from_secs(5 * 60);

/// The most addresses an admin may add to a link.
pub const MAX_EXTRA_ADDRS: usize = 16;

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
    if !proof_matches(server_key, code, &transcript, &proof)
        || !keys::verify(&device, &request_message(&transcript, &proof), &signature)
    {
        return Err(Refusal::Forbidden);
    }
    Ok(Checked { key, name: request.name, transcript })
}

/// The pairing link: `flicks://pair?key=<server key>&code=<code>&addr=<host:port>…`.
pub fn link(server_key: &[u8], code: &[u8], addrs: &[String]) -> String {
    let mut link = format!("flicks://pair?key={}&code={}", keys::encode(server_key), keys::encode(code));
    for addr in addrs {
        link.push_str("&addr=");
        link.push_str(addr);
    }
    link
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
        let addrs = ["192.168.1.5:7686", "[fd7a::1]:7686", "nas.tail1234.ts.net:7686"].map(String::from);
        let link = parse_link(&link(&server, &[3; CODE_LEN], &addrs)).unwrap();
        assert_eq!(&link.server_key, key.verifying_key());
        assert_eq!(link.code, [3; CODE_LEN]);
        assert_eq!(link.addrs, addrs);
        // A link with no addresses can't be used.
        assert!(parse_link(&super::link(&server, &[3; CODE_LEN], &[])).is_none());
        for bad in [
            "",
            "nas",
            "nas:",
            ":7686",
            "nas:99999",
            "a b:1",
            "a&b=c:1",
            "a#b:1",
            "a'b:1",
            "a\nb:1",
            "https://host:443",
            "host/path:443",
            "host?query:443",
            "host:443/path",
            "host:443?query",
            "fd7a::1:7686",
            "host:other:443",
            "[host:other]:443",
            "[::1:443",
            "::1]:443",
        ] {
            assert!(!valid_addr(bad), "{bad:?}");
        }
    }
}

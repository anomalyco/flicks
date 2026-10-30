//! The server's TLS certificate, minted in memory for the server key on every start. TLS 1.3 only;
//! clients pin the key (`flicks_client::net`).

use std::sync::Arc;

use p256::ecdsa::SigningKey;
use p256::pkcs8::EncodePrivateKey;
use rustls::ServerConfig;
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};

use crate::Error;

/// The name in the certificate. Clients pin the key, so the name carries no meaning.
pub const SERVER_NAME: &str = "flicks";

const CERT_DAYS: i64 = 365;

pub fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// A server config with a fresh self-signed certificate for `key`, valid for 365 days.
pub fn server_config(key: &SigningKey) -> Result<ServerConfig, Error> {
    let pkcs8 = key.to_pkcs8_der().map_err(|_| Error::KeyEncoding)?;
    let key_der = PrivatePkcs8KeyDer::from(pkcs8.as_bytes().to_vec());
    let key_pair = rcgen::KeyPair::try_from(&key_der)?;
    let mut params = rcgen::CertificateParams::new(vec![SERVER_NAME.to_string()])?;
    let now = time::OffsetDateTime::now_utc();
    params.not_before = now - time::Duration::days(1);
    params.not_after = now + time::Duration::days(CERT_DAYS);
    let cert = params.self_signed(&key_pair)?;

    let mut config = ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_no_client_auth()
        .with_single_cert(vec![cert.der().clone()], PrivateKeyDer::Pkcs8(key_der))?;
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(config)
}

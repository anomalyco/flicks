//! The server's TLS certificate, minted in memory for the server key on every start, and the
//! client side's pinned verifier, which trusts a certificate only if it carries the expected key.
//! Both sides speak TLS 1.3 only.

use std::sync::Arc;

use p256::ecdsa::{SigningKey, VerifyingKey};
use p256::pkcs8::{EncodePrivateKey, EncodePublicKey};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, WebPkiSupportedAlgorithms};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, ServerConfig, SignatureScheme};

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

/// A client config that accepts only a certificate for `server_key`.
pub fn pinned_client_config(server_key: &VerifyingKey) -> Result<ClientConfig, Error> {
    let provider = provider();
    let spki = server_key.to_public_key_der().map_err(|_| Error::KeyEncoding)?.into_vec();
    let verifier = Pinned { spki, algorithms: provider.signature_verification_algorithms };
    let mut config = ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_no_client_auth();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(config)
}

#[derive(Debug)]
struct Pinned {
    spki: Vec<u8>,
    algorithms: WebPkiSupportedAlgorithms,
}

impl ServerCertVerifier for Pinned {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let cert = webpki::EndEntityCert::try_from(end_entity)
            .map_err(|_| rustls::Error::InvalidCertificate(rustls::CertificateError::BadEncoding))?;
        if cert.subject_public_key_info().as_ref() == self.spki.as_slice() {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::InvalidCertificate(rustls::CertificateError::ApplicationVerificationFailure))
        }
    }

    /// Never called: both sides speak TLS 1.3 only.
    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::PeerIncompatible(rustls::PeerIncompatible::Tls12NotOffered))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

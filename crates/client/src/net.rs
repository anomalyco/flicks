//! One HTTPS request per connection to a `host:port` address, with SNI and `Host` for names, and
//! the TLS rule: a certificate for the server's own key is always accepted (the server itself, at
//! any address); otherwise only a name, with a certificate valid for it under the public web roots
//! (a tunnel or reverse proxy, which terminates TLS; the signed `/info` probe then checks the
//! server behind it).

use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use http_body_util::{BodyExt, Full, Limited};
use hyper::body::Bytes;
use hyper::{Request, StatusCode};
use hyper_util::rt::TokioIo;
use p256::ecdsa::VerifyingKey;
use p256::pkcs8::EncodePublicKey;
use rustls::client::WebPkiServerVerifier;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, WebPkiSupportedAlgorithms};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;

use crate::Error;

const TIMEOUT: Duration = Duration::from_secs(10);

/// The largest response read; callers ask for byte ranges well under it.
pub const MAX_RESPONSE: usize = 16 * 1024 * 1024;

pub struct Response {
    pub status: StatusCode,
    pub body: Bytes,
}

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// The public web roots, plus any extra authorities.
pub fn web_verifier(extra: &[CertificateDer<'static>]) -> Result<Arc<WebPkiServerVerifier>, Error> {
    let mut roots = RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
    for cert in extra {
        roots.add(cert.clone())?;
    }
    WebPkiServerVerifier::builder_with_provider(Arc::new(roots), provider()).build().map_err(|_| Error::KeyEncoding)
}

/// A TLS 1.3 client config for one server.
pub fn client_config(server_key: &VerifyingKey, web: Arc<WebPkiServerVerifier>) -> Result<ClientConfig, Error> {
    let provider = provider();
    let spki = server_key.to_public_key_der().map_err(|_| Error::KeyEncoding)?.into_vec();
    let verifier = Verifier { spki, web, algorithms: provider.signature_verification_algorithms };
    let mut config = ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_no_client_auth();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(config)
}

#[derive(Debug)]
struct Verifier {
    spki: Vec<u8>,
    web: Arc<WebPkiServerVerifier>,
    algorithms: WebPkiSupportedAlgorithms,
}

impl ServerCertVerifier for Verifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let pinned = webpki::EndEntityCert::try_from(end_entity)
            .is_ok_and(|cert| cert.subject_public_key_info().as_ref() == self.spki.as_slice());
        match server_name {
            _ if pinned => Ok(ServerCertVerified::assertion()),
            ServerName::DnsName(_) => {
                self.web.verify_server_cert(end_entity, intermediates, server_name, ocsp_response, now)
            }
            _ => Err(rustls::Error::InvalidCertificate(rustls::CertificateError::ApplicationVerificationFailure)),
        }
    }

    /// Never called: TLS 1.3 only.
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

/// `host:port` split into a bare host (IPv6 brackets removed) and port.
fn split(addr: &str) -> Option<(&str, u16)> {
    let (host, port) = addr.rsplit_once(':')?;
    let host = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')).unwrap_or(host);
    Some((host, port.parse().ok()?))
}

/// The `Host` header for an address: as given, without `:443`.
pub fn host_header(addr: &str) -> &str {
    addr.strip_suffix(":443").unwrap_or(addr)
}

/// The host part of an address, for showing.
pub fn host(addr: &str) -> &str {
    split(addr).map_or(addr, |(host, _)| host)
}

/// Sends one request over a new TLS connection to `addr` (`host:port`).
pub async fn send(addr: &str, tls: Arc<ClientConfig>, request: Request<Full<Bytes>>) -> Result<Response, Error> {
    let (host, port) = split(addr).ok_or(Error::BadAddress)?;
    let name = match host.parse::<IpAddr>() {
        Ok(ip) => ServerName::IpAddress(ip.into()),
        Err(_) => ServerName::try_from(host.to_string()).map_err(|_| Error::BadAddress)?,
    };
    let exchange = async {
        let tcp = TcpStream::connect((host, port)).await?;
        let stream = TlsConnector::from(tls).connect(name, tcp).await?;
        let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream)).await?;
        tokio::spawn(connection);
        let response = sender.send_request(request).await?;
        let status = response.status();
        let body = Limited::new(response.into_body(), MAX_RESPONSE)
            .collect()
            .await
            .map_err(|_| Error::BadResponse)?
            .to_bytes();
        Ok(Response { status, body })
    };
    tokio::time::timeout(TIMEOUT, exchange).await.map_err(|_| Error::Io(std::io::ErrorKind::TimedOut.into()))?
}

//! A minimal HTTPS client, one connection per request: enough for the CLI to reach the running
//! server and for tests to reach it directly or through a proxy.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use http_body_util::{BodyExt, Full, Limited};
use hyper::body::Bytes;
use hyper::{Request, StatusCode};
use hyper_util::rt::TokioIo;
use rustls::ClientConfig;
use rustls::pki_types::ServerName;
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;

use crate::Error;

const TIMEOUT: Duration = Duration::from_secs(10);

/// The largest response this client reads.
const MAX_RESPONSE: usize = 16 * 1024 * 1024;

pub struct Response {
    pub status: StatusCode,
    pub body: Bytes,
}

/// Sends one request over a new TLS connection to `addr`.
pub async fn send(addr: SocketAddr, tls: Arc<ClientConfig>, request: Request<Full<Bytes>>) -> Result<Response, Error> {
    let exchange = async {
        let tcp = TcpStream::connect(addr).await?;
        let name = ServerName::try_from(crate::tls::SERVER_NAME).map_err(|_| Error::BadResponse)?.to_owned();
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

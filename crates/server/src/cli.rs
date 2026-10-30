//! The `flicks` command line. `serve` runs the server; the admin commands talk to the running
//! server at `https://127.0.0.1:7686`, signing with `server.key` as the root admin, so one process
//! writes the database.

use std::net::{Ipv4Addr, SocketAddr};
use std::path::Path;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use http_body_util::Full;
use hyper::body::Bytes;
use hyper::header::{AUTHORIZATION, CONTENT_TYPE, HOST};
use hyper::{Method, Request};
use p256::ecdsa::{SigningKey, VerifyingKey};
use rustls::ClientConfig;
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::server::{AddFolder, AddUser, OpenPairing, Pairing, Status};
use crate::{Error, cli_client, db, http, keys, server, service, tls};

#[derive(Debug, Parser)]
#[command(name = "flicks", version, about = "A Flicks server and its admin commands")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run the server on port 7686.
    Serve,
    /// Add, remove or list media folders.
    #[command(subcommand)]
    Folder(FolderCommand),
    /// Add or list users.
    #[command(subcommand)]
    User(UserCommand),
    /// Open a 5-minute window to pair a device, and print its QR code and link.
    Pair {
        /// The user the device belongs to, created if needed ("admin" when only --admin is given).
        #[arg(long)]
        user: Option<String>,
        /// Make that user an admin.
        #[arg(long)]
        admin: bool,
        /// Another address to put in the link, such as a tunnel or tailnet name (repeatable).
        #[arg(long = "addr", value_name = "HOST:PORT")]
        addrs: Vec<String>,
    },
    /// List or remove paired devices.
    #[command(subcommand)]
    Device(DeviceCommand),
    /// Show whether the server is running, and what it holds.
    Status,
    #[command(flatten)]
    Client(cli_client::Command),
    #[command(flatten)]
    Service(service::Command),
}

#[derive(Debug, Subcommand)]
pub enum FolderCommand {
    /// Add a folder of video.
    Add { path: std::path::PathBuf },
    /// Remove a folder by its id (see `flicks folder ls`).
    Rm { id: i64 },
    /// List folders.
    Ls,
}

#[derive(Debug, Subcommand)]
pub enum UserCommand {
    /// Add a user.
    Add {
        name: String,
        /// Make the user an admin.
        #[arg(long)]
        admin: bool,
    },
    /// List users.
    Ls,
}

#[derive(Debug, Subcommand)]
pub enum DeviceCommand {
    /// List paired devices.
    Ls,
    /// Remove a device by its id (see `flicks device ls`); its next request is refused.
    Rm { id: i64 },
}

pub async fn run(command: Command) -> Result<(), Error> {
    let data_dir = crate::data_dir()?;
    match command {
        Command::Serve => server::serve(&data_dir).await,
        Command::Client(command) => cli_client::run(command).await,
        Command::Service(command) => service::run(command).await,
        command => admin(&Client::local(&data_dir)?, command).await,
    }
}

async fn admin(client: &Client, command: Command) -> Result<(), Error> {
    match command {
        Command::Folder(FolderCommand::Add { path }) => {
            let path = std::path::absolute(&path)?.to_string_lossy().into_owned();
            let folder: db::Folder = client.call(Method::POST, "/admin/folders", Some(&AddFolder { path })).await?;
            println!("Added {} (id {}).", folder.path, folder.id);
        }
        Command::Folder(FolderCommand::Rm { id }) => {
            client.call::<()>(Method::DELETE, &format!("/admin/folders/{id}"), None::<&()>).await?;
            println!("Removed folder {id}.");
        }
        Command::Folder(FolderCommand::Ls) => {
            let folders: Vec<db::Folder> = client.call(Method::GET, "/admin/folders", None::<&()>).await?;
            if folders.is_empty() {
                println!("No folders yet. Add one with `flicks folder add <path>`.");
            }
            for folder in folders {
                println!("{:>4}  {}", folder.id, folder.path);
            }
        }
        Command::User(UserCommand::Add { name, admin }) => {
            let user: db::User = client.call(Method::POST, "/admin/users", Some(&AddUser { name, admin })).await?;
            println!("Added {} ({}).", user.name, role(user.admin));
        }
        Command::User(UserCommand::Ls) => {
            let users: Vec<db::User> = client.call(Method::GET, "/admin/users", None::<&()>).await?;
            if users.is_empty() {
                println!("No users yet. Pair a device with `flicks pair --user <name>`.");
            }
            for user in users {
                println!("{}  {}", user.name, role(user.admin));
            }
        }
        Command::Pair { user, admin, addrs } => {
            let user = match user {
                Some(user) => user,
                None if admin => "admin".to_string(),
                None => return Err(Error::Usage("say who the device is for: --user <name>")),
            };
            let pairing: Pairing =
                client.call(Method::POST, "/admin/pair", Some(&OpenPairing { user, admin, addrs })).await?;
            if let Ok(code) = qrcode::QrCode::new(pairing.link.as_bytes()) {
                use qrcode::render::unicode::Dense1x2;
                let qr = code.render::<Dense1x2>().dark_color(Dense1x2::Light).light_color(Dense1x2::Dark).build();
                println!("{qr}");
            }
            println!("{}", pairing.link);
            println!(
                "Scan this in Flicks on the new device, or paste the link. It works once, for {} minutes.",
                pairing.expires_in / 60
            );
        }
        Command::Device(DeviceCommand::Ls) => {
            let devices: Vec<db::Device> = client.call(Method::GET, "/admin/devices", None::<&()>).await?;
            if devices.is_empty() {
                println!("No devices yet. Pair one with `flicks pair --user <name>`.");
            }
            for device in devices {
                let date = time::OffsetDateTime::from_unix_timestamp(device.created_at)
                    .map(|t| t.date().to_string())
                    .unwrap_or_default();
                println!("{:>4}  {}  {}  paired {date}", device.id, device.user, device.name);
            }
        }
        Command::Device(DeviceCommand::Rm { id }) => {
            client.call::<()>(Method::DELETE, &format!("/admin/devices/{id}"), None::<&()>).await?;
            println!("Removed device {id}. Its next request will be refused.");
        }
        Command::Status => {
            let status: Status = client.call(Method::GET, "/admin/status", None::<&()>).await?;
            println!("Flicks {} is running on port {}.", status.version, status.port);
            println!("{} users, {} devices, {} folders.", status.users, status.devices, status.folders);
            if let Some(left) = status.pairing {
                println!("A pairing window is open for {}m {}s more.", left / 60, left % 60);
            }
        }
        Command::Serve | Command::Client(_) | Command::Service(_) => {}
    }
    Ok(())
}

fn role(admin: bool) -> &'static str {
    if admin { "admin" } else { "viewer" }
}

/// Signs requests to one server with one key.
pub struct Client {
    pub key: SigningKey,
    /// The server key, SEC1 compressed.
    pub server_key: Vec<u8>,
    pub addr: SocketAddr,
    pub tls: Arc<ClientConfig>,
}

impl Client {
    /// A client pinned to `server_key`.
    pub fn new(key: SigningKey, server_key: &VerifyingKey, addr: SocketAddr) -> Result<Client, Error> {
        Ok(Client {
            key,
            server_key: keys::public_bytes(server_key),
            addr,
            tls: Arc::new(tls::pinned_client_config(server_key)?),
        })
    }

    /// The local CLI: the running server's own key, at `127.0.0.1:7686`.
    pub fn local(data_dir: &Path) -> Result<Client, Error> {
        let key = match keys::load(&data_dir.join(crate::KEY_FILE)) {
            Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(Error::NotRunning);
            }
            other => other?,
        };
        let server_key = *key.verifying_key();
        Client::new(key, &server_key, SocketAddr::from((Ipv4Addr::LOCALHOST, crate::PORT)))
    }

    /// A signed request, stamped now.
    pub fn request(&self, method: Method, path_and_query: &str, body: Vec<u8>) -> Result<Request<Full<Bytes>>, Error> {
        let authorization = crate::auth::sign(
            &self.key,
            &self.server_key,
            method.as_str(),
            path_and_query,
            b"",
            &body,
            crate::unix_now(),
        )?;
        Request::builder()
            .method(method)
            .uri(path_and_query)
            .header(HOST, tls::SERVER_NAME)
            .header(AUTHORIZATION, authorization)
            .header(CONTENT_TYPE, "application/json")
            .body(Full::new(Bytes::from(body)))
            .map_err(|_| Error::BadResponse)
    }

    pub async fn send(&self, request: Request<Full<Bytes>>) -> Result<http::Response, Error> {
        http::send(self.addr, self.tls.clone(), request).await.map_err(|e| match e {
            Error::Io(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => Error::NotRunning,
            e => e,
        })
    }

    /// Sends a signed request and decodes the JSON answer; an error status becomes
    /// [`Error::Status`] with the server's message.
    pub async fn call<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: Option<&impl Serialize>,
    ) -> Result<T, Error> {
        let body = match body {
            Some(body) => serde_json::to_vec(body).map_err(|_| Error::BadResponse)?,
            None => Vec::new(),
        };
        let response = self.send(self.request(method, path, body)?).await?;
        if !response.status.is_success() {
            let message = serde_json::from_slice::<serde_json::Value>(&response.body)
                .ok()
                .and_then(|v| v.get("error")?.as_str().map(str::to_string))
                .unwrap_or_else(|| response.status.canonical_reason().unwrap_or("error").to_string());
            return Err(Error::Status(response.status.as_u16(), message));
        }
        let body: &[u8] = if response.body.is_empty() { b"null" } else { &response.body };
        serde_json::from_slice(body).map_err(|_| Error::BadResponse)
    }
}

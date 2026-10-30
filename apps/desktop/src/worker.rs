//! The network thread: it owns the merged library (this device's key, saved servers and their
//! cached catalogs), and answers the window's requests by updating its state through the app
//! handle. Nothing here polls; the thread sleeps on its channel.

use std::path::PathBuf;
use std::sync::Arc;

use flicks_client::Client;
use flicks_client::merge::Library;
use rune::AppHandle;
use tokio::sync::{Semaphore, mpsc};

use crate::Flicks;
use crate::library::catalog::Catalog;

/// What this device calls itself to a server it pairs with.
const DEVICE_NAME: &str = "Flicks for Mac";

/// Artwork downloads at once.
const ARTWORK_AT_ONCE: usize = 6;

pub enum Request {
    Refresh,
    Pair(String),
    /// A poster or backdrop: the server key and the artwork name.
    Artwork(String, String),
}

/// The window's end of the channel.
pub struct Worker(mpsc::UnboundedSender<Request>);

impl Worker {
    pub fn send(&self, request: Request) {
        // The thread only ends with the process.
        let _ = self.0.send(request);
    }
}

/// Starts the thread. It shows the cached library at once, then refreshes it, or opens Add
/// Server when there are no servers yet.
pub fn start(app: AppHandle<Flicks>) -> Worker {
    let (tx, rx) = mpsc::unbounded_channel();
    let spawned = std::thread::Builder::new().name("flicks-network".into()).spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build();
        match runtime {
            Ok(runtime) => runtime.block_on(serve(app, rx)),
            Err(error) => app.update(move |s, _| s.failed(format!("couldn't start networking: {error}"))),
        }
    });
    if let Err(error) = spawned {
        eprintln!("flicks: couldn't start the network thread: {error}");
    }
    tx.send(Request::Refresh).ok();
    Worker(tx)
}

async fn serve(app: AppHandle<Flicks>, mut rx: mpsc::UnboundedReceiver<Request>) {
    let Some(dir) = data_dir() else {
        return app.update(|s, _| s.failed("no home folder to keep this device's key in".into()));
    };
    let client = match Client::open(&dir) {
        Ok(client) => client,
        Err(error) => return app.update(move |s, _| s.failed(format!("couldn't open this device's key: {error}"))),
    };
    let mut library = Library::open(client);
    if !library.client.servers.is_empty() {
        let cached = Catalog::build(library.titles().to_vec());
        app.update(move |s, tx| s.loaded(cached, None, false, tx));
    }
    let downloads = Arc::new(Semaphore::new(ARTWORK_AT_ONCE));
    while let Some(request) = rx.recv().await {
        match request {
            Request::Refresh => refresh(&app, &mut library).await,
            Request::Pair(link) => {
                let paired = library.client.pair(&link, DEVICE_NAME).await.map(|s| s.name).map_err(|e| e.to_string());
                let ok = paired.is_ok();
                app.update(move |s, _| s.paired(paired));
                if ok {
                    refresh(&app, &mut library).await;
                }
            }
            Request::Artwork(server, name) => {
                let Some(connection) = library.connection(&server).cloned() else {
                    app.update(move |s, _| s.artwork(&server, &name, None));
                    continue;
                };
                let (app, downloads) = (app.clone(), downloads.clone());
                tokio::spawn(async move {
                    let Ok(_permit) = downloads.acquire().await else { return };
                    let bytes = connection.artwork(&name).await.ok();
                    app.update(move |s, _| s.artwork(&server, &name, bytes.map(|b| b.to_vec())));
                });
            }
        }
    }
}

async fn refresh(app: &AppHandle<Flicks>, library: &mut Library) {
    if library.client.servers.is_empty() {
        return app.update(|s, tx| s.no_servers(tx));
    }
    let error = library.refresh().await.err().map(|e| e.to_string());
    let catalog = Catalog::build(library.titles().to_vec());
    app.update(move |s, tx| s.loaded(catalog, error, true, tx));
}

/// Where this device's key and server list live: the same directory the `flicks` CLI uses
/// (flicks-server's `data_dir`), so servers paired from either show in both.
fn data_dir() -> Option<PathBuf> {
    if cfg!(windows) {
        return std::env::var_os("LOCALAPPDATA").map(|base| PathBuf::from(base).join("Flicks"));
    }
    if !cfg!(target_os = "macos")
        && let Some(base) = std::env::var_os("XDG_DATA_HOME").filter(|v| !v.is_empty())
    {
        return Some(PathBuf::from(base).join("flicks"));
    }
    let home = PathBuf::from(std::env::var_os("HOME").filter(|v| !v.is_empty())?);
    Some(if cfg!(target_os = "macos") {
        home.join("Library/Application Support/Flicks")
    } else {
        home.join(".local/share/flicks")
    })
}

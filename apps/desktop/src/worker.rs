//! The network thread: it owns the merged library (this device's key, saved servers and their
//! cached catalogs), and answers the window's requests by updating its state through the app
//! handle. Nothing here polls; the thread sleeps on its channel.

use std::sync::Arc;

use flicks_client::Client;
use flicks_client::download::Downloads;
use flicks_client::merge::Library;
use flicks_ui::{Catalog, Flicks, Request};
use rune::AppHandle;
use tokio::sync::{Semaphore, mpsc};

/// What this device calls itself to a server it pairs with.
const DEVICE_NAME: &str = "Flicks for Mac";

/// Artwork downloads at once.
const ARTWORK_AT_ONCE: usize = 6;

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
    app.update(|s, _| s.player_video = Some(Box::new(|id| crate::player::video::SLOT.switch(id))));
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
    let Ok(dir) = flicks_client::data_dir() else {
        return app.update(|s, _| s.failed("no home folder to keep this device's key in".into()));
    };
    let client = match Client::open(&dir) {
        Ok(client) => client,
        Err(error) => return app.update(move |s, _| s.failed(format!("couldn't open this device's key: {error}"))),
    };
    let mut library = Library::open(client);
    // Played instead of streaming when a title has one; holds progress made offline.
    let copies = Downloads::open(&dir).ok();
    if !library.client.servers.is_empty() {
        let cached = catalog(&library, copies.as_ref());
        app.update(move |s, tx| s.loaded(cached, None, false, tx));
    }
    let downloads = Arc::new(Semaphore::new(ARTWORK_AT_ONCE));
    while let Some(request) = rx.recv().await {
        match request {
            Request::Refresh => refresh(&app, &mut library, copies.as_ref()).await,
            Request::Pair(link) => {
                let paired = library.client.pair(&link, DEVICE_NAME).await.map(|s| s.name).map_err(|e| e.to_string());
                let ok = paired.is_ok();
                app.update(move |s, _| s.paired(paired));
                if ok {
                    refresh(&app, &mut library, copies.as_ref()).await;
                }
            }
            Request::Admin(server, op) => crate::admin::run(&app, &mut library, server, op).await,
            Request::Play(id, title) => crate::player::open(&app, &library, copies.as_ref(), id, &title),
            Request::Progress(key, server, file, progress) => {
                crate::player::save(&app, &mut library, copies.as_ref(), (key, server, file), progress).await
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

async fn refresh(app: &AppHandle<Flicks>, library: &mut Library, downloads: Option<&Downloads>) {
    if library.client.servers.is_empty() {
        return app.update(|s, tx| s.no_servers(tx));
    }
    let error = library.refresh().await.err().map(|e| e.to_string());
    if let Some(downloads) = downloads
        && let Err(error) = downloads.sync(library).await
    {
        eprintln!("flicks: couldn't sync offline progress: {error}");
    }
    let catalog = catalog(library, downloads);
    app.update(move |s, tx| s.loaded(catalog, error, true, tx));
    crate::admin::refreshed(app, library);
}

/// Local copies are playable even when their servers aren't, and carry the newest progress.
pub fn catalog(library: &Library, downloads: Option<&Downloads>) -> Catalog {
    let mut titles = library.titles().to_vec();
    if let Some(downloads) = downloads {
        for title in &mut titles {
            title.progress = downloads.progress(title);
            for source in &mut title.sources {
                source.available |= downloads.local_file(&source.server, source.item.id).is_some();
            }
        }
    }
    Catalog::build(titles)
}

#[cfg(test)]
mod tests {
    use super::*;
    use flicks_client::{Entry, Progress, Server};

    #[test]
    fn complete_download_is_playable_offline_and_local_progress_builds_continue_watching() {
        let dir = std::env::temp_dir().join(format!("flicks-offline-catalog-{}", std::process::id()));
        let mut client = Client::open(&dir).unwrap();
        client.servers.push(Server {
            key: "fixture".into(),
            name: "Offline".into(),
            addrs: vec![],
            user: "test".into(),
            admin: false,
        });
        let mut item = flicks_ui::library::fixture::item(1, "Downloaded", Some(1), 1, 30);
        item.size = 4;
        let entries =
            std::collections::BTreeMap::from([("fixture", vec![Entry { item: item.clone(), progress: None }])]);
        std::fs::write(dir.join(flicks_client::merge::CATALOG_FILE), serde_json::to_vec(&entries).unwrap()).unwrap();
        let mut library = Library::open(client);
        let downloads = Downloads::open(&dir).unwrap();
        assert!(!catalog(&library, Some(&downloads)).titles[0].available());

        // The persisted format of a completed download (fixture's SHA-256 prefix and file id).
        let base = dir.join("downloads/f16d05ec6b29-1");
        std::fs::write(
            base.with_extension("json"),
            serde_json::to_vec(&serde_json::json!({
                "server": "fixture", "server_name": "Offline", "item": item, "state": "Complete"
            }))
            .unwrap(),
        )
        .unwrap();
        std::fs::write(base.with_extension("mp4"), [0; 4]).unwrap();
        let progress = Progress { position: 90_000, played: false, updated_at: 1_700_000_000_000 };
        downloads.set_progress("fixture", 1, progress).unwrap();
        let ready = catalog(&library, Some(&downloads));
        assert!(ready.titles[0].available());
        assert_eq!(ready.titles[0].progress, Some(progress));
        assert_eq!(ready.rows[0].name, "Continue Watching");
        assert_eq!(ready.rows[0].titles, [0]);
        let mut ui = Flicks::new();
        ui.catalog = ready;
        ui.refreshed = true;
        assert!(!ui.offline(0));
        ui.open_player(0);
        assert!(ui.player.is_some());
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        assert_eq!(runtime.block_on(downloads.sync(&mut library)).unwrap(), 0);
        assert_eq!(catalog(&library, Some(&downloads)).titles[0].progress, Some(progress), "kept for reconnect");
        std::fs::remove_file(base.with_extension("mp4")).unwrap();
        assert!(!catalog(&library, Some(&downloads)).titles[0].available(), "a missing copy isn't playable");
        drop(downloads);
        std::fs::remove_dir_all(dir).unwrap();
    }
}

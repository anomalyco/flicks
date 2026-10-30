//! Playing for the player screen (`flicks_ui::player`): the [`engine`] thread runs
//! `flicks-player`, and [`video`] draws its frames in Rune's pass. These run on the network
//! thread: [`open`] starts a play, [`save`] keeps its progress.

mod engine;
pub mod video;

use std::sync::Arc;
use std::sync::mpsc;

use flicks_client::Progress;
use flicks_client::download::Downloads;
use flicks_client::merge::{self, Key, Library, Title};
use flicks_player::Source;
use flicks_ui::Flicks;
use rune::AppHandle;

/// Picks the copy to play (a download first), where to resume, and starts the playback thread.
pub fn open(app: &AppHandle<Flicks>, library: &Library, downloads: Option<&Downloads>, id: u64, title: &Title) {
    let best = merge::best_source(title);
    let local = downloads.and_then(|d| d.local_path(title));
    let source = match (local, best) {
        (Some(path), _) => Source::Local(path),
        (None, Some(best)) => match library.connection(&best.server) {
            Some(connection) => {
                Source::Network { connection: Arc::new(connection.clone()), file: best.item.id, size: best.item.size }
            }
            None => return app.update(move |s, _| s.player_failed(id, "Its server isn't answering.".into())),
        },
        (None, None) => return app.update(move |s, _| s.player_failed(id, "Its server isn't answering.".into())),
    };
    // Progress goes to the title; a download's server and file hold it while offline.
    let Some(first) = best.or(title.sources.first()) else { return };
    let progress = downloads.and_then(|d| d.progress(title)).or(title.progress);
    let resume = progress.filter(|p| !p.played).map_or(0.0, |p| p.position as f64 / 1000.0);
    let (commands, rx) = mpsc::channel();
    let saving = (title.key.clone(), first.server.clone(), first.item.id);
    engine::start(app.clone(), id, source, resume, saving, rx);
    app.update(move |s, _| s.player_started(id, Box::new(move |command| drop(commands.send(command)))));
}

/// Saves progress to the title's servers, or keeps it with the download to send later when none
/// of them answers.
pub async fn save(
    app: &AppHandle<Flicks>,
    library: &mut Library,
    downloads: Option<&Downloads>,
    (key, server, file): (Key, String, i64),
    progress: Progress,
) {
    if let Err(error) = library.set_progress(&key, progress).await {
        match downloads.map(|d| d.set_progress(&server, file, progress)) {
            Some(Ok(())) => {}
            _ => eprintln!("flicks: couldn't save progress: {error}"),
        }
    }
    let catalog = crate::worker::catalog(library, downloads);
    app.update(move |s, tx| s.progress_saved(catalog, tx));
}

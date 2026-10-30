//! A small catalog for headless scenes and tests: the two VisionQuest episodes the test server
//! serves, one of them half watched.

use flicks_client::{Entry, Item, Progress};

use super::catalog::{Catalog, Home, ServerCatalog};
use crate::{Flicks, Screen, Status};

pub const SCENES: &[(&str, fn(&mut Flicks))] = &[
    ("home", |s| ready(s, Screen::Home)),
    ("detail", |s| {
        ready(s, Screen::Home);
        let key = s.catalog.titles.first().map(|t| t.key.clone()).unwrap_or_default();
        s.go(Screen::Detail(key));
    }),
    ("search", |s| {
        ready(s, Screen::Search);
        s.query.replace_text("vision");
        s.search_changed();
    }),
    ("add-server", |s| {
        s.status = Status::NoServers;
        s.screen = Screen::AddServer;
    }),
    ("empty", |s| {
        s.status = Status::Ready;
        s.screen = Screen::Home;
    }),
];

fn ready(s: &mut Flicks, screen: Screen) {
    s.catalog = catalog();
    s.status = Status::Ready;
    s.go(screen);
}

pub fn catalog() -> Catalog {
    let episode = |id: i64, episode: u16, added_at: i64, minutes: u64| Item {
        id,
        title: "VisionQuest".into(),
        year: Some(2026),
        season: Some(1),
        episode: Some(episode),
        episode_end: None,
        edition: None,
        tmdb_id: None,
        tmdb_media: None,
        poster: None,
        backdrop: None,
        added_at,
        size: 1_200_000_000,
        oshash: format!("{id:016x}"),
        container: "mp4".into(),
        duration_ms: Some(minutes * 60_000),
        video_codec: Some("hevc".into()),
        width: Some(3840),
        height: Some(2160),
        hdr: Some("hdr10".into()),
    };
    let watched = Progress { position: 20 * 60_000, played: false, updated_at: 1_790_000_000_000 };
    let theseus = Entry { item: episode(1, 2, 1_790_000_000, 52), progress: Some(watched) };
    let deadlock = Entry { item: episode(2, 6, 1_790_000_100, 58), progress: None };
    let home =
        Home { continue_watching: vec![theseus.clone()], recently_added: vec![deadlock.clone(), theseus.clone()] };
    Catalog::build(vec![ServerCatalog {
        server: "fixture".into(),
        libraries: vec![("TV".into(), vec![theseus, deadlock])],
        home,
    }])
}

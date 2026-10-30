//! A small merged library for headless scenes and tests: the two VisionQuest episodes the test
//! server serves (one half watched), and a movie on a server that didn't answer.

use flicks_client::merge::{Key, Source, Title};
use flicks_client::{Item, Progress};

use super::catalog::Catalog;
use crate::{Flicks, Screen, Status};

pub const SCENES: &[(&str, fn(&mut Flicks))] = &[
    ("home", |s| ready(s, Screen::Home)),
    ("detail", |s| {
        ready(s, Screen::Home);
        if let Some(group) = s.catalog.groups.iter().find(|g| g.is_show()) {
            let key = group.key.clone();
            s.go(Screen::Detail(key));
        }
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
    s.refreshed = true;
    s.go(screen);
}

pub fn catalog() -> Catalog {
    let watched = Progress { position: 20 * 60_000, played: false, updated_at: 1_790_000_000_000 };
    Catalog::build(vec![
        title(item(1, "VisionQuest", Some(2), 1_790_000_000, 52), Some(watched), true),
        title(item(2, "VisionQuest", Some(6), 1_790_000_100, 58), None, true),
        title(item(3, "Arrival", None, 1_780_000_000, 116), None, false),
    ])
}

fn item(id: i64, name: &str, episode: Option<u16>, added_at: i64, minutes: u64) -> Item {
    Item {
        id,
        title: name.into(),
        year: Some(if episode.is_some() { 2026 } else { 2016 }),
        season: episode.map(|_| 1),
        episode,
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
    }
}

fn title(item: Item, progress: Option<Progress>, available: bool) -> Title {
    let key =
        Key::Name { title: item.title.to_lowercase(), year: item.year, season: item.season, episode: item.episode };
    let server = if available { "fixture" } else { "away" };
    let source = Source {
        server: server.into(),
        server_name: server.into(),
        item: item.clone(),
        progress,
        available,
        latency: None,
    };
    Title { key, item, sources: vec![source], progress }
}

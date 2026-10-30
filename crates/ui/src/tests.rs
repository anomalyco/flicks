use rune::{App, Env, Key, KeyEvent, Mods, Runtime};

use super::*;
use library::fixture;

/// The app as its window runs it (its `on_key`, image cache and titlebar), in a scene.
fn runtime(scene: &str) -> Runtime<Flicks> {
    let state = rune::tooling::initial_state(&spec(), Some(scene));
    let app = App::new(spec()).init(move || state);
    let mut rt = app.headless_runtime((1180.0, 760.0), Env { system_dark: true, scale: 2.0 });
    rt.frame(0.0);
    rt
}

fn press(rt: &mut Runtime<Flicks>, key: &str) {
    let (x, y) = rt.find(key).unwrap_or_else(|| panic!("no {key} on screen"));
    rt.pointer_down(x, y);
    rt.pointer_up(x, y);
    for _ in 0..30 {
        rt.frame(1.0 / 60.0);
    }
}

fn shows(rt: &Runtime<Flicks>, text: &str) -> bool {
    rt.elements().iter().any(|e| !e.exiting && e.text.as_deref() == Some(text))
}

#[test]
fn episodes_group_into_their_show_and_home_has_its_rows() {
    let catalog = fixture::catalog();
    let names: Vec<_> = catalog.groups.iter().map(|g| (g.name.as_str(), g.is_show(), g.titles.len())).collect();
    assert_eq!(names, [("VisionQuest", true, 2), ("Arrival", false, 1)]);
    let show = &catalog.groups[0];
    let episodes: Vec<_> = show.titles.iter().map(|&t| catalog.titles[t].item.episode).collect();
    assert_eq!(episodes, [Some(2), Some(6)], "in episode order");
    let rows: Vec<_> = catalog.rows.iter().map(|r| (r.name.as_str(), r.titles.len())).collect();
    assert_eq!(rows, [("Continue Watching", 1), ("Recently Added", 3), ("TV Shows", 1), ("Movies", 1)]);
    let newest = &catalog.titles[catalog.rows[1].titles[0]];
    assert_eq!(newest.item.episode, Some(6), "Recently Added is newest first");
}

#[test]
fn search_matches_every_word_of_the_title_ignoring_case() {
    let catalog = fixture::catalog();
    assert_eq!(catalog.search("VISION").len(), 2);
    assert_eq!(catalog.search("quest vision").len(), 2);
    assert!(catalog.search("vision deadlock").is_empty());
    assert!(catalog.search("   ").is_empty());
}

#[test]
fn a_tile_opens_its_show_where_play_records_the_episode_and_escape_goes_back() {
    let mut rt = runtime("home");
    assert!(shows(&rt, "Recently Added"));
    let newest = rt.state.catalog.rows[1].titles[0];
    press(&mut rt, &format!("title-{newest}"));
    assert!(matches!(rt.state.screen, Screen::Detail(_)));
    assert!(shows(&rt, "Episode 2") && shows(&rt, "Episode 6"));

    // Play resumes the episode in progress (episode 2), not the tile's.
    press(&mut rt, "play");
    let resumed = rt.state.catalog.groups[0].titles[0];
    let item = rt.state.catalog.titles[resumed].item.id;
    assert_eq!(rt.state.play, Some(PlayChoice { server: "fixture".into(), item }));

    rt.key(KeyEvent { key: Key::Escape, mods: Mods::default() });
    rt.frame(1.0 / 60.0);
    assert_eq!(rt.state.screen, Screen::Home);
}

#[test]
fn a_title_whose_servers_did_not_answer_is_dimmed_and_cannot_play() {
    let mut rt = runtime("home");
    let arrival = rt.state.catalog.titles.iter().position(|t| t.item.title == "Arrival").unwrap();
    let tile = |rt: &Runtime<Flicks>| {
        rt.elements().into_iter().find(|e| e.key.as_deref() == Some(&format!("title-{arrival}")))
    };
    assert!(tile(&rt).unwrap().opacity < 0.5);
    assert!(shows(&rt, "Offline"));

    press(&mut rt, &format!("title-{arrival}"));
    assert!(shows(&rt, "Offline: its server didn't answer."));
    assert!(rt.find("play").is_none());

    // From the cache, before any refresh, nothing is offline yet.
    rt.update(|s, _| {
        s.refreshed = false;
        s.go(Screen::Home);
    });
    for _ in 0..30 {
        rt.frame(1.0 / 60.0);
    }
    assert!(tile(&rt).unwrap().opacity > 0.99);
}

#[test]
fn typing_in_search_shows_the_matching_episodes() {
    let mut rt = runtime("home");
    rt.key(KeyEvent { key: Key::Command('f'), mods: Mods::default() });
    for _ in 0..3 {
        rt.frame(1.0 / 60.0);
    }
    assert_eq!(rt.state.screen, Screen::Search);
    for c in "vision".chars() {
        rt.key(KeyEvent { key: Key::Text(c.to_string()), mods: Mods::default() });
    }
    rt.frame(1.0 / 60.0);
    assert_eq!(rt.state.query.text, "vision");
    assert_eq!(rt.state.results.len(), 2);
    assert!(shows(&rt, "S1 · E2") && shows(&rt, "S1 · E6"));

    rt.key(KeyEvent { key: Key::Text("x".into()), mods: Mods::default() });
    rt.frame(1.0 / 60.0);
    assert!(shows(&rt, "Nothing matches \u{201c}visionx\u{201d}."));
}

#[test]
fn artwork_is_asked_for_once_and_the_oldest_goes_past_the_limit() {
    let (tx, rx) = std::sync::mpsc::channel();
    let mut s = Flicks::new();
    s.network = Some(Box::new(move |request| {
        if let Request::Artwork(_, name) = request {
            let _ = tx.send(name);
        }
    }));
    s.want_artwork("server".into(), "w342-1.jpg".into());
    // Not found: it stays asked for, so search keystrokes don't fetch it again.
    s.artwork("server", "w342-1.jpg", None);
    s.want_artwork("server".into(), "w342-1.jpg".into());
    assert_eq!(rx.try_iter().collect::<Vec<_>>(), ["w342-1.jpg"]);

    for i in 2..=MAX_ARTWORK + 1 {
        s.want_artwork("server".into(), format!("w342-{i}.jpg"));
    }
    assert_eq!(rx.try_iter().count(), MAX_ARTWORK, "past the limit, new artwork is still asked for");
    assert_eq!(s.requested.len(), MAX_ARTWORK);
    assert!(!s.requested.contains(&artwork_key("server", "w342-1.jpg")), "the oldest went");
}

#[test]
fn a_home_row_holds_at_most_60_tiles() {
    let movies = (0..100).map(|i| {
        let item = fixture::item(i, &format!("Movie {i}"), None, i, 90);
        fixture::title(item, None, true)
    });
    let catalog = Catalog::build(movies.collect());
    let movies = catalog.rows.iter().find(|r| r.name == "Movies").unwrap();
    assert_eq!(movies.titles.len(), 60);
}

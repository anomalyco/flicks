use rune::{Env, Key, KeyEvent, Mods, Runtime};

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
fn a_shows_episodes_are_one_title_and_home_has_its_rows() {
    let catalog = fixture::catalog();
    assert_eq!(catalog.titles.len(), 1);
    let show = &catalog.titles[0];
    assert!(show.show);
    let episodes: Vec<_> = show.media.iter().map(|&m| catalog.media[m].item.episode).collect();
    assert_eq!(episodes, [Some(2), Some(6)], "in episode order");
    let rows: Vec<_> = catalog.rows.iter().map(|r| (r.name.as_str(), r.media.len())).collect();
    assert_eq!(
        rows,
        [("Continue Watching", 1), ("Recently Added", 2), ("TV", 1)],
        "one tile for the show in its library"
    );
    let newest = &catalog.media[catalog.rows[1].media[0]];
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
fn a_tile_opens_its_title_where_play_records_the_episode_and_escape_goes_back() {
    let mut rt = runtime("home");
    assert!(shows(&rt, "Recently Added"));
    let newest = rt.state.catalog.rows[1].media[0];
    press(&mut rt, &format!("media-{newest}"));
    assert!(matches!(rt.state.screen, Screen::Detail(_)));
    assert!(shows(&rt, "Episode 2") && shows(&rt, "Episode 6"));

    // Play resumes the episode in progress (episode 2), not the tile's.
    press(&mut rt, "play");
    let resumed = rt.state.catalog.titles[0].media[0];
    let item = rt.state.catalog.media[resumed].item.id;
    assert_eq!(rt.state.play, Some(PlayChoice { server: "fixture".into(), item }));

    rt.key(KeyEvent { key: Key::Escape, mods: Mods::default() });
    rt.frame(1.0 / 60.0);
    assert_eq!(rt.state.screen, Screen::Home);
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

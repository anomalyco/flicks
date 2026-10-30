//! The Flicks app's screens, shared by the desktop app (`apps/desktop`) and the web app
//! (`apps/web`): the library (Home, a title's page, search) and Add Server, drawn with Rune. Each
//! app supplies the network: it answers [`Request`]s by updating the state through its app handle.

#![forbid(unsafe_code)]
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

mod add_server;
pub mod library;
pub mod server;
mod style;
#[cfg(test)]
mod tests;

use std::collections::{HashMap, HashSet, VecDeque};

use rune::{AppSpec, Cursor, Cx, El, Image, ImageCache, Key, KeyEvent, TextEdit, Tx, col, row, spacer, text};

pub use library::catalog::{self, Catalog, GroupKey};

#[derive(Clone, Debug, PartialEq)]
pub enum Screen {
    Home,
    /// A movie's or show's page.
    Detail(GroupKey),
    Search,
    AddServer,
    Servers,
    Server(String, server::Tab),
}

#[derive(Debug, PartialEq)]
pub enum Status {
    /// Nothing to show yet.
    Loading,
    Ready,
    NoServers,
    Failed(String),
}

/// The choice the Play button records until the player screen (FLK-6) takes it.
#[derive(Clone, Debug, PartialEq)]
pub struct PlayChoice {
    pub server: String,
    pub item: i64,
}

/// What the screens ask of the network.
pub enum Request {
    /// Fetch every server's catalog again.
    Refresh,
    /// Pair with the server a `flicks://pair` link names.
    Pair(String),
    /// A poster or backdrop: the server key and the artwork name.
    Artwork(String, String),
    /// A change on a server's admin routes: its key and the change.
    Admin(String, server::Op),
}

/// The most posters and backdrops kept in memory (about 1 MB each, decoded); past it the oldest
/// goes.
const MAX_ARTWORK: usize = 400;

pub struct Flicks {
    pub screen: Screen,
    /// Where Back goes from a title's page.
    pub back: Screen,
    pub status: Status,
    pub catalog: Catalog,
    /// The catalog comes from a refresh, not only the cache, so titles whose servers didn't
    /// answer are offline.
    pub refreshed: bool,
    /// Why the last refresh failed.
    pub error: Option<String>,
    pub query: TextEdit,
    /// Search results, indices into the catalog's media.
    pub results: Vec<usize>,
    pub link: TextEdit,
    pub pairing: bool,
    pub pair_error: Option<String>,
    pub play: Option<PlayChoice>,
    pub servers: server::Servers,
    /// Decoded artwork by `<server key>/<name>`.
    pub artwork: HashMap<String, Image>,
    requested: HashSet<String>,
    /// `requested` oldest first, for evicting.
    order: VecDeque<String>,
    images: Option<ImageCache>,
    /// Sends a request to the app's network, once it has started.
    pub network: Option<Box<dyn Fn(Request)>>,
}

impl Flicks {
    pub fn new() -> Flicks {
        Flicks {
            screen: Screen::Home,
            back: Screen::Home,
            status: Status::Loading,
            catalog: Catalog::default(),
            refreshed: false,
            error: None,
            query: TextEdit::new(""),
            results: Vec::new(),
            link: TextEdit::new(""),
            pairing: false,
            pair_error: None,
            play: None,
            servers: server::Servers::default(),
            artwork: HashMap::new(),
            requested: HashSet::new(),
            order: VecDeque::new(),
            images: None,
            network: None,
        }
    }

    pub fn go(&mut self, screen: Screen) {
        self.servers.link = None;
        if let Screen::Detail(key) = &screen {
            if !matches!(self.screen, Screen::Detail(_)) {
                self.back = self.screen.clone();
            }
            self.want_backdrop(key);
        }
        if screen == Screen::Search {
            self.query.focused = true;
        }
        self.screen = screen;
    }

    fn go_back(&mut self) {
        let back = std::mem::replace(&mut self.back, Screen::Home);
        self.go(back);
    }

    pub fn search_changed(&mut self) {
        self.results = self.catalog.search(&self.query.text);
        for m in self.results.clone() {
            self.want_poster(m);
        }
    }

    pub fn pair(&mut self) {
        let link = self.link.text.trim().to_string();
        if link.is_empty() || self.pairing {
            return;
        }
        self.pair_error = None;
        match &self.network {
            Some(send) => {
                self.pairing = true;
                send(Request::Pair(link));
            }
            None => self.pair_error = Some("Not connected to the network yet.".into()),
        }
    }

    pub fn play(&mut self, title: usize) {
        let source = self.catalog.titles.get(title).and_then(flicks_client::merge::best_source);
        if let Some(source) = source {
            self.play = Some(PlayChoice { server: source.server.clone(), item: source.item.id });
        }
    }

    /// Whether a title is shown as offline: a refresh found none of its servers.
    pub fn offline(&self, title: usize) -> bool {
        self.refreshed && self.catalog.titles.get(title).is_some_and(|t| !t.available())
    }

    // Answers from the network thread.

    pub fn failed(&mut self, message: String) {
        self.status = Status::Failed(message);
    }

    pub fn no_servers(&mut self, tx: &mut Tx) {
        tx.instant();
        self.status = Status::NoServers;
        self.screen = Screen::AddServer;
    }

    /// A catalog from the cache (`refreshed` false, at launch) or a refresh.
    pub fn loaded(&mut self, catalog: Catalog, error: Option<String>, refreshed: bool, tx: &mut Tx) {
        if self.status == Status::Loading || !self.refreshed {
            // The cache and the first refresh land still, as part of launch.
            tx.instant();
        }
        if refreshed {
            // Artwork that had no connection before is worth asking for again.
            self.requested.retain(|key| self.artwork.contains_key(key));
            self.order.retain(|key| self.artwork.contains_key(key));
        }
        self.catalog = catalog;
        self.refreshed = refreshed;
        self.error = error;
        self.status = Status::Ready;
        self.search_changed();
        let shown: Vec<usize> = self.catalog.rows.iter().flat_map(|r| r.titles.iter().copied()).collect();
        for t in shown {
            self.want_poster(t);
        }
        if let Screen::Detail(key) = &self.screen {
            let key = key.clone();
            self.want_backdrop(&key);
        }
    }

    pub fn paired(&mut self, result: Result<String, String>) {
        self.pairing = false;
        match result {
            Ok(_) => {
                self.link.replace_text("");
                self.status = Status::Loading;
                self.screen = Screen::Home;
            }
            Err(error) => self.pair_error = Some(error),
        }
    }

    pub fn artwork(&mut self, server: &str, name: &str, bytes: Option<Vec<u8>>) {
        let key = artwork_key(server, name);
        // A failed fetch stays requested, so typing in search doesn't ask again; the next refresh
        // does. Artwork evicted while on its way is dropped.
        if let (Some(bytes), Some(images)) = (bytes, &self.images)
            && self.requested.contains(&key)
        {
            self.artwork.insert(key, images.load(bytes));
        }
    }

    fn want_poster(&mut self, title: usize) {
        let Some(source) = self.catalog.titles.get(title).and_then(catalog::source) else { return };
        if let Some(name) = source.item.poster.clone() {
            let server = source.server.clone();
            self.want_artwork(server, name);
        }
    }

    fn want_backdrop(&mut self, group: &GroupKey) {
        let first = self.catalog.group(group).and_then(|g| g.titles.first().copied());
        let Some(source) = first.and_then(|t| self.catalog.titles.get(t)).and_then(catalog::source) else { return };
        if let Some(name) = source.item.backdrop.clone().or_else(|| source.item.poster.clone()) {
            let server = source.server.clone();
            self.want_artwork(server, name);
        }
    }

    /// Asks for artwork once, evicting the oldest past [`MAX_ARTWORK`].
    fn want_artwork(&mut self, server: String, name: String) {
        let key = artwork_key(&server, &name);
        if self.network.is_none() || self.requested.contains(&key) {
            return;
        }
        if self.order.len() >= MAX_ARTWORK
            && let Some(oldest) = self.order.pop_front()
        {
            self.requested.remove(&oldest);
            self.artwork.remove(&oldest);
        }
        self.requested.insert(key.clone());
        self.order.push_back(key);
        if let Some(send) = &self.network {
            send(Request::Artwork(server, name));
        }
    }
}

pub fn artwork_key(server: &str, name: &str) -> String {
    format!("{server}/{name}")
}

fn view(s: &Flicks, cx: &mut Cx<Flicks>) -> El {
    let body = match &s.screen {
        Screen::Home => library::home(s),
        Screen::Detail(key) => library::detail(s, key),
        Screen::Search => library::search(s),
        Screen::AddServer => add_server::view(s),
        Screen::Servers => server::list(s),
        Screen::Server(key, tab) => server::view(s, key, *tab),
    };
    let mut page = col().w_full().h_full().fill(style::bg()).child(top_bar(s, cx));
    if let Some(error) = &s.error {
        let line = format!("Couldn't refresh the library: {error}");
        page = page.child(text(line).font(style::SMALL).color(style::error()).px(style::GUTTER).pb(8.0).truncate());
    }
    page.child(body.w_full().grow(1.0).min_h(0.0))
}

fn top_bar(s: &Flicks, cx: &mut Cx<Flicks>) -> El {
    let nav = |label: &'static str, key: &'static str, screen: Screen| {
        let on = s.screen == screen;
        text(label)
            .key(key)
            .font(style::NAV)
            .color(if on { style::text() } else { style::muted() })
            .py(6.0)
            .px(10.0)
            .radius(6.0)
            .hover(style::faint())
            .cursor(Cursor::Pointer)
            .focusable(true)
            .on_press(move |s: &mut Flicks| s.go(screen.clone()))
    };
    let left = (cx.titlebar_inset() + 16.0).max(style::GUTTER);
    let servers = s.servers.any_admin().then(|| nav("Servers", "nav-servers", Screen::Servers));
    row()
        .key("top-bar")
        .w_full()
        .h(56.0)
        .pl(left)
        .pr(style::GUTTER - 10.0)
        .gap(6.0)
        .center_y()
        .window_drag()
        .child(text("FLICKS").font(style::BRAND).color(style::accent()).mr(18.0))
        .child(nav("Home", "nav-home", Screen::Home))
        .child(nav("Search", "nav-search", Screen::Search))
        .child(spacer())
        .children(servers)
        .child(nav("Add Server", "nav-add-server", Screen::AddServer))
}

fn on_key(s: &mut Flicks, event: &KeyEvent, _: &mut Tx) -> bool {
    match event.key {
        Key::Command('f') => s.go(Screen::Search),
        Key::Escape if matches!(s.screen, Screen::Detail(_)) => s.go_back(),
        _ => return false,
    }
    true
}

/// A message in the middle of a screen.
pub fn message(title: &str, detail: &str) -> El {
    col()
        .w_full()
        .h_full()
        .center()
        .gap(8.0)
        .child(text(title).font(style::HEADING).color(style::text()))
        .child(text(detail).font(style::BODY).color(style::muted()))
}

impl Default for Flicks {
    fn default() -> Flicks {
        Flicks::new()
    }
}

/// The app: its window, scenes for headless renders, image cache and keys.
pub fn spec() -> AppSpec<Flicks> {
    AppSpec {
        min_size: Some((720.0, 480.0)),
        scenes: library::fixture::SCENES,
        setup: Some(|s, waker| s.images = Some(ImageCache::new(waker))),
        on_key: Some(on_key),
        ..AppSpec::new("Flicks", (1180.0, 760.0), Flicks::new, view)
    }
}

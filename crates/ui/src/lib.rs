//! The Flicks app's screens, shared by the desktop app (`apps/desktop`) and the web app
//! (`apps/web`): the library (Home, a title's page, search) and Add Server, drawn with Rune. Each
//! app supplies the network: it answers [`Request`]s by updating the state through its app handle.

#![forbid(unsafe_code)]
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

mod add_server;
pub mod intro;
pub mod library;
pub mod player;
pub mod server;
mod style;
#[cfg(test)]
mod tests;

use std::collections::{HashMap, HashSet, VecDeque};

use rune::{
    Anim, AppSpec, Color, Cursor, Cx, El, Image, ImageCache, Key, KeyEvent, SpringPair, SurfaceFit, TextEdit,
    Transition, Tx, col, el, row, spacer, text,
};

pub use intro::{Intro, Sound};
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
    /// Play a title: the play's number (see [`Flicks::player_started`]) and the title.
    Play(u64, Box<flicks_client::merge::Title>),
    /// Save progress on a title: its key, and the server and file played.
    Progress(flicks_client::merge::Key, String, i64, flicks_client::Progress),
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
    /// The player, over every screen while it's open.
    pub player: Option<player::Screen>,
    /// Plays started, numbering each.
    plays: u64,
    /// The tile the pointer is over, which lifts (its key).
    pub lifted: Option<String>,
    /// The season a show's page lists.
    pub season: Option<u16>,
    /// Where each Home row is scrolled to, for its arrows: row key to x offset. The rows' scroll
    /// views write it as they move, without re-running the view.
    pub row_x: std::rc::Rc<std::cell::RefCell<HashMap<String, f32>>>,
    pub servers: server::Servers,
    /// Decoded artwork by `<server key>/<name>`.
    pub artwork: HashMap<String, Image>,
    requested: HashSet<String>,
    /// `requested` oldest first, for evicting.
    order: VecDeque<String>,
    images: Option<ImageCache>,
    /// The lockup, rasterized once the image cache starts.
    logo: Option<Image>,
    /// Sends a request to the app's network, once it has started.
    pub network: Option<Box<dyn Fn(Request)>>,
    /// The launch intro, over Home until it ends; the app shell sets it on a cold launch.
    pub intro: Option<Intro>,
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
            player: None,
            plays: 0,
            lifted: None,
            season: None,
            row_x: Default::default(),
            servers: server::Servers::default(),
            artwork: HashMap::new(),
            requested: HashSet::new(),
            order: VecDeque::new(),
            images: None,
            logo: None,
            network: None,
            intro: None,
        }
    }

    pub fn go(&mut self, screen: Screen) {
        self.servers.link = None;
        if let Screen::Detail(key) = &screen {
            if !matches!(self.screen, Screen::Detail(_)) {
                self.back = self.screen.clone();
            }
            self.want_backdrop(key);
            if self.screen != screen {
                self.season = None;
            }
        }
        self.lifted = None;
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
        self.open_player(title);
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
    if let Some(player) = &s.player {
        let screen = el().key("screen-player").w_full().h_full().transition(screen_transition());
        return screen.child(player::view(player, cx));
    }
    // Home and a title's page run full-bleed under the top bar; the rest start below it.
    let (name, body, hero) = match &s.screen {
        Screen::Home => ("home", library::home(s, cx), Some("hero")),
        Screen::Detail(key) => ("detail", library::detail(s, key), Some("detail-hero")),
        Screen::Search => ("search", library::search(s), None),
        Screen::AddServer => ("add-server", add_server::view(s), None),
        Screen::Servers => ("servers", server::list(s), None),
        Screen::Server(key, tab) => ("server", server::view(s, key, *tab), None),
    };
    // The bar turns solid once the hero has scrolled up under it.
    let solid = hero.and_then(|key| cx.layout_with(key, |r| r[1] + r[3] < style::BAR_H + 160.0)).unwrap_or(true);
    let body =
        if hero.is_some() { body } else { col().w_full().h_full().pt(style::BAR_H).child(body.grow(1.0).min_h(0.0)) };
    let screen = el()
        .key(format!("screen-{name}"))
        .absolute(0.0, 0.0)
        .w_full()
        .h_full()
        .transition(screen_transition())
        .child(body.w_full().h_full());
    let mut page = el().key("root").w_full().h_full().fill(style::bg()).child(screen).child(top_bar(s, cx, solid));
    if let Some(error) = &s.error {
        let line = format!("Couldn't refresh the library: {error}");
        page = page.child(
            text(line)
                .key("refresh-error")
                .absolute(style::GUTTER, style::BAR_H)
                .font(style::SMALL)
                .color(style::error())
                .truncate(),
        );
    }
    match &s.intro {
        Some(intro) => {
            page.child(intro::view(intro, cx)).on_window_focus(|s: &mut Flicks, focused, _| intro::focus(s, focused))
        }
        None => page,
    }
}

/// A screen replacing another: the new one fades up from slightly small, the old one fades out
/// growing slightly, both on one spring.
fn screen_transition() -> Transition {
    let enter = Transition::opacity().combined(Transition::scale(0.97));
    let exit = Transition::opacity().combined(Transition::scale(1.02));
    Transition::asymmetric(enter, exit).springs(SpringPair::new(style::SCREEN_SPRING, style::SCREEN_SPRING))
}

/// The bar floating over every screen: frosted over a hero, solid once the page scrolls.
fn top_bar(s: &Flicks, cx: &mut Cx<Flicks>, solid: bool) -> El {
    let nav = |label: &'static str, key: &'static str, screen: Screen| {
        let on = s.screen == screen;
        text(label)
            .key(key)
            .font(style::NAV)
            .color(if on { style::text() } else { style::text().alpha(0.7) })
            .fill(if on { style::glass() } else { Color::TRANSPARENT })
            .py(7.0)
            .px(14.0)
            .radius(16.0)
            .hover(style::faint())
            .cursor(Cursor::Pointer)
            .focusable(true)
            // The bar comes after the screen in the tree, to draw over it, but first for Tab.
            .tab_index(1)
            .on_press(move |s: &mut Flicks| s.go(screen.clone()))
    };
    let left = (cx.titlebar_inset() + 16.0).max(style::GUTTER);
    let servers = s.servers.any_admin().then(|| nav("Servers", "nav-servers", Screen::Servers));
    // Behind the bar: frosted glass, lightly tinted over a hero, and the page colour over it
    // once the page scrolls (its opacity springs).
    let frost = el().absolute(0.0, 0.0).w_full().h_full().backdrop_blur(30.0).fill(style::bg().alpha(0.22));
    let fill =
        el().absolute(0.0, 0.0).w_full().h_full().fill(style::bg().alpha(0.94)).opacity(if solid { 1.0 } else { 0.0 });
    row()
        .key("top-bar")
        .absolute(0.0, 0.0)
        .w_full()
        .h(style::BAR_H)
        .pl(left)
        .pr(style::GUTTER - 14.0)
        .gap(4.0)
        .center_y()
        .window_drag()
        .child(frost)
        .child(fill)
        .child(logo(s).opacity(if intro::hides_logo(s, cx) { 0.0 } else { 1.0 }).opacity_motion(Anim::INSTANT).mr(22.0))
        .child(nav("Home", "nav-home", Screen::Home))
        .child(nav("Search", "nav-search", Screen::Search))
        .child(spacer())
        .children(servers)
        .child(nav("Add Server", "nav-add-server", Screen::AddServer))
}

/// The lockup, drawn from its SVG at three device pixels a point so it stays sharp on every
/// screen.
const LOCKUP: &[u8] = include_bytes!("../../../assets/brand/lockup.svg");

fn logo(s: &Flicks) -> El {
    let mut logo = el().key("logo").size(style::LOGO_W, style::LOGO_H).label("Flicks");
    if let Some(image) = &s.logo {
        logo = logo.pixels_fit(image.pixels(), SurfaceFit::Fit);
    }
    logo
}

fn on_key(s: &mut Flicks, event: &KeyEvent, _: &mut Tx) -> bool {
    if s.intro.is_some() {
        intro::skip(s);
        return true;
    }
    if s.player.is_some() {
        return player::on_key(s, event);
    }
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
        setup: Some(|s, waker| {
            let images = ImageCache::new(waker);
            s.logo = Some(images.svg(LOCKUP, style::LOGO_W, 3.0));
            if let Some(intro) = &mut s.intro {
                intro.load(&images);
            }
            s.images = Some(images);
        }),
        poll: Some(intro::poll),
        on_key: Some(on_key),
        ..AppSpec::new("Flicks", (1180.0, 760.0), Flicks::new, view)
    }
}

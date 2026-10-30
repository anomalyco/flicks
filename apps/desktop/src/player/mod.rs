//! The player screen: the video fills the window (drawn by the compositor in [`video`]), with
//! subtitles as Rune text over it and controls that hide after a couple of seconds without the
//! pointer moving. The [`engine`] thread does the playing; this screen sends it commands and
//! shows what it reports. Progress goes to the library through the network thread.

mod engine;
pub mod video;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use flicks_client::Progress;
use flicks_client::download::Downloads;
use flicks_client::merge::{self, Key, Library, Title};
use flicks_player::{Cue, Source};
use rune::{
    Align, AppHandle, Color, Cursor, Cx, Drag, El, Key as KeyName, KeyEvent, Phase, Span, Tx, col, el, rich, row, text,
};

pub use engine::Command;

use crate::library::catalog::Catalog;
use crate::{Flicks, style, worker};

/// How long the controls stay after the pointer stops moving, while playing.
const CONTROLS_FOR: Duration = Duration::from_millis(2500);
/// The arrow keys seek this far.
const ARROW_SEEK: f64 = 10.0;

/// An audio or subtitle track the menus offer.
#[derive(Clone, Debug, PartialEq)]
pub struct Track {
    pub id: usize,
    pub audio: bool,
    pub label: String,
}

/// What the playback thread last reported.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Shown {
    /// Whole seconds played, so the screen updates once a second.
    pub second: u64,
    pub playing: bool,
    pub audio: Option<usize>,
    pub subtitle: Option<usize>,
    /// The name of a loaded subtitle file, shown instead of `subtitle`.
    pub sidecar: Option<String>,
    pub cues: Vec<Cue>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Menu {
    Audio,
    Subtitles,
}

pub struct Screen {
    commands: Sender<Command>,
    name: String,
    /// The title being played, for its progress.
    key: Key,
    pub server: String,
    pub file: i64,
    opening: bool,
    error: Option<String>,
    /// A message that doesn't stop playback (a subtitle file that didn't load).
    notice: Option<String>,
    duration: f64,
    position: f64,
    tracks: Vec<Track>,
    shown: Shown,
    /// Where the seek bar is being dragged to, in seconds.
    scrub: Option<f64>,
    controls: bool,
    moved: Instant,
    menu: Option<Menu>,
}

impl Screen {
    fn send(&self, command: Command) {
        // The thread is gone only after it failed, which the screen already shows.
        let _ = self.commands.send(command);
    }

    fn show_controls(&mut self) -> bool {
        self.moved = Instant::now();
        !std::mem::replace(&mut self.controls, true)
    }
}

impl Flicks {
    /// Opens the player on a title, from where it was left.
    pub fn open_player(&mut self, title: usize) {
        let Some(t) = self.catalog.titles.get(title) else { return };
        let Some(source) = merge::best_source(t) else { return };
        let mut name = t.item.title.clone();
        if let (Some(season), Some(episode)) = (t.item.season, t.item.episode) {
            name = format!("{name}  ·  S{season} E{episode}");
        }
        let (commands, rx) = mpsc::channel();
        self.player = Some(Screen {
            commands,
            name,
            key: t.key.clone(),
            server: source.server.clone(),
            file: source.item.id,
            opening: self.worker.is_some(),
            error: self.worker.is_none().then(|| "Not connected to the network yet.".into()),
            notice: None,
            duration: t.item.duration_ms.map_or(0.0, |ms| ms as f64 / 1000.0),
            position: 0.0,
            tracks: Vec::new(),
            shown: Shown::default(),
            scrub: None,
            controls: true,
            moved: Instant::now(),
            menu: None,
        });
        if let Some(worker) = &self.worker {
            worker.send(worker::Request::Play(Box::new(t.clone()), rx));
        }
    }

    pub fn close_player(&mut self) {
        if let Some(player) = self.player.take() {
            player.send(Command::Stop);
        }
    }

    fn player_opened(&mut self, tracks: Vec<Track>, duration: f64) {
        let Some(p) = &mut self.player else { return };
        p.opening = false;
        p.tracks = tracks;
        p.duration = duration;
    }

    fn player_shown(&mut self, shown: Shown, position: f64) {
        let Some(p) = &mut self.player else { return };
        p.opening = false;
        p.position = position;
        if !shown.playing {
            p.controls = true;
        } else if p.controls && p.menu.is_none() && p.scrub.is_none() && p.moved.elapsed() >= CONTROLS_FOR {
            p.controls = false;
        }
        p.shown = shown;
    }

    fn player_failed(&mut self, error: String) {
        if let Some(p) = &mut self.player {
            p.opening = false;
            p.controls = true;
            p.error = Some(error);
        }
    }

    fn player_notice(&mut self, notice: String) {
        if let Some(p) = &mut self.player {
            p.notice = Some(notice);
        }
    }

    fn save_progress(&mut self, progress: Progress) {
        let (Some(p), Some(worker)) = (&self.player, &self.worker) else { return };
        worker.send(worker::Request::Progress(p.key.clone(), p.server.clone(), p.file, progress));
    }

    /// The library after a progress save: Continue Watching and the title pages show it.
    pub fn progress_saved(&mut self, catalog: Catalog, tx: &mut Tx) {
        let error = self.error.take();
        self.loaded(catalog, error, self.refreshed, tx);
    }
}

/// On the network thread: picks the copy to play (a download first), where to resume, and
/// starts the playback thread.
pub fn open(
    app: &AppHandle<Flicks>,
    library: &Library,
    downloads: Option<&Downloads>,
    title: &Title,
    rx: Receiver<Command>,
) {
    let local = downloads.and_then(|d| d.local_path(title));
    let source = match (local, merge::best_source(title)) {
        (Some(path), _) => Source::Local(path),
        (None, Some(best)) => match library.connection(&best.server) {
            Some(connection) => {
                Source::Network { connection: Arc::new(connection.clone()), file: best.item.id, size: best.item.size }
            }
            None => return app.update(|s, _| s.player_failed("Its server isn't answering.".into())),
        },
        (None, None) => return app.update(|s, _| s.player_failed("Its server isn't answering.".into())),
    };
    let progress = downloads.and_then(|d| d.progress(title)).or(title.progress);
    let resume = progress.filter(|p| !p.played).map_or(0.0, |p| p.position as f64 / 1000.0);
    engine::start(app.clone(), source, resume, rx);
}

/// On the network thread: saves progress to the title's servers, or keeps it with the download
/// to send later when none of them answers.
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
    let catalog = Catalog::build(library.titles().to_vec());
    app.update(move |s, tx| s.progress_saved(catalog, tx));
}

pub fn on_key(s: &mut Flicks, event: &KeyEvent) -> bool {
    let Some(p) = &mut s.player else { return false };
    match &event.key {
        KeyName::Escape if p.menu.is_some() => p.menu = None,
        KeyName::Escape => s.close_player(),
        KeyName::Text(t) if t == " " => p.send(Command::Toggle),
        KeyName::Left => p.send(Command::SeekBy(-ARROW_SEEK)),
        KeyName::Right => p.send(Command::SeekBy(ARROW_SEEK)),
        _ => return false,
    }
    if let Some(p) = &mut s.player {
        p.show_controls();
    }
    true
}

pub fn view(p: &Screen, cx: &mut Cx<Flicks>) -> El {
    let mut screen = col()
        .key("player")
        .w_full()
        .h_full()
        .fill(Color::BLACK)
        .child(el().key("video").effect(video::MARK).absolute(0.0, 0.0).w_full().h_full().on_press(|s: &mut Flicks| {
            if let Some(p) = &s.player {
                p.send(Command::Toggle);
            }
        }))
        .on_pointer_move(|s: &mut Flicks, _| {
            if let Some(p) = &mut s.player {
                p.show_controls();
            }
        })
        .on_drop_files(|s: &mut Flicks, paths: &[PathBuf], _: &mut Tx| {
            let Some(p) = &mut s.player else { return };
            if let Some(path) = paths.first() {
                p.notice = None;
                p.send(Command::Sidecar(path.clone()));
            }
        });
    if !p.controls {
        screen = screen.cursor(Cursor::Hidden);
    }
    if let Some(message) = p.error.as_deref().or(p.opening.then_some("Opening…")) {
        screen = screen.child(
            col()
                .absolute(0.0, 0.0)
                .w_full()
                .h_full()
                .center()
                .child(text(message.to_string()).font(style::HEADING).color(style::text())),
        );
    }
    screen = screen.child(subtitles(&p.shown.cues, p.controls));
    if p.controls {
        screen = screen.child(top(p, cx)).child(bottom(p));
        if let Some(menu) = p.menu {
            screen = screen.child(menu_panel(p, menu));
        }
    }
    screen
}

fn subtitles(cues: &[Cue], controls: bool) -> El {
    let mut lines = col().gap(2.0).items_center();
    for cue in cues {
        for line in &cue.lines {
            let mut content = String::new();
            let mut spans = Vec::new();
            for span in line {
                let start = content.len();
                content.push_str(&span.text);
                let mut styled = Span::new(start, content.len());
                if span.italic {
                    styled = styled.italic();
                }
                if span.bold {
                    styled = styled.bold();
                }
                if span.underline {
                    styled = styled.underline(Color::WHITE);
                }
                spans.push(styled);
            }
            lines = lines.child(
                rich(content, spans)
                    .font(SUBTITLE)
                    .color(Color::WHITE)
                    .align(Align::Center)
                    .px(10.0)
                    .py(2.0)
                    .radius(4.0)
                    .fill(Color::BLACK.alpha(0.6)),
            );
        }
    }
    // Above the controls while they show.
    let bottom = if controls { 120.0 } else { 56.0 };
    col().key("subtitles").pin(Some(48.0), None, Some(48.0), Some(bottom)).items_center().child(lines)
}

const SUBTITLE: rune::TextStyle = rune::TextStyle::new(26.0, 550.0).lh(34.0);
const TIME: rune::TextStyle = rune::TextStyle::new(13.0, 550.0).features("'tnum' 1");

fn top(p: &Screen, cx: &mut Cx<Flicks>) -> El {
    let left = cx.titlebar_inset() + 16.0;
    row()
        .key("player-top")
        .pin(Some(0.0), Some(0.0), Some(0.0), None)
        .h(64.0)
        .pl(left.max(style::GUTTER))
        .pr(style::GUTTER)
        .gap(14.0)
        .center_y()
        .gradient(Color::BLACK.alpha(0.6), Color::BLACK.alpha(0.0))
        .window_drag()
        .child(button("player-back", "‹  Back", Flicks::close_player))
        .child(text(p.name.clone()).font(style::HEADING).color(style::text()).truncate())
}

fn bottom(p: &Screen) -> El {
    let position = p.scrub.unwrap_or(p.position);
    let fraction = if p.duration > 0.0 { (position / p.duration).clamp(0.0, 1.0) as f32 } else { 0.0 };
    let duration = p.duration;
    let seek = el()
        .key("seek")
        .grow(1.0)
        .h(20.0)
        .center_y()
        .cursor(Cursor::Pointer)
        .child(
            el().w_full()
                .h(4.0)
                .radius(2.0)
                .fill(Color::WHITE.alpha(0.25))
                .child(el().w_frac(fraction).h_full().radius(2.0).fill(style::accent())),
        )
        .on_drag(move |s: &mut Flicks, drag: &mut Drag, _: &mut Tx| {
            let Some(p) = &mut s.player else { return };
            let at = (drag.x / drag.w.max(1.0)).clamp(0.0, 1.0) as f64 * duration;
            p.show_controls();
            if drag.phase == Phase::Up {
                p.scrub = None;
                if !drag.cancelled() {
                    p.position = at;
                    p.send(Command::Seek(at));
                }
            } else {
                p.scrub = Some(at);
            }
        });
    let mut bar = row()
        .key("player-controls")
        .w_full()
        .gap(16.0)
        .center_y()
        .child(button("toggle", if p.shown.playing { "Pause" } else { "Play" }, |s| {
            if let Some(p) = &s.player {
                p.send(Command::Toggle);
            }
        }))
        .child(text(clock(position)).font(TIME).color(style::text()))
        .child(seek)
        .child(text(clock(p.duration)).font(TIME).color(style::muted()));
    if p.tracks.iter().filter(|t| t.audio).count() > 1 {
        bar = bar.child(menu_button("audio-menu", "Audio", Menu::Audio));
    }
    bar = bar.child(menu_button("subtitle-menu", "Subtitles", Menu::Subtitles));
    let mut panel = col()
        .key("player-bottom")
        .pin(Some(0.0), None, Some(0.0), Some(0.0))
        .px(style::GUTTER)
        .pt(40.0)
        .pb(24.0)
        .gap(10.0)
        .gradient(Color::BLACK.alpha(0.0), Color::BLACK.alpha(0.7));
    if let Some(notice) = &p.notice {
        panel = panel.child(text(notice.clone()).font(style::SMALL).color(style::error()));
    }
    panel.child(bar)
}

fn menu_panel(p: &Screen, menu: Menu) -> El {
    let mut options: Vec<(String, bool, Command)> = Vec::new();
    match menu {
        Menu::Audio => {
            for t in p.tracks.iter().filter(|t| t.audio) {
                options.push((t.label.clone(), p.shown.audio == Some(t.id), Command::Audio(t.id)));
            }
        }
        Menu::Subtitles => {
            let off = p.shown.subtitle.is_none() && p.shown.sidecar.is_none();
            options.push(("Off".into(), off, Command::Subtitle(None)));
            for t in p.tracks.iter().filter(|t| !t.audio) {
                let on = p.shown.sidecar.is_none() && p.shown.subtitle == Some(t.id);
                options.push((t.label.clone(), on, Command::Subtitle(Some(t.id))));
            }
            if let Some(name) = &p.shown.sidecar {
                options.push((name.clone(), true, Command::Subtitle(None)));
            }
        }
    }
    let mut list = col()
        .key("player-menu")
        .pin(None, None, Some(style::GUTTER), Some(76.0))
        .w(260.0)
        .padding(6.0)
        .radius(10.0)
        .fill(style::raised().alpha(0.96))
        .shadow(8.0, 24.0, Color::BLACK.alpha(0.5));
    for (i, (label, on, command)) in options.into_iter().enumerate() {
        let command = std::cell::Cell::new(Some(command));
        let pick = move |s: &mut Flicks| {
            let Some(p) = &mut s.player else { return };
            if let Some(command) = command.take() {
                p.send(command);
            }
            p.menu = None;
        };
        list = list.child(
            row()
                .key(format!("menu-{i}"))
                .h(34.0)
                .px(10.0)
                .gap(10.0)
                .center_y()
                .radius(6.0)
                .hover(style::faint())
                .cursor(Cursor::Pointer)
                .child(text(if on { "✓" } else { "" }).font(style::BODY).color(style::accent()).w(14.0))
                .child(text(label).font(style::BODY).color(style::text()).truncate())
                .on_press(pick),
        );
    }
    if menu == Menu::Subtitles {
        list = list.child(
            text("Drop an .srt or .vtt file on the video to add one.")
                .font(style::SMALL)
                .color(style::muted())
                .px(10.0)
                .py(8.0)
                .wrap(),
        );
    }
    list
}

fn button(key: &'static str, label: &'static str, press: impl Fn(&mut Flicks) + 'static) -> El {
    text(label)
        .key(key)
        .font(style::NAV)
        .color(style::text())
        .py(6.0)
        .px(10.0)
        .radius(6.0)
        .hover(style::faint())
        .cursor(Cursor::Pointer)
        .on_press(press)
}

fn menu_button(key: &'static str, label: &'static str, menu: Menu) -> El {
    button(key, label, move |s| {
        if let Some(p) = &mut s.player {
            p.menu = if p.menu == Some(menu) { None } else { Some(menu) };
            p.show_controls();
        }
    })
}

/// `1:02:03` or `2:03`.
fn clock(seconds: f64) -> String {
    let s = seconds.max(0.0) as u64;
    let (h, m, s) = (s / 3600, s / 60 % 60, s % 60);
    if h > 0 { format!("{h}:{m:02}:{s:02}") } else { format!("{m}:{s:02}") }
}

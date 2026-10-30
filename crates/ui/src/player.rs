//! The player screen: the video fills the window (drawn by the app at the [`MARK`] effect
//! slot), with subtitles as Rune text over it and controls that hide after a couple of seconds
//! without the pointer moving. The app plays: [`Request::Play`] starts it, and it answers with
//! [`Flicks::player_started`] (where to send [`Command`]s) and reports through the other
//! `player_*` methods. Progress goes back to the app as [`Request::Progress`].

use std::path::PathBuf;
use std::time::{Duration, Instant};

use flicks_client::Progress;
use flicks_client::merge::{self, Key};
use flicks_subtitle::Cue;
use rune::{
    Align, Color, Cursor, Cx, Drag, El, Key as KeyName, KeyEvent, Phase, Span, Transition, Tx, col, el, rich, row, text,
};

use crate::{Catalog, Flicks, Request, style};

/// The video's effect slot (`El::effect`), where the app's compositor draws frames.
pub const MARK: u32 = 0x464c_4b56;

/// How long the controls stay after the pointer stops moving, while playing.
const CONTROLS_FOR: Duration = Duration::from_millis(2500);
/// The arrow keys seek this far.
const ARROW_SEEK: f64 = 10.0;

/// What the screen asks of the playing app.
pub enum Command {
    Toggle,
    /// Seek to seconds.
    Seek(f64),
    /// Seek by seconds from where it is.
    SeekBy(f64),
    Audio(usize),
    /// A subtitle track, or `None` for off.
    Subtitle(Option<usize>),
    /// An SRT or WebVTT file to show instead of the file's own subtitles.
    Sidecar(PathBuf),
    Stop,
}

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
    /// Waiting on data for a moment or longer.
    pub buffering: bool,
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
    /// Which play this is: reports from an earlier one are ignored.
    id: u64,
    /// Where commands go, once the app has started playing.
    commands: Option<Box<dyn Fn(Command)>>,
    name: String,
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
    /// The pointer is over the controls: they stay.
    over_controls: bool,
    menu: Option<Menu>,
}

impl Screen {
    fn send(&self, command: Command) {
        if let Some(send) = &self.commands {
            send(command);
        }
    }

    fn show_controls(&mut self) {
        self.moved = Instant::now();
        self.controls = true;
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
        self.plays += 1;
        let connected = self.network.is_some();
        self.player = Some(Screen {
            id: self.plays,
            commands: None,
            name,
            server: source.server.clone(),
            file: source.item.id,
            opening: connected,
            error: (!connected).then(|| "Not connected to the network yet.".into()),
            notice: None,
            duration: t.item.duration_ms.map_or(0.0, |ms| ms as f64 / 1000.0),
            position: 0.0,
            tracks: Vec::new(),
            shown: Shown::default(),
            scrub: None,
            controls: true,
            moved: Instant::now(),
            over_controls: false,
            menu: None,
        });
        if let Some(send) = &self.network {
            send(Request::Play(self.plays, Box::new(t.clone())));
        }
    }

    pub fn close_player(&mut self) {
        if let Some(player) = self.player.take() {
            player.send(Command::Stop);
        }
    }

    fn playing(&mut self, id: u64) -> Option<&mut Screen> {
        self.player.as_mut().filter(|p| p.id == id)
    }

    // Answers from the app that plays.

    /// Play `id` started: commands go to `commands`. A player closed meanwhile stops it.
    pub fn player_started(&mut self, id: u64, commands: Box<dyn Fn(Command)>) {
        match self.playing(id) {
            Some(p) => p.commands = Some(commands),
            None => commands(Command::Stop),
        }
    }

    pub fn player_opened(&mut self, id: u64, tracks: Vec<Track>, duration: f64) {
        let Some(p) = self.playing(id) else { return };
        p.opening = false;
        p.tracks = tracks;
        p.duration = duration;
    }

    pub fn player_shown(&mut self, id: u64, shown: Shown, position: f64) {
        let Some(p) = self.playing(id) else { return };
        p.opening = false;
        p.position = position;
        if !shown.playing || !p.shown.playing {
            // Paused, or just started playing: the controls show for a while.
            p.show_controls();
        } else if p.controls
            && p.menu.is_none()
            && p.scrub.is_none()
            && !p.over_controls
            && p.moved.elapsed() >= CONTROLS_FOR
        {
            p.controls = false;
        }
        p.shown = shown;
    }

    /// Playback ended: back to where Play was pressed.
    pub fn player_ended(&mut self, id: u64) {
        if self.playing(id).is_some() {
            self.close_player();
        }
    }

    pub fn player_failed(&mut self, id: u64, error: String) {
        let Some(p) = self.playing(id) else { return };
        p.opening = false;
        p.controls = true;
        p.error = Some(error);
    }

    pub fn player_notice(&mut self, id: u64, notice: String) {
        if let Some(p) = self.playing(id) {
            p.notice = Some(notice);
        }
    }

    /// Progress of play `id`, to save. It may come after the player closed (its last position).
    pub fn player_progress(&mut self, key: Key, server: String, file: i64, progress: Progress) {
        if let Some(send) = &self.network {
            send(Request::Progress(key, server, file, progress));
        }
    }

    /// The library after a progress save: Continue Watching and the title pages show it.
    pub fn progress_saved(&mut self, catalog: Catalog, tx: &mut Tx) {
        let error = self.error.take();
        self.loaded(catalog, error, self.refreshed, tx);
    }
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
        .child(el().key("video").effect(MARK).absolute(0.0, 0.0).w_full().h_full().on_press(|s: &mut Flicks| {
            if let Some(p) = &mut s.player {
                p.send(Command::Toggle);
                p.show_controls();
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
    let waiting = (p.opening || p.shown.buffering).then_some(if p.opening { "Opening…" } else { "Buffering…" });
    if let Some(message) = p.error.as_deref().or(waiting) {
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
        .on_hover_change(over_controls)
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
    panel.child(bar).on_hover_change(over_controls)
}

/// The controls stay while the pointer rests on them.
fn over_controls(s: &mut Flicks, over: bool, _: &mut Tx) {
    if let Some(p) = &mut s.player {
        p.over_controls = over;
        p.show_controls();
    }
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
        .transition(Transition::offset(0.0, 8.0).combined(Transition::opacity()))
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
            p.show_controls();
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
        .on_press(move |s: &mut Flicks| {
            // A press is activity: the controls stay a while longer.
            if let Some(p) = &mut s.player {
                p.show_controls();
            }
            press(s);
        })
}

fn menu_button(key: &'static str, label: &'static str, menu: Menu) -> El {
    button(key, label, move |s| {
        if let Some(p) = &mut s.player {
            p.menu = if p.menu == Some(menu) { None } else { Some(menu) };
        }
    })
}

/// `1:02:03` or `2:03`.
fn clock(seconds: f64) -> String {
    let s = seconds.max(0.0) as u64;
    let (h, m, s) = (s / 3600, s / 60 % 60, s % 60);
    if h > 0 { format!("{h}:{m:02}:{s:02}") } else { format!("{m}:{s:02}") }
}

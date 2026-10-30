//! The library screens: Home's rows of posters, a title's page and search.

pub mod catalog;
pub mod fixture;

use std::f32::consts::PI;

use rune::{Color, Cursor, El, ScrollSpec, SurfaceFit, col, el, oklch, row, spacer, text, text_input};

use crate::{Flicks, Screen, Status, artwork_key, message, style};
use catalog::Media;

pub fn home(s: &Flicks) -> El {
    match &s.status {
        Status::Loading => return message("Loading your library…", ""),
        Status::Failed(error) => return message("Something went wrong", error),
        Status::NoServers => return message("No servers yet", "Add one to see its movies and shows."),
        Status::Ready if s.catalog.rows.is_empty() => {
            return message("Nothing here yet", "Add a folder of movies or shows on your server.");
        }
        Status::Ready => {}
    }
    let mut rows = col().w_full().gap(36.0).pt(12.0).pb(48.0);
    for (i, r) in s.catalog.rows.iter().enumerate() {
        let tiles: Vec<El> = r.media.iter().map(|&m| tile(s, m, r.titles)).collect();
        rows = rows.child(
            col()
                .key(format!("row-{i}"))
                .w_full()
                .gap(14.0)
                .child(text(r.name.clone()).font(style::HEADING).color(style::text()).px(style::GUTTER))
                .child(row().w_full().scroll_x().child(row().gap(14.0).px(style::GUTTER).children(tiles))),
        );
    }
    scrolling(rows)
}

/// A screen's content in a vertical scroll view.
fn scrolling(content: El) -> El {
    col().w_full().h_full().scroll(ScrollSpec { stick_bottom: false }).child(content)
}

/// A poster with its caption. A library row's tile stands for its whole title; the others name
/// the episode.
fn tile(s: &Flicks, m: usize, whole_title: bool) -> El {
    let (Some(media), Some(title)) = (s.catalog.media.get(m), s.catalog.title_for(m)) else { return el() };
    let (line, sub) = match (whole_title, media.episode_label()) {
        (false, Some(episode)) => (title.name.clone(), episode),
        _ if title.show => (title.name.clone(), episodes(title.media.len())),
        _ => (title.name.clone(), title.year.map(|y| y.to_string()).unwrap_or_default()),
    };
    let poster = media.item.poster.as_ref().and_then(|name| s.artwork.get(&artwork_key(&media.server, name)));
    let art = match poster {
        Some(image) => {
            el().size(style::POSTER_W, style::POSTER_H).radius(6.0).pixels_fit(image.pixels(), SurfaceFit::Fill)
        }
        None => title_card(&title.name, style::POSTER_W, style::POSTER_H, 18.0),
    };
    let mut art = art.border(1.0, style::faint());
    if let Some(watched) = media.watched() {
        let inset = 10.0;
        art = art.child(
            progress_bar(watched).absolute(inset, style::POSTER_H - inset - 3.0).w(style::POSTER_W - 2.0 * inset),
        );
    }
    let key = title.key.clone();
    col()
        .key(format!("media-{m}"))
        .w(style::POSTER_W)
        .gap(4.0)
        .label(format!("{line}, {sub}"))
        .cursor(Cursor::Pointer)
        .focusable(true)
        .press_scale(0.97)
        .child(art.mb(6.0))
        .child(text(line).font(style::TILE).color(style::text()).truncate())
        .child(text(sub).font(style::SMALL).color(style::muted()).truncate())
        .on_press(move |s: &mut Flicks| s.go(Screen::Detail(key.clone())))
}

fn episodes(n: usize) -> String {
    if n == 1 { "1 episode".into() } else { format!("{n} episodes") }
}

fn progress_bar(watched: f32) -> El {
    el().w_full()
        .h(3.0)
        .radius(1.5)
        .fill(Color::WHITE.alpha(0.25))
        .child(el().w_frac(watched).h_full().radius(1.5).fill(style::accent()))
}

/// A colour picked from a title's name, for when there's no artwork.
fn tint(name: &str) -> (Color, Color) {
    let hue = (name.bytes().fold(0u32, |h, b| h.wrapping_mul(31).wrapping_add(b as u32)) % 360) as f32;
    (oklch(0.42, 0.09, hue), oklch(0.22, 0.05, hue + 40.0))
}

/// What shows until (or instead of) a poster: the name on its tint.
fn title_card(name: &str, w: f32, h: f32, size: f32) -> El {
    let (top, bottom) = tint(name);
    col().size(w, h).radius(6.0).padding(14.0).pb(24.0).justify_end().gradient(top, bottom).child(
        text(name)
            .font(rune::TextStyle::new(size, 800.0).lh(size * 1.1).tracking(-0.3))
            .color(Color::WHITE)
            .max_lines(4),
    )
}

pub fn detail(s: &Flicks, key: &str) -> El {
    let Some(title) = s.catalog.title(key) else {
        return message("Not in your library", "It may have been removed from its server.");
    };
    let episodes_of: Vec<(usize, &Media)> =
        title.media.iter().filter_map(|&m| s.catalog.media.get(m).map(|media| (m, media))).collect();
    let Some(&(first, lead)) = episodes_of.first() else { return message("Not in your library", "") };
    // Resume the episode in progress, else start at the first.
    let (next, next_media) =
        episodes_of.iter().copied().find(|(_, media)| media.watched().is_some()).unwrap_or((first, lead));

    let art = lead.item.backdrop.as_ref().or(lead.item.poster.as_ref());
    let backdrop = match art.and_then(|name| s.artwork.get(&artwork_key(&lead.server, name))) {
        Some(image) => el().pixels_fit(image.pixels(), SurfaceFit::Fill),
        None => {
            let (top, bottom) = tint(&title.name);
            el().linear_gradient(PI * 0.75, [(0.0, top), (1.0, bottom)])
        }
    };
    let scrim_left = el().absolute(0.0, 0.0).w_full().h_full().linear_gradient(
        PI / 2.0,
        [(0.0, style::bg().alpha(0.92)), (0.55, style::bg().alpha(0.35)), (1.0, style::bg().alpha(0.0))],
    );
    let scrim_bottom = el().absolute(0.0, 0.0).w_full().h_full().gradient(style::bg().alpha(0.0), style::bg());

    let mut facts = Vec::new();
    facts.extend(title.year.map(|y| y.to_string()));
    if title.show {
        facts.push(episodes(title.media.len()));
    } else if let Some(ms) = lead.item.duration_ms {
        facts.push(duration(ms));
    }
    facts.extend(quality(&lead.item));

    let play_label = if next_media.watched().is_some() { "Resume" } else { "Play" };
    let play_label = match (title.show, next_media.episode_label()) {
        (true, Some(episode)) => format!("{play_label}  {episode}"),
        _ => play_label.to_string(),
    };
    let mut info = col()
        .key("detail-info")
        .max_w(640.0)
        .gap(14.0)
        .child(text(title.name.clone()).key("detail-title").font(style::HERO).color(style::text()).wrap())
        .child(text(facts.join("  ·  ")).font(style::META).color(style::muted()))
        .child(play_button(&play_label, next).mt(8.0));
    if s.play.as_ref().is_some_and(|p| episodes_of.iter().any(|(_, m)| m.server == p.server && m.item.id == p.item)) {
        info = info.child(
            text("Playback comes with the player, which isn't built yet.").font(style::SMALL).color(style::muted()),
        );
    }
    let hero = col()
        .key("detail-hero")
        .w_full()
        .h(460.0)
        .justify_end()
        .px(style::GUTTER)
        .pb(28.0)
        .child(backdrop.absolute(0.0, 0.0).w_full().h_full())
        .child(scrim_left)
        .child(scrim_bottom)
        .child(info);

    let mut page = col().w_full().pb(48.0).child(hero);
    if title.show {
        let mut list = col()
            .key("episodes")
            .w_full()
            .px(style::GUTTER)
            .gap(2.0)
            .child(text("Episodes").font(style::HEADING).color(style::text()).mb(12.0));
        for &(m, media) in &episodes_of {
            list = list.child(episode_row(media, m));
        }
        page = page.child(list);
    }
    scrolling(page)
}

fn play_button(label: &str, media: usize) -> El {
    row()
        .key("play")
        .h(44.0)
        .px(24.0)
        .gap(10.0)
        .center_y()
        .self_start()
        .radius(6.0)
        .fill(Color::WHITE)
        .hover(Color::BLACK.alpha(0.12))
        .press(Color::BLACK.alpha(0.2))
        .cursor(Cursor::Pointer)
        .focusable(true)
        .label(label.to_string())
        .child(play_icon(Color::BLACK, 14.0))
        .child(text(label.to_string()).font(style::BUTTON).color(Color::BLACK))
        .on_press(move |s: &mut Flicks| s.play(media))
}

fn play_icon(color: Color, size: f32) -> El {
    let triangle = rune::Shape::new().fill(vec![[0.0, 0.0], [10.0, 6.0], [0.0, 12.0]], color).view_box(10.0, 12.0);
    el().size(size * 10.0 / 12.0, size).shape(triangle)
}

fn episode_row(media: &Media, m: usize) -> El {
    let number = media.item.episode.map(|e| e.to_string()).unwrap_or_default();
    let name = match (media.item.episode, media.item.episode_end) {
        (Some(first), Some(last)) if last > first => format!("Episodes {first}–{last}"),
        (Some(episode), _) => format!("Episode {episode}"),
        _ => media.item.title.clone(),
    };
    let mut facts = Vec::new();
    facts.extend(media.item.season.map(|s| format!("Season {s}")));
    facts.extend(media.item.duration_ms.map(duration));
    facts.extend(quality(&media.item));
    let mut middle = col()
        .grow(1.0)
        .gap(4.0)
        .child(text(name).font(style::TILE.lh(19.0)).color(style::text()))
        .child(text(facts.join("  ·  ")).font(style::SMALL).color(style::muted()));
    if let Some(watched) = media.watched() {
        middle = middle.child(progress_bar(watched).w(220.0).mt(4.0));
    }
    row()
        .key(format!("episode-{m}"))
        .w_full()
        .h(76.0)
        .px(16.0)
        .gap(20.0)
        .center_y()
        .radius(8.0)
        .hover(style::faint())
        .cursor(Cursor::Pointer)
        .focusable(true)
        .child(text(number).font(style::HEADING.lh(24.0)).color(style::muted()).w(28.0))
        .child(middle)
        .child(spacer())
        .child(play_icon(style::text(), 14.0).mr(8.0))
        .on_press(move |s: &mut Flicks| s.play(m))
}

fn duration(ms: u64) -> String {
    let minutes = ms / 60_000;
    if minutes >= 60 { format!("{}h {}m", minutes / 60, minutes % 60) } else { format!("{minutes}m") }
}

/// "4K", "HDR" and the like.
fn quality(item: &flicks_client::Item) -> Vec<String> {
    let mut facts = Vec::new();
    match item.height {
        Some(h) if h >= 2000 => facts.push("4K".to_string()),
        Some(h) if h >= 1000 => facts.push("HD".to_string()),
        _ => {}
    }
    if item.hdr.is_some() {
        facts.push("HDR".into());
    }
    facts
}

pub fn search(s: &Flicks) -> El {
    let field = text_input(&s.query, style::HERO.lh(52.0))
        .key("search-field")
        .single_line()
        .w_full()
        .color(style::text())
        .placeholder("Search titles", style::muted().alpha(0.6))
        .on_edit(|s: &mut Flicks| &mut s.query)
        .on_input_change(|s: &mut Flicks, _| s.search_changed());
    let results = if s.query.text.trim().is_empty() {
        text("Type to search every server's movies and shows.").font(style::BODY).color(style::muted())
    } else if s.results.is_empty() {
        text(format!("Nothing matches \u{201c}{}\u{201d}.", s.query.text.trim()))
            .font(style::BODY)
            .color(style::muted())
    } else {
        let tiles: Vec<El> = s.results.iter().map(|&m| tile(s, m, false)).collect();
        row().key("results").w_full().gap(14.0).flex_wrap().children(tiles)
    };
    let content = col()
        .w_full()
        .px(style::GUTTER)
        .pt(12.0)
        .pb(48.0)
        .gap(28.0)
        .child(field)
        .child(el().w_full().h(1.0).fill(style::faint()))
        .child(results);
    scrolling(content)
}

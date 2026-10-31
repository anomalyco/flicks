//! Add Server: paste the link `flicks pair` prints on the server.

use rune::{Color, Cursor, El, col, row, text, text_input};

use crate::{Flicks, style};

pub fn view(s: &Flicks) -> El {
    let field = text_input(&s.link, style::BODY.lh(22.0))
        .key("link-field")
        .single_line()
        .w_full()
        .h(46.0)
        .px(14.0)
        .py(12.0)
        .radius(8.0)
        .fill(style::raised())
        .border(1.0, style::faint())
        .color(style::text())
        .placeholder("flicks://pair?…", style::muted().alpha(0.6))
        .on_edit(|s: &mut Flicks| &mut s.link)
        .on_submit(|s: &mut Flicks, _| s.pair());
    let ready = !s.link.text.trim().is_empty() && !s.pairing;
    let button = row()
        .key("add")
        .h(44.0)
        .px(24.0)
        .center()
        .self_start()
        .radius(10.0)
        .fill(if ready { style::text() } else { style::faint() })
        .hover(Color::BLACK.alpha(if ready { 0.08 } else { 0.0 }))
        .cursor(if ready { Cursor::Pointer } else { Cursor::Default })
        .focusable(ready)
        .child(text(if s.pairing { "Adding…" } else { "Add Server" }).font(style::BUTTON).color(if ready {
            style::bg()
        } else {
            style::muted()
        }))
        .on_press(|s: &mut Flicks| s.pair());
    let mut panel = col()
        .key("add-server")
        .w_full()
        .max_w(560.0)
        .gap(16.0)
        .child(text("Add a server").font(style::HERO).color(style::text()))
        .child(
            text("On the computer with your movies, run flicks pair and paste the link it prints here.")
                .font(style::BODY)
                .color(style::muted())
                .wrap(),
        )
        .child(field.mt(8.0))
        .child(button);
    if let Some(error) = &s.pair_error {
        panel = panel.child(text(error.clone()).key("pair-error").font(style::BODY).color(style::error()).wrap());
    }
    col().w_full().h_full().px(style::GUTTER).pt(72.0).items_center().child(panel)
}

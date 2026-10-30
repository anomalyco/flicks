//! Server screens, for servers where this device is an admin: the server list, then per server
//! its folders, users and devices, and Add Device (a pairing window's link as a QR code). They
//! call the server's `/admin/` routes, the ones the `flicks` CLI uses. The pairing link is a
//! one-time secret: it is shown only as a QR code and copied without clipboard history, and
//! never logged.

pub mod fixture;
#[cfg(test)]
mod tests;

use std::time::Duration;

use flicks_client::merge::Library;
use flicks_client::{Connection, Error, keys};
use hyper::Method;
use rune::{Color, Cursor, El, Path, ScrollSpec, Shape, TextEdit, Tx, canvas, col, row, spacer, text, text_input};
use serde::de::IgnoredAny;
use serde::{Deserialize, Serialize};

use crate::{Flicks, Screen, style, worker};

/// How long a pairing window stays open (the server's `pair::WINDOW`) and the copied link with it.
const PAIRING_OPEN: Duration = Duration::from_secs(5 * 60);

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Tab {
    Folders,
    Users,
    Devices,
    AddDevice,
}

/// A paired server as the list shows it.
#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub key: String,
    pub name: String,
    pub admin: bool,
    /// It answered the last refresh.
    pub online: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
pub struct Folder {
    pub id: i64,
    pub path: String,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
pub struct User {
    pub id: i64,
    pub name: String,
    pub admin: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
pub struct Device {
    pub id: i64,
    pub user: String,
    pub name: String,
    /// Its public key, base64url.
    pub key: String,
}

/// One server's folders, users and devices.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Lists {
    pub folders: Vec<Folder>,
    pub users: Vec<User>,
    pub devices: Vec<Device>,
}

/// What the network thread does on a server; every one answers with the server's lists.
#[derive(Clone, Debug, PartialEq)]
pub enum Op {
    Load,
    AddFolder(String),
    RemoveFolder(i64),
    AddUser { name: String, admin: bool },
    RemoveDevice(i64),
    Pair { user: String, admin: bool },
}

/// The Server screens' state.
pub struct Servers {
    pub list: Vec<Row>,
    /// This device's key, base64url, to mark it in the device list.
    pub me: String,
    /// The open server's lists, once loaded.
    pub lists: Option<Lists>,
    /// A change is on its way to the server.
    pub busy: bool,
    pub error: Option<String>,
    /// The Remove button pressed once, waiting for a second press.
    pub confirm: Option<String>,
    pub folder: TextEdit,
    pub user: TextEdit,
    pub user_admin: bool,
    pub pair_user: TextEdit,
    pub pair_admin: bool,
    /// The open pairing window's link.
    pub link: Option<String>,
}

impl Default for Servers {
    fn default() -> Servers {
        Servers {
            list: Vec::new(),
            me: String::new(),
            lists: None,
            busy: false,
            error: None,
            confirm: None,
            folder: TextEdit::new(""),
            user: TextEdit::new(""),
            user_admin: false,
            pair_user: TextEdit::new(""),
            pair_admin: false,
            link: None,
        }
    }
}

impl Servers {
    /// Whether the Server screens are shown at all: this device is an admin somewhere.
    pub fn any_admin(&self) -> bool {
        self.list.iter().any(|r| r.admin)
    }
}

/// Opens a server's tab, fetching its lists afresh. Viewers' servers don't open.
pub fn open(s: &mut Flicks, key: &str, tab: Tab) {
    if !s.servers.list.iter().any(|r| r.key == key && r.admin) {
        return;
    }
    let same = matches!(&s.screen, Screen::Server(open, _) if open == key);
    if !same {
        s.servers.lists = None;
    }
    s.servers.error = None;
    s.servers.confirm = None;
    s.servers.link = None;
    s.screen = Screen::Server(key.to_string(), tab);
    send(s, Op::Load);
}

/// Sends `op` for the open server.
pub fn send(s: &mut Flicks, op: Op) {
    let Screen::Server(key, _) = &s.screen else { return };
    if s.servers.busy && op != Op::Load {
        return;
    }
    match &s.worker {
        Some(worker) => {
            s.servers.busy = op != Op::Load;
            s.servers.error = None;
            worker.send(worker::Request::Admin(key.clone(), op));
        }
        None => s.servers.error = Some("Not connected to the network yet.".into()),
    }
}

/// A Remove button's press: the first arms it, the second sends `op`.
pub fn remove(s: &mut Flicks, confirm: String, op: Op) {
    if s.servers.confirm.as_ref() == Some(&confirm) {
        s.servers.confirm = None;
        send(s, op);
    } else {
        s.servers.confirm = Some(confirm);
    }
}

/// The network thread's answer to `op` on the server `key`.
pub fn answered(s: &mut Flicks, key: &str, op: Op, result: Result<(Lists, Option<String>), String>) {
    if op != Op::Load {
        s.servers.busy = false;
    }
    if !matches!(&s.screen, Screen::Server(open, _) if open == key) {
        return;
    }
    match result {
        Ok((lists, link)) => {
            s.servers.lists = Some(lists);
            match op {
                Op::AddFolder(_) => s.servers.folder.replace_text(""),
                Op::AddUser { .. } => {
                    s.servers.user.replace_text("");
                    s.servers.user_admin = false;
                }
                Op::Pair { .. } => s.servers.link = link,
                _ => {}
            }
        }
        Err(error) => s.servers.error = Some(error),
    }
}

// The network thread's side.

/// After a refresh: every paired server, whether it answered, and this device's key.
pub fn refreshed(app: &rune::AppHandle<Flicks>, library: &Library) {
    let list: Vec<Row> = library
        .client
        .servers
        .iter()
        .map(|server| Row {
            key: server.key.clone(),
            name: server.name.clone(),
            admin: server.admin,
            online: library.connection(&server.key).is_some(),
        })
        .collect();
    let me = keys::encode(&library.client.public_key());
    app.update(move |s, _| {
        s.servers.list = list;
        s.servers.me = me;
    });
}

/// Runs `op` on the server with this key and answers with its lists.
pub async fn run(app: &rune::AppHandle<Flicks>, library: &mut Library, key: String, op: Op) {
    let result = admin(library, &key, &op).await.map_err(|e| e.to_string());
    app.update(move |s, _| answered(s, &key, op, result));
}

#[derive(Serialize)]
struct AddFolder<'a> {
    path: &'a str,
}

#[derive(Serialize)]
struct AddUser<'a> {
    name: &'a str,
    admin: bool,
}

#[derive(Serialize)]
struct OpenPairing<'a> {
    user: &'a str,
    admin: bool,
}

#[derive(Deserialize)]
struct Pairing {
    link: String,
}

async fn admin(library: &mut Library, key: &str, op: &Op) -> Result<(Lists, Option<String>), Error> {
    let connection = match library.connection(key) {
        Some(connection) => connection.clone(),
        None => library.client.connect(key).await?,
    };
    let c = &connection;
    let none = None::<&()>;
    let mut link = None;
    match op {
        Op::Load => {}
        Op::AddFolder(path) => {
            c.call::<IgnoredAny>(Method::POST, "/admin/folders", Some(&AddFolder { path: path.trim() })).await?;
        }
        Op::RemoveFolder(id) => c.call::<()>(Method::DELETE, &format!("/admin/folders/{id}"), none).await?,
        Op::AddUser { name, admin } => {
            let body = AddUser { name: name.trim(), admin: *admin };
            c.call::<IgnoredAny>(Method::POST, "/admin/users", Some(&body)).await?;
        }
        Op::RemoveDevice(id) => c.call::<()>(Method::DELETE, &format!("/admin/devices/{id}"), none).await?,
        Op::Pair { user, admin } => {
            let body = OpenPairing { user: user.trim(), admin: *admin };
            link = Some(c.call::<Pairing>(Method::POST, "/admin/pair", Some(&body)).await?.link);
        }
    }
    let lists = lists(c).await?;
    Ok((lists, link))
}

async fn lists(c: &Connection) -> Result<Lists, Error> {
    let none = None::<&()>;
    Ok(Lists {
        folders: c.call(Method::GET, "/admin/folders", none).await?,
        users: c.call(Method::GET, "/admin/users", none).await?,
        devices: c.call(Method::GET, "/admin/devices", none).await?,
    })
}

// Views.

/// The server list: every paired server, its state and this device's role; admins' open.
pub fn list(s: &Flicks) -> El {
    let rows = s.servers.list.iter().enumerate().map(|(i, server)| {
        let state = if server.online { "Online" } else { "Offline" };
        let dot = col().size(8.0, 8.0).radius(4.0).fill(if server.online { online() } else { style::muted() });
        let mut line = row()
            .key(format!("server-{i}"))
            .w_full()
            .h(64.0)
            .px(20.0)
            .gap(14.0)
            .center_y()
            .radius(8.0)
            .fill(style::raised())
            .child(text(server.name.clone()).font(style::HEADING).color(style::text()).truncate())
            .child(spacer())
            .child(row().gap(8.0).center_y().child(dot).child(text(state).font(style::META).color(style::muted())))
            .child(text(if server.admin { "Admin" } else { "Viewer" }).font(style::META).color(style::muted()).w(60.0));
        if server.admin {
            let key = server.key.clone();
            line = line
                .hover(style::faint())
                .cursor(Cursor::Pointer)
                .focusable(true)
                .on_press(move |s: &mut Flicks| open(s, &key, Tab::Folders));
        }
        line
    });
    page(col().gap(10.0).child(title("Servers").mb(14.0)).children(rows))
}

/// One server's tab.
pub fn view(s: &Flicks, key: &str, tab: Tab) -> El {
    let name = s.servers.list.iter().find(|r| r.key == key).map_or("Server", |r| r.name.as_str());
    let back = text("‹ Servers")
        .key("back")
        .font(style::META)
        .color(style::muted())
        .cursor(Cursor::Pointer)
        .focusable(true)
        .self_start()
        .on_press(|s: &mut Flicks| s.go(Screen::Servers));
    let tabs = [
        ("Folders", "tab-folders", Tab::Folders),
        ("Users", "tab-users", Tab::Users),
        ("Devices", "tab-devices", Tab::Devices),
        ("Add Device", "tab-add-device", Tab::AddDevice),
    ]
    .into_iter()
    .map(|(label, k, t)| {
        let key = key.to_string();
        text(label)
            .key(k)
            .font(style::NAV)
            .color(if t == tab { style::text() } else { style::muted() })
            .fill(if t == tab { style::faint() } else { Color::TRANSPARENT })
            .py(6.0)
            .px(12.0)
            .radius(6.0)
            .hover(style::faint())
            .cursor(Cursor::Pointer)
            .focusable(true)
            .on_press(move |s: &mut Flicks| open(s, &key, t))
    });
    let mut content =
        col().gap(16.0).child(back).child(title(name)).child(row().gap(4.0).ml(-12.0).children(tabs).mb(8.0));
    if let Some(error) = &s.servers.error {
        content = content.child(text(error.clone()).key("server-error").font(style::BODY).color(style::error()).wrap());
    }
    let body = match (&s.servers.lists, tab) {
        (_, Tab::AddDevice) => add_device(s),
        (None, _) => text("Loading…").font(style::BODY).color(style::muted()),
        (Some(lists), Tab::Folders) => folders(s, lists),
        (Some(lists), Tab::Users) => users(s, lists),
        (Some(lists), Tab::Devices) => devices(s, lists),
    };
    page(content.child(body))
}

fn folders(s: &Flicks, lists: &Lists) -> El {
    let rows = lists.folders.iter().map(|folder| {
        let confirm = format!("remove-folder-{}", folder.id);
        let button = remove_button(s, &confirm, Op::RemoveFolder(folder.id));
        item(text(folder.path.clone()).font(style::BODY).color(style::text()).truncate(), button)
    });
    let ready = !s.servers.folder.text.trim().is_empty() && !s.servers.busy;
    let add = row()
        .gap(10.0)
        .w_full()
        .child(
            field(&s.servers.folder, "folder-field", "/Volumes/Media/Movies")
                .on_edit(|s: &mut Flicks| &mut s.servers.folder)
                .on_submit(|s: &mut Flicks, _| add_folder(s)),
        )
        .child(button("add-folder", "Add Folder", ready).on_press(add_folder));
    col().w_full().gap(8.0).children(rows).child(empty(lists.folders.is_empty(), "No folders yet.")).child(add.mt(12.0))
}

fn add_folder(s: &mut Flicks) {
    let path = s.servers.folder.text.trim().to_string();
    if !path.is_empty() {
        send(s, Op::AddFolder(path));
    }
}

fn users(s: &Flicks, lists: &Lists) -> El {
    let rows = lists.users.iter().map(|user| {
        let role = text(if user.admin { "Admin" } else { "Viewer" }).font(style::META).color(style::muted());
        item(text(user.name.clone()).font(style::BODY).color(style::text()).truncate(), role)
    });
    let ready = !s.servers.user.text.trim().is_empty() && !s.servers.busy;
    let add = row()
        .gap(10.0)
        .w_full()
        .center_y()
        .child(
            field(&s.servers.user, "user-field", "name")
                .on_edit(|s: &mut Flicks| &mut s.servers.user)
                .on_submit(|s: &mut Flicks, _| add_user(s)),
        )
        .child(check("user-admin", "Admin", s.servers.user_admin).on_press(|s: &mut Flicks| {
            s.servers.user_admin = !s.servers.user_admin;
        }))
        .child(button("add-user", "Add User", ready).on_press(add_user));
    col().w_full().gap(8.0).children(rows).child(add.mt(12.0))
}

fn add_user(s: &mut Flicks) {
    let name = s.servers.user.text.trim().to_string();
    if !name.is_empty() {
        send(s, Op::AddUser { name, admin: s.servers.user_admin });
    }
}

fn devices(s: &Flicks, lists: &Lists) -> El {
    let rows = lists.devices.iter().map(|device| {
        let label = col()
            .gap(2.0)
            .min_w(0.0)
            .child(text(device.name.clone()).font(style::BODY).color(style::text()).truncate())
            .child(text(device.user.clone()).font(style::SMALL).color(style::muted()));
        let end = if device.key == s.servers.me {
            text("This device").font(style::META).color(style::muted())
        } else {
            remove_button(s, &format!("remove-device-{}", device.id), Op::RemoveDevice(device.id))
        };
        item(label, end)
    });
    col().w_full().gap(8.0).children(rows).child(empty(lists.devices.is_empty(), "No devices."))
}

fn add_device(s: &Flicks) -> El {
    let ready = !s.servers.pair_user.text.trim().is_empty() && !s.servers.busy;
    let form = row()
        .gap(10.0)
        .w_full()
        .center_y()
        .child(
            field(&s.servers.pair_user, "pair-user", "user it belongs to")
                .on_edit(|s: &mut Flicks| &mut s.servers.pair_user)
                .on_submit(|s: &mut Flicks, _| open_pairing(s)),
        )
        .child(check("pair-admin", "Admin", s.servers.pair_admin).on_press(|s: &mut Flicks| {
            s.servers.pair_admin = !s.servers.pair_admin;
        }))
        .child(button("open-pairing", "Open Pairing Window", ready).on_press(open_pairing));
    let mut content = col()
        .w_full()
        .gap(14.0)
        .child(
            text("Pair a phone, TV or computer: choose the user it belongs to, then scan the code in Flicks there or paste the link in its Add Server.")
                .font(style::BODY)
                .color(style::muted())
                .wrap(),
        )
        .child(form);
    if let Some(link) = &s.servers.link {
        let copy = button("copy-link", "Copy Link", true).on_press_tx(|s: &mut Flicks, tx: &mut Tx| {
            if let Some(link) = &s.servers.link {
                tx.copy_secret(link.clone(), PAIRING_OPEN);
            }
        });
        let side = col()
            .gap(14.0)
            .child(text("Open for 5 minutes, for one device.").font(style::BODY).color(style::text()))
            .child(copy);
        content = content.child(row().gap(28.0).mt(12.0).child(qr(link)).child(side));
    }
    content
}

fn open_pairing(s: &mut Flicks) {
    let user = s.servers.pair_user.text.trim().to_string();
    if !user.is_empty() {
        s.servers.link = None;
        send(s, Op::Pair { user, admin: s.servers.pair_admin });
    }
}

/// The link as a QR code, dark modules on white with a four-module quiet zone.
pub fn qr(link: &str) -> El {
    const SIZE: f32 = 220.0;
    let Ok(code) = qrcode::QrCode::new(link.as_bytes()) else {
        return text("The link is too long for a QR code; copy it instead.").font(style::BODY).color(style::muted());
    };
    let width = code.width();
    let colors = code.to_colors();
    // One rectangle per run of dark modules in a row, all in one path so they don't seam.
    let mut path = Path::new();
    for (y, line) in colors.chunks(width.max(1)).enumerate() {
        let mut x = 0;
        while x < line.len() {
            if line.get(x) != Some(&qrcode::Color::Dark) {
                x += 1;
                continue;
            }
            let start = x;
            while line.get(x) == Some(&qrcode::Color::Dark) {
                x += 1;
            }
            let (x0, x1, y0, y1) = (start as f32 + 4.0, x as f32 + 4.0, y as f32 + 4.0, y as f32 + 5.0);
            path = path.move_to([x0, y0]).line_to([x1, y0]).line_to([x1, y1]).line_to([x0, y1]).close();
        }
    }
    let modules = width as f32 + 8.0;
    let shape = Shape::new().fill_path(&path, Color::BLACK).view_box(modules, modules);
    canvas(shape).key("qr").size(SIZE, SIZE).radius(8.0).fill(Color::WHITE)
}

// Pieces.

fn page(content: El) -> El {
    let inner = col().w_full().max_w(760.0).px(style::GUTTER).pt(32.0).pb(48.0).child(content.w_full());
    col().w_full().h_full().scroll(ScrollSpec { stick_bottom: false }).child(col().w_full().items_center().child(inner))
}

fn title(label: &str) -> El {
    text(label.to_string()).font(style::HERO).color(style::text())
}

/// A row of a list: its label, and a role or button at the end.
fn item(label: El, end: El) -> El {
    row()
        .w_full()
        .min_h(52.0)
        .px(16.0)
        .py(8.0)
        .gap(14.0)
        .center_y()
        .radius(8.0)
        .fill(style::raised())
        .child(label.grow(1.0).min_w(0.0))
        .child(end)
}

fn empty(is_empty: bool, message: &str) -> El {
    if is_empty { text(message.to_string()).font(style::BODY).color(style::muted()) } else { col() }
}

fn field(edit: &TextEdit, key: &str, placeholder: &str) -> El {
    text_input(edit, style::BODY.lh(22.0))
        .key(key.to_string())
        .single_line()
        .grow(1.0)
        .min_w(0.0)
        .h(44.0)
        .px(14.0)
        .py(11.0)
        .radius(8.0)
        .fill(style::raised())
        .border(1.0, style::faint())
        .color(style::text())
        .placeholder(placeholder.to_string(), style::muted().alpha(0.6))
}

/// A button like Add Server's: white when it can be pressed.
fn button(key: &str, label: &str, ready: bool) -> El {
    row()
        .key(key.to_string())
        .h(44.0)
        .px(20.0)
        .center()
        .radius(6.0)
        .fill(if ready { Color::WHITE } else { style::faint() })
        .hover(Color::BLACK.alpha(if ready { 0.12 } else { 0.0 }))
        .cursor(if ready { Cursor::Pointer } else { Cursor::Default })
        .focusable(ready)
        .child(text(label.to_string()).font(style::BUTTON).color(if ready { Color::BLACK } else { style::muted() }))
}

/// Remove, which asks to be pressed again before it sends `op`.
fn remove_button(s: &Flicks, key: &str, op: Op) -> El {
    let armed = s.servers.confirm.as_deref() == Some(key);
    let confirm = key.to_string();
    text(if armed { "Confirm Remove" } else { "Remove" })
        .key(key.to_string())
        .font(style::META)
        .color(if armed { Color::WHITE } else { style::muted() })
        .fill(if armed { style::accent() } else { Color::TRANSPARENT })
        .py(6.0)
        .px(10.0)
        .radius(6.0)
        .hover(style::faint())
        .cursor(Cursor::Pointer)
        .focusable(true)
        .on_press(move |s: &mut Flicks| remove(s, confirm.clone(), op.clone()))
}

/// A check box with its label.
fn check(key: &str, label: &str, on: bool) -> El {
    let mut b = row().size(18.0, 18.0).radius(4.0).center().border(1.5, style::muted());
    if on {
        b = b.fill(Color::WHITE).child(text("✓").font(style::SMALL).color(Color::BLACK));
    }
    row()
        .key(key.to_string())
        .gap(8.0)
        .px(6.0)
        .h(44.0)
        .center_y()
        .cursor(Cursor::Pointer)
        .focusable(true)
        .child(b)
        .child(text(label.to_string()).font(style::BODY).color(style::text()))
}

fn online() -> Color {
    rune::oklch(0.75, 0.15, 150.0)
}

use std::sync::mpsc;

use rune::{App, Env, Key, KeyEvent, Mods, Runtime};

use super::*;

/// The app in a scene, with a network that only records what it's asked.
fn runtime(scene: &str) -> (Runtime<Flicks>, mpsc::Receiver<Request>) {
    let state = rune::tooling::initial_state(&crate::spec(), Some(scene));
    let app = App::new(crate::spec()).init(move || state);
    let mut rt = app.headless_runtime((1180.0, 760.0), Env { system_dark: true, scale: 2.0 });
    let (tx, rx) = mpsc::channel();
    rt.update(move |s, _| {
        s.network = Some(Box::new(move |request| {
            let _ = tx.send(request);
        }))
    });
    settle(&mut rt);
    (rt, rx)
}

fn settle(rt: &mut Runtime<Flicks>) {
    for _ in 0..30 {
        rt.frame(1.0 / 60.0);
    }
}

fn press(rt: &mut Runtime<Flicks>, key: &str) {
    let (x, y) = rt.find(key).unwrap_or_else(|| panic!("no {key} on screen"));
    rt.pointer_down(x, y);
    rt.pointer_up(x, y);
    settle(rt);
}

fn type_in(rt: &mut Runtime<Flicks>, field: &str, text: &str) {
    press(rt, field);
    for c in text.chars() {
        rt.key(KeyEvent { key: Key::Text(c.to_string()), mods: Mods::default() });
    }
    settle(rt);
}

fn shows(rt: &Runtime<Flicks>, text: &str) -> bool {
    rt.elements().iter().any(|e| !e.exiting && e.text.as_deref() == Some(text))
}

/// The admin requests sent since the last call, as (server key, change).
fn sent(rx: &mut mpsc::Receiver<Request>) -> Vec<(String, Op)> {
    let mut ops = Vec::new();
    while let Ok(request) = rx.try_recv() {
        if let Request::Admin(key, op) = request {
            ops.push((key, op));
        }
    }
    ops
}

fn answer(rt: &mut Runtime<Flicks>, op: Op, lists: Lists, link: Option<&str>) {
    let link = link.map(str::to_string);
    rt.update(move |s, _| answered(s, "home", op, Ok((lists, link))));
    settle(rt);
}

#[test]
fn only_admins_see_the_server_screens() {
    let (mut rt, mut rx) = runtime("servers");
    assert!(shows(&rt, "Living Room Mac") && shows(&rt, "Ana's PC"));
    assert!(shows(&rt, "Admin") && shows(&rt, "Viewer") && shows(&rt, "Online") && shows(&rt, "Offline"));
    assert!(rt.find("nav-servers").is_some());

    // A viewer's server doesn't open.
    press(&mut rt, "server-1");
    assert_eq!(rt.state.screen, Screen::Servers);
    assert!(sent(&mut rx).is_empty());

    // A device that is only a viewer has no Servers in the top bar.
    rt.update(|s, _| {
        s.servers.list.retain(|r| !r.admin);
        s.go(Screen::Home);
    });
    settle(&mut rt);
    assert!(rt.find("nav-servers").is_none());

    // An admin's opens on Folders and loads its lists, from the keyboard too.
    let (mut rt, mut rx) = runtime("servers");
    // Shift-Tab from the top wraps to the last control: the admin's row (the viewer's isn't one).
    rt.key(KeyEvent { key: Key::Tab, mods: Mods { shift: true, ..Mods::default() } });
    rt.key(KeyEvent { key: Key::Text(" ".into()), mods: Mods::default() });
    settle(&mut rt);
    assert_eq!(rt.state.screen, Screen::Server("home".into(), Tab::Folders));
    assert_eq!(sent(&mut rx), [("home".into(), Op::Load)]);
    assert!(shows(&rt, "Loading…"));
    answer(&mut rt, Op::Load, fixture::lists(), None);
    assert!(shows(&rt, "/Volumes/Media/TV"));
}

#[test]
fn a_folder_is_added_by_path_and_removed_on_the_second_press() {
    let (mut rt, mut rx) = runtime("server-folders");
    type_in(&mut rt, "folder-field", "/Volumes/Media/Films");
    press(&mut rt, "add-folder");
    assert_eq!(sent(&mut rx), [("home".into(), Op::AddFolder("/Volumes/Media/Films".into()))]);
    assert!(rt.state.servers.busy);

    let mut lists = fixture::lists();
    lists.folders.push(Folder { id: 3, path: "/Volumes/Media/Films".into() });
    answer(&mut rt, Op::AddFolder("/Volumes/Media/Films".into()), lists, None);
    assert!(!rt.state.servers.busy);
    assert!(shows(&rt, "/Volumes/Media/Films"));
    assert_eq!(rt.state.servers.folder.text, "", "the field empties once the server has it");

    press(&mut rt, "remove-folder-3");
    assert!(sent(&mut rx).is_empty(), "the first press only arms it");
    assert!(shows(&rt, "Confirm Remove"));
    press(&mut rt, "remove-folder-3");
    assert_eq!(sent(&mut rx), [("home".into(), Op::RemoveFolder(3))]);

    // A refusal keeps what was typed and shows the server's reason.
    type_in(&mut rt, "folder-field", "/etc");
    rt.update(|s, _| s.servers.busy = false);
    press(&mut rt, "add-folder");
    let refused = "that is a system or secret location (403)".to_string();
    rt.update(move |s, _| answered(s, "home", Op::AddFolder("/etc".into()), Err(refused)));
    settle(&mut rt);
    assert!(shows(&rt, "that is a system or secret location (403)"));
    assert_eq!(rt.state.servers.folder.text, "/etc");
}

#[test]
fn a_user_is_added_with_its_role() {
    let (mut rt, mut rx) = runtime("server-users");
    assert!(shows(&rt, "admin") && shows(&rt, "ana"));
    type_in(&mut rt, "user-field", "sam");
    press(&mut rt, "user-admin");
    press(&mut rt, "add-user");
    assert_eq!(sent(&mut rx), [("home".into(), Op::AddUser { name: "sam".into(), admin: true })]);
    answer(&mut rt, Op::AddUser { name: "sam".into(), admin: true }, fixture::lists(), None);
    assert_eq!(rt.state.servers.user.text, "");
    assert!(!rt.state.servers.user_admin);
}

#[test]
fn every_device_but_this_one_can_be_removed() {
    let (mut rt, mut rx) = runtime("server-devices");
    assert!(shows(&rt, "This device"));
    assert!(rt.find("remove-device-1").is_none());
    press(&mut rt, "remove-device-2");
    press(&mut rt, "remove-device-2");
    assert_eq!(sent(&mut rx), [("home".into(), Op::RemoveDevice(2))]);
}

#[test]
fn add_device_opens_a_pairing_window_and_shows_its_link_as_a_qr_code() {
    let (mut rt, mut rx) = runtime("server-folders");
    press(&mut rt, "tab-add-device");
    assert_eq!(sent(&mut rx), [("home".into(), Op::Load)]);
    assert!(rt.find("qr").is_none() && rt.find("copy-link").is_none());

    type_in(&mut rt, "pair-user", "ana");
    press(&mut rt, "open-pairing");
    assert_eq!(sent(&mut rx), [("home".into(), Op::Pair { user: "ana".into(), admin: false })]);
    answer(&mut rt, Op::Pair { user: "ana".into(), admin: false }, fixture::lists(), Some(fixture::LINK));
    assert!(rt.find("qr").is_some() && rt.find("copy-link").is_some());
    assert!(!shows(&rt, fixture::LINK), "the link itself is never on screen");

    // Leaving the tab forgets the link.
    press(&mut rt, "tab-devices");
    assert_eq!(rt.state.servers.link, None);
}

#[test]
fn an_answer_for_a_server_no_longer_open_is_dropped() {
    let (mut rt, _rx) = runtime("server-folders");
    rt.update(|s, _| {
        s.servers.busy = true;
        s.go(Screen::Servers);
    });
    rt.update(|s, _| answered(s, "home", Op::RemoveFolder(1), Ok((Lists::default(), None))));
    assert!(!rt.state.servers.busy);
    assert_eq!(rt.state.servers.lists, Some(fixture::lists()));
}

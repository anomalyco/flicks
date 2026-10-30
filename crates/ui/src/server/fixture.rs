//! Server screens for headless scenes and tests: this device is an admin of one server and a
//! viewer of another that didn't answer. The pairing link is a dummy, never a real one.

use super::{Device, Folder, Lists, Row, Tab, User};
use crate::{Flicks, Screen, Status};

pub const ME: &str = "this-device-key";
pub const LINK: &str = "flicks://pair?dummy-link-for-headless-scenes";

pub fn servers(s: &mut Flicks) {
    s.status = Status::Ready;
    s.servers.me = ME.into();
    s.servers.list = vec![
        Row { key: "home".into(), name: "Living Room Mac".into(), admin: true, online: true },
        Row { key: "away".into(), name: "Ana's PC".into(), admin: false, online: false },
    ];
    s.screen = Screen::Servers;
}

fn tab(s: &mut Flicks, tab: Tab) {
    servers(s);
    s.servers.lists = Some(lists());
    s.screen = Screen::Server("home".into(), tab);
}

pub fn folders(s: &mut Flicks) {
    tab(s, Tab::Folders);
}

pub fn users(s: &mut Flicks) {
    tab(s, Tab::Users);
}

pub fn devices(s: &mut Flicks) {
    tab(s, Tab::Devices);
}

pub fn pairing(s: &mut Flicks) {
    tab(s, Tab::AddDevice);
    s.servers.pair_user.replace_text("ana");
    s.servers.link = Some(LINK.into());
}

pub fn lists() -> Lists {
    Lists {
        folders: vec![
            Folder { id: 1, path: "/Volumes/Media/Movies".into() },
            Folder { id: 2, path: "/Volumes/Media/TV".into() },
        ],
        users: vec![
            User { id: 1, name: "admin".into(), admin: true },
            User { id: 2, name: "ana".into(), admin: false },
        ],
        devices: vec![
            Device { id: 1, user: "admin".into(), name: "Flicks for Mac".into(), key: ME.into() },
            Device { id: 2, user: "ana".into(), name: "Ana's iPhone".into(), key: "other-device-key".into() },
        ],
    }
}

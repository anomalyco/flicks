//! The Server screens' network side (`flicks_ui::server`): runs their admin changes on the
//! server's `/admin/` routes, the ones the `flicks` CLI uses, and tells them which servers are
//! paired after each refresh.

use flicks_client::merge::Library;
use flicks_client::{Connection, Error, keys};
use flicks_ui::Flicks;
use flicks_ui::server::{Lists, Op, Row, answered};
use hyper::Method;
use serde::de::IgnoredAny;
use serde::{Deserialize, Serialize};

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

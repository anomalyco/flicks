//! The catalog API (FLK-11). Signed requests outside `/admin/` land here once authenticated.

use std::sync::Arc;

use hyper::body::Bytes;
use hyper::http::request::Parts;

use crate::Error;
use crate::server::{Caller, Reply, State};

/// Starts the catalog's background work; called once when the server starts.
pub fn start(_state: &Arc<State>) -> Result<(), Error> {
    Ok(())
}

/// Called after a folder is added or removed.
pub fn folders_changed(_state: &State) {}

/// Answers a signed request, or `None` for "not found".
pub async fn route(_state: &State, _caller: &Caller, _parts: &Parts, _body: &Bytes) -> Option<Reply> {
    None
}

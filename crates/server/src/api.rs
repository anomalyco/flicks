//! The catalog API (FLK-11). Signed requests outside `/admin/` land here once authenticated.

use hyper::body::Bytes;
use hyper::http::request::Parts;

use crate::server::{Caller, Reply, State};

/// Answers a signed request, or `None` for "not found".
pub async fn route(_state: &State, _caller: &Caller, _parts: &Parts, _body: &Bytes) -> Option<Reply> {
    None
}

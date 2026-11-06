//! Place for the `/api/...` routes that are not part of the static / ui-extension groups
//! (chats, input history, settings, models, libraries, system). Each API route unit adds its
//! handlers in [`add_api_routes`]; the layers (auth, sessions, coi headers) are applied to the
//! whole router afterwards in `server::build_router`, so nothing needs wiring there.

use crate::server::Shared;
use axum::Router;
use std::sync::Arc;

/// adds the `/api` routes owned by the API route units. nothing is registered yet
pub fn add_api_routes(router: Router<Arc<Shared>>) -> Router<Arc<Shared>> {
    router
}

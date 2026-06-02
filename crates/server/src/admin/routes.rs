//! Axum router for the non-gRPC admin surface.
//!
//! Post-migration this is small: just the two cookie endpoints
//! (`/api/login`, `/api/logout`). The admin SPA is served by the
//! standalone `admin-ui/` service, not here. The gRPC `Admin` service is
//! merged in separately by [`super::run`] under `/toki.admin.v1.Admin/*`.

use axum::{routing::post, Router};

use super::{handlers, AppState};

/// Build the HTTP router: just the two cookie endpoints, with state
/// applied. **No fallback** — [`super::run`] merges this with the gRPC
/// router, which carries tonic's own fallback for unmatched paths. (axum
/// panics if you merge two routers that both have a fallback.)
pub fn build(state: AppState) -> Router {
    Router::new()
        .route("/api/login", post(handlers::login))
        .route("/api/logout", post(handlers::logout))
        .with_state(state)
}

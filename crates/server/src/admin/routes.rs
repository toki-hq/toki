//! Axum router for the non-gRPC admin surface.
//!
//! Post-migration this is small: the two cookie endpoints
//! (`/api/login`, `/api/logout`) and a catch-all that serves the embedded
//! React SPA (with client-side-routing history fallback). The gRPC `Admin`
//! service is merged in separately by [`super::run`] — its `/toki.admin.v1.Admin/*`
//! routes take precedence over the SPA fallback, so there's no overlap.

use axum::{routing::post, Router};

use super::{handlers, AppState};

/// Build the HTTP router: just the two cookie endpoints, with state
/// applied. **No fallback** — [`super::run`] merges this with the gRPC
/// router (which carries tonic's own fallback) and then sets
/// [`handlers::spa`] as the single fallback on the merged router. (axum
/// panics if you merge two routers that both have a fallback.) Tests that
/// want the SPA served standalone append `.fallback(handlers::spa)`
/// themselves.
pub fn build(state: AppState) -> Router {
    Router::new()
        .route("/api/login", post(handlers::login))
        .route("/api/logout", post(handlers::logout))
        .with_state(state)
}

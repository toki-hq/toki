//! Axum router wiring.
//!
//! Routes are split into a *public* tree (`/`, `/static/*`,
//! `/api/login`) and a *protected* tree under `/api`, the latter
//! layered with [`auth::require_session`]. Putting the middleware on
//! a sub-router rather than per-handler keeps the auth boundary
//! visible at a glance: anything created on `protected` requires a
//! valid session.

use axum::{
    middleware,
    routing::{get, post},
    Router,
};

use super::{auth, handlers, AppState};

/// Build the top-level router. Consumes `AppState` so axum's `Clone`
/// requirement is satisfied at the Router level (each handler then
/// receives a per-request clone via `State<AppState>`).
pub fn build(state: AppState) -> Router {
    let protected = Router::new()
        .route("/state", get(handlers::state_snapshot))
        .route("/events", get(handlers::events))
        .route("/clients/{id}/kick", post(handlers::kick))
        .route("/clients/{id}/move", post(handlers::move_client))
        .route("/clients/{id}/rename", post(handlers::rename))
        .route("/logout", post(handlers::logout))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth::require_session,
        ));

    Router::new()
        // Public surface.
        .route("/", get(handlers::index))
        .route("/static/app.js", get(handlers::static_js))
        .route("/static/style.css", get(handlers::static_css))
        .route("/api/login", post(handlers::login))
        // Protected `/api/*` subtree (login + logout excluded above).
        .nest("/api", protected)
        .with_state(state)
}

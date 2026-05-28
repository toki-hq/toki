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
    routing::{get, post, put},
    Router,
};

use super::{auth, handlers, AppState};

/// Build the top-level router. Consumes `AppState` so axum's `Clone`
/// requirement is satisfied at the Router level (each handler then
/// receives a per-request clone via `State<AppState>`).
pub fn build(state: AppState) -> Router {
    let protected = Router::new()
        .route("/state", get(handlers::state_snapshot))
        .route("/server-info", get(handlers::server_info))
        .route(
            "/server-config",
            get(handlers::get_server_config).put(handlers::put_server_config),
        )
        .route("/server-password", put(handlers::put_server_password))
        .route("/events", get(handlers::events))
        .route("/clients/{id}/kick", post(handlers::kick))
        .route("/clients/{id}/move", post(handlers::move_client))
        .route("/clients/{id}/rename", post(handlers::rename))
        .route("/account/password", post(handlers::change_password))
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
        .route("/static/fonts/ui.ttf", get(handlers::font_ui))
        .route("/static/fonts/ui-bold.ttf", get(handlers::font_ui_bold))
        .route("/static/fonts/mono.ttf", get(handlers::font_mono))
        .route("/api/login", post(handlers::login))
        // Protected `/api/*` subtree (login + logout excluded above).
        .nest("/api", protected)
        .with_state(state)
}

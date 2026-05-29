//! HTTP handlers for the thin non-gRPC admin surface.
//!
//! After the gRPC-Web migration this module is small: it owns only the
//! two endpoints that have to be plain HTTP because they manage the
//! session **cookie** (`POST /api/login`, `POST /api/logout` — gRPC can't
//! ergonomically issue `Set-Cookie`), plus the SPA file server that hands
//! the embedded React build to the browser. Everything else moved to the
//! gRPC `Admin` service in [`super::grpc`].

use std::net::{IpAddr, SocketAddr};

use axum::{
    extract::{ConnectInfo, FromRequestParts, State},
    http::{header, request::Parts, StatusCode, Uri},
    response::{IntoResponse, Json, Response},
};
use serde::{Deserialize, Serialize};
use tracing::info;

use super::auth::{self, generate_session_token, session_clear_cookie, session_set_cookie};
use super::db::now_unix;
use super::AppState;
use crate::throttle::ThrottleReject;

/// Body of `POST /api/login`.
#[derive(Debug, Deserialize)]
pub struct LoginRequest {
    pub username: String,
    pub password: String,
}

/// Uniform JSON error envelope for the two HTTP endpoints. (gRPC RPCs
/// use `tonic::Status` instead.)
#[derive(Debug, Serialize)]
pub struct ApiError {
    pub error: String,
}

impl ApiError {
    pub fn new(msg: impl Into<String>) -> Self {
        Self { error: msg.into() }
    }
}

fn internal_error<E: std::fmt::Debug>(e: E) -> (StatusCode, Json<ApiError>) {
    tracing::error!(error = ?e, "admin HTTP internal error");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(ApiError::new("internal error")),
    )
}

// ── Embedded SPA ──────────────────────────────────────────────────

/// The built React admin SPA (Vite `dist/`). In debug builds rust-embed
/// reads from the filesystem at runtime (so `npm run build` is picked up
/// without recompiling); in release the bytes are baked into the binary.
#[derive(rust_embed::RustEmbed)]
#[folder = "admin-ui/dist/"]
struct Assets;

/// SPA file server. Serves an embedded asset by path; for any
/// extension-less path that isn't a real file (a client-router route like
/// `/server`), falls back to `index.html` so the SPA can route it. gRPC
/// paths (`/toki.admin.v1.Admin/*`) and `/api/*` are matched by their own
/// routes before this fallback runs.
pub async fn spa(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };

    if let Some(file) = Assets::get(path) {
        return asset_response(path, file.data.into_owned(), false);
    }
    // History fallback: no file extension → hand back index.html.
    if !path.contains('.') {
        if let Some(index) = Assets::get("index.html") {
            return asset_response("index.html", index.data.into_owned(), true);
        }
    }
    (StatusCode::NOT_FOUND, "not found").into_response()
}

/// Build an asset response: guessed Content-Type + a cache policy. Hashed
/// build assets (Vite emits content-hashed filenames under `assets/`) are
/// immutable and cached hard; `index.html` is `no-cache` so a redeploy is
/// always picked up.
fn asset_response(path: &str, body: Vec<u8>, is_index: bool) -> Response {
    let mime = mime_guess::from_path(path).first_or_octet_stream();
    let cache = if is_index || path == "index.html" {
        "no-cache, must-revalidate"
    } else {
        "public, max-age=31536000, immutable"
    };
    (
        [
            (header::CONTENT_TYPE, mime.as_ref()),
            (header::CACHE_CONTROL, cache),
        ],
        body,
    )
        .into_response()
}

// ── Cookie endpoints ──────────────────────────────────────────────

/// `POST /api/login`. Public route. Verifies the username/password
/// against argon2id, mints a session row, sets the cookie. Returns
/// `204 No Content` on success. Gated by [`crate::throttle::IpThrottle`].
pub async fn login(
    State(state): State<AppState>,
    MaybePeerIp(peer_ip): MaybePeerIp,
    Json(body): Json<LoginRequest>,
) -> Result<Response, (StatusCode, Json<ApiError>)> {
    if let Some(ip) = peer_ip {
        if let Err(reject) = state.login_throttle.try_register(ip).await {
            tracing::warn!(?ip, ?reject, "admin login throttled");
            let msg = match reject {
                ThrottleReject::RateLimited => "too many login attempts; slow down",
                ThrottleReject::Backoff => "too many failed attempts; try again later",
            };
            return Err((StatusCode::TOO_MANY_REQUESTS, Json(ApiError::new(msg))));
        }
    }

    let _ = state.db.prune_expired_sessions().await;

    let stored = state
        .db
        .get_password_hash(&body.username)
        .await
        .map_err(internal_error)?;
    // Always run a verify (sentinel hash on missing user) so the timing
    // signal between "user exists" and "doesn't" is argon2-dominated.
    let ok = match stored {
        Some(hash) => auth::verify_password(&body.password, &hash),
        None => {
            let _ = auth::verify_password(
                &body.password,
                "$argon2id$v=19$m=19456,t=2,p=1$YWFhYWFhYWFhYWFhYWFhYQ$AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            );
            false
        }
    };
    if !ok {
        if let Some(ip) = peer_ip {
            state.login_throttle.record_auth_failure(ip).await;
        }
        tracing::warn!(?peer_ip, username = %body.username, "admin login failed");
        crate::audit::record(
            &state.audit,
            "auth-fail",
            crate::audit::SYSTEM_ACTOR,
            "",
            &format!(
                "failed admin login for '{}' from {}",
                body.username,
                peer_ip.map(|i| i.to_string()).unwrap_or_else(|| "?".into())
            ),
        );
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(ApiError::new("invalid username or password")),
        ));
    }
    if let Some(ip) = peer_ip {
        state.login_throttle.record_auth_success(ip).await;
    }

    let token = generate_session_token();
    let ttl_secs = state.session_ttl.as_secs();
    let expires_at = now_unix() + ttl_secs as i64;
    state
        .db
        .create_session(&token, &body.username, expires_at)
        .await
        .map_err(internal_error)?;
    info!(username = %body.username, "admin login success");
    crate::audit::record(
        &state.audit,
        "auth-ok",
        &body.username,
        "",
        &format!(
            "admin login from {}",
            peer_ip.map(|i| i.to_string()).unwrap_or_else(|| "?".into())
        ),
    );

    let mut response = StatusCode::NO_CONTENT.into_response();
    response
        .headers_mut()
        .insert(header::SET_COOKIE, session_set_cookie(&token, ttl_secs));
    Ok(response)
}

/// `POST /api/logout`. Deletes the session row and clears the cookie.
pub async fn logout(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
) -> Result<Response, StatusCode> {
    if let Some(token) = auth::extract_session_cookie(&headers) {
        let _ = state.db.delete_session(&token).await;
    }
    let mut response = StatusCode::NO_CONTENT.into_response();
    response
        .headers_mut()
        .insert(header::SET_COOKIE, session_clear_cookie());
    Ok(response)
}

// ── Peer-IP extractor (used by login throttle) ────────────────────

/// Optional peer-IP extractor. Production binds via
/// `into_make_service_with_connect_info::<SocketAddr>()`, so the
/// `ConnectInfo<SocketAddr>` extension is present; tests driving the
/// router via `oneshot` don't set it, so the extractor returns `None`
/// and the login throttle skips its check. `Infallible` so a missing IP
/// never 500s a request.
pub struct MaybePeerIp(pub Option<IpAddr>);

impl<S> FromRequestParts<S> for MaybePeerIp
where
    S: Send + Sync,
{
    type Rejection = std::convert::Infallible;
    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        Ok(Self(
            parts
                .extensions
                .get::<ConnectInfo<SocketAddr>>()
                .map(|c| c.0.ip()),
        ))
    }
}

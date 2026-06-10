//! Integration tests for the admin panel's HTTP surface.
//!
//! After the gRPC-Web migration the HTTP router only owns the cookie
//! endpoints (`/api/login`, `/api/logout`) and the embedded-SPA file
//! server. These tests drive that router in-process via
//! `tower::ServiceExt::oneshot`. The gRPC `Admin` service (state +
//! mutations) is unit-tested in `src/admin/grpc.rs`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};

use tower::ServiceExt;

use toki_server::admin::{self, auth, db::AdminDb, AppState};
use toki_server::server_config;
use toki_server::state::{Registry, SharedRegistry};
use toki_server::throttle::IpThrottle;

/// Build the admin HTTP router backed by an in-memory sqlite with a
/// pre-seeded `admin`/`hunter2` user. Returns the router + the cleartext
/// password.
async fn boot() -> (axum::Router, &'static str) {
    let db = AdminDb::open_in_memory().await.unwrap();
    db.migrate().await.unwrap();
    db.insert_user("admin", &auth::hash_password("hunter2").unwrap())
        .await
        .unwrap();

    let (tx, _) = tokio::sync::broadcast::channel(8);
    let registry: SharedRegistry = Arc::new(tokio::sync::Mutex::new(Registry::default()));
    let state = AppState {
        registry,
        db,
        broadcaster: tx,
        session_ttl: Duration::from_secs(3600),
        started_at: Instant::now(),
        admin_bind: "127.0.0.1:0".to_string(),
        login_throttle: Arc::new(IpThrottle::new()),
        server_config: server_config::shared_default(),
        channel_names: toki_server::state::shared_channel_names(Default::default()),
        channel_mutes: toki_server::state::shared_channel_mutes(Default::default()),
        bans: toki_server::state::shared_bans(Default::default()),
        health: toki_server::metrics::shared_health(),
        live_rate: toki_server::metrics::shared_live_rate(),
        audit: toki_server::audit::channel().0,
        toml_password_override: false,
    };
    // Tests drive the HTTP router standalone (no gRPC merge). The SPA is
    // served by the standalone UI service now, so there's no fallback.
    (admin::routes::build(state), "hunter2")
}

fn extract_session_cookie(set_cookie: &str) -> Option<String> {
    let first = set_cookie.split(';').next()?;
    let (k, v) = first.split_once('=')?;
    (k.trim() == "toki_admin_session").then(|| format!("toki_admin_session={}", v.trim()))
}

async fn login(app: &axum::Router, body: serde_json::Value) -> axum::http::Response<Body> {
    app.clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/login")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn login_with_bad_password_is_401() {
    let (app, _pw) = boot().await;
    let res = login(
        &app,
        serde_json::json!({"username":"admin","password":"wrong"}),
    )
    .await;
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn login_with_unknown_user_is_401() {
    let (app, _pw) = boot().await;
    let res = login(&app, serde_json::json!({"username":"ghost","password":"x"})).await;
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn login_success_sets_secure_cookie_then_logout_clears_it() {
    let (app, pw) = boot().await;
    let res = login(&app, serde_json::json!({"username":"admin","password":pw})).await;
    assert_eq!(res.status(), StatusCode::NO_CONTENT);
    let set_cookie = res.headers()["set-cookie"].to_str().unwrap().to_string();
    assert!(set_cookie.contains("toki_admin_session="));
    assert!(set_cookie.contains("HttpOnly"));
    assert!(set_cookie.contains("Secure"));
    assert!(set_cookie.contains("SameSite=Strict"));
    let cookie = extract_session_cookie(&set_cookie).expect("session cookie");

    // Logout clears the cookie (Max-Age=0).
    let res = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/logout")
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NO_CONTENT);
    let cleared = res.headers()["set-cookie"].to_str().unwrap();
    assert!(cleared.contains("Max-Age=0"));
}

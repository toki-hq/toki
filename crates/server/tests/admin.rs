//! Integration tests for the admin web panel.
//!
//! Drives the axum router in-process via `tower::ServiceExt::oneshot`
//! (no TCP listener) so the tests are fast, deterministic, and don't
//! depend on a free port. SQLite runs in-memory via
//! `AdminDb::open_in_memory`, and we hand-roll a registry so we can
//! drop fake clients into rooms without going through the gRPC stack.
//!
//! These tests cover the full HTTP→handler→registry→db path:
//!
//! * login flow (bad pw → 401, good pw → 204 + cookie)
//! * cookie gating on `/api/state`
//! * end-to-end kick → registry mutation visible in next snapshot

use std::sync::Arc;
use std::time::{Duration, Instant};
// (Re-import via the use line above for clarity in tests.)

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use tokio::sync::Mutex;
use tower::ServiceExt;

use toki_server::admin::{self, auth, db::AdminDb, AppState};
use toki_server::server_config;
use toki_server::state::{Client, Registry, Room, SharedRegistry, TOKEN_HASH_LEN};
use toki_server::throttle::IpThrottle;

/// Build a fully-stitched admin Router + AppState backed by an
/// in-memory sqlite, a pre-seeded `admin` user with a known
/// password, and the provided registry handle. Returns the router
/// plus the cleartext password so the caller can use it to log in.
async fn boot(registry: SharedRegistry) -> (axum::Router, &'static str) {
    let db = AdminDb::open_in_memory().unwrap();
    db.migrate().await.unwrap();
    let pw_hash = auth::hash_password("hunter2").unwrap();
    db.insert_user("admin", &pw_hash).await.unwrap();

    let (tx, _) = tokio::sync::broadcast::channel(8);
    let state = AppState {
        registry,
        db,
        broadcaster: tx,
        session_ttl: Duration::from_secs(3600),
        started_at: Instant::now(),
        admin_bind: "127.0.0.1:0".to_string(),
        // Fresh throttle per test so failure backoffs from one case
        // don't bleed into the next. Tests reach the router via
        // `oneshot` which doesn't carry ConnectInfo, so the throttle
        // gate skips them — but the field must still be present.
        login_throttle: Arc::new(IpThrottle::new()),
        // Default ServerConfig; individual cases that test config
        // mutation get a fresh handle they can poke at.
        server_config: server_config::shared_default(),
        // Default: TOML didn't pin a password, so the runtime
        // db is the source of truth. A dedicated test toggles this
        // on to exercise the 409 response from PUT /api/server-password.
        toml_password_override: false,
    };
    (admin::routes::build(state), "hunter2")
}

/// Tiny helper: hand-build a `Client` populated with the minimum
/// fields the snapshot path actually reads.
fn mk_client(id: &str, name: &str, freq: Option<&str>) -> Client {
    Client {
        id: id.to_string(),
        display_name: name.to_string(),
        audio_token_hash: [0u8; TOKEN_HASH_LEN],
        audio_mac_key: [0u8; toki_proto::wire::MAC_KEY_LEN],
        audio_last_seq: 0,
        audio_outbound_seq: 1,
        audio_addr: None,
        events_tx: None,
        current_frequency: freq.map(str::to_string),
        last_seen: Instant::now(),
        connected_at: Instant::now(),
        priority_freq: None,
        expected_ip: None,
    }
}

fn shared_registry_with(clients: Vec<Client>, rooms: Vec<(&str, Room)>) -> SharedRegistry {
    let mut reg = Registry::default();
    for c in clients {
        reg.clients.insert(c.id.clone(), c);
    }
    for (f, r) in rooms {
        reg.rooms.insert(f.to_string(), r);
    }
    Arc::new(Mutex::new(reg))
}

/// Pull the `toki_admin_session` cookie out of a Set-Cookie header
/// (the `cookie=value; …` first segment).
fn extract_session_cookie(set_cookie: &str) -> Option<String> {
    let first = set_cookie.split(';').next()?;
    let (k, v) = first.split_once('=')?;
    (k.trim() == "toki_admin_session").then(|| format!("toki_admin_session={}", v.trim()))
}

/// Log in as `admin` with `pw` and return the session cookie header
/// value. Folds the repeated login boilerplate the priority tests
/// would otherwise duplicate.
async fn login_cookie(app: &axum::Router, pw: &str) -> String {
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/login")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({"username": "admin", "password": pw}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NO_CONTENT);
    extract_session_cookie(res.headers()["set-cookie"].to_str().unwrap()).unwrap()
}

#[tokio::test]
async fn state_without_session_is_401() {
    let registry = shared_registry_with(vec![], vec![]);
    let (app, _pw) = boot(registry).await;
    let res = app
        .oneshot(
            Request::builder()
                .uri("/api/state")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn login_with_bad_password_is_401() {
    let registry = shared_registry_with(vec![], vec![]);
    let (app, _pw) = boot(registry).await;
    let body = serde_json::json!({"username": "admin", "password": "wrong"}).to_string();
    let res = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/login")
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn login_with_unknown_user_is_401() {
    // The constant-time-ish path: unknown user still runs an argon2
    // verify against a sentinel, surface 401, doesn't leak existence.
    let registry = shared_registry_with(vec![], vec![]);
    let (app, _pw) = boot(registry).await;
    let body = serde_json::json!({"username": "ghost", "password": "anything"}).to_string();
    let res = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/login")
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn login_then_state_then_kick_round_trip() {
    // Full happy path: login, fetch state, kick a client, fetch state
    // again, the kicked client must be gone.
    let registry = shared_registry_with(
        vec![
            mk_client("alpha", "Alice", Some("446.05")),
            mk_client("bravo", "Bob", Some("446.05")),
        ],
        vec![(
            "446.05",
            Room {
                members: vec!["alpha".into(), "bravo".into()],
                holder: None,
            },
        )],
    );
    let (app, pw) = boot(registry.clone()).await;

    // 1. Login → 204 + Set-Cookie
    let body = serde_json::json!({"username": "admin", "password": pw}).to_string();
    let login_res = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/login")
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(login_res.status(), StatusCode::NO_CONTENT);
    let set_cookie = login_res
        .headers()
        .get("set-cookie")
        .expect("set-cookie header")
        .to_str()
        .unwrap()
        .to_string();
    let cookie = extract_session_cookie(&set_cookie).expect("session cookie");

    // 2. GET /api/state with the cookie → 200, contains both clients
    let state_res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/state")
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(state_res.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(state_res.into_body(), usize::MAX)
        .await
        .unwrap();
    let snap: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let members = snap["rooms"][0]["members"]
        .as_array()
        .expect("members array");
    assert_eq!(members.len(), 2);

    // 3. POST /api/clients/alpha/kick → 204
    let kick_res = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/clients/alpha/kick")
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(kick_res.status(), StatusCode::NO_CONTENT);

    // 4. State now shows just Bob in the room.
    let state_res2 = app
        .oneshot(
            Request::builder()
                .uri("/api/state")
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(state_res2.status(), StatusCode::OK);
    let bytes2 = axum::body::to_bytes(state_res2.into_body(), usize::MAX)
        .await
        .unwrap();
    let snap2: serde_json::Value = serde_json::from_slice(&bytes2).unwrap();
    let members2 = snap2["rooms"][0]["members"]
        .as_array()
        .expect("members array");
    assert_eq!(members2.len(), 1);
    assert_eq!(members2[0]["id"], "bravo");

    // 5. Registry state matches the API view (no orphan tokens, etc.)
    let r = registry.lock().await;
    assert!(!r.clients.contains_key("alpha"));
    assert!(r.clients.contains_key("bravo"));
}

#[tokio::test]
async fn kick_unknown_client_is_404() {
    let registry = shared_registry_with(vec![], vec![]);
    let (app, pw) = boot(registry).await;
    let body = serde_json::json!({"username": "admin", "password": pw}).to_string();
    let login = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/login")
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    let cookie = extract_session_cookie(login.headers()["set-cookie"].to_str().unwrap()).unwrap();
    let res = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/clients/does-not-exist/kick")
                .header("cookie", cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn rename_validates_display_name() {
    // Server-side validation rejects control characters; the admin
    // path must surface the same 400 a malformed gRPC Register
    // would get.
    let registry = shared_registry_with(
        vec![mk_client("c1", "Old", Some("446.05"))],
        vec![(
            "446.05",
            Room {
                members: vec!["c1".into()],
                holder: None,
            },
        )],
    );
    let (app, pw) = boot(registry).await;
    let login = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/login")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({"username": "admin", "password": pw}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let cookie = extract_session_cookie(login.headers()["set-cookie"].to_str().unwrap()).unwrap();

    let bad = serde_json::json!({"displayName": "evil\nname"}).to_string();
    let res = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/clients/c1/rename")
                .header("cookie", cookie)
                .header("content-type", "application/json")
                .body(Body::from(bad))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn server_config_get_put_round_trip() {
    let registry = shared_registry_with(vec![], vec![]);
    let (app, pw) = boot(registry).await;
    let login = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/login")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({"username": "admin", "password": pw}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let cookie = extract_session_cookie(login.headers()["set-cookie"].to_str().unwrap()).unwrap();

    // GET: defaults out of the box.
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/server-config")
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    let cfg: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(cfg["maxPeers"], 256);
    assert_eq!(cfg["idleKickSecs"], 10);

    // PUT: valid update lands.
    let put = serde_json::json!({
        "serverName": "Test Box",
        "maxPeers": 512,
        "idleKickSecs": 20,
    })
    .to_string();
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::PUT)
                .uri("/api/server-config")
                .header("cookie", &cookie)
                .header("content-type", "application/json")
                .body(Body::from(put))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    // GET reflects the new values.
    let res = app
        .oneshot(
            Request::builder()
                .uri("/api/server-config")
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    let cfg: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(cfg["serverName"], "Test Box");
    assert_eq!(cfg["maxPeers"], 512);
    assert_eq!(cfg["idleKickSecs"], 20);
}

#[tokio::test]
async fn server_config_put_validates_bounds() {
    let registry = shared_registry_with(vec![], vec![]);
    let (app, pw) = boot(registry).await;
    let login = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/login")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({"username": "admin", "password": pw}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let cookie = extract_session_cookie(login.headers()["set-cookie"].to_str().unwrap()).unwrap();

    // max_peers = 0 must 400.
    let bad = serde_json::json!({
        "serverName": "",
        "maxPeers": 0,
        "idleKickSecs": 10,
    })
    .to_string();
    let res = app
        .oneshot(
            Request::builder()
                .method(Method::PUT)
                .uri("/api/server-config")
                .header("cookie", &cookie)
                .header("content-type", "application/json")
                .body(Body::from(bad))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn server_password_put_sets_and_clears() {
    // Round-trip: set a value, GET reflects it, clear with "",
    // GET reflects the disarmed state.
    let registry = shared_registry_with(vec![], vec![]);
    let (app, pw) = boot(registry).await;
    let cookie = login_and_cookie(&app, pw).await;

    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::PUT)
                .uri("/api/server-password")
                .header("cookie", &cookie)
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({"password": "hunter2"}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NO_CONTENT);
    let cfg = fetch_server_config(&app, &cookie).await;
    assert_eq!(cfg["grpcPassword"], "hunter2");

    // Clear.
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::PUT)
                .uri("/api/server-password")
                .header("cookie", &cookie)
                .header("content-type", "application/json")
                .body(Body::from(serde_json::json!({"password": ""}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NO_CONTENT);
    let cfg = fetch_server_config(&app, &cookie).await;
    assert_eq!(cfg["grpcPassword"], "");
}

#[tokio::test]
async fn server_password_put_blocked_by_toml_override() {
    // Build an AppState explicitly with toml_password_override = true.
    // The PUT must 409 and the db value must stay untouched.
    let registry = shared_registry_with(vec![], vec![]);
    let db = AdminDb::open_in_memory().unwrap();
    db.migrate().await.unwrap();
    let pw_hash = auth::hash_password("hunter2").unwrap();
    db.insert_user("admin", &pw_hash).await.unwrap();
    let (tx, _) = tokio::sync::broadcast::channel(8);
    let state = AppState {
        registry,
        db: db.clone(),
        broadcaster: tx,
        session_ttl: Duration::from_secs(3600),
        started_at: Instant::now(),
        admin_bind: "127.0.0.1:0".to_string(),
        login_throttle: Arc::new(IpThrottle::new()),
        server_config: server_config::shared_default(),
        toml_password_override: true,
    };
    let app = admin::routes::build(state);
    let cookie = login_and_cookie(&app, "hunter2").await;

    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::PUT)
                .uri("/api/server-password")
                .header("cookie", cookie)
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({"password": "from-ui"}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CONFLICT);
    let row = db.load_server_config().await.unwrap();
    assert_eq!(row.grpc_password, "");
}

#[tokio::test]
async fn server_info_surfaces_toml_password_override_false_by_default() {
    let registry = shared_registry_with(vec![], vec![]);
    let (app, pw) = boot(registry).await;
    let cookie = login_and_cookie(&app, pw).await;
    let res = app
        .oneshot(
            Request::builder()
                .uri("/api/server-info")
                .header("cookie", cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    let info: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(info["tomlPasswordOverride"], false);
}

/// Test helper: POST /api/login + return the `Cookie:` header value.
async fn login_and_cookie(app: &axum::Router, password: &str) -> String {
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/login")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({"username": "admin", "password": password}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    extract_session_cookie(res.headers()["set-cookie"].to_str().unwrap()).unwrap()
}

/// Test helper: GET /api/server-config and parse as JSON.
async fn fetch_server_config(app: &axum::Router, cookie: &str) -> serde_json::Value {
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/server-config")
                .header("cookie", cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn change_password_happy_path_keeps_current_session() {
    // Log in, change password, then verify:
    //   * old password no longer authenticates a fresh login
    //   * new password authenticates
    //   * the cookie we changed password with still works (the
    //     handler is supposed to delete OTHER sessions, not ours)
    let registry = shared_registry_with(vec![], vec![]);
    let (app, pw) = boot(registry).await;

    let login = app.clone().oneshot(login_req("admin", pw)).await.unwrap();
    let cookie = extract_session_cookie(login.headers()["set-cookie"].to_str().unwrap()).unwrap();

    // Change password
    let body = serde_json::json!({"current": pw, "new": "n3w-passw0rd-here"}).to_string();
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/account/password")
                .header("cookie", &cookie)
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NO_CONTENT);

    // Current session cookie still works.
    let state = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/state")
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(state.status(), StatusCode::OK);

    // Fresh login with OLD password fails.
    let stale = app.clone().oneshot(login_req("admin", pw)).await.unwrap();
    assert_eq!(stale.status(), StatusCode::UNAUTHORIZED);

    // Fresh login with NEW password succeeds.
    let fresh = app
        .oneshot(login_req("admin", "n3w-passw0rd-here"))
        .await
        .unwrap();
    assert_eq!(fresh.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn change_password_rejects_wrong_current() {
    let registry = shared_registry_with(vec![], vec![]);
    let (app, pw) = boot(registry).await;
    let login = app.clone().oneshot(login_req("admin", pw)).await.unwrap();
    let cookie = extract_session_cookie(login.headers()["set-cookie"].to_str().unwrap()).unwrap();

    let body = serde_json::json!({"current": "WRONG", "new": "n3w-passw0rd-here"}).to_string();
    let res = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/account/password")
                .header("cookie", cookie)
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn change_password_rejects_short_new() {
    let registry = shared_registry_with(vec![], vec![]);
    let (app, pw) = boot(registry).await;
    let login = app.clone().oneshot(login_req("admin", pw)).await.unwrap();
    let cookie = extract_session_cookie(login.headers()["set-cookie"].to_str().unwrap()).unwrap();

    let body = serde_json::json!({"current": pw, "new": "short"}).to_string();
    let res = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/account/password")
                .header("cookie", cookie)
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn change_password_invalidates_other_sessions() {
    // Two concurrent sessions ("two browsers" pattern). After
    // changing the password from session A, session B's cookie
    // must stop working.
    let registry = shared_registry_with(vec![], vec![]);
    let (app, pw) = boot(registry).await;

    let a = app.clone().oneshot(login_req("admin", pw)).await.unwrap();
    let cookie_a = extract_session_cookie(a.headers()["set-cookie"].to_str().unwrap()).unwrap();
    let b = app.clone().oneshot(login_req("admin", pw)).await.unwrap();
    let cookie_b = extract_session_cookie(b.headers()["set-cookie"].to_str().unwrap()).unwrap();
    assert_ne!(cookie_a, cookie_b);

    // From session A, change password.
    let body = serde_json::json!({"current": pw, "new": "n3w-passw0rd-here"}).to_string();
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/account/password")
                .header("cookie", &cookie_a)
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NO_CONTENT);

    // Session A still works.
    let still_ok = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/state")
                .header("cookie", cookie_a)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(still_ok.status(), StatusCode::OK);

    // Session B is now revoked.
    let gone = app
        .oneshot(
            Request::builder()
                .uri("/api/state")
                .header("cookie", cookie_b)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(gone.status(), StatusCode::UNAUTHORIZED);
}

/// Small helper to keep the password-change tests readable. Builds
/// a POST /api/login Request with JSON credentials.
fn login_req(user: &str, password: &str) -> Request<Body> {
    Request::builder()
        .method(Method::POST)
        .uri("/api/login")
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::json!({"username": user, "password": password}).to_string(),
        ))
        .unwrap()
}

#[tokio::test]
async fn move_validates_frequency() {
    let registry = shared_registry_with(
        vec![mk_client("c1", "Alice", Some("446.05"))],
        vec![(
            "446.05",
            Room {
                members: vec!["c1".into()],
                holder: None,
            },
        )],
    );
    let (app, pw) = boot(registry).await;
    let login = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/login")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({"username": "admin", "password": pw}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let cookie = extract_session_cookie(login.headers()["set-cookie"].to_str().unwrap()).unwrap();

    // Out-of-band frequency must 400.
    let res = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/clients/c1/move")
                .header("cookie", cookie)
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({"frequency": "999.99"}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn priority_grant_then_revoke_reflected_in_snapshot() {
    // Grant priority to a channel member, confirm the snapshot flips
    // `priority` to true on the room they're in, then revoke and
    // confirm it flips back.
    let registry = shared_registry_with(
        vec![mk_client("c1", "Alice", Some("446.05"))],
        vec![(
            "446.05",
            Room {
                members: vec!["c1".into()],
                holder: None,
            },
        )],
    );
    let (app, pw) = boot(registry.clone()).await;
    let cookie = login_cookie(&app, pw).await;

    // Helper: fetch the snapshot's first-room first-member `priority`.
    async fn first_member_priority(app: &axum::Router, cookie: &str) -> bool {
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/state")
                    .header("cookie", cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        let snap: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        snap["rooms"][0]["members"][0]["priority"]
            .as_bool()
            .unwrap()
    }

    // Baseline: ordinary member.
    assert!(!first_member_priority(&app, &cookie).await);

    // Grant.
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/clients/c1/priority")
                .header("cookie", &cookie)
                .header("content-type", "application/json")
                .body(Body::from(serde_json::json!({"grant": true}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NO_CONTENT);
    assert!(first_member_priority(&app, &cookie).await);
    // Registry now anchors the priority frequency.
    assert_eq!(
        registry.lock().await.clients["c1"].priority_freq.as_deref(),
        Some("446.05")
    );

    // Revoke.
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/clients/c1/priority")
                .header("cookie", &cookie)
                .header("content-type", "application/json")
                .body(Body::from(serde_json::json!({"grant": false}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NO_CONTENT);
    assert!(!first_member_priority(&app, &cookie).await);
    assert!(registry.lock().await.clients["c1"].priority_freq.is_none());
}

#[tokio::test]
async fn priority_grant_on_lobby_member_is_400() {
    // Priority is per-channel; a member sitting in the lobby (no
    // current_frequency) can't be promoted.
    let registry = shared_registry_with(vec![mk_client("c1", "Alice", None)], vec![]);
    let (app, pw) = boot(registry).await;
    let cookie = login_cookie(&app, pw).await;

    let res = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/clients/c1/priority")
                .header("cookie", &cookie)
                .header("content-type", "application/json")
                .body(Body::from(serde_json::json!({"grant": true}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
}

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

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use tokio::sync::Mutex;
use tower::ServiceExt;

use toki_server::admin::{self, auth, db::AdminDb, AppState};
use toki_server::state::{Client, Registry, Room, SharedRegistry, TOKEN_HASH_LEN};

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

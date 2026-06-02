//! Admin authentication: argon2id password hashing, session cookies,
//! and the axum middleware that gates every `/api/*` route except
//! `/api/login`.
//!
//! # Cookie format
//!
//! `toki_admin_session=<32-hex-bytes>; Path=/; HttpOnly; SameSite=Strict; Secure; Max-Age=<ttl>`
//!
//! The admin panel is HTTPS-only, so the cookie is unconditionally
//! `Secure`: browsers refuse to send it over a non-TLS connection.
//! `HttpOnly` keeps JS from reading the value via `document.cookie`
//! (defends against XSS-driven theft if we ever inline user-supplied
//! HTML — we don't today, but it costs nothing). `SameSite=Strict`
//! blocks the cookie from being sent on any cross-site request, so
//! a hostile page on another origin can't ride an active session.
//!
//! # Session lifetime
//!
//! TTL is fixed at startup from `AdminConfig.session_ttl_hours` (default
//! 12h). On every login we opportunistically prune expired rows so the
//! sqlite table stays small.

use anyhow::Result;
use argon2::{
    password_hash::{rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
    Argon2,
};
use axum::{
    extract::{Request, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    middleware::Next,
    response::Response,
};
use rand::RngCore;

use super::{db::AdminDb, AppState};

/// Cookie name. Kept short to save a few bytes per request; the
/// `toki_` prefix makes it easy to spot in a browser inspector
/// alongside cookies from other services on the same origin.
pub const COOKIE_NAME: &str = "toki_admin_session";

/// Length in bytes of the random session token (before hex encoding).
/// 16 bytes ≈ 128 bits of entropy — overkill for sessions but cheap.
const SESSION_TOKEN_BYTES: usize = 16;

/// Length of the seeded admin password (chars in the alphanumeric
/// alphabet rand provides). 24 chars ≈ 142 bits — well past what an
/// offline brute-force on the argon2id hash can reach.
const SEEDED_PASSWORD_LEN: usize = 24;

/// Argon2id hash the given cleartext password. Returns the PHC-string
/// form (`$argon2id$v=19$m=...,t=...,p=...$salt$hash`) suitable for
/// storage in `admin_users.password_hash`.
///
/// We use `Argon2::default()` which picks the OWASP-recommended params
/// (m=19MiB, t=2, p=1). On a modern x86_64 server, that lands around
/// 10ms per hash — comfortable for login latency, prohibitive for a
/// GPU bruteforce against a stolen hash.
pub fn hash_password(cleartext: &str) -> Result<String> {
    let salt = SaltString::generate(&mut OsRng);
    let phc = Argon2::default()
        .hash_password(cleartext.as_bytes(), &salt)
        .map_err(|e| anyhow::anyhow!("argon2 hash: {e}"))?;
    Ok(phc.to_string())
}

/// Verify a cleartext password against a stored argon2id hash. Returns
/// `true` only on an exact match. A malformed `stored_hash` (corrupt
/// row) returns `false` rather than erroring so a single bad row
/// doesn't take down the whole login path.
pub fn verify_password(cleartext: &str, stored_hash: &str) -> bool {
    let Ok(parsed) = PasswordHash::new(stored_hash) else {
        // Don't differentiate "bad hash" from "wrong password" in the
        // response — both surface as 401. Logging is the operator's
        // path here, not the API consumer's.
        tracing::warn!("admin password hash failed to parse — login will always fail");
        return false;
    };
    Argon2::default()
        .verify_password(cleartext.as_bytes(), &parsed)
        .is_ok()
}

/// Generate a fresh random session token, hex-encoded. Hex (not
/// base64) so the value is `A-Za-z0-9` — safe in any cookie context
/// without escaping concerns.
pub fn generate_session_token() -> String {
    let mut bytes = [0u8; SESSION_TOKEN_BYTES];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    hex::encode(bytes)
}

/// Generate a fresh random alphanumeric password. Used exactly once,
/// in [`seed_admin_if_empty`], to bootstrap an operator account on
/// first boot.
pub fn generate_admin_password() -> String {
    use rand::distributions::{Alphanumeric, DistString};
    Alphanumeric.sample_string(&mut rand::thread_rng(), SEEDED_PASSWORD_LEN)
}

/// On first boot (empty `admin_users` table), generate a random
/// password, argon2 hash it, insert an `admin` row, and log the
/// cleartext password once at `WARN`. Idempotent: a second call on a
/// populated db is a no-op.
///
/// # The WARN log is your only chance
///
/// We deliberately don't write the cleartext to disk anywhere. The
/// operator must capture it from journalctl / docker logs / wherever
/// their tracing subscriber lands. If they miss it, the recovery
/// path is "rm admin.db and restart" — there is no UI to reset.
pub async fn seed_admin_if_empty(db: &AdminDb) -> Result<()> {
    if db.user_count().await? > 0 {
        return Ok(());
    }
    let password = generate_admin_password();
    let hash = hash_password(&password)?;
    db.insert_user("admin", &hash).await?;
    // The `password = %password` field uses Display, so the cleartext
    // lands in the structured log payload — easy to grep, easy to
    // redact later if needed.
    tracing::warn!(
        user = "admin",
        password = %password,
        "admin user seeded — record this password now, it will NOT be shown again",
    );
    Ok(())
}

/// Extract the `toki_admin_session` cookie value from a request's
/// headers. Returns `None` if the header is absent or the cookie
/// jar doesn't contain our session cookie. We parse manually rather
/// than pulling in the `cookie` crate — one cookie, one place to
/// read it.
pub fn extract_session_cookie(headers: &HeaderMap) -> Option<String> {
    // A request can carry more than one `Cookie` header (HTTP permits
    // it, and some proxies split them), and each header packs several
    // `;`-separated `name=value` pairs. Scan every header and every
    // pair, returning our session value as soon as the key appears —
    // unrelated cookies on the same origin (analytics, a reverse
    // proxy's own session, etc.) must never shadow it. `headers.get`
    // would only see the *first* header; `get_all` covers them all.
    // A header value that isn't valid UTF-8 is skipped rather than
    // aborting the whole lookup.
    for header_val in headers.get_all(header::COOKIE) {
        let Ok(raw) = header_val.to_str() else {
            continue;
        };
        if let Some(v) = parse_session_cookie(raw) {
            return Some(v);
        }
    }
    None
}

/// Pull the `toki_admin_session` value out of a single raw `Cookie`
/// header string (the `;`-separated `name=value` list). Shared by the
/// HTTP middleware (above) and the gRPC auth interceptor, which reads
/// the cookie from request metadata rather than an axum `HeaderMap`.
pub fn parse_session_cookie(raw: &str) -> Option<String> {
    for part in raw.split(';') {
        let Some((k, v)) = part.trim().split_once('=') else {
            continue;
        };
        if k.trim() == COOKIE_NAME {
            return Some(v.trim().to_string());
        }
    }
    None
}

/// Build the `Set-Cookie` value for a freshly-issued session.
///
/// `Secure` is unconditional — the admin panel only serves over
/// TLS, and we want browsers to refuse to send the cookie back if
/// they're ever tricked into trying a plaintext connection to the
/// same host (e.g. via misconfigured reverse proxy).
pub fn session_set_cookie(token: &str, ttl_secs: u64) -> HeaderValue {
    let v = format!(
        "{name}={token}; Path=/; HttpOnly; Secure; SameSite=Strict; Max-Age={ttl}",
        name = COOKIE_NAME,
        token = token,
        ttl = ttl_secs,
    );
    // The format string only contains `Path=/; HttpOnly; Secure;
    // SameSite=Strict`, `Max-Age=` followed by a decimal, and the
    // token (hex digits) — all valid header bytes, so `from_str`
    // here can't realistically fail.
    HeaderValue::from_str(&v).expect("session cookie is ASCII-safe by construction")
}

/// Build the `Set-Cookie` value that *clears* the session — used by
/// `/api/logout`. Same name, empty value, `Max-Age=0`. Carries the
/// same attributes as the set-form so browsers reliably scope the
/// clear to the right cookie.
pub fn session_clear_cookie() -> HeaderValue {
    let v = format!(
        "{name}=; Path=/; HttpOnly; Secure; SameSite=Strict; Max-Age=0",
        name = COOKIE_NAME
    );
    HeaderValue::from_str(&v).expect("clear cookie is ASCII-safe by construction")
}

/// Axum middleware that requires a valid session cookie. Applied as
/// a layer over the protected `/api/*` subtree in
/// [`routes::build`](super::routes::build). Wraps the rest of the
/// chain by extracting + verifying the cookie; on failure, short-
/// circuits with `401`.
pub async fn require_session(
    State(state): State<AppState>,
    req: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    // Diagnostic: at debug level, surface whether the browser sent a
    // Cookie header at all and whether our session cookie was among
    // its parts. Helpful when triaging "login succeeds but next call
    // is 401" — turn on with `RUST_LOG=toki_server::admin=debug` to
    // see exactly what the middleware is seeing.
    let raw_cookie = req
        .headers()
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("(absent)");
    let path = req.uri().path().to_string();
    let token = match extract_session_cookie(req.headers()) {
        Some(t) => t,
        None => {
            tracing::debug!(
                %path,
                cookie_header = %raw_cookie,
                "401: no session cookie in request",
            );
            return Err(StatusCode::UNAUTHORIZED);
        }
    };
    let row = match state.db.lookup_session(&token).await.map_err(|e| {
        tracing::error!(error = ?e, "session lookup failed");
        StatusCode::INTERNAL_SERVER_ERROR
    })? {
        Some(r) => r,
        None => {
            // Most common failure mode after a server restart: the
            // browser still has a cookie from the previous run whose
            // token row is gone (or expired) in the new sqlite. Log
            // a hint so the operator sees the cause in the journal.
            tracing::debug!(
                %path,
                token_prefix = &token[..token.len().min(8)],
                "401: session token unknown or expired (stale cookie?)",
            );
            return Err(StatusCode::UNAUTHORIZED);
        }
    };
    // Stash the username on the request extensions so handlers can
    // pull it out for audit logging without a second db hit.
    let mut req = req;
    req.extensions_mut().insert(AdminUser(row.username));
    Ok(next.run(req).await)
}

/// Marker carried in request extensions by [`require_session`].
/// Handlers extract it via `Extension(AdminUser(name))` to attribute
/// mutations in the audit log.
#[derive(Clone, Debug)]
pub struct AdminUser(pub String);

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderMap;

    #[test]
    fn hash_then_verify_round_trips() {
        let hash = hash_password("hunter2").unwrap();
        assert!(verify_password("hunter2", &hash));
        // A different password must not verify.
        assert!(!verify_password("hunter3", &hash));
        // Empty input is a different password, not a special case.
        assert!(!verify_password("", &hash));
    }

    #[test]
    fn verify_against_garbage_hash_is_false_not_panic() {
        // A row corrupted by manual db edits must surface as "wrong
        // password" rather than blowing up the login handler.
        assert!(!verify_password("anything", "not-a-real-phc-string"));
        assert!(!verify_password("", ""));
    }

    #[test]
    fn generated_session_tokens_are_unique_and_hex() {
        let a = generate_session_token();
        let b = generate_session_token();
        assert_ne!(a, b);
        assert_eq!(a.len(), SESSION_TOKEN_BYTES * 2);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn generated_admin_password_has_expected_shape() {
        let pw = generate_admin_password();
        assert_eq!(pw.len(), SEEDED_PASSWORD_LEN);
        // Alphanumeric → no shell metacharacters, easy to copy.
        assert!(pw.chars().all(|c| c.is_ascii_alphanumeric()));
    }

    #[test]
    fn extract_cookie_finds_the_session() {
        let mut h = HeaderMap::new();
        h.insert(
            header::COOKIE,
            "foo=bar; toki_admin_session=abc123; baz=qux"
                .parse()
                .unwrap(),
        );
        assert_eq!(extract_session_cookie(&h).as_deref(), Some("abc123"));
    }

    #[test]
    fn extract_cookie_returns_none_when_absent() {
        let mut h = HeaderMap::new();
        // No Cookie header at all.
        assert!(extract_session_cookie(&h).is_none());
        // Cookie header without our key.
        h.insert(header::COOKIE, "other=value".parse().unwrap());
        assert!(extract_session_cookie(&h).is_none());
    }

    #[test]
    fn extract_cookie_scans_multiple_cookie_headers() {
        // A client/proxy may split cookies across several `Cookie`
        // headers. `headers.get` would only see the first (here without
        // our key); we must scan them all and still find the session.
        let mut h = HeaderMap::new();
        h.append(
            header::COOKIE,
            "analytics=xyz; ph_session=1".parse().unwrap(),
        );
        h.append(
            header::COOKIE,
            "toki_admin_session=tok42; theme=dark".parse().unwrap(),
        );
        assert_eq!(extract_session_cookie(&h).as_deref(), Some("tok42"));
    }

    #[test]
    fn extract_cookie_finds_session_in_any_position() {
        // First, middle, and last positions among other fields.
        for raw in [
            "toki_admin_session=tok; a=1; b=2",
            "a=1; toki_admin_session=tok; b=2",
            "a=1; b=2; toki_admin_session=tok",
        ] {
            let mut h = HeaderMap::new();
            h.insert(header::COOKIE, raw.parse().unwrap());
            assert_eq!(
                extract_session_cookie(&h).as_deref(),
                Some("tok"),
                "failed for: {raw}"
            );
        }
    }

    #[tokio::test]
    async fn seed_admin_is_idempotent() {
        let db = AdminDb::open_in_memory().await.unwrap();
        db.migrate().await.unwrap();
        seed_admin_if_empty(&db).await.unwrap();
        assert_eq!(db.user_count().await.unwrap(), 1);
        // Second call must not create another user.
        seed_admin_if_empty(&db).await.unwrap();
        assert_eq!(db.user_count().await.unwrap(), 1);
    }
}

//! ACME (Let's Encrypt) automatic certificate issuance + renewal via
//! the HTTP-01 challenge.
//!
//! When `[acme]` is enabled, a background task obtains a browser-trusted
//! certificate for the configured domain(s) and renews it before expiry,
//! hot-swapping each new cert into the shared [`CertResolver`] so both
//! the gRPC and admin listeners serve it with **no restart**.
//!
//! HTTP-01 flow: for each domain authorization we publish a
//! `token → key_authorization` entry into a [`ChallengeMap`] that the
//! port-80 listener (see `admin::serve_acme_http`) answers at
//! `/.well-known/acme-challenge/{token}`; Let's Encrypt fetches it over
//! plain HTTP on port 80 to prove we control the domain.
//!
//! Account credentials and the issued cert/key (+ an `issued_at` marker)
//! are cached under `{data_dir}/tls/acme/` so restarts reuse the cert
//! and we stay well within Let's Encrypt's issuance rate limits.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use instant_acme::{
    Account, AccountCredentials, ChallengeType, Identifier, LetsEncrypt, NewAccount, NewOrder,
    OrderStatus, RetryPolicy,
};

use crate::cert_store::{certified_key_from_pem, CertResolver};
use crate::config::{AcmeConfig, DEFAULT_ACME_DIR};

/// Token → key-authorization map the port-80 challenge listener reads.
/// Held very briefly under a std `RwLock` (no `.await` while locked).
pub type ChallengeMap = Arc<RwLock<HashMap<String, String>>>;

/// Certificates live ~90 days; renew once two-thirds of that has passed
/// so a transient renewal failure has ~30 days of runway to recover in.
const RENEW_AFTER: Duration = Duration::from_secs(60 * 24 * 60 * 60);
/// Backoff between failed issuance attempts (kept well under the renewal
/// runway so repeated failures still get many tries before expiry).
const RETRY_BACKOFF: Duration = Duration::from_secs(15 * 60);

/// Fresh, empty challenge map.
pub fn challenge_map() -> ChallengeMap {
    Arc::new(RwLock::new(HashMap::new()))
}

/// Answer for `/.well-known/acme-challenge/{token}`, or `None` if the
/// token isn't an outstanding challenge.
pub fn challenge_response(map: &ChallengeMap, token: &str) -> Option<String> {
    map.read().ok()?.get(token).cloned()
}

fn acme_dir(data_dir: &Path) -> PathBuf {
    data_dir.join(DEFAULT_ACME_DIR)
}
fn account_path(data_dir: &Path) -> PathBuf {
    acme_dir(data_dir).join("account.json")
}
fn cert_path(data_dir: &Path) -> PathBuf {
    acme_dir(data_dir).join("cert.pem")
}
fn key_path(data_dir: &Path) -> PathBuf {
    acme_dir(data_dir).join("key.pem")
}
fn issued_at_path(data_dir: &Path) -> PathBuf {
    acme_dir(data_dir).join("issued_at")
}

/// Load a cached ACME cert/key pair from `{data_dir}/tls/acme/`, if both
/// are present. Used by `main.rs` to seed the resolver at boot with the
/// real cert (avoiding a self-signed window) before the renewal task runs.
pub fn load_cached(data_dir: &Path) -> Option<(Vec<u8>, Vec<u8>)> {
    let cert = std::fs::read(cert_path(data_dir)).ok()?;
    let key = std::fs::read(key_path(data_dir)).ok()?;
    Some((cert, key))
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Seconds-since-epoch the cached cert was issued, if recorded.
fn cached_issued_at(data_dir: &Path) -> Option<u64> {
    std::fs::read_to_string(issued_at_path(data_dir))
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// Background driver: issue now if there's no fresh cached cert, then
/// loop renewing ~30 days before expiry. Failures are logged and retried
/// with backoff; the currently-served cert (seeded by `main.rs`) keeps
/// working throughout, so a renewal hiccup never takes the server down.
pub async fn run(
    cfg: AcmeConfig,
    data_dir: PathBuf,
    resolver: Arc<CertResolver>,
    challenges: ChallengeMap,
) -> Result<()> {
    loop {
        // How long until the cached cert is due for renewal? `None` →
        // no/expired cache → issue immediately.
        let wait = cached_issued_at(&data_dir).and_then(|issued| {
            let due = issued + RENEW_AFTER.as_secs();
            due.checked_sub(now_unix()).filter(|&s| s > 0)
        });

        if let Some(secs) = wait {
            tracing::info!(
                renew_in_days = secs / 86_400,
                "ACME cert valid; sleeping until renewal"
            );
            tokio::time::sleep(Duration::from_secs(secs)).await;
        }

        match obtain_and_store(&cfg, &data_dir, &resolver, &challenges).await {
            Ok(()) => {
                tracing::info!(domains = ?cfg.domains, "ACME certificate installed");
                // Loop back: next iteration computes the long sleep until
                // the new cert's renewal window.
            }
            Err(e) => {
                tracing::error!(error = %format!("{e:#}"),
                    "ACME issuance failed; retrying after backoff (serving existing cert)");
                tokio::time::sleep(RETRY_BACKOFF).await;
            }
        }
    }
}

/// One full issuance: load/create account, run the HTTP-01 order, swap
/// the resolver, and persist the cert/key/marker for restart reuse.
async fn obtain_and_store(
    cfg: &AcmeConfig,
    data_dir: &Path,
    resolver: &Arc<CertResolver>,
    challenges: &ChallengeMap,
) -> Result<()> {
    std::fs::create_dir_all(acme_dir(data_dir))
        .with_context(|| format!("create {}", acme_dir(data_dir).display()))?;

    let account = load_or_create_account(cfg, data_dir).await?;
    let (cert_pem, key_pem) = issue(&account, cfg, challenges).await?;

    // Swap into the live resolver first — that's what makes the new cert
    // take effect on the next handshake on both listeners.
    let ck = certified_key_from_pem(cert_pem.as_bytes(), key_pem.as_bytes())
        .context("build CertifiedKey from freshly issued PEM")?;
    resolver.store(ck);

    // Then persist so a restart reuses this cert instead of re-issuing.
    std::fs::write(cert_path(data_dir), &cert_pem).context("write acme cert.pem")?;
    std::fs::write(key_path(data_dir), &key_pem).context("write acme key.pem")?;
    crate::tls::tighten_key_perms(&key_path(data_dir));
    std::fs::write(issued_at_path(data_dir), now_unix().to_string())
        .context("write acme issued_at")?;
    Ok(())
}

/// Restore the ACME account from cached credentials, or register a new
/// one and persist the credentials for next time.
async fn load_or_create_account(cfg: &AcmeConfig, data_dir: &Path) -> Result<Account> {
    let directory = if cfg.staging {
        LetsEncrypt::Staging.url()
    } else {
        LetsEncrypt::Production.url()
    }
    .to_owned();

    if let Ok(json) = std::fs::read_to_string(account_path(data_dir)) {
        let creds: AccountCredentials =
            serde_json::from_str(&json).context("parse cached ACME account.json")?;
        let account = Account::builder()
            .context("ACME account builder")?
            .from_credentials(creds)
            .await
            .context("restore ACME account from cached credentials")?;
        tracing::info!("restored ACME account from cache");
        return Ok(account);
    }

    let mailto = format!("mailto:{}", cfg.contact_email.trim());
    let contact = [mailto.as_str()];
    let (account, creds) = Account::builder()
        .context("ACME account builder")?
        .create(
            &NewAccount {
                contact: &contact,
                terms_of_service_agreed: cfg.terms_of_service_agreed,
                only_return_existing: false,
            },
            directory,
            None,
        )
        .await
        .context("register new ACME account")?;
    std::fs::write(
        account_path(data_dir),
        serde_json::to_string(&creds).context("serialize ACME credentials")?,
    )
    .context("write acme account.json")?;
    crate::tls::tighten_key_perms(&account_path(data_dir));
    tracing::info!(staging = cfg.staging, "registered new ACME account");
    Ok(account)
}

/// Drive a single order through the HTTP-01 challenge to a cert chain.
/// Returns `(cert_chain_pem, private_key_pem)`.
async fn issue(
    account: &Account,
    cfg: &AcmeConfig,
    challenges: &ChallengeMap,
) -> Result<(String, String)> {
    let identifiers: Vec<Identifier> = cfg
        .domains
        .iter()
        .map(|d| Identifier::Dns(d.clone()))
        .collect();
    let mut order = account
        .new_order(&NewOrder::new(&identifiers))
        .await
        .context("create ACME order")?;

    // For each pending authorization, publish the HTTP-01 key
    // authorization under its token and tell the server it's ready.
    // Scoped so the `&mut order` borrow ends before `poll_ready` below.
    {
        let mut authorizations = order.authorizations();
        while let Some(result) = authorizations.next().await {
            let mut authz = result.context("fetch ACME authorization")?;
            if let instant_acme::AuthorizationStatus::Valid = authz.status {
                continue;
            }
            let mut challenge = authz
                .challenge(ChallengeType::Http01)
                .context("server offered no HTTP-01 challenge")?;
            let token = challenge.token.clone();
            let key_auth = challenge.key_authorization().as_str().to_owned();
            if let Ok(mut map) = challenges.write() {
                map.insert(token, key_auth);
            }
            challenge
                .set_ready()
                .await
                .context("mark HTTP-01 challenge ready")?;
        }
    }

    // Wait for validation, then finalize (instant-acme generates the
    // keypair and returns its PEM) and download the issued chain.
    let status = order
        .poll_ready(&RetryPolicy::default())
        .await
        .context("poll ACME order to ready")?;
    if status != OrderStatus::Ready {
        // Clear our challenge entries before bailing.
        if let Ok(mut map) = challenges.write() {
            map.clear();
        }
        anyhow::bail!("ACME order did not become ready (status {status:?})");
    }

    let key_pem = order.finalize().await.context("finalize ACME order")?;
    let cert_pem = order
        .poll_certificate(&RetryPolicy::default())
        .await
        .context("download issued certificate")?;

    // Challenges are spent — drop them so the port-80 path returns 404.
    if let Ok(mut map) = challenges.write() {
        map.clear();
    }
    Ok((cert_pem, key_pem))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn challenge_response_round_trips() {
        let map = challenge_map();
        map.write()
            .unwrap()
            .insert("tok123".into(), "tok123.thumb".into());
        assert_eq!(
            challenge_response(&map, "tok123").as_deref(),
            Some("tok123.thumb")
        );
        assert_eq!(challenge_response(&map, "missing"), None);
    }

    #[test]
    fn load_cached_returns_none_without_files() {
        let dir = std::env::temp_dir().join(format!("toki-acme-empty-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(load_cached(&dir).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cached_issued_at_parses_marker() {
        let dir = std::env::temp_dir().join(format!("toki-acme-marker-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(acme_dir(&dir)).unwrap();
        assert_eq!(cached_issued_at(&dir), None);
        std::fs::write(issued_at_path(&dir), "1700000000\n").unwrap();
        assert_eq!(cached_issued_at(&dir), Some(1_700_000_000));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

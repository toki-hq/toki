//! Hot-swappable TLS certificate shared by the gRPC and admin listeners.
//!
//! Both TLS listeners (gRPC signaling and the admin panel) resolve their
//! certificate through a single shared [`CertResolver`]. The resolver
//! holds the current cert in an [`ArcSwap`], so a Let's Encrypt renewal
//! can [`CertResolver::store`] a freshly-issued cert and **every
//! subsequent handshake on either port** serves it — no listener
//! restart, no in-flight connection dropped.
//!
//! We build the listeners' [`rustls::ServerConfig`]s from this resolver
//! ([`server_config`]) instead of baking in a static identity, which is
//! what lets the cert change at runtime (tonic's `ServerTlsConfig::
//! identity` is fixed for the life of the server).

use std::fmt;
use std::sync::Arc;

use anyhow::{Context, Result};
use arc_swap::ArcSwap;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use rustls::ServerConfig;

/// A [`ResolvesServerCert`] whose certificate can be swapped atomically
/// at runtime. SNI is ignored — Toki serves one cert (covering the
/// operator's configured domain set), the same on both ports.
pub struct CertResolver {
    current: ArcSwap<CertifiedKey>,
}

// `ResolvesServerCert: Debug`. `CertifiedKey`/`ArcSwap` don't give us a
// useful derive, and we don't want to print key material anyway.
impl fmt::Debug for CertResolver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CertResolver").finish_non_exhaustive()
    }
}

impl CertResolver {
    /// Create a resolver seeded with an initial cert (typically the
    /// self-signed or cached-ACME pair resolved at boot).
    pub fn new(initial: Arc<CertifiedKey>) -> Self {
        Self {
            current: ArcSwap::new(initial),
        }
    }

    /// Atomically replace the served certificate. Picked up by the next
    /// handshake on every listener sharing this resolver.
    pub fn store(&self, ck: Arc<CertifiedKey>) {
        self.current.store(ck);
    }
}

impl ResolvesServerCert for CertResolver {
    fn resolve(&self, _client_hello: ClientHello) -> Option<Arc<CertifiedKey>> {
        Some(self.current.load_full())
    }
}

/// Parse a PEM certificate chain + private key into a rustls
/// [`CertifiedKey`], signed via the ring provider. Used both to seed
/// the resolver at boot and to build the new cert on each ACME renewal.
pub fn certified_key_from_pem(cert_pem: &[u8], key_pem: &[u8]) -> Result<Arc<CertifiedKey>> {
    let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut &cert_pem[..])
        .collect::<std::result::Result<_, _>>()
        .context("parse TLS certificate chain PEM")?;
    anyhow::ensure!(!certs.is_empty(), "no certificates found in cert PEM");

    let key: PrivateKeyDer<'static> = rustls_pemfile::private_key(&mut &key_pem[..])
        .context("parse TLS private key PEM")?
        .context("no private key found in key PEM")?;

    let signing_key = rustls::crypto::ring::sign::any_supported_type(&key)
        .context("build signing key from private key")?;
    Ok(Arc::new(CertifiedKey::new(certs, signing_key)))
}

/// Build a [`rustls::ServerConfig`] that resolves its certificate
/// through the shared `resolver`, advertising the given ALPN protocols
/// (`[b"h2"]` for the gRPC port; `[b"h2", b"http/1.1"]` for the admin
/// panel). The ring provider is passed explicitly so the build can't
/// trip on an ambiguous default provider — both `ring` and `aws-lc-rs`
/// are linked transitively via rustls's default features.
pub fn server_config(resolver: Arc<CertResolver>, alpn: &[&[u8]]) -> Result<Arc<ServerConfig>> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut cfg = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .context("rustls: safe default protocol versions")?
        .with_no_client_auth()
        .with_cert_resolver(resolver);
    cfg.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
    Ok(Arc::new(cfg))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Generate a self-signed cert/key PEM pair for tests.
    fn self_signed_pem(cn: &str) -> (Vec<u8>, Vec<u8>) {
        let cert = rcgen::generate_simple_self_signed(vec![cn.to_string()]).unwrap();
        (
            cert.cert.pem().into_bytes(),
            cert.key_pair.serialize_pem().into_bytes(),
        )
    }

    #[test]
    fn certified_key_round_trips_from_pem() {
        let (cert, key) = self_signed_pem("localhost");
        let ck = certified_key_from_pem(&cert, &key).expect("build CertifiedKey");
        assert!(!ck.cert.is_empty(), "cert chain populated");
    }

    #[test]
    fn certified_key_rejects_garbage() {
        assert!(certified_key_from_pem(b"not a cert", b"not a key").is_err());
    }

    #[test]
    fn resolver_swaps_the_served_cert() {
        let (c1, k1) = self_signed_pem("first.example");
        let (c2, k2) = self_signed_pem("second.example");
        let first = certified_key_from_pem(&c1, &k1).unwrap();
        let second = certified_key_from_pem(&c2, &k2).unwrap();

        let resolver = Arc::new(CertResolver::new(first.clone()));
        // Before swap: resolver hands back the first cert.
        assert!(Arc::ptr_eq(&resolver.current.load_full(), &first));
        resolver.store(second.clone());
        // After swap: the second cert, no rebuild of the resolver.
        assert!(Arc::ptr_eq(&resolver.current.load_full(), &second));
    }

    #[test]
    fn builds_server_config_with_alpn() {
        let (cert, key) = self_signed_pem("localhost");
        let ck = certified_key_from_pem(&cert, &key).unwrap();
        let resolver = Arc::new(CertResolver::new(ck));
        let cfg = server_config(resolver, &[b"h2", b"http/1.1"]).expect("server config");
        assert_eq!(
            cfg.alpn_protocols,
            vec![b"h2".to_vec(), b"http/1.1".to_vec()]
        );
    }
}

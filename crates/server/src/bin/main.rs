use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Context as _;
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
use tokio_stream::wrappers::ReceiverStream;
use tonic::transport::Server;
use tracing_subscriber::EnvFilter;

use toki_server::{
    acme, admin, audio, audit, cert_store,
    config::{self, Config},
    metrics, reaper, server_config,
    signaling::SignalingSvc,
    state, tls,
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    // rustls 0.23 requires a process-level `CryptoProvider` to be
    // installed before the first TLS handshake. The `ring` feature on
    // our rustls dep gives us the backend; we wire it as the default
    // here so neither tonic's server-side TLS plumbing nor any of our
    // own rustls usage panics on first use.
    rustls::crypto::ring::default_provider()
        .install_default()
        .map_err(|_| anyhow::anyhow!("rustls crypto provider already installed"))?;

    // Config file is optional; missing => open mode. A *malformed*
    // file aborts startup so we don't silently disarm the password
    // gate because of a TOML typo. Log both signals explicitly so an
    // operator can see at a glance whether the file was read and
    // whether the password gate ended up armed.
    let (config, config_path) = Config::load()?;
    match &config_path {
        Some(path) => tracing::info!(path = %path.display(), "config file loaded"),
        None => tracing::info!(
            "no config file resolved (TOKI_CONFIG unset, ./config.toml absent); using defaults"
        ),
    }
    let password = config.normalised_password();
    if password.is_some() {
        tracing::info!("password gate ARMED — clients must supply the configured password");
    } else {
        tracing::info!("password gate DISARMED — server is in open mode");
    }

    // Resolve the runtime data root (`$TOKI_DATA_DIR`, defaults to
    // `.`). Auto-generated TLS certs land under `{data_dir}/tls/`,
    // and any relative `[admin] db_path` resolves against it too —
    // so Docker images can pin everything stateful to `/data` with
    // a single env var while leaving absolute operator paths
    // (e.g. `/etc/letsencrypt/...`) untouched.
    let data_dir = config::data_dir();
    std::fs::create_dir_all(&data_dir)
        .with_context(|| format!("create data dir {}", data_dir.display()))?;
    tracing::info!(data_dir = %data_dir.display(), "data dir resolved");

    // TLS is mandatory. Precedence for the cert the server serves:
    //   1. operator `[tls]` cert/key paths (operator-managed; ACME off);
    //   2. `[acme]` Let's Encrypt (HTTP-01) — auto-issued + renewed;
    //   3. auto-generated self-signed pair under `{data_dir}/tls/`.
    // Either way gRPC + admin are always HTTPS — there is no plaintext.
    //
    // ACME only runs when enabled *and* no explicit `[tls]` is set
    // (explicit operator certs win). When both are configured we honour
    // `[tls]` and warn that ACME is being ignored.
    let acme_active = config.acme.is_active() && config.tls.is_none();
    if config.acme.is_active() && config.tls.is_some() {
        tracing::warn!("[tls] cert paths take precedence; [acme] is configured but ignored");
    }

    // Resolve the *seed* cert the listeners bind with immediately. With
    // ACME active we prefer a cached previously-issued cert (so a restart
    // serves the real cert at once); otherwise the operator/self-signed
    // pair. The ACME task swaps in a fresh cert in the background.
    let tls_material = tls::TlsMaterial::resolve(config.tls.as_ref(), &data_dir)?;
    let (seed_cert, seed_key) = match acme_active.then(|| acme::load_cached(&data_dir)).flatten() {
        Some((cert, key)) => {
            tracing::info!("seeding TLS from cached ACME certificate");
            (cert, key)
        }
        None => (tls_material.cert_pem.clone(), tls_material.key_pem.clone()),
    };

    // One shared, hot-swappable resolver feeds both listeners. The gRPC
    // config advertises `h2`; the admin config adds `http/1.1` for
    // browsers. An ACME renewal calls `resolver.store(..)` and both
    // pick it up on the next handshake — no restart.
    let initial_ck = cert_store::certified_key_from_pem(&seed_cert, &seed_key)
        .context("build initial TLS cert")?;
    let resolver = Arc::new(cert_store::CertResolver::new(initial_ck));
    let grpc_tls = cert_store::server_config(resolver.clone(), &[b"h2"])?;
    let admin_tls = cert_store::server_config(resolver.clone(), &[b"h2", b"http/1.1"])?;
    tracing::info!(
        acme = acme_active,
        seed_source = %tls_material.source.display(),
        "TLS ARMED — gRPC + admin serve HTTPS/2 via shared cert resolver"
    );

    // HTTP-01 challenge map, shared between the ACME task (writer) and
    // the admin port-80 listener (reader). Only meaningful when ACME is
    // active, but cheap to always create.
    let challenges = acme::challenge_map();
    if acme_active {
        tracing::info!(
            domains = ?config.acme.domains,
            http_bind = %config.acme.http_bind,
            staging = config.acme.staging,
            "ACME enabled — obtaining/renewing Let's Encrypt certificate (HTTP-01)"
        );
        tokio::spawn(acme::run(
            config.acme.clone(),
            data_dir.clone(),
            resolver.clone(),
            challenges.clone(),
        ));
    }

    // gRPC (TCP) and audio (UDP) default to the **same** port
    // number. `(TCP, 50051)` and `(UDP, 50051)` are distinct binding
    // tuples at the kernel level — DNS, NTP, QUIC+HTTPS all do the
    // same — so a single firewall/NAT rule (50051 TCP+UDP) covers
    // both. Either env var can still pick a different port; this
    // is just the out-of-the-box default.
    let grpc_addr: SocketAddr = std::env::var("TOKI_GRPC_ADDR")
        .unwrap_or_else(|_| "0.0.0.0:50051".into())
        .parse()?;
    let audio_bind: SocketAddr = std::env::var("TOKI_AUDIO_ADDR")
        .unwrap_or_else(|_| "0.0.0.0:50051".into())
        .parse()?;
    let advertised_audio =
        std::env::var("TOKI_AUDIO_PUBLIC").unwrap_or_else(|_| audio_bind.to_string());

    let registry = state::shared();

    // Admin-assigned channel names. Starts empty; the admin task, if
    // enabled, loads the persisted names from sqlite right after it
    // opens the db (same bootstrap dance as `server_config` below).
    // Headless deployments keep it empty — and with named channels
    // off by default, the signaling path never consults it anyway.
    let channel_names = state::shared_channel_names(std::collections::HashMap::new());

    // Runtime-mutable server settings. Starts at hardcoded defaults
    // (same values the code shipped before this lived in the db);
    // the admin task, if enabled, will overwrite from sqlite right
    // after it opens. Headless deployments without `[admin]` just
    // keep these defaults for the lifetime of the process.
    let server_config = server_config::shared_default();

    // Voice-relay byte counters (ingress/egress), shared with the UDP
    // audio task that bumps them and the admin metrics sampler that
    // reads deltas.
    let byte_counters = metrics::shared_counters();

    // Audit-log pipeline: producers (signaling, reaper, admin RPCs,
    // login) push events onto this sink; the admin task owns the single
    // writer that drains it to sqlite (it holds the db handle).
    let (audit_tx, audit_rx) = audit::channel();

    // Reaper runs forever in the background; we don't .await it in the
    // select! below — if it panics, tracing surfaces it but the server
    // keeps serving (just without stale-client cleanup).
    tokio::spawn(reaper::run(
        registry.clone(),
        server_config.clone(),
        audit_tx.clone(),
    ));

    let audio_task = tokio::spawn(audio::run(
        audio_bind,
        registry.clone(),
        byte_counters.clone(),
    ));

    // Admin panel is always exposed. When `[admin]` is omitted from
    // config.toml the defaults take over (bind = 127.0.0.1, port = 8000,
    // db_path = admin.db). The admin task lives alongside grpc + audio
    // in the select! below; any error there brings the process down so
    // the operator can see the failure in journalctl. We hand the
    // admin task its own clone of `tls_material` so it can stand up an
    // HTTPS listener with the same identity as the gRPC channel —
    // operators only have one cert fingerprint to pin. The shared
    // `server_config` handle gets cloned in so the admin task can
    // load + mutate it; reads from gRPC + reaper see the updates
    // without restart. The TOML password (if any) takes precedence
    // over the runtime db; capture that as a boolean so the admin UI
    // can lock its server-password input accordingly.
    // Clone the audit sink for the signaling service before the original
    // is moved into the admin task below.
    let audit_tx_sig = audit_tx.clone();
    let toml_password_override = password.is_some();
    let mut admin_cfg = config.admin;
    // Anchor a relative `db_path` under `TOKI_DATA_DIR`. Absolute
    // operator paths (e.g. `/var/lib/toki/admin.db`) are honoured
    // verbatim — same posture as `[tls]` operator paths.
    admin_cfg.db_path = config::resolve_under(&data_dir, &admin_cfg.db_path);
    tracing::info!(
        bind = %admin_cfg.bind,
        port = admin_cfg.port,
        db_path = %admin_cfg.db_path.display(),
        toml_password_override,
        "admin panel starting",
    );
    // When ACME is active the admin task owns the port-80 listener
    // (HTTP-01 challenge + 308 redirect); otherwise it falls back to the
    // optional `http_redirect_port`.
    let acme_http = acme_active.then(|| (config.acme.http_bind.clone(), challenges.clone()));
    let admin_task = tokio::spawn(admin::run(
        admin_cfg,
        registry.clone(),
        admin_tls,
        server_config.clone(),
        channel_names.clone(),
        byte_counters.clone(),
        audit_tx,
        audit_rx,
        data_dir.clone(),
        toml_password_override,
        acme_http,
    ));

    tracing::info!(
        ?grpc_addr,
        password_required = password.is_some(),
        "signaling listening",
    );
    // Cap inbound message size well below Tonic's 4 MB default. Every
    // legitimate request the client sends is small: `RegisterRequest`
    // is two short strings, `JoinRequest` is a UUID + a six-character
    // frequency, `PttEvent` is even smaller. 8 KB is a comfortable
    // ceiling — anything above this is either a bug or a memory-
    // amplification probe (the proto decoder allocates before the
    // handler runs, so a 4 MB request burns 4 MB of server heap even
    // when the password check is going to reject it).
    // We terminate the gRPC TLS handshake ourselves (rather than tonic's
    // static `ServerTlsConfig::identity`) so the cert can hot-swap on
    // ACME renewal via the shared resolver. Accept TCP, hand each socket
    // to the rustls acceptor on its own task (so a slow/stalled handshake
    // can't block other accepts), and feed the resulting TLS streams to
    // tonic through `serve_with_incoming`. tonic preserves the real peer
    // address through `TlsStream`'s `Connected` impl, so the per-IP
    // register throttle + audit log keep seeing the true client IP.
    let grpc_listener = TcpListener::bind(grpc_addr)
        .await
        .with_context(|| format!("bind gRPC TLS listener at {grpc_addr}"))?;
    let acceptor = TlsAcceptor::from(grpc_tls);
    type TlsConn = tokio_rustls::server::TlsStream<tokio::net::TcpStream>;
    type BoxError = Box<dyn std::error::Error + Send + Sync>;
    let (conn_tx, conn_rx) = tokio::sync::mpsc::channel::<Result<TlsConn, BoxError>>(128);
    tokio::spawn(async move {
        loop {
            match grpc_listener.accept().await {
                Ok((tcp, _peer)) => {
                    let _ = tcp.set_nodelay(true);
                    let acceptor = acceptor.clone();
                    let conn_tx = conn_tx.clone();
                    tokio::spawn(async move {
                        let res = acceptor
                            .accept(tcp)
                            .await
                            .map_err(|e| Box::new(e) as BoxError);
                        // Receiver gone => server shutting down; drop quietly.
                        let _ = conn_tx.send(res).await;
                    });
                }
                Err(e) => {
                    // Transient accept error (fd limits, etc.) — log and
                    // keep the loop alive rather than tearing down gRPC.
                    tracing::warn!(error = %e, "gRPC TCP accept failed");
                }
            }
        }
    });

    let grpc = Server::builder()
        .add_service(
            SignalingSvc::new(
                registry,
                advertised_audio,
                password,
                server_config.clone(),
                channel_names,
                audit_tx_sig,
            )
            .max_decoding_message_size(8 * 1024),
        )
        .serve_with_incoming(ReceiverStream::new(conn_rx));

    tokio::select! {
        res = grpc => res?,
        res = audio_task => res??,
        res = admin_task => res??,
    }

    Ok(())
}

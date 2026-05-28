use std::net::SocketAddr;

use tonic::transport::{Identity, Server, ServerTlsConfig};
use tracing_subscriber::EnvFilter;

use toki_server::{
    admin, audio, config::Config, reaper, server_config, signaling::SignalingSvc, state, tls,
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

    // TLS is mandatory. Either the operator supplied cert + key
    // paths in [tls], or we auto-generate a self-signed pair to
    // `tls/{cert,key}.pem` on first run and reuse it thereafter.
    // Either way, gRPC is always HTTPS — there is no plaintext mode.
    let tls_material = tls::TlsMaterial::resolve(config.tls.as_ref())?;
    tracing::info!(
        source = %tls_material.source.display(),
        "TLS ARMED — gRPC channel will serve HTTPS/2"
    );
    let tls_config = ServerTlsConfig::new().identity(Identity::from_pem(
        &tls_material.cert_pem,
        &tls_material.key_pem,
    ));

    let grpc_addr: SocketAddr = std::env::var("TOKI_GRPC_ADDR")
        .unwrap_or_else(|_| "0.0.0.0:50051".into())
        .parse()?;
    let audio_bind: SocketAddr = std::env::var("TOKI_AUDIO_ADDR")
        .unwrap_or_else(|_| "0.0.0.0:50052".into())
        .parse()?;
    let advertised_audio =
        std::env::var("TOKI_AUDIO_PUBLIC").unwrap_or_else(|_| audio_bind.to_string());

    let registry = state::shared();

    // Runtime-mutable server settings. Starts at hardcoded defaults
    // (same values the code shipped before this lived in the db);
    // the admin task, if enabled, will overwrite from sqlite right
    // after it opens. Headless deployments without `[admin]` just
    // keep these defaults for the lifetime of the process.
    let server_config = server_config::shared_default();

    // Reaper runs forever in the background; we don't .await it in the
    // select! below — if it panics, tracing surfaces it but the server
    // keeps serving (just without stale-client cleanup).
    tokio::spawn(reaper::run(registry.clone(), server_config.clone()));

    let audio_task = tokio::spawn(audio::run(audio_bind, registry.clone()));

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
    let toml_password_override = password.is_some();
    let admin_cfg = config.admin;
    tracing::info!(
        bind = %admin_cfg.bind,
        port = admin_cfg.port,
        db_path = %admin_cfg.db_path.display(),
        toml_password_override,
        "admin panel starting",
    );
    let admin_task = tokio::spawn(admin::run(
        admin_cfg,
        registry.clone(),
        tls_material.clone(),
        server_config.clone(),
        toml_password_override,
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
    let grpc = Server::builder()
        .tls_config(tls_config)?
        .add_service(
            SignalingSvc::new(registry, advertised_audio, password, server_config.clone())
                .max_decoding_message_size(8 * 1024),
        )
        .serve(grpc_addr);

    tokio::select! {
        res = grpc => res?,
        res = audio_task => res??,
        res = admin_task => res??,
    }

    Ok(())
}

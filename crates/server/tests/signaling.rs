//! Integration tests for the gRPC signaling service.
//!
//! Spins up `SignalingSvc` in-process and drives it through real
//! Tonic clients over a `tokio::io::duplex` channel — same code path
//! the production server runs, minus TCP/TLS. Each test is `serial`
//! because the throttle's per-IP state is shared across the
//! constructed service and parallel runs would clobber each other.

use std::time::Duration;

use tonic::transport::{Channel, Endpoint, Server, Uri};
use tonic::Code;

use toki_proto::v1::{
    signaling_client::SignalingClient, ChangeFrequencyRequest, JoinRequest, LeaveRequest,
    RegisterRequest,
};
use toki_server::{server_config, signaling::SignalingSvc, state};

/// Stand up the signaling server over an in-memory duplex socket
/// and hand back a connected gRPC client + a join handle for the
/// server task. Closing the channel + dropping the handle is enough
/// cleanup — Tonic drains gracefully when the duplex hits EOF.
async fn boot(password: Option<&str>) -> SignalingClient<Channel> {
    let registry = state::shared();
    // Default ServerConfig is fine for every existing test — max_peers
    // = 256 is well above what any of these cases register. A dedicated
    // capacity test seeds a tighter cap explicitly via `boot_with_cap`.
    let svc = SignalingSvc::new(
        registry,
        "127.0.0.1:50052".to_string(),
        password.map(|s| s.to_string()),
        server_config::shared_default(),
        state::shared_channel_names(Default::default()),
        state::shared_identities(Default::default()),
        tokio::sync::mpsc::unbounded_channel().0,
        toki_server::audit::channel().0,
    );

    let (client_side, server_side) = tokio::io::duplex(64 * 1024);

    // Server side: tonic's server pumps requests off the socket we
    // hand it. The single-element `iter` yields the duplex socket
    // exactly once and then EOFs, which is fine — each test boots
    // its own service so there's no need to accept further conns.
    tokio::spawn(async move {
        let _ = Server::builder()
            .add_service(svc)
            .serve_with_incoming(tokio_stream::iter(vec![Ok::<_, std::io::Error>(
                server_side,
            )]))
            .await;
    });

    // Client side: a custom connector hands the in-memory client
    // half back to tonic on connect. The URI is a placeholder
    // (Tonic still requires *something* valid) — the connector
    // ignores it.
    let mut client_socket = Some(client_side);
    let channel = Endpoint::try_from("http://[::1]:50051")
        .unwrap()
        .connect_timeout(Duration::from_secs(2))
        .connect_with_connector(tower::service_fn(move |_: Uri| {
            let sock = client_socket.take().expect("connector called twice");
            async move { Ok::<_, std::io::Error>(hyper_util::rt::TokioIo::new(sock)) }
        }))
        .await
        .expect("in-process tonic connect");

    SignalingClient::new(channel)
}

/// Most tests don't care about exhaustively asserting every field
/// — they just need a registered client to chain into a Join.
async fn register_or_fail(client: &mut SignalingClient<Channel>, name: &str) -> (String, Vec<u8>) {
    let resp = client
        .register(RegisterRequest {
            display_name: name.into(),
            password: String::new(),
            client_version: env!("CARGO_PKG_VERSION").into(),
            ..Default::default()
        })
        .await
        .expect("register should succeed")
        .into_inner();
    assert!(!resp.client_id.is_empty());
    assert_eq!(resp.audio_token.len(), 16);
    assert_eq!(resp.audio_mac_key.len(), 32);
    (resp.client_id, resp.audio_token)
}

#[tokio::test]
#[serial_test::serial]
async fn register_open_mode_succeeds() {
    let mut client = boot(None).await;
    let (id, token) = register_or_fail(&mut client, "anon").await;
    assert_eq!(token.len(), 16);
    assert!(!id.is_empty());
}

#[tokio::test]
#[serial_test::serial]
async fn register_advertises_opus_by_default() {
    // Default server_config audio_quality = 2 (Standard) → clients are
    // asked to Opus-encode at ~24 kbps.
    let mut client = boot(None).await;
    let resp = client
        .register(RegisterRequest {
            display_name: "anon".into(),
            password: String::new(),
            client_version: env!("CARGO_PKG_VERSION").into(),
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner();
    assert!(resp.opus_enabled, "Standard quality advertises Opus");
    assert_eq!(resp.opus_bitrate, 24_000);
}

#[tokio::test]
#[serial_test::serial]
async fn register_rejects_incompatible_minor_version() {
    let mut client = boot(None).await;
    // A wildly different MAJOR.MINOR than the server → refused up front
    // with FailedPrecondition (not Unauthenticated — it's not an auth
    // failure), so the user sees "please update" instead of dead audio.
    let err = client
        .register(RegisterRequest {
            display_name: "anon".into(),
            password: String::new(),
            client_version: "99.99.0".into(),
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::FailedPrecondition);
}

#[tokio::test]
#[serial_test::serial]
async fn register_rejects_missing_client_version() {
    let mut client = boot(None).await;
    // Pre-gate clients send no version → treated as incompatible.
    let err = client
        .register(RegisterRequest {
            display_name: "anon".into(),
            password: String::new(),
            client_version: String::new(),
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::FailedPrecondition);
}

#[tokio::test]
#[serial_test::serial]
async fn register_accepts_matching_major_minor_with_different_patch() {
    let mut client = boot(None).await;
    // Same MAJOR.MINOR as the server but a different patch → accepted,
    // since patch releases are guaranteed wire-compatible.
    let (major, minor) = toki_proto::version::major_minor(env!("CARGO_PKG_VERSION")).unwrap();
    let resp = client
        .register(RegisterRequest {
            display_name: "anon".into(),
            password: String::new(),
            client_version: format!("{major}.{minor}.999"),
            ..Default::default()
        })
        .await
        .expect("matching major.minor should be accepted");
    assert!(!resp.into_inner().client_id.is_empty());
}

#[tokio::test]
#[serial_test::serial]
async fn register_password_required_rejects_wrong_password() {
    let mut client = boot(Some("hunter2")).await;
    let err = client
        .register(RegisterRequest {
            display_name: "anon".into(),
            password: "wrong".into(),
            client_version: env!("CARGO_PKG_VERSION").into(),
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::Unauthenticated);
}

#[tokio::test]
#[serial_test::serial]
async fn register_password_required_accepts_correct_password() {
    let mut client = boot(Some("hunter2")).await;
    let resp = client
        .register(RegisterRequest {
            display_name: "anon".into(),
            password: "hunter2".into(),
            client_version: env!("CARGO_PKG_VERSION").into(),
            ..Default::default()
        })
        .await
        .expect("good password should succeed");
    assert_eq!(resp.into_inner().audio_token.len(), 16);
}

#[tokio::test]
#[serial_test::serial]
async fn register_rejects_control_chars_in_display_name() {
    let mut client = boot(None).await;
    let err = client
        .register(RegisterRequest {
            display_name: "evil\n[INFO] root logged in".into(),
            password: String::new(),
            client_version: env!("CARGO_PKG_VERSION").into(),
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::InvalidArgument);
    assert!(err.message().contains("control"));
}

#[tokio::test]
#[serial_test::serial]
async fn register_rejects_empty_display_name() {
    let mut client = boot(None).await;
    let err = client
        .register(RegisterRequest {
            display_name: String::new(),
            password: String::new(),
            client_version: env!("CARGO_PKG_VERSION").into(),
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::InvalidArgument);
}

#[tokio::test]
#[serial_test::serial]
async fn join_rejects_out_of_band_frequency() {
    let mut client = boot(None).await;
    let (id, _) = register_or_fail(&mut client, "anon").await;
    let err = client
        .join(JoinRequest {
            client_id: id,
            frequency: "999.99".into(),
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::InvalidArgument);
}

#[tokio::test]
#[serial_test::serial]
async fn join_rejects_not_step_aligned_frequency() {
    let mut client = boot(None).await;
    let (id, _) = register_or_fail(&mut client, "anon").await;
    let err = client
        .join(JoinRequest {
            client_id: id,
            frequency: "446.07".into(), // 0.07 isn't a multiple of 0.05
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::InvalidArgument);
}

#[tokio::test]
#[serial_test::serial]
async fn join_accepts_valid_frequency_and_streams_events() {
    let mut client = boot(None).await;
    let (id, _) = register_or_fail(&mut client, "anon").await;
    // Joining returns a stream; for the smoke test we just need it
    // to not error and to give us a stream we can drop cleanly.
    let stream = client
        .join(JoinRequest {
            client_id: id,
            frequency: "446.05".into(),
        })
        .await
        .expect("valid frequency should succeed")
        .into_inner();
    drop(stream);
}

#[tokio::test]
#[serial_test::serial]
async fn change_frequency_canonicalises_equivalent_strings() {
    // The server canonicalises frequencies to the "446.05" form,
    // so a Join on "446.05" followed by a ChangeFrequency to
    // "446.050" must be detected as a no-op (no leave/rejoin
    // cycle) rather than as a fresh room creation.
    let mut client = boot(None).await;
    let (id, _) = register_or_fail(&mut client, "anon").await;
    let _stream = client
        .join(JoinRequest {
            client_id: id.clone(),
            frequency: "446.05".into(),
        })
        .await
        .unwrap()
        .into_inner();
    // No-op change: should succeed without errors.
    client
        .change_frequency(ChangeFrequencyRequest {
            client_id: id,
            frequency: "446.050".into(),
        })
        .await
        .expect("equivalent frequency should be accepted");
}

/// Boot the service with a custom ServerConfig — used by capacity
/// tests that need to seed a tight `max_peers` value without
/// touching the global default.
async fn boot_with_config(
    cfg: toki_server::server_config::ServerConfig,
) -> SignalingClient<Channel> {
    use std::sync::Arc;
    use tokio::sync::RwLock;
    let registry = state::shared();
    let server_config = Arc::new(RwLock::new(cfg));
    let svc = SignalingSvc::new(
        registry,
        "127.0.0.1:50052".to_string(),
        None,
        server_config,
        state::shared_channel_names(Default::default()),
        state::shared_identities(Default::default()),
        tokio::sync::mpsc::unbounded_channel().0,
        toki_server::audit::channel().0,
    );
    let (client_side, server_side) = tokio::io::duplex(64 * 1024);
    tokio::spawn(async move {
        let _ = tonic::transport::Server::builder()
            .add_service(svc)
            .serve_with_incoming(tokio_stream::iter(vec![Ok::<_, std::io::Error>(
                server_side,
            )]))
            .await;
    });
    let mut client_socket = Some(client_side);
    let channel = tonic::transport::Endpoint::try_from("http://[::1]:50051")
        .unwrap()
        .connect_timeout(Duration::from_secs(2))
        .connect_with_connector(tower::service_fn(move |_: tonic::transport::Uri| {
            let sock = client_socket.take().expect("connector called twice");
            async move { Ok::<_, std::io::Error>(hyper_util::rt::TokioIo::new(sock)) }
        }))
        .await
        .expect("in-process tonic connect");
    SignalingClient::new(channel)
}

/// Variant of `boot_with_config` that also pins the TOML password
/// (`Some` = armed gate). Used by the password-precedence tests so
/// each case can mock its own pair of (TOML, DB) and observe which
/// one wins at the Register handler.
async fn boot_with_passwords(
    toml_password: Option<&str>,
    db_password: &str,
) -> SignalingClient<Channel> {
    use std::sync::Arc;
    use tokio::sync::RwLock;
    let registry = state::shared();
    let cfg = toki_server::server_config::ServerConfig {
        server_name: "test".into(),
        max_peers: 256,
        idle_kick_secs: 10,
        grpc_password: db_password.to_string(),
        named_channels_enabled: false,
        audio_quality: 2,
    };
    let server_config = Arc::new(RwLock::new(cfg));
    let svc = SignalingSvc::new(
        registry,
        "127.0.0.1:50052".to_string(),
        toml_password.map(|s| s.to_string()),
        server_config,
        state::shared_channel_names(Default::default()),
        state::shared_identities(Default::default()),
        tokio::sync::mpsc::unbounded_channel().0,
        toki_server::audit::channel().0,
    );
    let (client_side, server_side) = tokio::io::duplex(64 * 1024);
    tokio::spawn(async move {
        let _ = tonic::transport::Server::builder()
            .add_service(svc)
            .serve_with_incoming(tokio_stream::iter(vec![Ok::<_, std::io::Error>(
                server_side,
            )]))
            .await;
    });
    let mut client_socket = Some(client_side);
    let channel = tonic::transport::Endpoint::try_from("http://[::1]:50051")
        .unwrap()
        .connect_timeout(Duration::from_secs(2))
        .connect_with_connector(tower::service_fn(move |_: tonic::transport::Uri| {
            let sock = client_socket.take().expect("connector called twice");
            async move { Ok::<_, std::io::Error>(hyper_util::rt::TokioIo::new(sock)) }
        }))
        .await
        .expect("in-process tonic connect");
    SignalingClient::new(channel)
}

#[tokio::test]
#[serial_test::serial]
async fn db_password_arms_the_gate_when_no_toml_override() {
    // No TOML; DB has a password → gate is armed with the DB value.
    let mut client = boot_with_passwords(None, "from-db").await;
    let err = client
        .register(RegisterRequest {
            display_name: "anon".into(),
            password: "wrong".into(),
            client_version: env!("CARGO_PKG_VERSION").into(),
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::Unauthenticated);

    let mut client = boot_with_passwords(None, "from-db").await;
    let ok = client
        .register(RegisterRequest {
            display_name: "anon".into(),
            password: "from-db".into(),
            client_version: env!("CARGO_PKG_VERSION").into(),
            ..Default::default()
        })
        .await
        .expect("DB-sourced password should authenticate");
    assert_eq!(ok.into_inner().audio_token.len(), 16);
}

#[tokio::test]
#[serial_test::serial]
async fn toml_password_overrides_db() {
    // Both set with different values; TOML wins. The DB value is
    // present but shadowed — clients must use the TOML password.
    let mut client = boot_with_passwords(Some("from-toml"), "from-db").await;
    let err = client
        .register(RegisterRequest {
            display_name: "anon".into(),
            password: "from-db".into(),
            client_version: env!("CARGO_PKG_VERSION").into(),
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::Unauthenticated);

    let mut client = boot_with_passwords(Some("from-toml"), "from-db").await;
    let ok = client
        .register(RegisterRequest {
            display_name: "anon".into(),
            password: "from-toml".into(),
            client_version: env!("CARGO_PKG_VERSION").into(),
            ..Default::default()
        })
        .await
        .expect("TOML override should win");
    assert_eq!(ok.into_inner().audio_token.len(), 16);
}

#[tokio::test]
#[serial_test::serial]
async fn both_unset_means_open_mode() {
    // Neither TOML nor DB has a password → any value (including
    // empty) authenticates. Confirms we don't accidentally arm
    // the gate with an empty-string sentinel.
    let mut client = boot_with_passwords(None, "").await;
    let ok = client
        .register(RegisterRequest {
            display_name: "anon".into(),
            password: String::new(),
            client_version: env!("CARGO_PKG_VERSION").into(),
            ..Default::default()
        })
        .await
        .expect("open mode should accept any caller");
    assert_eq!(ok.into_inner().audio_token.len(), 16);
}

#[tokio::test]
#[serial_test::serial]
async fn register_rejected_when_at_max_peers() {
    // Seed a server with max_peers=2. The first two registers must
    // succeed; the third must be refused with RESOURCE_EXHAUSTED.
    let mut client = boot_with_config(toki_server::server_config::ServerConfig {
        server_name: "test".into(),
        max_peers: 2,
        idle_kick_secs: 10,
        grpc_password: String::new(),
        named_channels_enabled: false,
        audio_quality: 2,
    })
    .await;
    for i in 0..2 {
        client
            .register(RegisterRequest {
                display_name: format!("peer-{i}"),
                password: String::new(),
                client_version: env!("CARGO_PKG_VERSION").into(),
                ..Default::default()
            })
            .await
            .expect("under-cap register must succeed");
    }
    let err = client
        .register(RegisterRequest {
            display_name: "overflow".into(),
            password: String::new(),
            client_version: env!("CARGO_PKG_VERSION").into(),
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::ResourceExhausted);
}

#[tokio::test]
#[serial_test::serial]
async fn leave_unknown_client_is_noop() {
    let mut client = boot(None).await;
    // Leaving without ever joining must not return an error — the
    // client may have crashed between register and join, and we
    // don't want to surface that as a hard failure.
    client
        .leave(LeaveRequest {
            client_id: "ghost".into(),
        })
        .await
        .expect("leave of unknown client should be a no-op");
}

// ── Client identity handshake ───────────────────────────────────────

/// Sign `nonce` exactly like the real client does (domain-separated
/// payload, ed25519) and build the register request around it.
fn identity_register(signing: &ed25519_dalek::SigningKey, nonce: Vec<u8>) -> RegisterRequest {
    use ed25519_dalek::Signer as _;
    let signature = signing
        .sign(&toki_proto::identity::signing_payload(&nonce))
        .to_vec();
    RegisterRequest {
        display_name: "anon".into(),
        client_version: env!("CARGO_PKG_VERSION").into(),
        identity_pubkey: signing.verifying_key().to_bytes().to_vec(),
        challenge_nonce: nonce,
        identity_signature: signature,
        ..Default::default()
    }
}

#[tokio::test]
#[serial_test::serial]
async fn identity_challenge_then_register_succeeds() {
    let mut client = boot(None).await;
    let signing = ed25519_dalek::SigningKey::from_bytes(&[42u8; 32]);

    let nonce = client
        .identity_challenge(toki_proto::v1::IdentityChallengeRequest {})
        .await
        .expect("challenge should be issued")
        .into_inner()
        .nonce;
    assert!(!nonce.is_empty());

    let resp = client
        .register(identity_register(&signing, nonce))
        .await
        .expect("identity-ful register should succeed")
        .into_inner();
    assert!(!resp.client_id.is_empty());
}

#[tokio::test]
#[serial_test::serial]
async fn identity_register_rejects_wrong_key_signature() {
    let mut client = boot(None).await;
    let signing = ed25519_dalek::SigningKey::from_bytes(&[42u8; 32]);
    let impostor = ed25519_dalek::SigningKey::from_bytes(&[43u8; 32]);

    let nonce = client
        .identity_challenge(toki_proto::v1::IdentityChallengeRequest {})
        .await
        .unwrap()
        .into_inner()
        .nonce;

    // Claim signing's pubkey but sign with the impostor's key.
    let mut req = identity_register(&signing, nonce.clone());
    use ed25519_dalek::Signer as _;
    req.identity_signature = impostor
        .sign(&toki_proto::identity::signing_payload(&nonce))
        .to_vec();
    let err = client.register(req).await.unwrap_err();
    assert_eq!(err.code(), Code::Unauthenticated);
}

#[tokio::test]
#[serial_test::serial]
async fn identity_register_rejects_forged_nonce() {
    let mut client = boot(None).await;
    let signing = ed25519_dalek::SigningKey::from_bytes(&[42u8; 32]);
    // A self-invented nonce was never issued by this server boot.
    let forged = vec![0u8; 56];
    let err = client
        .register(identity_register(&signing, forged))
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::Unauthenticated);
}

#[tokio::test]
#[serial_test::serial]
async fn identity_register_twice_with_same_key_succeeds() {
    // Two registers with the same key (reconnect): the second must be
    // accepted; the identity string is purely key-derived, so it's
    // identical both times. The first_seen/origin merge semantics are
    // covered by the unit tests (identity::merged_identity + db upsert).
    let mut client = boot(None).await;
    let signing = ed25519_dalek::SigningKey::from_bytes(&[44u8; 32]);

    for _ in 0..2 {
        let nonce = client
            .identity_challenge(toki_proto::v1::IdentityChallengeRequest {})
            .await
            .unwrap()
            .into_inner()
            .nonce;
        client
            .register(identity_register(&signing, nonce))
            .await
            .expect("register should succeed");
    }
}

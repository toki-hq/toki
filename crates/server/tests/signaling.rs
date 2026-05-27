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
use toki_server::{signaling::SignalingSvc, state};

/// Stand up the signaling server over an in-memory duplex socket
/// and hand back a connected gRPC client + a join handle for the
/// server task. Closing the channel + dropping the handle is enough
/// cleanup — Tonic drains gracefully when the duplex hits EOF.
async fn boot(password: Option<&str>) -> SignalingClient<Channel> {
    let registry = state::shared();
    let svc = SignalingSvc::new(
        registry,
        "127.0.0.1:50052".to_string(),
        password.map(|s| s.to_string()),
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
async fn register_password_required_rejects_wrong_password() {
    let mut client = boot(Some("hunter2")).await;
    let err = client
        .register(RegisterRequest {
            display_name: "anon".into(),
            password: "wrong".into(),
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

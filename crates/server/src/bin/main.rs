use std::net::SocketAddr;

use tonic::transport::Server;
use tracing_subscriber::EnvFilter;

use toki_server::{audio, reaper, signaling::SignalingSvc, state};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let grpc_addr: SocketAddr = std::env::var("TOKI_GRPC_ADDR")
        .unwrap_or_else(|_| "0.0.0.0:50051".into())
        .parse()?;
    let audio_bind: SocketAddr = std::env::var("TOKI_AUDIO_ADDR")
        .unwrap_or_else(|_| "0.0.0.0:50052".into())
        .parse()?;
    let advertised_audio = std::env::var("TOKI_AUDIO_PUBLIC")
        .unwrap_or_else(|_| audio_bind.to_string());

    let registry = state::shared();

    // Reaper runs forever in the background; we don't .await it in the
    // select! below — if it panics, tracing surfaces it but the server
    // keeps serving (just without stale-client cleanup).
    tokio::spawn(reaper::run(registry.clone()));

    let audio_task = tokio::spawn(audio::run(audio_bind, registry.clone()));

    tracing::info!(?grpc_addr, "signaling listening");
    let grpc = Server::builder()
        .add_service(SignalingSvc::new(registry, advertised_audio))
        .serve(grpc_addr);

    tokio::select! {
        res = grpc => res?,
        res = audio_task => res??,
    }

    Ok(())
}

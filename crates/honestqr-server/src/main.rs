use std::net::IpAddr;

use clap::Parser;
use honestqr_http::{AppConfig, try_router};
use tracing::info;
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(
    name = "honestqr-server",
    version,
    about = "Honest QR Code HTTP server"
)]
struct Args {
    #[arg(long, env = "HONESTQR_HOST", default_value = "0.0.0.0")]
    host: IpAddr,
    #[arg(long, env = "HONESTQR_PORT", default_value_t = 8080)]
    port: u16,
    #[arg(
        long,
        env = "HONESTQR_MAX_BODY_BYTES",
        default_value_t = honestqr_http::DEFAULT_MAX_BODY_BYTES
    )]
    max_body_bytes: usize,
    #[arg(
        long,
        env = "HONESTQR_MAX_BATCH_ITEMS",
        default_value_t = honestqr_http::DEFAULT_MAX_BATCH_ITEMS
    )]
    max_batch_items: usize,
    #[arg(
        long,
        env = "HONESTQR_MAX_CONCURRENCY",
        default_value_t = honestqr_http::DEFAULT_MAX_CONCURRENCY
    )]
    max_concurrency: usize,
    #[arg(
        long,
        env = "HONESTQR_REQUEST_TIMEOUT_SECONDS",
        default_value_t = honestqr_http::DEFAULT_REQUEST_TIMEOUT_SECONDS
    )]
    request_timeout_seconds: u64,
    #[arg(
        long,
        env = "HONESTQR_MAX_ACTIVE_MEMORY_KIB",
        default_value_t = honestqr_http::DEFAULT_MAX_ACTIVE_COST_KIB
    )]
    max_active_cost_kib: u32,
    #[arg(long, env = "HONESTQR_JSON_LOGS", default_value_t = false)]
    json_logs: bool,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    init_tracing(args.json_logs);
    let address = (args.host, args.port);
    let listener = tokio::net::TcpListener::bind(address).await?;
    let local_address = listener.local_addr()?;
    info!(address = %local_address, "Honest QR Code API listening");

    let app = try_router(AppConfig {
        max_body_bytes: args.max_body_bytes,
        max_batch_items: args.max_batch_items,
        max_concurrency: args.max_concurrency,
        request_timeout_seconds: args.request_timeout_seconds,
        max_active_cost_kib: args.max_active_cost_kib,
    })?;
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

fn init_tracing(json: bool) {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("honestqr=info,tower_http=info"));
    if json {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .json()
            .with_current_span(false)
            .init();
    } else {
        tracing_subscriber::fmt().with_env_filter(filter).init();
    }
}

async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(error) = tokio::signal::ctrl_c().await {
            tracing::error!(%error, "failed to install Ctrl-C handler");
        }
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(error) => {
                tracing::error!(%error, "failed to install SIGTERM handler");
                std::future::pending::<()>().await;
            }
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
    info!("shutdown signal received");
}

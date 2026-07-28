use std::time::Duration;

use clap::Parser;
use honestqr_http::DEFAULT_MAX_CONCURRENCY;
use honestqr_stress::{
    EmbeddedServer, MemoryProfile, Scenario, StressConfig, print_human_report, run,
};

#[derive(Debug, Parser)]
#[command(
    name = "honestqr-stress",
    version,
    about = "HTTP stress harness for Honest QR Code API capacity testing"
)]
struct Args {
    /// Target API base URL. When omitted, an embedded server is started locally.
    #[arg(long)]
    base_url: Option<String>,
    /// How long to keep issuing requests.
    #[arg(long, default_value = "30")]
    duration_secs: u64,
    /// Number of concurrent client workers.
    #[arg(long, default_value_t = 16)]
    concurrency: usize,
    /// Request pattern to exercise.
    #[arg(long, value_enum, default_value_t = Scenario::Mixed)]
    scenario: Scenario,
    /// Container memory profile used to size the admission budget.
    #[arg(long, value_enum, default_value_t = MemoryProfile::Mib256)]
    memory_profile: MemoryProfile,
    /// Server-side render concurrency limit.
    #[arg(long, default_value_t = DEFAULT_MAX_CONCURRENCY)]
    max_concurrency: usize,
    /// Emit the report as JSON.
    #[arg(long)]
    json: bool,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let args = Args::parse();
    let embedded = if args.base_url.is_none() {
        Some(EmbeddedServer::start(args.memory_profile, args.max_concurrency).await?)
    } else {
        None
    };
    let base_url = args
        .base_url
        .clone()
        .or_else(|| embedded.as_ref().map(EmbeddedServer::base_url))
        .expect("base URL");

    let report = run(StressConfig {
        base_url,
        duration: Duration::from_secs(args.duration_secs),
        concurrency: args.concurrency,
        scenario: args.scenario,
        max_concurrency: args.max_concurrency,
        memory_profile: args.memory_profile,
    })
    .await?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_human_report(&report);
    }

    drop(embedded);
    Ok(())
}

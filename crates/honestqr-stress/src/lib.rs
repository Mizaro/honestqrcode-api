//! HTTP load generator for realistic Honest QR API capacity testing.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use honestqr_http::{
    AppConfig, DEFAULT_MAX_ACTIVE_COST_KIB, PROFILE_256_MIB_ACTIVE_COST_KIB, try_router,
};
use reqwest::{Client, StatusCode};
use serde::Serialize;

const URL_PNG_BODY: &str = r#"{
  "data": {"kind": "url", "url": "https://honestqrcode.com/checkout?id=stress"},
  "render": {"format": "png", "width": 256, "margin": 4, "error_correction": "high"}
}"#;

const URL_SVG_BODY: &str = r#"{
  "data": {"kind": "url", "url": "https://honestqrcode.com/menu?table=12"},
  "render": {"format": "svg", "width": 256, "margin": 4}
}"#;

const MATRIX_BODY: &str = r#"{
  "data": {"kind": "text", "value": "wifi-setup-token-abc123"},
  "render": {"format": "matrix", "width": 256}
}"#;

const GET_TEXT: &str = "https://honestqrcode.com/promo?ref=stress-test";

#[derive(Debug, Clone, Copy, clap::ValueEnum, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Scenario {
    /// POST /v1/qr URL payload rendered as 256px PNG.
    UrlPng,
    /// POST /v1/qr URL payload rendered as SVG.
    UrlSvg,
    /// GET /v1/qr with a public URL in the query string.
    TextGet,
    /// Weighted mix: 55% PNG, 25% SVG, 15% GET, 5% matrix JSON.
    Mixed,
}

#[derive(Debug, Clone, Copy)]
enum RequestKind {
    UrlPng,
    UrlSvg,
    TextGet,
    Matrix,
}

#[derive(Debug, Clone, Copy, clap::ValueEnum, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum MemoryProfile {
    /// 128 MiB container profile (64 MiB active render budget).
    Mib128,
    /// 256 MiB container profile (192 MiB active render budget).
    Mib256,
}

impl MemoryProfile {
    pub fn active_cost_kib(self) -> u32 {
        match self {
            Self::Mib128 => DEFAULT_MAX_ACTIVE_COST_KIB,
            Self::Mib256 => PROFILE_256_MIB_ACTIVE_COST_KIB,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Mib128 => "128 MiB",
            Self::Mib256 => "256 MiB",
        }
    }
}

#[derive(Debug, Clone)]
pub struct StressConfig {
    pub base_url: String,
    pub duration: Duration,
    pub concurrency: usize,
    pub scenario: Scenario,
    pub max_concurrency: usize,
    pub memory_profile: MemoryProfile,
}

#[derive(Debug, Default)]
struct Counters {
    total: AtomicU64,
    success_2xx: AtomicU64,
    client_errors: AtomicU64,
    status_408: AtomicU64,
    status_413: AtomicU64,
    status_429: AtomicU64,
    status_5xx: AtomicU64,
    transport_errors: AtomicU64,
    response_bytes: AtomicU64,
}

#[derive(Debug, Clone, Serialize)]
pub struct LatencySummary {
    pub samples: u64,
    pub min_ms: f64,
    pub p50_ms: f64,
    pub p90_ms: f64,
    pub p99_ms: f64,
    pub max_ms: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct StressReport {
    pub scenario: Scenario,
    pub memory_profile: MemoryProfile,
    pub target_concurrency: usize,
    pub server_max_concurrency: usize,
    pub active_render_budget_kib: u32,
    pub elapsed_seconds: f64,
    pub total_requests: u64,
    pub successful_requests: u64,
    pub requests_per_second: f64,
    pub success_rate: f64,
    pub response_megabytes: f64,
    pub status_408_timeouts: u64,
    pub status_413_too_large: u64,
    pub status_429_capacity: u64,
    pub status_5xx: u64,
    pub client_errors_4xx: u64,
    pub transport_errors: u64,
    pub latency_ms: LatencySummary,
}

pub struct EmbeddedServer {
    address: SocketAddr,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<()>,
}

impl EmbeddedServer {
    pub async fn start(
        memory_profile: MemoryProfile,
        max_concurrency: usize,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let app = try_router(AppConfig {
            max_concurrency,
            max_active_cost_kib: memory_profile.active_cost_kib(),
            ..AppConfig::default()
        })?;
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = shutdown_rx.await;
                })
                .await;
        });
        Ok(Self {
            address,
            shutdown: Some(shutdown_tx),
            task,
        })
    }

    pub fn base_url(&self) -> String {
        format!("http://{}", self.address)
    }
}

impl Drop for EmbeddedServer {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        self.task.abort();
    }
}

pub async fn run(
    config: StressConfig,
) -> Result<StressReport, Box<dyn std::error::Error + Send + Sync>> {
    let client = Client::builder()
        .pool_max_idle_per_host(config.concurrency.max(1))
        .build()?;
    let counters = Arc::new(Counters::default());
    let latencies = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let deadline = Instant::now() + config.duration;
    let mut workers = Vec::with_capacity(config.concurrency);

    for _ in 0..config.concurrency {
        let client = client.clone();
        let counters = Arc::clone(&counters);
        let latencies = Arc::clone(&latencies);
        let base_url = config.base_url.clone();
        let scenario = config.scenario;
        workers.push(tokio::spawn(async move {
            while Instant::now() < deadline {
                let kind = pick_request_kind(scenario);
                let started = Instant::now();
                let outcome = send_request(&client, &base_url, kind).await;
                let elapsed = started.elapsed();
                latencies.lock().await.push(elapsed);
                record_outcome(&counters, outcome);
            }
        }));
    }

    for worker in workers {
        let _ = worker.await;
    }

    let mut samples = latencies.lock().await.clone();
    samples.sort_unstable();
    let elapsed = config.duration;
    let total = counters.total.load(Ordering::Relaxed);
    let success = counters.success_2xx.load(Ordering::Relaxed);

    Ok(StressReport {
        scenario: config.scenario,
        memory_profile: config.memory_profile,
        target_concurrency: config.concurrency,
        server_max_concurrency: config.max_concurrency,
        active_render_budget_kib: config.memory_profile.active_cost_kib(),
        elapsed_seconds: elapsed.as_secs_f64(),
        total_requests: total,
        successful_requests: success,
        requests_per_second: total as f64 / elapsed.as_secs_f64().max(f64::MIN_POSITIVE),
        success_rate: if total == 0 {
            0.0
        } else {
            success as f64 / total as f64
        },
        response_megabytes: counters.response_bytes.load(Ordering::Relaxed) as f64
            / (1024.0 * 1024.0),
        status_408_timeouts: counters.status_408.load(Ordering::Relaxed),
        status_413_too_large: counters.status_413.load(Ordering::Relaxed),
        status_429_capacity: counters.status_429.load(Ordering::Relaxed),
        status_5xx: counters.status_5xx.load(Ordering::Relaxed),
        client_errors_4xx: counters.client_errors.load(Ordering::Relaxed),
        transport_errors: counters.transport_errors.load(Ordering::Relaxed),
        latency_ms: summarize_latencies(&samples),
    })
}

fn pick_request_kind(scenario: Scenario) -> RequestKind {
    match scenario {
        Scenario::UrlPng => RequestKind::UrlPng,
        Scenario::UrlSvg => RequestKind::UrlSvg,
        Scenario::TextGet => RequestKind::TextGet,
        Scenario::Mixed => {
            let roll = rand::random_range(0..100);
            if roll < 55 {
                RequestKind::UrlPng
            } else if roll < 80 {
                RequestKind::UrlSvg
            } else if roll < 95 {
                RequestKind::TextGet
            } else {
                RequestKind::Matrix
            }
        }
    }
}

enum RequestOutcome {
    Response { status: StatusCode, bytes: usize },
    Transport,
}

async fn send_request(client: &Client, base_url: &str, kind: RequestKind) -> RequestOutcome {
    let response = match kind {
        RequestKind::UrlPng => {
            client
                .post(format!("{base_url}/v1/qr"))
                .header("content-type", "application/json")
                .body(URL_PNG_BODY)
                .send()
                .await
        }
        RequestKind::UrlSvg => {
            client
                .post(format!("{base_url}/v1/qr"))
                .header("content-type", "application/json")
                .body(URL_SVG_BODY)
                .send()
                .await
        }
        RequestKind::Matrix => {
            client
                .post(format!("{base_url}/v1/qr"))
                .header("content-type", "application/json")
                .body(MATRIX_BODY)
                .send()
                .await
        }
        RequestKind::TextGet => {
            let encoded = urlencoding_encode(GET_TEXT);
            client
                .get(format!("{base_url}/v1/qr?data={encoded}"))
                .send()
                .await
        }
    };

    let response = match response {
        Ok(response) => response,
        Err(_) => return RequestOutcome::Transport,
    };

    let status = response.status();
    let bytes = match response.bytes().await {
        Ok(bytes) => bytes.len(),
        Err(_) => return RequestOutcome::Transport,
    };
    RequestOutcome::Response { status, bytes }
}

fn record_outcome(counters: &Counters, outcome: RequestOutcome) {
    counters.total.fetch_add(1, Ordering::Relaxed);
    let RequestOutcome::Response { status, bytes } = outcome else {
        counters.transport_errors.fetch_add(1, Ordering::Relaxed);
        return;
    };
    counters
        .response_bytes
        .fetch_add(bytes as u64, Ordering::Relaxed);
    match status {
        StatusCode::OK => {
            counters.success_2xx.fetch_add(1, Ordering::Relaxed);
        }
        StatusCode::REQUEST_TIMEOUT => {
            counters.status_408.fetch_add(1, Ordering::Relaxed);
            counters.client_errors.fetch_add(1, Ordering::Relaxed);
        }
        StatusCode::PAYLOAD_TOO_LARGE => {
            counters.status_413.fetch_add(1, Ordering::Relaxed);
            counters.client_errors.fetch_add(1, Ordering::Relaxed);
        }
        StatusCode::TOO_MANY_REQUESTS => {
            counters.status_429.fetch_add(1, Ordering::Relaxed);
            counters.client_errors.fetch_add(1, Ordering::Relaxed);
        }
        status if status.is_client_error() => {
            counters.client_errors.fetch_add(1, Ordering::Relaxed);
        }
        status if status.is_server_error() => {
            counters.status_5xx.fetch_add(1, Ordering::Relaxed);
        }
        _ => {}
    }
}

fn summarize_latencies(samples: &[Duration]) -> LatencySummary {
    if samples.is_empty() {
        return LatencySummary {
            samples: 0,
            min_ms: 0.0,
            p50_ms: 0.0,
            p90_ms: 0.0,
            p99_ms: 0.0,
            max_ms: 0.0,
        };
    }
    let to_ms = |duration: Duration| duration.as_secs_f64() * 1000.0;
    LatencySummary {
        samples: samples.len() as u64,
        min_ms: to_ms(samples[0]),
        p50_ms: to_ms(percentile(samples, 0.50)),
        p90_ms: to_ms(percentile(samples, 0.90)),
        p99_ms: to_ms(percentile(samples, 0.99)),
        max_ms: to_ms(*samples.last().expect("non-empty samples")),
    }
}

fn percentile(samples: &[Duration], quantile: f64) -> Duration {
    let index = ((samples.len() - 1) as f64 * quantile).round() as usize;
    samples[index]
}

fn urlencoding_encode(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(byte as char);
            }
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

pub fn print_human_report(report: &StressReport) {
    println!("Honest QR stress report");
    println!("=======================");
    println!("Scenario:              {:?}", report.scenario);
    println!("Memory profile:        {}", report.memory_profile.label());
    println!(
        "Active render budget:  {} KiB",
        report.active_render_budget_kib
    );
    println!("Client concurrency:    {}", report.target_concurrency);
    println!("Server max concurrency:{}", report.server_max_concurrency);
    println!("Duration:              {:.1}s", report.elapsed_seconds);
    println!();
    println!(
        "Throughput:            {:.1} req/s",
        report.requests_per_second
    );
    println!(
        "Successful (2xx):      {} / {} ({:.1}%)",
        report.successful_requests,
        report.total_requests,
        report.success_rate * 100.0
    );
    println!(
        "Response volume:       {:.2} MiB",
        report.response_megabytes
    );
    println!();
    println!("Rejections:");
    println!("  408 timeouts:        {}", report.status_408_timeouts);
    println!("  413 too large:       {}", report.status_413_too_large);
    println!("  429 capacity:        {}", report.status_429_capacity);
    println!("  5xx server errors:   {}", report.status_5xx);
    println!("  other 4xx:           {}", report.client_errors_4xx);
    println!("  transport errors:    {}", report.transport_errors);
    println!();
    println!("Latency (ms):");
    println!("  min / p50 / p90 / p99 / max");
    println!(
        "  {:.2} / {:.2} / {:.2} / {:.2} / {:.2}",
        report.latency_ms.min_ms,
        report.latency_ms.p50_ms,
        report.latency_ms.p90_ms,
        report.latency_ms.p99_ms,
        report.latency_ms.max_ms
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use honestqr_http::DEFAULT_MAX_CONCURRENCY;

    #[tokio::test]
    async fn embedded_server_handles_a_short_mixed_burst() {
        let server = EmbeddedServer::start(MemoryProfile::Mib256, DEFAULT_MAX_CONCURRENCY)
            .await
            .expect("embedded server");
        let report = run(StressConfig {
            base_url: server.base_url(),
            duration: Duration::from_secs(2),
            concurrency: 8,
            scenario: Scenario::Mixed,
            max_concurrency: DEFAULT_MAX_CONCURRENCY,
            memory_profile: MemoryProfile::Mib256,
        })
        .await
        .expect("stress run");
        assert!(report.total_requests > 0);
        assert!(
            report.success_rate > 0.95,
            "success rate {}",
            report.success_rate
        );
        assert!(report.requests_per_second > 10.0);
        drop(server);
    }
}

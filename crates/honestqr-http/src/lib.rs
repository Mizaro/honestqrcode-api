//! Axum adapter for the transport-independent Honest QR renderer.

use bytes::Bytes;
use std::convert::Infallible;
use std::io::{Cursor, Write};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::rejection::{JsonRejection, QueryRejection};
use axum::extract::{DefaultBodyLimit, Query, State};
use axum::http::{HeaderValue, Request, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_core::Stream;
use honestqr_core::{
    ErrorCorrection, MAX_WIDTH, MIN_WIDTH, Margin, QrArtifact, QrData, QrError, QrFormat,
    QrMetadata, QrSpec, RenderOptions, Width, render_validated,
};
use prometheus_client::encoding::text::encode;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::histogram::{Histogram, exponential_buckets};
use prometheus_client::registry::Registry;
use serde::{Deserialize, Serialize};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tower::limit::ConcurrencyLimitLayer;
use tower_http::catch_panic::CatchPanicLayer;
use tower_http::cors::CorsLayer;
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::set_header::SetResponseHeaderLayer;
use tower_http::trace::TraceLayer;
use tracing::{Span, info_span, warn};
use utoipa::{IntoParams, OpenApi, ToSchema};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

pub const DEFAULT_MAX_BODY_BYTES: usize = 64 * 1024;
pub const DEFAULT_MAX_BATCH_ITEMS: usize = 100;
pub const DEFAULT_MAX_CONCURRENCY: usize = 8;
pub const DEFAULT_REQUEST_TIMEOUT_SECONDS: u64 = 15;

/// Active render-memory budget for the documented 128 MiB Kubernetes pod.
pub const PROFILE_128_MIB_ACTIVE_COST_KIB: u32 = 64 * 1024;
/// Active render-memory budget for a 256 MiB container (64 MiB runtime headroom).
pub const PROFILE_256_MIB_ACTIVE_COST_KIB: u32 = 192 * 1024;
pub const DEFAULT_MAX_ACTIVE_COST_KIB: u32 = PROFILE_128_MIB_ACTIVE_COST_KIB;

const MAX_SAFE_BODY_BYTES: usize = DEFAULT_MAX_BODY_BYTES;
const MAX_SAFE_CONCURRENCY: usize = 64;
const RESPONSE_CHUNK_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone)]
pub struct AppConfig {
    pub max_body_bytes: usize,
    pub max_batch_items: usize,
    pub max_concurrency: usize,
    pub request_timeout_seconds: u64,
    pub max_active_cost_kib: u32,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            max_body_bytes: DEFAULT_MAX_BODY_BYTES,
            max_batch_items: DEFAULT_MAX_BATCH_ITEMS,
            max_concurrency: DEFAULT_MAX_CONCURRENCY,
            request_timeout_seconds: DEFAULT_REQUEST_TIMEOUT_SECONDS,
            max_active_cost_kib: DEFAULT_MAX_ACTIVE_COST_KIB,
        }
    }
}

#[derive(Clone)]
struct AppState {
    config: AppConfig,
    metrics: Metrics,
    admission: Admission,
}

#[derive(Clone)]
struct Admission {
    permits: Arc<Semaphore>,
    max_request_cost_kib: u32,
}

impl Admission {
    fn new(max_active_cost_kib: u32) -> Self {
        Self {
            permits: Arc::new(Semaphore::new(max_active_cost_kib as usize)),
            max_request_cost_kib: max_active_cost_kib,
        }
    }

    fn try_acquire(&self, cost: u32) -> Result<OwnedSemaphorePermit, ApiError> {
        if cost > self.max_request_cost_kib {
            return Err(ApiError::with_status(
                StatusCode::PAYLOAD_TOO_LARGE,
                "request_too_expensive",
                "requested render exceeds the maximum work cost",
            )
            .with_detail(format!(
                "estimated cost is {cost} KiB; maximum is {} KiB",
                self.max_request_cost_kib
            )));
        }
        self.permits
            .clone()
            .try_acquire_many_owned(cost)
            .map_err(|_| {
                ApiError::with_status(
                    StatusCode::TOO_MANY_REQUESTS,
                    "capacity_exhausted",
                    "renderer capacity is temporarily exhausted",
                )
            })
    }
}

#[derive(Clone)]
struct Metrics {
    registry: Arc<Mutex<Registry>>,
    requests: Counter,
    successes: Counter,
    failures: Counter,
    render_duration: Histogram,
}

impl Metrics {
    fn new() -> Self {
        let requests = Counter::default();
        let successes = Counter::default();
        let failures = Counter::default();
        let render_duration = Histogram::new(exponential_buckets(0.000_25, 2.0, 16));
        let mut registry = Registry::default();
        registry.register(
            "honestqr_requests",
            "QR render and batch requests received.",
            requests.clone(),
        );
        registry.register(
            "honestqr_render_successes",
            "QR artifacts rendered successfully.",
            successes.clone(),
        );
        registry.register(
            "honestqr_render_failures",
            "QR render attempts rejected or failed.",
            failures.clone(),
        );
        registry.register(
            "honestqr_render_duration_seconds",
            "Time spent rendering QR artifacts.",
            render_duration.clone(),
        );
        Self {
            registry: Arc::new(Mutex::new(registry)),
            requests,
            successes,
            failures,
            render_duration,
        }
    }
}

/// Construct the complete HTTP adapter. The same router is used by integration
/// tests with safe default configuration.
pub fn router(config: AppConfig) -> Router {
    try_router(config)
        .unwrap_or_else(|error| panic!("invalid honestqr HTTP configuration: {error}"))
}

/// Construct the HTTP adapter after validating that its limits are safe for
/// the documented 128 MiB deployment. Use this constructor when configuration
/// errors should be reported instead of treated as startup failures.
pub fn try_router(config: AppConfig) -> Result<Router, AppConfigError> {
    validate_config(&config)?;
    let max_body_bytes = config.max_body_bytes;
    let max_concurrency = config.max_concurrency.max(1);
    let request_timeout = Duration::from_secs(config.request_timeout_seconds.max(1));
    let state = AppState {
        config: config.clone(),
        metrics: Metrics::new(),
        admission: Admission::new(config.max_active_cost_kib),
    };

    let render_routes = Router::new()
        .route("/v1/qr", get(get_qr).post(post_qr))
        .route("/v1/batch", post(post_batch))
        .layer(ConcurrencyLimitLayer::new(max_concurrency))
        .layer(middleware::from_fn_with_state(
            request_timeout,
            request_timeout_middleware,
        ));

    Ok(Router::new()
        .route("/", get(info))
        .route("/healthz", get(health))
        .route("/readyz", get(ready))
        .route("/metrics", get(metrics))
        .route("/docs", get(docs))
        .route("/openapi.json", get(openapi))
        .merge(render_routes)
        .method_not_allowed_fallback(method_not_allowed)
        .fallback(not_found)
        .with_state(state)
        .layer(DefaultBodyLimit::max(max_body_bytes))
        .layer(RequestBodyLimitLayer::new(max_body_bytes))
        .layer(middleware::from_fn(normalize_body_limit_response))
        .layer(SetResponseHeaderLayer::if_not_present(
            header::X_CONTENT_TYPE_OPTIONS,
            HeaderValue::from_static("nosniff"),
        ))
        .layer(SetResponseHeaderLayer::if_not_present(
            header::REFERRER_POLICY,
            HeaderValue::from_static("no-referrer"),
        ))
        .layer(CorsLayer::permissive())
        .layer(CatchPanicLayer::custom(panic_response))
        .layer(TraceLayer::new_for_http().make_span_with(make_request_span)))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppConfigError {
    message: String,
}

impl std::fmt::Display for AppConfigError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for AppConfigError {}

fn validate_config(config: &AppConfig) -> Result<(), AppConfigError> {
    let invalid = if config.max_body_bytes == 0 {
        Some("max_body_bytes must be greater than zero".to_owned())
    } else if config.max_body_bytes > MAX_SAFE_BODY_BYTES {
        Some(format!(
            "max_body_bytes must not exceed {MAX_SAFE_BODY_BYTES} in the 128 MiB profile"
        ))
    } else if config.max_batch_items == 0 {
        Some("max_batch_items must be greater than zero".to_owned())
    } else if config.max_concurrency == 0 {
        Some("max_concurrency must be greater than zero".to_owned())
    } else if config.max_concurrency > MAX_SAFE_CONCURRENCY {
        Some(format!(
            "max_concurrency must not exceed {MAX_SAFE_CONCURRENCY} in the 128 MiB profile"
        ))
    } else if config.request_timeout_seconds == 0 {
        Some("request_timeout_seconds must be greater than zero".to_owned())
    } else if config.max_active_cost_kib == 0 {
        Some("max_active_cost_kib must be greater than zero".to_owned())
    } else if config.max_active_cost_kib > PROFILE_256_MIB_ACTIVE_COST_KIB {
        Some(format!(
            "max_active_cost_kib must not exceed {PROFILE_256_MIB_ACTIVE_COST_KIB}"
        ))
    } else {
        None
    };

    match invalid {
        Some(message) => Err(AppConfigError { message }),
        None => Ok(()),
    }
}

async fn request_timeout_middleware(
    State(timeout): State<Duration>,
    request: Request<Body>,
    next: Next,
) -> Response {
    match tokio::time::timeout(timeout, next.run(request)).await {
        Ok(response) => response,
        Err(_) => ApiError::with_status(
            StatusCode::REQUEST_TIMEOUT,
            "request_timeout",
            "request exceeded the configured time limit",
        )
        .into_response(),
    }
}

async fn normalize_body_limit_response(request: Request<Body>, next: Next) -> Response {
    let response = next.run(request).await;
    let is_json = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("application/json"));
    if response.status() == StatusCode::PAYLOAD_TOO_LARGE && !is_json {
        ApiError::with_status(
            StatusCode::PAYLOAD_TOO_LARGE,
            "body_too_large",
            "request body exceeds the configured size limit",
        )
        .into_response()
    } else {
        response
    }
}

fn make_request_span<B>(request: &Request<B>) -> Span {
    // Deliberately use `uri().path()` rather than the URI/display value: query
    // parameters may contain QR payloads and must never enter tracing fields.
    info_span!(
        "http_request",
        method = %request.method(),
        path = %trace_path(request)
    )
}

fn trace_path<B>(request: &Request<B>) -> &str {
    request.uri().path()
}

fn panic_response(_: Box<dyn std::any::Any + Send + 'static>) -> Response {
    ApiError::with_status(
        StatusCode::INTERNAL_SERVER_ERROR,
        "internal_error",
        "request processing failed",
    )
    .into_response()
}

#[derive(OpenApi)]
#[openapi(
    paths(info, health, ready, get_qr, post_qr, post_batch, metrics),
    components(schemas(
        ApiError,
        ApiInfo,
        BatchRequest,
        ErrorCorrection,
        QrData,
        QrFormat,
        QrMetadata,
        QrSpec,
        RenderOptions,
        SimpleQrQuery
    )),
    tags(
        (name = "render", description = "Stateless QR rendering"),
        (name = "operations", description = "Health and observability")
    )
)]
struct ApiDoc;

#[derive(Debug, Serialize, ToSchema)]
struct ApiInfo {
    name: &'static str,
    version: &'static str,
    documentation: &'static str,
    license: &'static str,
    privacy: &'static str,
}

#[utoipa::path(
    get,
    path = "/",
    tag = "operations",
    responses(
        (status = 200, description = "API information", body = ApiInfo),
        (status = 405, description = "HTTP method is not supported", body = ApiError, content_type = "application/json")
    )
)]
async fn info() -> Json<ApiInfo> {
    Json(ApiInfo {
        name: "Honest QR Code API",
        version: env!("CARGO_PKG_VERSION"),
        documentation: "/docs",
        license: "Apache-2.0",
        privacy: "Request bodies and QR payloads are not logged by the application.",
    })
}

#[utoipa::path(
    get,
    path = "/healthz",
    tag = "operations",
    responses(
        (status = 200, description = "Process is alive"),
        (status = 405, description = "HTTP method is not supported", body = ApiError, content_type = "application/json")
    )
)]
async fn health() -> &'static str {
    "ok\n"
}

#[utoipa::path(
    get,
    path = "/readyz",
    tag = "operations",
    responses(
        (status = 200, description = "Renderer is operational"),
        (status = 405, description = "HTTP method is not supported", body = ApiError, content_type = "application/json")
    )
)]
async fn ready() -> Response {
    let spec = QrSpec {
        data: QrData::Text {
            value: "ready".to_owned(),
        },
        render: RenderOptions {
            format: QrFormat::Matrix,
            width: Width::try_from(128).expect("ready width"),
            ..RenderOptions::default()
        },
    };
    match spec
        .validate()
        .and_then(|validated| render_validated(&validated))
    {
        Ok(_) => (StatusCode::OK, "ready\n").into_response(),
        Err(error) => {
            warn!(error = %error, "readiness render failed");
            (StatusCode::SERVICE_UNAVAILABLE, "not ready\n").into_response()
        }
    }
}

#[derive(Debug, Deserialize, IntoParams, ToSchema)]
#[into_params(parameter_in = Query)]
struct SimpleQrQuery {
    /// Public, non-sensitive text. Prefer POST for private payloads.
    data: String,
    #[serde(default)]
    format: Option<QrFormat>,
    #[serde(default)]
    width: Option<u32>,
    #[serde(default)]
    margin: Option<u8>,
    #[serde(default)]
    error_correction: Option<ErrorCorrection>,
    #[serde(default)]
    foreground: Option<String>,
    #[serde(default)]
    background: Option<String>,
}

impl TryFrom<SimpleQrQuery> for QrSpec {
    type Error = QrError;

    fn try_from(query: SimpleQrQuery) -> Result<Self, Self::Error> {
        let mut options = RenderOptions::default();
        if let Some(format) = query.format {
            options.format = format;
        }
        if let Some(width) = query.width {
            options.width = Width::try_from(width)?;
        }
        if let Some(margin) = query.margin {
            options.margin = Margin::try_from(margin)?;
        }
        if let Some(error_correction) = query.error_correction {
            options.error_correction = error_correction;
        }
        if let Some(foreground) = query.foreground {
            options.foreground = foreground;
        }
        if let Some(background) = query.background {
            options.background = background;
        }
        Ok(Self {
            data: QrData::Text { value: query.data },
            render: options,
        })
    }
}

#[utoipa::path(
    get,
    path = "/v1/qr",
    tag = "render",
    params(SimpleQrQuery),
    responses(
        (status = 200, description = "QR artifact"),
        (status = 400, description = "Invalid query or render specification", body = ApiError, content_type = "application/json"),
        (status = 405, description = "HTTP method is not supported", body = ApiError, content_type = "application/json"),
        (status = 408, description = "Request timed out", body = ApiError, content_type = "application/json"),
        (status = 413, description = "Payload or requested work is too large", body = ApiError, content_type = "application/json"),
        (status = 429, description = "Renderer capacity exhausted", body = ApiError, content_type = "application/json"),
        (status = 500, description = "Internal request processing failure", body = ApiError, content_type = "application/json")
    )
)]
async fn get_qr(
    State(state): State<AppState>,
    query: Result<Query<SimpleQrQuery>, QueryRejection>,
) -> Response {
    let Query(query) = match query {
        Ok(query) => query,
        Err(rejection) => return ApiError::from_query_rejection(rejection).into_response(),
    };
    let spec = match QrSpec::try_from(query) {
        Ok(spec) => spec,
        Err(error) => return ApiError::from_qr(error).into_response(),
    };
    render_response(&state, spec, CachePolicy::Public).await
}

#[utoipa::path(
    post,
    path = "/v1/qr",
    tag = "render",
    request_body = QrSpec,
    responses(
        (status = 200, description = "QR artifact"),
        (status = 400, description = "Malformed JSON or invalid render specification", body = ApiError, content_type = "application/json"),
        (status = 405, description = "HTTP method is not supported", body = ApiError, content_type = "application/json"),
        (status = 408, description = "Request timed out", body = ApiError, content_type = "application/json"),
        (status = 413, description = "Body, payload, or requested work is too large", body = ApiError, content_type = "application/json"),
        (status = 415, description = "Content-Type is not application/json", body = ApiError, content_type = "application/json"),
        (status = 429, description = "Renderer capacity exhausted", body = ApiError, content_type = "application/json"),
        (status = 500, description = "Internal request processing failure", body = ApiError, content_type = "application/json")
    )
)]
async fn post_qr(
    State(state): State<AppState>,
    spec: Result<Json<QrSpec>, JsonRejection>,
) -> Response {
    let Json(spec) = match spec {
        Ok(spec) => spec,
        Err(rejection) => return ApiError::from_json_rejection(rejection).into_response(),
    };
    render_response(&state, spec, CachePolicy::Private).await
}

#[derive(Debug, Deserialize, ToSchema)]
struct BatchRequest {
    items: Vec<QrSpec>,
}

#[derive(Debug, Serialize)]
struct BatchManifest {
    version: &'static str,
    items: Vec<BatchManifestItem>,
}

#[derive(Debug, Serialize)]
struct BatchManifestItem {
    index: usize,
    filename: String,
    metadata: QrMetadata,
}

#[utoipa::path(
    post,
    path = "/v1/batch",
    tag = "render",
    request_body = BatchRequest,
    responses(
        (status = 200, description = "ZIP containing rendered artifacts and manifest", content_type = "application/zip"),
        (status = 400, description = "Malformed JSON or invalid batch", body = ApiError, content_type = "application/json"),
        (status = 405, description = "HTTP method is not supported", body = ApiError, content_type = "application/json"),
        (status = 408, description = "Request timed out", body = ApiError, content_type = "application/json"),
        (status = 413, description = "Body, batch, or requested work is too large", body = ApiError, content_type = "application/json"),
        (status = 415, description = "Content-Type is not application/json", body = ApiError, content_type = "application/json"),
        (status = 429, description = "Renderer capacity exhausted", body = ApiError, content_type = "application/json"),
        (status = 500, description = "Archive or request processing failure", body = ApiError, content_type = "application/json")
    )
)]
async fn post_batch(
    State(state): State<AppState>,
    batch: Result<Json<BatchRequest>, JsonRejection>,
) -> Response {
    let Json(batch) = match batch {
        Ok(batch) => batch,
        Err(rejection) => return ApiError::from_json_rejection(rejection).into_response(),
    };
    state.metrics.requests.inc();
    if batch.items.is_empty() {
        state.metrics.failures.inc();
        return ApiError::new("empty_batch", "items must not be empty").into_response();
    }
    if batch.items.len() > state.config.max_batch_items {
        state.metrics.failures.inc();
        return ApiError::with_status(
            StatusCode::PAYLOAD_TOO_LARGE,
            "batch_too_large",
            format!(
                "batch has {} items; maximum is {}",
                batch.items.len(),
                state.config.max_batch_items
            ),
        )
        .into_response();
    }

    let cost = match batch_cost(&batch.items, state.config.max_active_cost_kib) {
        Ok(cost) => cost,
        Err(error) => {
            state.metrics.failures.inc();
            return error.into_response();
        }
    };
    let permit = match state.admission.try_acquire(cost) {
        Ok(permit) => permit,
        Err(error) => {
            state.metrics.failures.inc();
            return error.into_response();
        }
    };

    let started = Instant::now();
    let item_count = batch.items.len();
    let cancellation = Cancellation::new();
    let worker_cancellation = cancellation.clone();
    let _cancel_on_drop = CancelOnDrop(cancellation);
    let work = tokio::task::spawn_blocking(move || {
        // The permit belongs to the blocking worker, not its JoinHandle. If the
        // request times out and drops the handle, capacity stays reserved until
        // work stops; on success it transfers to the response body.
        create_zip(&batch.items, &worker_cancellation)
            .map(|archive| (archive, permit))
            .map_err(|error| match error {
                BatchArchiveError::Render { index, error } => {
                    ApiError::from_qr(error).with_detail(format!("item {index}"))
                }
                BatchArchiveError::Cancelled => ApiError::with_status(
                    StatusCode::REQUEST_TIMEOUT,
                    "request_timeout",
                    "request exceeded the configured time limit",
                ),
                error => {
                    warn!(error = %error, "failed to build batch archive");
                    ApiError::with_status(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "archive_failed",
                        "failed to build batch archive",
                    )
                }
            })
    })
    .await;

    let (archive, permit) = match work {
        Ok(Ok(result)) => result,
        Ok(Err(error)) => {
            state.metrics.failures.inc();
            return error.into_response();
        }
        Err(error) => {
            state.metrics.failures.inc();
            warn!(error = %error, "batch worker failed");
            return ApiError::with_status(
                StatusCode::INTERNAL_SERVER_ERROR,
                "worker_failed",
                "render worker failed",
            )
            .into_response();
        }
    };

    state.metrics.successes.inc_by(item_count as u64);
    state
        .metrics
        .render_duration
        .observe(started.elapsed().as_secs_f64());

    // Keep weighted admission reserved while the archive is buffered for a
    // slow client. Small chunks prevent Hyper from accepting the entire body
    // and dropping the guard before socket backpressure has done its job.
    let mut response = Body::from_stream(BudgetedBytesStream::new(archive, permit)).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/zip"),
    );
    response.headers_mut().insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_static("attachment; filename=honestqr-batch.zip"),
    );
    response
}

#[derive(Debug)]
enum BatchArchiveError {
    Cancelled,
    Render { index: usize, error: QrError },
    Zip(zip::result::ZipError),
    Io(std::io::Error),
    Manifest,
}

impl std::fmt::Display for BatchArchiveError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cancelled => formatter.write_str("batch was cancelled"),
            Self::Render { index, error } => write!(formatter, "item {index}: {error}"),
            Self::Zip(error) => error.fmt(formatter),
            Self::Io(error) => error.fmt(formatter),
            Self::Manifest => formatter.write_str("manifest serialization failed"),
        }
    }
}

impl From<zip::result::ZipError> for BatchArchiveError {
    fn from(error: zip::result::ZipError) -> Self {
        Self::Zip(error)
    }
}

impl From<std::io::Error> for BatchArchiveError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

fn create_zip(specs: &[QrSpec], cancellation: &Cancellation) -> Result<Vec<u8>, BatchArchiveError> {
    let cursor = Cursor::new(Vec::new());
    let mut archive = ZipWriter::new(cursor);
    let options = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .unix_permissions(0o644);
    let mut manifest_items = Vec::with_capacity(specs.len());

    for (index, spec) in specs.iter().enumerate() {
        if cancellation.is_cancelled() {
            return Err(BatchArchiveError::Cancelled);
        }
        let validated = spec
            .validate()
            .map_err(|error| BatchArchiveError::Render { index, error })?;
        let artifact = render_validated(&validated)
            .map_err(|error| BatchArchiveError::Render { index, error })?;
        let filename = format!("qr-{:03}.{}", index + 1, artifact.metadata.extension());
        archive.start_file(&filename, options)?;
        archive.write_all(&artifact.bytes)?;
        manifest_items.push(BatchManifestItem {
            index,
            filename,
            metadata: artifact.metadata,
        });
    }

    archive.start_file("manifest.json", options)?;
    let manifest = serde_json::to_vec_pretty(&BatchManifest {
        version: env!("CARGO_PKG_VERSION"),
        items: manifest_items,
    })
    .map_err(|_| BatchArchiveError::Manifest)?;
    archive.write_all(&manifest)?;
    let cursor = archive.finish()?;
    Ok(cursor.into_inner())
}

#[derive(Clone)]
struct Cancellation(Arc<AtomicBool>);

impl Cancellation {
    fn new() -> Self {
        Self(Arc::new(AtomicBool::new(false)))
    }

    fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

struct CancelOnDrop(Cancellation);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

struct BudgetedBytesStream {
    bytes: Bytes,
    offset: usize,
    _permit: OwnedSemaphorePermit,
}

impl BudgetedBytesStream {
    fn new(bytes: Vec<u8>, permit: OwnedSemaphorePermit) -> Self {
        Self {
            bytes: Bytes::from(bytes),
            offset: 0,
            _permit: permit,
        }
    }
}

impl Stream for BudgetedBytesStream {
    type Item = Result<Bytes, Infallible>;

    fn poll_next(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.offset == self.bytes.len() {
            return Poll::Ready(None);
        }
        let end = self
            .offset
            .saturating_add(RESPONSE_CHUNK_BYTES)
            .min(self.bytes.len());
        let chunk = self.bytes.slice(self.offset..end);
        self.offset = end;
        Poll::Ready(Some(Ok(chunk)))
    }
}

fn render_cost(spec: &QrSpec, max_active_cost_kib: u32) -> Result<u32, ApiError> {
    // Invalid widths are left for the core validator; clamping here only keeps
    // estimation arithmetic bounded and does not make them renderable.
    let width = u64::from(spec.render.width.get().clamp(MIN_WIDTH, MAX_WIDTH));
    let estimated_bytes = match spec.render.format {
        // PNG rendering holds an RGBA image plus encoder/output buffers.
        QrFormat::Png => width
            .saturating_mul(width)
            .saturating_mul(8)
            .saturating_add(512 * 1024),
        // Vector/matrix complexity is QR-size bounded, but width still affects
        // validation and serialized metadata, so it remains in the estimate.
        QrFormat::Svg => 512 * 1024 + width * 128,
        QrFormat::Matrix => 256 * 1024 + width * 64,
        _ => 256 * 1024 + width * 64,
    };
    let units = estimated_bytes.saturating_add(1023) / 1024;
    if units > u64::from(max_active_cost_kib) {
        return Err(request_cost_error(units, max_active_cost_kib));
    }
    Ok(units as u32)
}

fn batch_cost(specs: &[QrSpec], max_active_cost_kib: u32) -> Result<u32, ApiError> {
    // Batch rendering is sequential. Account for the archive retained from
    // prior items separately from the current item's peak rendering memory,
    // including a second output-sized allowance while ZIP compression writes.
    let mut retained = 64_u64;
    let mut peak = retained;
    for spec in specs {
        let render = u64::from(render_cost(spec, max_active_cost_kib)?);
        let output = retained_artifact_cost(spec);
        peak = peak.max(retained.saturating_add(render).saturating_add(output));
        retained = retained.saturating_add(output);
        peak = peak.max(retained);
        if peak > u64::from(max_active_cost_kib) {
            return Err(request_cost_error(peak, max_active_cost_kib));
        }
    }
    Ok(peak as u32)
}

fn retained_artifact_cost(spec: &QrSpec) -> u64 {
    let width = u64::from(spec.render.width.get().clamp(MIN_WIDTH, MAX_WIDTH));
    let estimated_bytes = match spec.render.format {
        // QR pixels are binary and compress well, but one byte per output
        // pixel plus framing is a conservative retained-PNG allowance.
        QrFormat::Png => width.saturating_mul(width).saturating_add(64 * 1024),
        // Vector and matrix output is bounded mainly by QR module count, not
        // requested display width.
        QrFormat::Svg => 512 * 1024,
        QrFormat::Matrix => 256 * 1024,
        _ => 256 * 1024,
    };
    estimated_bytes.saturating_add(1023) / 1024
}

fn request_cost_error(cost: u64, max_active_cost_kib: u32) -> ApiError {
    ApiError::with_status(
        StatusCode::PAYLOAD_TOO_LARGE,
        "request_too_expensive",
        "requested render exceeds the maximum work cost",
    )
    .with_detail(format!(
        "estimated cost is {cost} KiB; maximum is {max_active_cost_kib} KiB"
    ))
}

enum CachePolicy {
    Public,
    Private,
}

async fn render_response(state: &AppState, spec: QrSpec, cache: CachePolicy) -> Response {
    state.metrics.requests.inc();
    let cost = match render_cost(&spec, state.config.max_active_cost_kib) {
        Ok(cost) => cost,
        Err(error) => {
            state.metrics.failures.inc();
            return error.into_response();
        }
    };
    let validated = match spec.validate() {
        Ok(validated) => validated,
        Err(error) => {
            state.metrics.failures.inc();
            return ApiError::from_qr(error).into_response();
        }
    };
    let permit = match state.admission.try_acquire(cost) {
        Ok(permit) => permit,
        Err(error) => {
            state.metrics.failures.inc();
            return error.into_response();
        }
    };
    let started = Instant::now();
    match tokio::task::spawn_blocking(move || {
        render_validated(&validated).map(|artifact| (artifact, permit))
    })
    .await
    {
        Ok(Ok((artifact, permit))) => {
            state.metrics.successes.inc();
            state
                .metrics
                .render_duration
                .observe(started.elapsed().as_secs_f64());
            artifact_response(artifact, cache, permit)
        }
        Ok(Err(error)) => {
            state.metrics.failures.inc();
            ApiError::from_qr(error).into_response()
        }
        Err(error) => {
            state.metrics.failures.inc();
            warn!(error = %error, "render worker failed");
            ApiError::with_status(
                StatusCode::INTERNAL_SERVER_ERROR,
                "worker_failed",
                "render worker failed",
            )
            .into_response()
        }
    }
}

fn artifact_response(
    artifact: QrArtifact,
    cache: CachePolicy,
    permit: OwnedSemaphorePermit,
) -> Response {
    let mut response =
        Body::from_stream(BudgetedBytesStream::new(artifact.bytes, permit)).into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(artifact.metadata.content_type())
            .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream")),
    );
    headers.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_str(&format!(
            "inline; filename=qr.{}",
            artifact.metadata.extension()
        ))
        .unwrap_or_else(|_| HeaderValue::from_static("inline")),
    );
    headers.insert(
        header::ETAG,
        HeaderValue::from_str(&format!("\"{}\"", artifact.metadata.sha256))
            .unwrap_or_else(|_| HeaderValue::from_static("\"honestqr\"")),
    );
    headers.insert(
        header::CACHE_CONTROL,
        match cache {
            CachePolicy::Public => HeaderValue::from_static("public, max-age=86400"),
            CachePolicy::Private => HeaderValue::from_static("no-store"),
        },
    );
    insert_metadata_header(headers, "x-qr-width", artifact.metadata.width);
    insert_metadata_header(headers, "x-qr-modules", artifact.metadata.modules);
    insert_metadata_header(headers, "x-qr-version", artifact.metadata.version);
    insert_metadata_header(
        headers,
        "x-qr-payload-bytes",
        artifact.metadata.payload_bytes,
    );
    if let Ok(value) = HeaderValue::from_str(&artifact.metadata.sha256) {
        headers.insert("x-qr-sha256", value);
    }
    response
}

fn insert_metadata_header(
    headers: &mut axum::http::HeaderMap,
    name: &'static str,
    value: impl ToString,
) {
    if let (Ok(name), Ok(value)) = (
        axum::http::HeaderName::from_bytes(name.as_bytes()),
        HeaderValue::from_str(&value.to_string()),
    ) {
        headers.insert(name, value);
    }
}

#[derive(Debug, Serialize, ToSchema)]
struct ApiError {
    code: String,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
    #[serde(skip)]
    status: StatusCode,
}

impl ApiError {
    fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::with_status(StatusCode::BAD_REQUEST, code, message)
    }

    fn with_status(
        status: StatusCode,
        code: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            detail: None,
            status,
        }
    }

    fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    fn from_qr(error: QrError) -> Self {
        let status = match error {
            QrError::PayloadTooLarge { .. } => StatusCode::PAYLOAD_TOO_LARGE,
            QrError::RenderFailed { .. } => StatusCode::INTERNAL_SERVER_ERROR,
            _ => StatusCode::BAD_REQUEST,
        };
        Self::with_status(status, error.code(), error.to_string())
    }

    fn from_query_rejection(_rejection: QueryRejection) -> Self {
        Self::with_status(
            StatusCode::BAD_REQUEST,
            "invalid_query",
            "query parameters are missing or invalid",
        )
    }

    fn from_json_rejection(rejection: JsonRejection) -> Self {
        match rejection.status() {
            StatusCode::PAYLOAD_TOO_LARGE => Self::with_status(
                StatusCode::PAYLOAD_TOO_LARGE,
                "body_too_large",
                "request body exceeds the configured size limit",
            ),
            StatusCode::UNSUPPORTED_MEDIA_TYPE => Self::with_status(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "unsupported_media_type",
                "Content-Type must be application/json",
            ),
            _ => Self::with_status(
                StatusCode::BAD_REQUEST,
                "invalid_json",
                "request body is not valid JSON for this endpoint",
            ),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(self)).into_response()
    }
}

#[utoipa::path(
    get,
    path = "/metrics",
    tag = "operations",
    responses(
        (status = 200, description = "OpenMetrics exposition"),
        (status = 405, description = "HTTP method is not supported", body = ApiError, content_type = "application/json")
    )
)]
async fn metrics(State(state): State<AppState>) -> Response {
    let mut output = String::new();
    let encoded = state
        .metrics
        .registry
        .lock()
        .map_err(|_| ())
        .and_then(|registry| encode(&mut output, &registry).map_err(|_| ()));
    match encoded {
        Ok(()) => (
            StatusCode::OK,
            [(
                header::CONTENT_TYPE,
                "application/openmetrics-text; version=1.0.0",
            )],
            output,
        )
            .into_response(),
        Err(()) => ApiError::with_status(
            StatusCode::INTERNAL_SERVER_ERROR,
            "metrics_failed",
            "failed to encode metrics",
        )
        .into_response(),
    }
}

async fn docs() -> Html<&'static str> {
    Html(include_str!("../../../docs/index.html"))
}

async fn openapi() -> Json<utoipa::openapi::OpenApi> {
    Json(ApiDoc::openapi())
}

async fn method_not_allowed() -> Response {
    ApiError::with_status(
        StatusCode::METHOD_NOT_ALLOWED,
        "method_not_allowed",
        "HTTP method is not supported for this route",
    )
    .into_response()
}

async fn not_found() -> Response {
    ApiError::with_status(StatusCode::NOT_FOUND, "not_found", "route not found").into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use http_body_util::BodyExt;
    use std::sync::mpsc;
    use tower::ServiceExt;

    async fn assert_api_error(response: Response, status: StatusCode, code: &str) {
        assert_eq!(response.status(), status);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "application/json");
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("error body")
            .to_bytes();
        let error: serde_json::Value = serde_json::from_slice(&bytes).expect("ApiError JSON");
        assert_eq!(error["code"], code);
        assert!(
            error["message"]
                .as_str()
                .is_some_and(|value| !value.is_empty())
        );
        assert!(error.get("status").is_none());
    }

    fn valid_spec(format: QrFormat, width: u32) -> QrSpec {
        QrSpec {
            data: QrData::Text {
                value: "test".to_owned(),
            },
            render: RenderOptions {
                format,
                width: Width::try_from(width).expect("valid width"),
                ..RenderOptions::default()
            },
        }
    }

    #[tokio::test]
    async fn health_and_ready_are_available() {
        for path in ["/healthz", "/readyz"] {
            let response = router(AppConfig::default())
                .oneshot(
                    axum::http::Request::builder()
                        .uri(path)
                        .body(Body::empty())
                        .expect("request"),
                )
                .await
                .expect("response");
            assert_eq!(response.status(), StatusCode::OK);
        }
    }

    #[tokio::test]
    async fn post_returns_a_private_png() {
        let body = serde_json::json!({
            "data": {"kind": "text", "value": "hello"},
            "render": {"format": "png", "width": 256}
        });
        let response = router(AppConfig::default())
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/v1/qr")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(body.to_string()))
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        assert_eq!(response.headers()["x-qr-payload-bytes"], "5");
        assert_eq!(response.headers()["x-qr-sha256"].as_bytes().len(), 64);
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");
    }

    #[tokio::test]
    async fn invalid_spec_returns_structured_error() {
        let body = serde_json::json!({
            "data": {"kind": "text", "value": ""},
            "render": {"format": "svg"}
        });
        let response = router(AppConfig::default())
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/v1/qr")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(body.to_string()))
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        let error: serde_json::Value = serde_json::from_slice(&bytes).expect("error JSON");
        assert_eq!(error["code"], "empty_payload");
    }

    #[tokio::test]
    async fn missing_query_data_returns_api_error_contract() {
        let response = router(AppConfig::default())
            .oneshot(
                Request::builder()
                    .uri("/v1/qr?width=256")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_api_error(response, StatusCode::BAD_REQUEST, "invalid_query").await;
    }

    #[tokio::test]
    async fn unsupported_method_returns_api_error_contract() {
        let response = router(AppConfig::default())
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/v1/qr")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_api_error(
            response,
            StatusCode::METHOD_NOT_ALLOWED,
            "method_not_allowed",
        )
        .await;
    }

    #[test]
    fn openapi_documents_method_not_allowed_contract() {
        let document = serde_json::to_value(ApiDoc::openapi()).expect("OpenAPI JSON");
        for (path, method) in [
            ("/", "get"),
            ("/healthz", "get"),
            ("/readyz", "get"),
            ("/metrics", "get"),
            ("/v1/qr", "get"),
            ("/v1/qr", "post"),
            ("/v1/batch", "post"),
        ] {
            assert!(
                document["paths"][path][method]["responses"]["405"]["content"]["application/json"]
                    .is_object(),
                "missing JSON 405 response for {method} {path}"
            );
        }
    }

    #[tokio::test]
    async fn malformed_json_returns_api_error_contract() {
        let response = router(AppConfig::default())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/qr")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from("{"))
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_api_error(response, StatusCode::BAD_REQUEST, "invalid_json").await;
    }

    #[tokio::test]
    async fn missing_and_wrong_content_type_return_api_error_contract() {
        let body = serde_json::to_string(&valid_spec(QrFormat::Svg, 256)).expect("spec JSON");
        for content_type in [None, Some("text/plain")] {
            let mut builder = Request::builder().method("POST").uri("/v1/qr");
            if let Some(content_type) = content_type {
                builder = builder.header(header::CONTENT_TYPE, content_type);
            }
            let response = router(AppConfig::default())
                .oneshot(builder.body(Body::from(body.clone())).expect("request"))
                .await
                .expect("response");
            assert_api_error(
                response,
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "unsupported_media_type",
            )
            .await;
        }
    }

    #[tokio::test]
    async fn oversized_body_returns_api_error_contract() {
        let config = AppConfig {
            max_body_bytes: 128,
            ..AppConfig::default()
        };
        let body = serde_json::json!({
            "data": {"kind": "text", "value": "x".repeat(256)},
            "render": {"format": "svg"}
        });
        let response = router(config)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/qr")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(body.to_string()))
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_api_error(response, StatusCode::PAYLOAD_TOO_LARGE, "body_too_large").await;
    }

    #[tokio::test]
    async fn maximum_cost_rejects_unsafe_render_and_batch() {
        let body = serde_json::to_string(&valid_spec(QrFormat::Png, MAX_WIDTH)).expect("spec JSON");
        let response = router(AppConfig::default())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/qr")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(body))
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_api_error(
            response,
            StatusCode::PAYLOAD_TOO_LARGE,
            "request_too_expensive",
        )
        .await;

        let items = (0..DEFAULT_MAX_BATCH_ITEMS)
            .map(|_| valid_spec(QrFormat::Png, 1024))
            .collect::<Vec<_>>();
        let response = router(AppConfig::default())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/batch")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        serde_json::to_string(&serde_json::json!({ "items": items }))
                            .expect("batch JSON"),
                    ))
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_api_error(
            response,
            StatusCode::PAYLOAD_TOO_LARGE,
            "request_too_expensive",
        )
        .await;
    }

    #[test]
    fn cost_estimate_is_width_and_format_aware() {
        let small_png = render_cost(&valid_spec(QrFormat::Png, 256), DEFAULT_MAX_ACTIVE_COST_KIB)
            .expect("small PNG cost");
        let large_png = render_cost(
            &valid_spec(QrFormat::Png, 1024),
            DEFAULT_MAX_ACTIVE_COST_KIB,
        )
        .expect("large PNG cost");
        let svg = render_cost(
            &valid_spec(QrFormat::Svg, 1024),
            DEFAULT_MAX_ACTIVE_COST_KIB,
        )
        .expect("SVG cost");
        let matrix = render_cost(
            &valid_spec(QrFormat::Matrix, 1024),
            DEFAULT_MAX_ACTIVE_COST_KIB,
        )
        .expect("matrix cost");
        assert!(large_png > small_png);
        assert!(large_png > svg);
        assert!(svg > matrix);
    }

    #[test]
    fn documented_default_batch_fits_the_memory_budget() {
        let items = (0..DEFAULT_MAX_BATCH_ITEMS)
            .map(|_| valid_spec(QrFormat::Png, RenderOptions::default().width.get()))
            .collect::<Vec<_>>();
        let cost = batch_cost(&items, DEFAULT_MAX_ACTIVE_COST_KIB).expect("default batch cost");

        assert!(cost <= DEFAULT_MAX_ACTIVE_COST_KIB);
        assert!(
            cost > render_cost(&items[0], DEFAULT_MAX_ACTIVE_COST_KIB).expect("single render cost")
        );
    }

    #[tokio::test]
    async fn admission_rejects_concurrent_cost() {
        let admission = Admission::new(DEFAULT_MAX_ACTIVE_COST_KIB);
        let _all_capacity = admission
            .try_acquire(DEFAULT_MAX_ACTIVE_COST_KIB)
            .expect("initial capacity");
        let error = admission.try_acquire(1).expect_err("capacity rejection");
        assert_eq!(error.status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(error.code, "capacity_exhausted");
    }

    #[tokio::test]
    async fn response_body_holds_admission_until_consumed_or_dropped() {
        let admission = Admission::new(DEFAULT_MAX_ACTIVE_COST_KIB);
        let cost = 1024;
        let permit = admission.try_acquire(cost).expect("response capacity");
        let body = Body::from_stream(BudgetedBytesStream::new(
            vec![0_u8; RESPONSE_CHUNK_BYTES * 2],
            permit,
        ));
        assert_eq!(
            admission.permits.available_permits(),
            DEFAULT_MAX_ACTIVE_COST_KIB as usize - cost as usize
        );

        let bytes = body.collect().await.expect("consume response").to_bytes();
        assert_eq!(bytes.len(), RESPONSE_CHUNK_BYTES * 2);
        assert_eq!(
            admission.permits.available_permits(),
            DEFAULT_MAX_ACTIVE_COST_KIB as usize
        );

        let permit = admission.try_acquire(cost).expect("response capacity");
        let body = Body::from_stream(BudgetedBytesStream::new(vec![0_u8; 1], permit));
        drop(body);
        assert_eq!(
            admission.permits.available_permits(),
            DEFAULT_MAX_ACTIVE_COST_KIB as usize
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dropped_blocking_handle_does_not_release_capacity_early() {
        let admission = Admission::new(DEFAULT_MAX_ACTIVE_COST_KIB);
        let permit = admission
            .try_acquire(DEFAULT_MAX_ACTIVE_COST_KIB)
            .expect("all capacity");
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let worker = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            started_tx.send(()).expect("started signal");
            release_rx.recv().expect("release signal");
        });
        started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("worker started");

        drop(worker); // Models the timeout middleware dropping the handler future.
        assert!(admission.try_acquire(1).is_err());

        release_tx.send(()).expect("release worker");
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if admission.permits.available_permits() == DEFAULT_MAX_ACTIVE_COST_KIB as usize {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("worker released permit");
    }

    #[tokio::test]
    async fn timeout_returns_api_error_and_batch_cancellation_is_cooperative() {
        async fn slow() -> &'static str {
            tokio::time::sleep(Duration::from_secs(1)).await;
            "late"
        }
        let app = Router::new()
            .route("/slow", get(slow))
            .layer(middleware::from_fn_with_state(
                Duration::from_millis(5),
                request_timeout_middleware,
            ));
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/slow")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_api_error(response, StatusCode::REQUEST_TIMEOUT, "request_timeout").await;

        let cancellation = Cancellation::new();
        cancellation.cancel();
        let error = create_zip(&[valid_spec(QrFormat::Svg, 256)], &cancellation)
            .expect_err("cancelled batch");
        assert!(matches!(error, BatchArchiveError::Cancelled));
    }

    #[tokio::test]
    async fn panic_returns_api_error_contract() {
        async fn panic_handler() -> &'static str {
            panic!("test panic must not escape the JSON contract");
        }

        let response = Router::new()
            .route("/panic", get(panic_handler))
            .layer(CatchPanicLayer::custom(panic_response))
            .oneshot(
                Request::builder()
                    .uri("/panic")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_api_error(
            response,
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
        )
        .await;
    }

    #[test]
    fn trace_path_never_contains_sensitive_query_data() {
        let request = Request::builder()
            .uri("/v1/qr?data=top-secret&foreground=%23000000")
            .body(())
            .expect("request");
        assert_eq!(trace_path(&request), "/v1/qr");
        assert!(!trace_path(&request).contains("top-secret"));
    }

    #[test]
    fn unsafe_runtime_configuration_is_rejected() {
        let error = try_router(AppConfig {
            max_concurrency: MAX_SAFE_CONCURRENCY + 1,
            ..AppConfig::default()
        })
        .expect_err("unsafe concurrency");
        assert!(error.to_string().contains("128 MiB profile"));

        let error = try_router(AppConfig {
            max_body_bytes: MAX_SAFE_BODY_BYTES + 1,
            ..AppConfig::default()
        })
        .expect_err("unsafe body limit");
        assert!(error.to_string().contains("128 MiB profile"));
    }

    #[tokio::test]
    async fn batch_returns_zip_with_manifest() {
        let body = serde_json::json!({
            "items": [
                {"data": {"kind": "text", "value": "one"}, "render": {"format": "svg"}},
                {"data": {"kind": "text", "value": "two"}, "render": {"format": "png"}}
            ]
        });
        let response = router(AppConfig::default())
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/v1/batch")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(body.to_string()))
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "application/zip");
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        assert!(bytes.starts_with(b"PK"));
    }
}

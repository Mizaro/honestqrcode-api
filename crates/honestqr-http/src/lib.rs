//! Axum adapter for the transport-independent Honest QR renderer.

use std::io::{Cursor, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Query, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use honestqr_core::{
    ErrorCorrection, QrArtifact, QrData, QrError, QrFormat, QrMetadata, QrSpec, RenderOptions,
    render,
};
use prometheus_client::encoding::text::encode;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::histogram::{Histogram, exponential_buckets};
use prometheus_client::registry::Registry;
use serde::{Deserialize, Serialize};
use tower::limit::ConcurrencyLimitLayer;
use tower_http::catch_panic::CatchPanicLayer;
use tower_http::cors::CorsLayer;
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::set_header::SetResponseHeaderLayer;
use tower_http::timeout::TimeoutLayer;
use tower_http::trace::TraceLayer;
use tracing::warn;
use utoipa::{IntoParams, OpenApi, ToSchema};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

pub const DEFAULT_MAX_BODY_BYTES: usize = 64 * 1024;
pub const DEFAULT_MAX_BATCH_ITEMS: usize = 100;
pub const DEFAULT_MAX_CONCURRENCY: usize = 64;
pub const DEFAULT_REQUEST_TIMEOUT_SECONDS: u64 = 15;

#[derive(Debug, Clone)]
pub struct AppConfig {
    pub max_body_bytes: usize,
    pub max_batch_items: usize,
    pub max_concurrency: usize,
    pub request_timeout_seconds: u64,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            max_body_bytes: DEFAULT_MAX_BODY_BYTES,
            max_batch_items: DEFAULT_MAX_BATCH_ITEMS,
            max_concurrency: DEFAULT_MAX_CONCURRENCY,
            request_timeout_seconds: DEFAULT_REQUEST_TIMEOUT_SECONDS,
        }
    }
}

#[derive(Clone)]
struct AppState {
    config: AppConfig,
    metrics: Metrics,
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

/// Construct the complete HTTP adapter. The same router is used by the native
/// server, integration tests, and Lambda adapter.
pub fn router(config: AppConfig) -> Router {
    let max_body_bytes = config.max_body_bytes;
    let max_concurrency = config.max_concurrency.max(1);
    let request_timeout = Duration::from_secs(config.request_timeout_seconds.max(1));
    let state = AppState {
        config,
        metrics: Metrics::new(),
    };

    let render_routes = Router::new()
        .route("/v1/qr", get(get_qr).post(post_qr))
        .route("/v1/batch", post(post_batch))
        .layer(ConcurrencyLimitLayer::new(max_concurrency))
        .layer(TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            request_timeout,
        ));

    Router::new()
        .route("/", get(info))
        .route("/healthz", get(health))
        .route("/readyz", get(ready))
        .route("/metrics", get(metrics))
        .route("/docs", get(docs))
        .route("/openapi.json", get(openapi))
        .merge(render_routes)
        .fallback(not_found)
        .with_state(state)
        .layer(DefaultBodyLimit::disable())
        .layer(RequestBodyLimitLayer::new(max_body_bytes))
        .layer(SetResponseHeaderLayer::if_not_present(
            header::X_CONTENT_TYPE_OPTIONS,
            HeaderValue::from_static("nosniff"),
        ))
        .layer(SetResponseHeaderLayer::if_not_present(
            header::REFERRER_POLICY,
            HeaderValue::from_static("no-referrer"),
        ))
        .layer(CorsLayer::permissive())
        .layer(CatchPanicLayer::new())
        .layer(TraceLayer::new_for_http())
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
    responses((status = 200, description = "API information", body = ApiInfo))
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
    responses((status = 200, description = "Process is alive"))
)]
async fn health() -> &'static str {
    "ok\n"
}

#[utoipa::path(
    get,
    path = "/readyz",
    tag = "operations",
    responses((status = 200, description = "Renderer is operational"))
)]
async fn ready() -> Response {
    let spec = QrSpec {
        data: QrData::Text {
            value: "ready".to_owned(),
        },
        render: RenderOptions {
            format: QrFormat::Matrix,
            width: 128,
            ..RenderOptions::default()
        },
    };
    match render(&spec) {
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

impl From<SimpleQrQuery> for QrSpec {
    fn from(query: SimpleQrQuery) -> Self {
        let mut options = RenderOptions::default();
        if let Some(format) = query.format {
            options.format = format;
        }
        if let Some(width) = query.width {
            options.width = width;
        }
        if let Some(margin) = query.margin {
            options.margin = margin;
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
        Self {
            data: QrData::Text { value: query.data },
            render: options,
        }
    }
}

#[utoipa::path(
    get,
    path = "/v1/qr",
    tag = "render",
    params(SimpleQrQuery),
    responses(
        (status = 200, description = "QR artifact"),
        (status = 400, description = "Invalid render specification", body = ApiError),
        (status = 413, description = "Payload too large", body = ApiError)
    )
)]
async fn get_qr(State(state): State<AppState>, Query(query): Query<SimpleQrQuery>) -> Response {
    render_response(&state.metrics, QrSpec::from(query), CachePolicy::Public).await
}

#[utoipa::path(
    post,
    path = "/v1/qr",
    tag = "render",
    request_body = QrSpec,
    responses(
        (status = 200, description = "QR artifact"),
        (status = 400, description = "Invalid render specification", body = ApiError),
        (status = 413, description = "Payload too large", body = ApiError)
    )
)]
async fn post_qr(State(state): State<AppState>, Json(spec): Json<QrSpec>) -> Response {
    render_response(&state.metrics, spec, CachePolicy::Private).await
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
        (status = 400, description = "Invalid batch", body = ApiError),
        (status = 413, description = "Batch too large", body = ApiError)
    )
)]
async fn post_batch(State(state): State<AppState>, Json(batch): Json<BatchRequest>) -> Response {
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

    let started = Instant::now();
    let item_count = batch.items.len();
    let work = tokio::task::spawn_blocking(move || {
        let mut rendered = Vec::with_capacity(batch.items.len());
        for (index, spec) in batch.items.iter().enumerate() {
            match render(spec) {
                Ok(artifact) => rendered.push((index, artifact)),
                Err(error) => {
                    return Err(ApiError::from_qr(error).with_detail(format!("item {index}")));
                }
            }
        }
        create_zip(rendered).map_err(|error| {
            warn!(error = %error, "failed to build batch archive");
            ApiError::with_status(
                StatusCode::INTERNAL_SERVER_ERROR,
                "archive_failed",
                "failed to build batch archive",
            )
        })
    })
    .await;

    let archive = match work {
        Ok(Ok(archive)) => archive,
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

    let mut response = archive.into_response();
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

fn create_zip(rendered: Vec<(usize, QrArtifact)>) -> Result<Vec<u8>, zip::result::ZipError> {
    let cursor = Cursor::new(Vec::new());
    let mut archive = ZipWriter::new(cursor);
    let options = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .unix_permissions(0o644);
    let mut manifest_items = Vec::with_capacity(rendered.len());

    for (index, artifact) in rendered {
        let filename = format!("qr-{:03}.{}", index + 1, artifact.metadata.extension);
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
    .map_err(|_| zip::result::ZipError::InvalidArchive("manifest serialization failed".into()))?;
    archive.write_all(&manifest)?;
    let cursor = archive.finish()?;
    Ok(cursor.into_inner())
}

enum CachePolicy {
    Public,
    Private,
}

async fn render_response(metrics: &Metrics, spec: QrSpec, cache: CachePolicy) -> Response {
    metrics.requests.inc();
    let started = Instant::now();
    match tokio::task::spawn_blocking(move || render(&spec)).await {
        Ok(Ok(artifact)) => {
            metrics.successes.inc();
            metrics
                .render_duration
                .observe(started.elapsed().as_secs_f64());
            artifact_response(artifact, cache)
        }
        Ok(Err(error)) => {
            metrics.failures.inc();
            ApiError::from_qr(error).into_response()
        }
        Err(error) => {
            metrics.failures.inc();
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

fn artifact_response(artifact: QrArtifact, cache: CachePolicy) -> Response {
    let mut response = Body::from(artifact.bytes).into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(&artifact.metadata.content_type)
            .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream")),
    );
    headers.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_str(&format!(
            "inline; filename=qr.{}",
            artifact.metadata.extension
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
    responses((status = 200, description = "OpenMetrics exposition"))
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

async fn not_found() -> Response {
    ApiError::with_status(StatusCode::NOT_FOUND, "not_found", "route not found").into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

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

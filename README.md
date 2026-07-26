# Honest QR Code API

**A fast, deterministic QR rendering API that is genuinely easy to self-host.**

[![CI](https://github.com/honestqrcode/honestqrcode-api/actions/workflows/ci.yml/badge.svg)](https://github.com/honestqrcode/honestqrcode-api/actions/workflows/ci.yml)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.88%2B-orange.svg)](rust-toolchain.toml)

Honest QR Code API turns text and structured data into PNG, SVG, or a JSON module matrix. It stores nothing, makes no outbound requests, and its application tracing records only HTTP methods and paths—never request bodies or query strings. Run the same rendering engine as an HTTP server, a tiny container, an AWS Lambda function, a CLI, or a Rust library.

The browser-only generator at [honestqrcode.com](https://honestqrcode.com/) remains free, private, and independent of this API.

## Why this one?

- **Portable:** one Rust codebase for servers, containers, Lambda, command line, and library use.
- **Private by construction:** no database, cookies, trackers, remote fetches, or request-body logging.
- **Predictable:** bounded inputs, deterministic output, stable errors, ETags, and exact pixel sizing.
- **Production-ready:** health/readiness probes, OpenMetrics, structured logs, graceful shutdown, body limits, batches, OpenAPI, and security headers.
- **Useful payloads:** text, URL, raw bytes, WiFi, email, phone, SMS, WhatsApp, vCard, location, and calendar events.
- **Open:** Apache-2.0 licensed. Fork it, ship it, and sell services around it.

## Run it in 30 seconds

### Container

```bash
docker run --rm -p 8080:8080 ghcr.io/honestqrcode/honestqrcode-api:0.1.1
curl "http://localhost:8080/v1/qr?data=Hello%20world" --output hello.png
```

Or build locally:

```bash
docker compose up --build
```

### Rust

```bash
cargo run --release -p honestqr-server
cargo run --release -p honestqr-cli -- "https://honestqrcode.com" -o honest.png
```

The server listens on `0.0.0.0:8080`. Open <http://localhost:8080/docs> for the built-in docs or <http://localhost:8080/openapi.json> for OpenAPI 3.1.

Container releases use protected SemVer tags and the release workflow refuses
to overwrite an existing image tag. There is intentionally no mutable
`latest` tag.

## API

Use `POST /v1/qr` for private or structured data:

```bash
curl -X POST http://localhost:8080/v1/qr \
  -H "content-type: application/json" \
  --output wifi.svg \
  -d '{
    "data": {
      "kind": "wifi",
      "ssid": "My Network",
      "password": "correct horse battery staple",
      "security": "wpa",
      "hidden": false
    },
    "render": {
      "format": "svg",
      "width": 512,
      "margin": 4,
      "error_correction": "medium",
      "foreground": "#000000",
      "background": "#ffffff"
    }
  }'
```

For public, non-sensitive content, `GET /v1/qr` provides an image-friendly URL:

```html
<img src="https://api.example.com/v1/qr?data=https%3A%2F%2Fexample.com&format=svg"
     width="256" height="256" alt="QR code">
```

Query strings may be recorded by browsers and reverse proxies. Use POST for passwords, contact details, private links, and other sensitive values.

### Endpoints

| Endpoint | Purpose |
|---|---|
| `GET /v1/qr` | Simple QR response; cacheable for public content |
| `POST /v1/qr` | Structured QR response; always `no-store` |
| `POST /v1/batch` | ZIP containing up to 100 QR files and a manifest |
| `GET /openapi.json` | Machine-readable OpenAPI 3.1 document |
| `GET /docs` | Built-in API documentation |
| `GET /healthz` | Process liveness |
| `GET /readyz` | Real renderer readiness check |
| `GET /metrics` | OpenMetrics counters and latency histogram |

All successful renders include `ETag`, `X-QR-SHA256`, `X-QR-Modules`, and `X-QR-Payload-Bytes` headers. Errors are stable JSON objects with a machine-readable `code`, a human-readable `message`, and an optional `field`.

The authoritative schema and every payload variant are in the generated [OpenAPI document](http://localhost:8080/openapi.json).

## Configuration

| Environment variable | Default | Meaning |
|---|---:|---|
| `HONESTQR_HOST` | `0.0.0.0` | Listen address |
| `HONESTQR_PORT` | `8080` | Listen port |
| `HONESTQR_MAX_BODY_BYTES` | `65536` | Maximum JSON request size |
| `HONESTQR_MAX_BATCH_ITEMS` | `100` | Maximum batch size |
| `HONESTQR_MAX_CONCURRENCY` | `8` | Maximum in-flight rendering requests (safe range: 1–64) |
| `HONESTQR_REQUEST_TIMEOUT_SECONDS` | `15` | Rendering request deadline |
| `HONESTQR_JSON_LOGS` | `false` | Emit production-friendly JSON logs |
| `RUST_LOG` | `honestqr=info,tower_http=info` | Log filter |

Put authentication, quotas, and TLS at your gateway or reverse proxy. The service deliberately has no user database or credential store.

The standard runtime profile admits at most 64 MiB of estimated active render
work, leaving headroom in the supplied 128 MiB Kubernetes pod. Requests that
would exceed the per-request budget receive JSON `413`; requests that arrive
while the shared budget is occupied receive JSON `429`. A timed-out blocking
render retains its reservation until the worker actually stops, and batches
render directly into their ZIP archive one item at a time. Rendered artifacts
are sent in bounded chunks and keep their reservation until the response body
is consumed or disconnected, so slow clients remain inside the same budget.

## Deployment

- **Docker / Podman:** use the multi-stage [Dockerfile](Dockerfile). The final image is scratch-based and runs as an unprivileged user.
- **Kubernetes:** `kubectl apply -f deploy/kubernetes/`; probes, limits, and a hardened security context are included.
- **AWS Lambda:** see [deploy/aws-lambda/README.md](deploy/aws-lambda/README.md) and the SAM template.
- **Cloud Run, Fly.io, Render, Railway:** deploy the Dockerfile and route traffic to port `8080`.
- **Linux server:** install the release binary and adapt [honestqr.service](deploy/systemd/honestqr.service).

For public internet deployments, add gateway rate limits and a CDN. POST responses are marked `private, no-store`; public GET responses are cacheable and have deterministic ETags.

## Architecture

The core is a deep, pure module with one main interface:

```rust
pub fn render(spec: &QrSpec) -> Result<QrArtifact, QrError>
```

It owns validation, standards-compliant payload construction, QR encoding, rendering, and deterministic metadata. The HTTP server, Lambda handler, and CLI are thin adapters around that seam. This keeps framework churn out of the rendering engine and makes behavior identical in every deployment shape.

```text
HTTP server ─┐
AWS Lambda ──┼──> honestqr-core::render ──> PNG / SVG / matrix
CLI ─────────┘
```

## Development

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
cargo bench -p honestqr-core
```

Tests include independent decoding of generated images, determinism checks, escaping, validation, HTTP contracts, and batch archives. See [CONTRIBUTING.md](CONTRIBUTING.md) before sending a change and [SECURITY.md](SECURITY.md) for private vulnerability reports.

## License

Apache License 2.0. See [LICENSE](LICENSE).

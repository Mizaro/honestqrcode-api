# Changelog

All notable changes are documented here. This project follows Semantic Versioning.

## [Unreleased]

## [0.1.1] - 2026-07-26

### Security

- Added weighted render-memory admission, timeout-safe worker permits, and cooperative batch cancellation.
- Restricted container publication to protected SemVer tags on `main` and pinned every workflow action to an immutable commit.
- Removed query strings from application tracing spans.

### Fixed

- Returned the stable JSON error contract for malformed requests, body limits, timeouts, capacity limits, and panics.
- Emitted validated, deterministic RFC 5545 event payloads and safely encoded RFC 6068 mailbox addresses.
- Handled SIGTERM during graceful server shutdown and kept the Lambda platform deadline above the application deadline.

## [0.1.0] - 2026-07-22

### Added

- Deterministic PNG, SVG, and matrix QR rendering.
- Structured text, URL, bytes, WiFi, email, phone, SMS, WhatsApp, vCard, location, and calendar payloads.
- Axum HTTP API with single and batch endpoints, OpenAPI, embedded docs, health checks, and OpenMetrics.
- Standalone server, CLI, AWS Lambda adapter, scratch container, Compose, Kubernetes, SAM, and systemd deployment assets.
- Bounded inputs, stable JSON errors, security headers, private POST cache policy, and payload-free logs.

[Unreleased]: https://github.com/honestqrcode/honestqrcode-api/compare/v0.1.1...HEAD
[0.1.1]: https://github.com/honestqrcode/honestqrcode-api/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/honestqrcode/honestqrcode-api/releases/tag/v0.1.0

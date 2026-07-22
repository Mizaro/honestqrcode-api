# Contributing

Thank you for helping make private QR generation easier to deploy.

## Before opening a pull request

1. Open an issue for behavior changes or new payload formats so the contract can be agreed first.
2. Keep payload validation and rendering in `honestqr-core`; adapters should translate transport concerns only.
3. Add tests that decode the output with an independent decoder when rendering behavior changes.
4. Do not add telemetry, request-body logging, remote asset fetches, or persistent storage.
5. Run the complete local gate:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
```

Keep commits focused and explain user-visible behavior in the pull request. By contributing, you agree that your work is licensed under Apache-2.0.


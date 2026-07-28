#!/usr/bin/env bash
# Run a cgroup-limited Honest QR API container (1 CPU, 256 MiB RAM) and stress it.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
IMAGE="${IMAGE:-honestqr:stress}"
PORT="${PORT:-18080}"
DURATION="${DURATION:-30}"
CONCURRENCY="${CONCURRENCY:-16}"
SCENARIO="${SCENARIO:-mixed}"
MAX_CONCURRENCY="${MAX_CONCURRENCY:-8}"

cd "$ROOT"

echo "Building image ${IMAGE}..."
docker build -t "$IMAGE" .

echo "Starting API with --cpus=1 --memory=256m on port ${PORT}..."
cid="$(docker run --detach --rm \
  --name honestqr-stress \
  --cpus=1 \
  --memory=256m \
  --memory-swap=256m \
  -e HONESTQR_MAX_ACTIVE_MEMORY_KIB=196608 \
  -e HONESTQR_MAX_CONCURRENCY="${MAX_CONCURRENCY}" \
  -p "${PORT}:8080" \
  "$IMAGE")"

cleanup() {
  docker stop "$cid" >/dev/null 2>&1 || true
}
trap cleanup EXIT

echo "Waiting for /healthz..."
for _ in $(seq 1 30); do
  if curl -fsS "http://127.0.0.1:${PORT}/healthz" >/dev/null; then
    break
  fi
  sleep 1
done

echo "Running stress for ${DURATION}s at concurrency ${CONCURRENCY} (${SCENARIO})..."
cargo run --release -p honestqr-stress -- \
  --base-url "http://127.0.0.1:${PORT}" \
  --duration-secs "$DURATION" \
  --concurrency "$CONCURRENCY" \
  --scenario "$SCENARIO" \
  --memory-profile mib256 \
  --max-concurrency "$MAX_CONCURRENCY"

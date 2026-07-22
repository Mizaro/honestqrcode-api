# syntax=docker/dockerfile:1.7
FROM rust:1.97-alpine AS builder
WORKDIR /src
RUN apk add --no-cache musl-dev
COPY . .
RUN cargo build --locked --release -p honestqr-server

FROM scratch
COPY --from=builder /src/target/release/honestqr-server /honestqr-server
USER 65532:65532
EXPOSE 8080
ENV HONESTQR_HOST=0.0.0.0 \
    HONESTQR_PORT=8080 \
    HONESTQR_JSON_LOGS=true
ENTRYPOINT ["/honestqr-server"]


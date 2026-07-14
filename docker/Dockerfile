FROM rust:1.96.1 AS builder
WORKDIR /app
COPY . .
RUN cargo build --release

FROM debian:bookworm-slim
LABEL org.opencontainers.image.title="pummel" \
      org.opencontainers.image.description="HTTP load-testing CLI" \
      org.opencontainers.image.source="https://github.com/OrekGames/pummel"

# rustls-tls bundles its own webpki roots, so system TLS libraries are not
# required; ca-certificates is kept as a harmless convenience.
RUN apt-get update && apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/*

# Run as a non-root user rather than root.
RUN useradd --create-home --user-group app
COPY --from=builder /app/target/release/pummel /usr/local/bin/pummel
COPY --from=builder /app/examples/ /examples/
USER app
ENTRYPOINT ["pummel"]

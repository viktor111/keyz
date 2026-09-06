FROM rust:1.94.1-bookworm AS builder
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --bins --locked

FROM debian:bookworm-slim
RUN useradd --uid 10001 --create-home keyz
WORKDIR /app
COPY --from=builder /build/target/release/keyz /usr/local/bin/keyz
COPY --from=builder /build/target/release/keyz-cli /usr/local/bin/keyz-cli
COPY docker/keyz.toml /app/keyz.toml
USER keyz
EXPOSE 7667 7668
STOPSIGNAL SIGTERM
HEALTHCHECK --interval=30s --timeout=10s --start-period=10s CMD ["keyz-cli", "status"]
CMD ["keyz"]

FROM rust:1.61 AS builder
COPY . .
RUN cargo build --release

FROM debian:buster-slim
COPY --from=builder ./target/release/keyz ./target/release/keyz
CMD ["/target/release/keyz"]
FROM rust:1.61.0-slim as builder

WORKDIR /usr/src

# Create blank project
RUN USER=root cargo new keyz_docker

# We want dependencies cached, so copy those first.
COPY Cargo.toml Cargo.lock /usr/src/keyz_docker/

# Set the working directory
WORKDIR /usr/src/keyz_docker

## Install target platform (Cross-Compilation) --> Needed for Alpine
RUN rustup target add x86_64-unknown-linux-musl

# This is a dummy build to get the dependencies cached.
RUN cargo build --release

# Now copy in the rest of the sources
COPY src /usr/src/keyz_docker/src/

## Touch main.rs to prevent cached release build
RUN touch /usr/src/keyz_docker/src/main.rs

# This is the actual application build.
RUN cargo build --release

################
##### Runtime
FROM alpine:3.16.0 AS runtime 

# Copy application binary from builder image
COPY --from=builder /usr/src/keyz_docker/target/release/keyz_docker /usr/local/bin

EXPOSE 7667

# Run the application
CMD ["/usr/local/bin/keyz_docker"]
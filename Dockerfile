# Multi-stage build → a static musl binary on a distroless/static base
# (M7 §7.2). No shell, no libc in the final image; just the gateway.
#
#   docker build -t ferrogate .
#   docker run -p 8080:8080 -v ./config.toml:/config.toml -e OPENAI_API_KEY ferrogate

# --- build stage: static musl binary --------------------------------------
# Alpine's Rust defaults to the *-musl target, producing a fully static binary.
# build-base gives the C toolchain that libsqlite3-sys (bundled) needs.
FROM rust:1.97-alpine AS builder
RUN apk add --no-cache musl-dev build-base
WORKDIR /build
COPY . .
# The release profile already strips and LTO-thins (see Cargo.toml).
RUN cargo build --release --bin ferrogate \
    && cp target/release/ferrogate /ferrogate

# --- runtime stage: distroless static, non-root ---------------------------
FROM gcr.io/distroless/static-debian12:nonroot
COPY --from=builder /ferrogate /ferrogate
EXPOSE 8080
# Bind to all interfaces inside the container (override in config as needed).
ENV FERROGATE_SERVER__HOST=0.0.0.0
ENTRYPOINT ["/ferrogate"]
CMD ["--config", "/config.toml"]

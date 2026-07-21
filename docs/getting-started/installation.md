# Installation

LUMEN ships as a single static binary. Pick one of three ways to get it:
Docker, a prebuilt binary from a GitHub release, or a build from source.

## Docker

```bash
docker run -p 8080:8080 \
  -v ./config.toml:/config.toml \
  -e OPENAI_API_KEY=sk-... \
  ghcr.io/qdequele/lumen:latest
```

The image sets `LUMEN_SERVER__HOST=0.0.0.0` for you, so the server binds to
all interfaces inside the container. The image is multi-arch: `linux/amd64`
and `linux/arm64`.

## Prebuilt binary

Static musl binaries for `x86_64-unknown-linux-musl` and
`aarch64-unknown-linux-musl` are attached to every GitHub release cut from a
`v*` tag, each with a `.sha256` checksum file alongside it. Verify a download
before unpacking:

```bash
shasum -a 256 -c lumen-x86_64-unknown-linux-musl.tar.gz.sha256
```

## From source

Needs a recent stable Rust toolchain (MSRV 1.88, per `Cargo.toml` and checked
in CI against the committed `Cargo.lock`):

```bash
cargo build --release -p server --bin lumen
```

The binary lands at `target/release/lumen`. Run it with:

```bash
lumen --config config.toml
```

## Validate a config without booting

`lumen --check-config [--config <PATH>]` validates a config file the same way
the server does at boot (parsing, semantic validation and provider registry
construction) and exits: `0` if valid, non-zero otherwise. It binds no
listener, opens no database, and contacts no provider, so it is safe to run
in a CI or deploy pipeline ahead of a real boot:

```bash
lumen --check-config --config config.toml
```

## Next

Continue to the [Quickstart](quickstart.md), or browse the fully commented
[`config.example.toml`](https://github.com/qdequele/lumen/blob/main/config.example.toml)
on GitHub.

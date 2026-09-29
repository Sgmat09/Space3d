# syntax=docker/dockerfile:1
# Game-server image template. Build from the workspace root (the build context):
#   docker build -f crates/space3d-server/deploy/Dockerfile -t space3d-player-service .
#   docker build -f crates/space3d-server/deploy/Dockerfile --build-arg BIN=server \
#     --build-arg CARGO_ARGS="-p multiplayer_arena --no-default-features" -t multiplayer-arena-server .
#   docker build -f crates/space3d-server/deploy/Dockerfile --build-arg BIN=arena-server -t arena-server .
# BIN=space3d-server (the default) builds the stock player-service binary. CARGO_ARGS selects the
# package / features (e.g. leave out a game's windowed client and its GPU dependencies).
#
# * Needs BuildKit (Docker 23+ default). Network is used by exactly three steps: pulling the two
#   base images, `apt-get` (cmake, clang, perl, pkg-config; a cross gcc when the target differs from
#   the build machine), and `rustup target add` / `cargo fetch`. Compiling runs with
#   `--network=none` from the cargo registry cache, so the compile step is hermetic.
# * Dependencies stay cached across builds on one builder (BuildKit cache mounts for the cargo
#   registry and `target/`): after a source edit only the changed crates and their dependents
#   recompile. A fresh builder (CI) starts cold; export the cache (`--cache-to`) to keep it.
# * RUST_VERSION must equal `channel` in rust-toolchain.toml (checked below), so the image is
#   built with the toolchain the repository pins and never downloads another one.
# * `--platform linux/amd64` / `linux/arm64` CROSS-COMPILES on the build machine's native
#   toolchain (a cross gcc as linker) instead of emulating the compiler, which is slow and
#   crashes intermittently (`cc` segfaults under qemu). The final stage is not executed, so it
#   needs no emulation either.
ARG RUST_VERSION=1.98.1
ARG RUNTIME_IMAGE=gcr.io/distroless/cc-debian12:nonroot
# Names the BuildKit cache mounts (cargo registry, target/): change it for a private / cold cache.
ARG CACHE_ID=space3d

FROM --platform=$BUILDPLATFORM rust:${RUST_VERSION}-slim-bookworm AS build
ARG RUST_VERSION
ARG BUILDARCH
ARG TARGETARCH
ARG CACHE_ID
# The image's own toolchain, always: without this rustup follows rust-toolchain.toml, whose
# `components` / `targets` (rustfmt, clippy, wasm32) it would download at the first cargo call.
ENV RUSTUP_TOOLCHAIN=${RUST_VERSION}
# Rust target, and the linker gcc for it: the plain `cc` when target = build machine.
RUN set -eu; \
    case "$TARGETARCH" in \
      amd64) triple=x86_64-unknown-linux-gnu; cross=x86_64-linux-gnu; pkgs="gcc-x86-64-linux-gnu libc6-dev-amd64-cross" ;; \
      arm64) triple=aarch64-unknown-linux-gnu; cross=aarch64-linux-gnu; pkgs="gcc-aarch64-linux-gnu libc6-dev-arm64-cross" ;; \
      *) echo "unsupported TARGETARCH '$TARGETARCH' (amd64 and arm64 are)" >&2; exit 1 ;; \
    esac; \
    if [ "$BUILDARCH" = "$TARGETARCH" ]; then cc=cc; pkgs=""; else cc="$cross-gcc"; fi; \
    printf '%s\n' "$triple" > /rust-target; printf '%s\n' "$cc" > /rust-cc; \
    apt-get update && apt-get install -y --no-install-recommends cmake clang perl pkg-config $pkgs \
    && rm -rf /var/lib/apt/lists/* \
    && rustup target add "$triple"
WORKDIR /src
COPY . .
# Refuse a base image whose Rust differs from the repository's pinned toolchain.
RUN want="$(sed -n 's/^channel *= *"\(.*\)"/\1/p' rust-toolchain.toml)"; have="$(rustc --version | cut -d' ' -f2)"; \
    [ "$want" = "$have" ] || { echo "rust-toolchain.toml pins $want but the image has $have: set --build-arg RUST_VERSION=$want" >&2; exit 1; }
ARG BIN=space3d-server
ARG CARGO_ARGS=""
# 1. download (the only network use of the compile stage; a no-op when the registry cache is warm)
RUN --mount=type=cache,id=${CACHE_ID}-cargo-registry,target=/usr/local/cargo/registry \
    --mount=type=cache,id=${CACHE_ID}-cargo-git,target=/usr/local/cargo/git \
    cargo fetch --locked --target "$(rustc -vV | sed -n 's/^host: //p')" --target "$(cat /rust-target)"
# 2. compile offline; `target/` lives in a cache mount, the binary is copied out of it
RUN --network=none \
    --mount=type=cache,id=${CACHE_ID}-cargo-registry,target=/usr/local/cargo/registry \
    --mount=type=cache,id=${CACHE_ID}-cargo-git,target=/usr/local/cargo/git \
    --mount=type=cache,id=${CACHE_ID}-target-${TARGETARCH},target=/src/target \
    set -eu; triple="$(cat /rust-target)"; cc="$(cat /rust-cc)"; \
    env "CC_$(echo "$triple" | tr - _)=$cc" \
      cargo build --locked --offline --profile server --target "$triple" \
        --config "target.$triple.linker=\"$cc\"" ${CARGO_ARGS} --bin ${BIN} \
    && cp "target/$triple/server/${BIN}" /server \
    && mkdir -p /skel/data
# 3. The final image has no shell and no package manager: /data is prepared here, owned by
#    nonroot (a named volume copies that ownership the first time it is mounted).

FROM ${RUNTIME_IMAGE}
COPY --from=build /server /usr/local/bin/server
COPY --from=build --chown=65532:65532 /skel/data /data
COPY crates/space3d-server/deploy/server.example.toml /etc/space3d/server.toml
# The config file comes from SPACE3D_CONFIG (not an ENTRYPOINT flag), so `server healthcheck` and
# `docker run IMAGE --check-config` read the same file. State lives under /data.
ENV SPACE3D_CONFIG=/etc/space3d/server.toml \
    SPACE3D__ENV=production \
    SPACE3D__LOG__FORMAT=json \
    SPACE3D__PERSISTENCE__DATA_DIR=/data \
    SPACE3D__PLAYER_SERVICE__DB_PATH=/data/players.db
VOLUME ["/data"]
EXPOSE 7777 8080 9090
STOPSIGNAL SIGTERM
USER nonroot
ENTRYPOINT ["/usr/local/bin/server"]

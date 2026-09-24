# Game-server image template. Build from the workspace root:
#   docker build -f crates/space3d-server/deploy/Dockerfile --build-arg BIN=arena-server -t arena-server .
#   docker build -f crates/space3d-server/deploy/Dockerfile --build-arg BIN=server \
#     --build-arg CARGO_ARGS="-p multiplayer_arena --no-default-features" -t multiplayer-arena .
# BIN=space3d-server builds the stock player-service binary. CARGO_ARGS selects the package /
# features (e.g. leave out a game's windowed client and its GPU dependencies).
FROM rust:1.98-slim-bookworm AS build
RUN apt-get update && apt-get install -y --no-install-recommends cmake clang perl pkg-config \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY . .
ARG BIN=space3d-server
ARG CARGO_ARGS=""
RUN --mount=type=cache,target=/usr/local/cargo/registry --mount=type=cache,target=/src/target \
    cargo build --locked --profile server ${CARGO_ARGS} --bin ${BIN} && cp target/server/${BIN} /server

FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /server /usr/local/bin/server
COPY crates/space3d-server/deploy/server.example.toml /etc/space3d/server.toml
ENV SPACE3D__LOG__FORMAT=json SPACE3D__ENV=production
VOLUME ["/data"]
EXPOSE 7777 8080 9090
STOPSIGNAL SIGTERM
USER nonroot
ENTRYPOINT ["/usr/local/bin/server", "--config", "/etc/space3d/server.toml"]

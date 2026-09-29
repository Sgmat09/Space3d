# Space3d dedicated game server, player service and replication

**Status:** implemented. Both crates, the examples and the acceptance tests are in the tree; §18.3–§18.4 record what changed during implementation and integration. The operator's guide (quickstart, Docker, config reference, admin API, scaling, measured numbers) is [`docs/server.md`](../server.md).
**Crates:** `crates/space3d-replication` is shared and wasm32-clean. `crates/space3d-server` is native only.
**Prerequisite:** the `space3d-net` hardening is already implemented (§17.1).
**Basis:** synthesized from three competing designs. The scores and rationale are in the appendix.

- The **winner is Design C** (ops and scale first). It needs no `space3d-core` change, uses sans-IO gateway and rooms, covers the fleet, checkpoints, outbox and failure matrix, and has the best fit with the real engine APIs.
- From **B** (ECS-native) we took:
  - the registration and DX API (`replicate`, `ComponentRule`, `ClientMode`, `Audience`);
  - encode-once plus byte-compare change detection;
  - flow control against WebSocket bufferbloat;
  - the shared `SimSchedule`.
- From **A** (snapshot-interp) we took:
  - reliability that comes from the transport, with no hand-written retransmit layer;
  - input-slack feedback for tick pacing;
  - `quantize_authoritative` misprediction parity;
  - lag compensation, which is already carried on the wire.

---

## 0. Decisions at a glance

| Topic | Decision |
|---|---|
| **Roles** | **Player service (PS).** Owns accounts, sessions, profiles, player data, leaderboards, matchmaking, lobbies, the fleet registry and checkpoint storage. HTTP JSON. |
| | **Game server (GS).** Headless and authoritative. Runs rooms, where each room is one `App` with its own isolated `World`. Clients reach it over WebSocket at `/play`. |
| | **Deployment.** Both roles are in one library and one process model. `role = "all-in-one"` runs both, `"player-service"` and `"game-server"` run one each. |
| **Join path** | 1. The client logs in to the PS and gets matchmaking, quick play or a lobby. 2. The PS returns an `Assignment { server_url, room_id, join_ticket }`. 3. The client sends `Hello{Ticket}` over WS to the GS. 4. The GS verifies the ticket **offline** (HMAC). The ticket carries the `RoomSpec`, so the room is created lazily and match start needs no RPC. |
| **Fleet** | The GS opens a WS link to the PS; the GS needs no inbound internal port. Messages are JSON: register, heartbeat every 2 s, lease of 10 s, presence, outbox ops acked by `op_id`, and PS→GS commands (prepare room, kick, ban, broadcast, drain). In-process channels are used in all-in-one mode. |
| **Transport** | **v1:** WebSocket for every client (browser, native, mobile) through `space3d-net`, behind the `ClientTransport` / `Connector` / `LinkTx` / `LinkEvent` traits. **Reliability always comes from the transport.** Protocol seq/acks exist only on datagram transports, and only to advance baselines. |
| | **Deferred:** QUIC/WebTransport datagrams. The in-memory transport plus a seeded conditioner (stream or datagram semantics, TCP-like head-of-line stalls) keeps both code paths tested today. |
| **Wire** | Packet = 1 header byte, then either a postcard handshake (Hello/Welcome/Reject/Disconnect) or a Data header plus channel blocks. Snapshots and inputs use custom varint/quantized encodings. Messages and control use postcard. |
| **Replication** | Components are **encoded once per send tick** and compared as bytes, which gives change ticks and suppresses sub-quantum jitter. Each client keeps a per-entity ack tick. Invariant: when an entity is written, it includes every component changed since that client's ack. Acks are optimistic on WS and packet-acked on datagrams. Spawn and despawn are part of the snapshot state, so they heal themselves. |
| **Bandwidth** | Room-wide or spatial-grid interest with hysteresis. Priority accumulators and a per-client byte budget. **Flow control on link backlog** (skip, then kick `SlowConsumer`) prevents TCP bufferbloat. |
| **Prediction** | Gameplay that must be predicted is registered once in `SimSchedule` (`add_sim_system`). The server runs it once per tick. The client runs it once per predicted tick and replays it on mismatch. Both sides simulate quantized values (`quantize_authoritative`), so mismatch detection is exact byte comparison. |
| **Interpolation** | Remote entities render at `server_estimate − adaptive delay`. The per-component `SnapshotBuffer` doubles as the jitter buffer. Extrapolation is capped at 100 ms. |
| **Time** | Ping/pong gives the RTT and a server-tick estimate. The server reports `input_slack` in every snapshot. A PI pacer scales `TickRate::hz` within ±5 %, and `SimTime.dt` and `Time::fixed_delta` stay pinned to the nominal step during the sim. **No `space3d-core` change is needed.** |
| **Threading** | tokio (`space3d_tasks::runtime()`) runs HTTP/WS, ingress, the fleet link and uploads. S shard threads own room `App`s; each `App` is built on its shard because `App` is `!Send`. The `space3d-db` worker thread handles SQLite and rayon handles compute. A room tick never awaits or blocks, and every cross-thread queue is bounded. |
| **Persistence** | The PS uses SQLite behind a `PlayerStore` trait (Postgres later). The GS keeps a durable outbox (at-least-once, idempotent by `op_id`) and writes room checkpoints (replication full-state codec plus optional server-only scene JSON), uploaded to the PS so any GS can restore. |
| **Security** | HMAC-SHA256 tokens with per-kind derived keys and `kid` rotation. Join tickets live 60 s, are single-use (`jti`) and are bound to `server_id`. Resume tokens are process-local secrets. Per-IP and per-link token buckets, strikes, the 16 KiB WS inbound cap, bounded queues everywhere, and an admin API on a private port. |
| **Ops** | TOML config with a `SPACE3D__A__B` env overlay, and secrets taken only from env or files. `/healthz` and `/readyz`, in-house Prometheus `/metrics`, JSON `tracing` logs. SIGTERM triggers drain, checkpoint, flush, exit. `profile.server` (`panic = "unwind"`) enables per-room `catch_unwind`. Distroless Docker image, compose file, k8s notes. |
| **Tests** | `NetHarness` is a room plus N clients over `MemoryNetwork` on one `ManualClock`, stepped tick by tick with byte-level determinism. `TestCluster` adds the PS, gatekeeper, tickets, outbox and recovery in process. There are bots and a load-test library with SLO gates for 200+ clients over real sockets. |

---

## 1. Goals and non-goals

**Goals**
- The server is authoritative for action games at 20–128 Hz, and also covers slower genres (MMO-lite zones, RTS-ish, turn-based) through configuration. Clients send intent (inputs and commands) and never send state.
- The same protocol and code path serve native and browser clients. The client crate compiles for `wasm32-unknown-unknown`.
- One process works for small games, and the same binary scales out to one PS plus N GSs.
- Rooms keep playing while the PS is down; results are buffered in the outbox.
- Bandwidth per client is predictable and capped. Late join and reconnect are cheap (state sync, no event log).
- Everything is testable deterministically in process (AGENTS rule 4). A room tick never blocks (rule 2).

**Non-goals (v1)**
- Peer-to-peer, GGPO-style full-world rollback, or deterministic lockstep.
- Raw UDP with a custom ARQ.
- A multi-instance PS: SQLite is single-writer, and `PlayerStore` keeps a Postgres door open.
- Rapier state rollback on clients.
- TLS termination inside the process (use an ingress or load balancer).

---

## 2. Deployment topology

```
                         ┌──────────────────── Player Service (1 instance) ───────────────────┐
 browser / native ─HTTPS─▶ /v1/auth/* /v1/me /v1/matchmaking/* /v1/play/quick /v1/lobbies/*    │
 client                  │ /v1/sessions/rejoin /v1/leaderboards/* /v1/status                  │
   │                     │ Matchmaker · Lobbies · FleetRegistry (supervisor App, 10 Hz)       │
   │                     │ /internal/v1/fleet (WS) · /internal/v1/rooms/{id}/checkpoint        │
   │                     │ SQLite (space3d-db): accounts, refresh_tokens, player_data, ...     │
   │ Assignment          └─────────────▲───────────────────────────────▲─────────────────────┘
   │ {server_url, room_id,             │ fleet link (WS JSON)          │
   │  join_ticket}                     │ Register/Heartbeat/Outbox ops │ PrepareRoom/Kick/Ban/Drain/Ack
   ▼                                   │                               │
 ┌──────────── Game Server gs-N ───────┴──────┐      ┌──── Game Server gs-M ────┴────┐
 │ WS :7777/play · GET /ping                  │  …   │                                │
 │ ingress + Gatekeeper (tokio)               │      │                                │
 │ shards × S ─ rooms (App + World) × k       │      │                                │
 │ admin/metrics/health :9090 (private)       │      │                                │
 │ local SQLite: outbox (+ local checkpoints) │      │                                │
 └────────────────────────────────────────────┘      └────────────────────────────────┘
 all-in-one: both boxes in one process, in-process fleet link, one SQLite file; the PS API and
 /play may share one port (equal binds → one HttpServer).
```

**Roles** (`role` in config; one library, same binaries):

| role | contents | binary |
|---|---|---|
| `all-in-one` | PS + GS | the game's server binary (`ServerMain::from_env(Game).run()`) |
| `player-service` | PS only (game-agnostic) | the stock `space3d-server` binary (`NoGame`) |
| `game-server` | GS only; registers over the fleet link | the game's server binary |

**Dev shortcut:** a GS with `game.allow_dev_auth = true` accepts `HelloAuth::Dev{name}` with no PS, but only on a loopback bind; config validation enforces this. Dev joins go to `GameDefinition::dev_room_spec()`, which is room 1 with mode `"dev"`.

### 2.1 Join sequence (split mode)

1. **Login.** The client sends `POST /v1/auth/guest {device_id, device_secret}` and receives `SessionTokens` (access token 1 h, refresh token 30 d, rotated on use).
2. **Queue.** `POST /v1/matchmaking/tickets {queue, region_pings}` returns `{ticket_id}`. The client long-polls `GET /v1/matchmaking/tickets/{id}?wait=20`.
3. **Match.** In the PS supervisor tick, the matcher forms a match. `FleetRegistry.allocate(region, game_id, schema_hash, seats)` picks a server (say gs-3) and reserves seats.
4. **Tickets.** The PS signs one `JoinClaims{player, name, team, server_id=gs-3, room, spec, jti, exp=+60s}` per player. It may also send `FleetDown::PrepareRoom{spec}` to gs-3 to pre-warm the room.
5. **Found.** The long poll returns `Found{Assignment}`.
6. **Hello.** The client opens a WS to `wss://gs-3/play` and sends `Hello{v, game_id, schema_hash, Ticket}`.
7. **Gatekeeper** on gs-3, in order:
   1. Upgrade checks: per-IP limits, IP bans, Origin.
   2. Hello checks: magic, version, lengths, draining.
   3. Ticket MAC, kind, `exp`, `server_id`, unused `jti`, account not banned, `spec` valid.
   4. `RoomManager::get_or_create(spec)`, then `RoomInbound::Join`.
8. **Room tick.** The room checks schema, game, capacity and joinable, allocates a slot, and sends `Welcome{slot, tick, rates, resume, limits}`. It fires `Events<PlayerJoined>`, and the game spawns the pawn.
9. **Sync.** Snapshots stream in by priority. `SYNC_COMPLETE` ends the loading screen.
10. **Gameplay.** Inputs go up at 60 Hz, snapshots come down at 30 Hz, pings run at 4 Hz.
11. **Results.** `RoomControl::end_match` puts a `FleetUp::MatchResult{op_id}` in the outbox. The PS applies it in one transaction, updating history, Elo and leaderboards, then sends `Ack`.

### 2.2 Configuration (TOML + env)

`crates/space3d-server/src/config.rs` holds `ServerConfig`. `deploy/server.example.toml` is the commented reference and is also printed by `--print-default-config`.

**Load order:** defaults ← the game binary's built-in TOML (`ServerMain::from_env_with_defaults`, optional) ← file (`--config` / `SPACE3D_CONFIG`) ← env `SPACE3D__SECTION__KEY=value` (split on `__`, lowercased, value parsed as a TOML value, else a string) ← CLI flags. Tables merge key by key; arrays (`[[matchmaking.queues]]`) replace.

**Parsing:**
- `${VAR}` expands in strings, and `{server_id}` expands in `game.public_url`.
- Durations are strings: `"250ms"`, `"30s"`, `"15m"`, `"24h"`, `"30d"`.
- Byte sizes are integers or strings: `"16KiB"`, `"1MiB"`.

**Sections:** `[log] [game] [limits] [player_service] [fleet] [admin] [security] [persistence] [matchmaking] [[matchmaking.queues]]`.

**Secrets never appear in the file.** The config only names where they live:
- `security.keys_env` (`"k2:<b64>,k1:<b64>"`) or `keys_file` (TOML `{k1="<b64>"}`);
- `fleet_token_env` / `fleet_token_file`;
- `admin_token_env` / `admin_token_file`.

`env = "production"` makes validation refuse keys that are missing, shorter than 32 bytes, or dev defaults.

### 2.3 Docker and orchestration

- **`crates/space3d-server/deploy/Dockerfile`:**
  - Build stage: `rust:${RUST_VERSION}-slim-bookworm`. `RUST_VERSION` equals the `rust-toolchain.toml` channel (the Dockerfile refuses a mismatch and `tests/deploy.rs` checks it), so the build never downloads another toolchain.
  - `cargo fetch --locked` is the only network use of the compile stage; `cargo build --locked --offline --profile server` runs under `RUN --network=none`. The cargo registry and `target/` live in BuildKit cache mounts, so after a source edit only the changed crates and their dependents recompile. The `cargo fetch` step holds the registry mounts with `sharing=locked`: `docker compose build` builds two images at once, and cargo's package-cache lock lives outside the mounts, so two cold fetches unpacked the same crate concurrently and failed. `--cache-to` exports layers, not cache-mount contents, so an empty CI runner compiles cold (docs/server.md §3).
  - Runtime stage: `gcr.io/distroless/cc-debian12:nonroot` (about 71 MB with the binary), `/data` prepared and owned by `nonroot`, `SPACE3D_CONFIG=/etc/space3d/server.toml`, `EXPOSE 7777 8080 9090`, `VOLUME /data`, `STOPSIGNAL SIGTERM`.
  - Distroless has no curl, so probes use `server healthcheck [--ready]` (or HTTP probes on the admin port).
  - The workspace-root `.dockerignore` keeps the build context to the sources (about 9 MB instead of the multi-GB `target/`).
- **`deploy/compose.yaml`:** one PS and two GSs for split mode, loopback ports, read-only root filesystem, no capabilities, `stop_grace_period` = drain timeout + 30 s. `GAME_BIN` / `GAME_CARGO_ARGS` pick the game server.
- **Kubernetes (`deploy/k8s/`, `docs/server.md` §3.1):**
  - PS: a `Deployment` (`replicas: 1`, `Recreate`, because SQLite is a single writer on a ReadWriteOnce volume) with a PVC and a `Service`.
  - GS: a `StatefulSet` so names are stable (`server_id` = the pod name) and the outbox and checkpoint PVC is durable, plus a headless `Service` and a `PodDisruptionBudget` (`maxUnavailable: 1`). Per-pod ingress hosts (`gs-N.play.example.com`) are in `ingress.example.yaml`.
  - The drain contract in manifest form: `readinessProbe` on `/readyz`, `preStop` sleep 5 s, `terminationGracePeriodSeconds = drain_timeout + 5 + 30 + slack`.
  - Config in a `ConfigMap` that restates every key of `server.example.toml` (a game's built-in defaults sit under the file and are dev-oriented), secrets in a `Secret` that is created out of band.
  - The manifests are schema-validated (kubeconform, `kustomize build`) and checked against the production config validation in `tests/deploy.rs`; they have not been run on a cluster.
  - `space3d_fleet_free_seats{region}` drives HPA or KEDA scaling (note in `docs/server.md` §6).

### 2.4 Health, readiness and graceful shutdown

- **`/healthz`** returns 200 while the supervisor ticks and every shard heartbeat is less than 5 s old. A stuck tick fails liveness.
- **`/readyz`** returns 200 when the server is not draining, the DB is ready, it has registered with the PS once (GS role; registration is a latch, so a later PS outage keeps it ready and `space3d_fleet_connected` reports the link), and its shards are alive.
- **Lifecycle** (`lifecycle.rs`): `Starting → Ready → Draining → Stopping → Stopped`.
  1. **Drain trigger:** SIGTERM or SIGINT, `POST /admin/v1/drain`, or `FleetDown::Drain`. `/readyz` returns 503, the heartbeat reports `draining`, and every room gets `RoomCommand::Drain{deadline}`, which sends `Notice::ShuttingDown`. The draining heartbeat goes out in the same supervisor tick (not one later). No room is created any more (a ticket that would create one gets `Reject(ServerDraining)`); a ticket into a room that still runs here is admitted for a player who holds a slot in it (a rejoin into a match that finishes here), and resumes still work. A match room refuses newcomers with `Reject(RoomClosed)`; a room that moves on drain refuses them as transient, `Reject(ServerDraining)` with `retry_after_ms` (`NetServer::refuse_joins`, `gs::room::moving_refusal`): a ticket the PS issued in the moment before it heard of the drain (quick play reads the draining state from the heartbeat) sends its holder back to the PS, which places them in the room once it moved, instead of reading the world as gone and opening a second copy of it.
  2. **During drain:** match rooms finish naturally within `drain_timeout`; a player's `POST /v1/sessions/rejoin` sends them back to the same room on the draining server (the PS never restores a room that is still in a live server's heartbeat, so a match is never forked). **Persistent rooms move:** they send `Notice::Reconnect` and report `FleetUp::RoomMoving`; the PS marks the room moving (from then on it holds the players' rejoins) and answers `FleetDown::RoomMovingAck`. Only then (or after `MOVE_ACK_WAIT`, 2 s, if no answer comes; at once if the fleet link is down) does the room **freeze**, at the end of that tick: every client gets `Disconnect(ServerShutdown)` (after that tick's snapshot went out) and the final checkpoint is captured right then, so it holds everything the players saw (nobody can change or observe the room afterwards; later ticks are never captured, and a failed upload retries that same capture). The handoff waits for exactly that capture: a room's capture times strictly increase (catch-up ticks can share a unix millisecond), so an earlier capture's upload never counts. Once it is stored the room closes and reports `RoomClosed { restorable: true }`, which the PS keeps as orphaned, so the players' rejoin restores it on another server from exactly that state. The room never reports itself restorable before that — not even after the 15 s shutdown wait (a server with only persistent rooms reaches `Stopping` at once): it keeps retrying for as long as the process runs. If the process stops first (store unreachable for the whole stop window), the shard writes the freeze capture to the local rescue store (uploaded on the next start, see step 4) and the room is lost with its server: its lease expires and a rejoin restores the newest stored checkpoint. A rejoin that arrives while the room is moving — `RoomMoving` received, or the room reports `RoomStatus::moves_on_drain` and its server's heartbeat says draining — waits (up to 10 s, real time; then `503 room_moving` with `retry_after_ms`) for `RoomClosed` instead of going back to the room being frozen (its draining server refuses the ticket, `Reject(ServerDraining)`) or restoring an older checkpoint. Quick play does the same for a mode whose persistent room is moving (`FleetRegistry::displaced_room_for`: moving → wait, bounded like a rejoin; handed off, lost with its server, or stalled → `PlayerService::restore_room`, as a rejoin would, and the player joins it there), so it never opens a second copy of a world next to one that is on its way; a room that was only orphaned by its server's re-registration is left alone until the next heartbeat settles whether that server still runs it. (Before the acknowledgement existed the room froze in its drain tick while `RoomMoving` still had two 10 Hz supervisor hops to go; clients rejoin within milliseconds, so their rejoin got a ticket straight back to the draining server.) A move the PS has heard of but that is not stored after `HANDOFF_STALL_MS` (30 s) counts as stalled: a rejoin restores the room elsewhere from its last stored checkpoint and the copy left behind is superseded. A room that panics while handing off is rebuilt still handing off (drain state, handoff, move notice and pending close carry over; a frozen room is rebuilt from, and uploads again, exactly its freeze capture). The frozen room's `PlayerLeft`s are not reported (its players are in transit) and game-code close requests are ignored (they would delete the checkpoint). (Keeping the room playable until the upload finished lost every change made during the upload.) Clients treat `Notice::Reconnect` as a warning; the example client rejoins on the disconnect.
  3. **Deadline:** checkpoint all rooms, then await uploads and the outbox flush (up to 20 s; a room gives up on its final upload after 15 s and keeps that capture in the local rescue store). Send `Disconnect(ServerShutdown)` (rooms report `RoomClosed { restorable: true }`: their checkpoints are kept for a rejoin; like a handoff, each such room first reports `RoomMoving` and keeps its clients until the PS acknowledged it), stop listeners, send `FleetUp::Deregister` (the PS marks the server dead at once; its lease covers a lost message), close the fleet WebSocket with a closing handshake (bounded; the PS logs a drained server's close at INFO), drop `Db` (its worker drains the queue), and exit 0.
  4. **Second signal:** every shard captures its rooms synchronously into the GS's own database (`LocalCheckpoints`, the "rescue" store; writes bounded to 5 s), sends `Disconnect(ServerShutdown, resume_allowed = false)`, and the process exits 1. The next start uploads those checkpoints to the configured store unless it already holds a newer one (`gs::checkpoint::upload_rescued`), so a rejoin restores them.

---

## 3. Crate layout and module ownership

Two builders work in parallel against the frozen contract (§18):

- The **replication builder** owns everything under `crates/space3d-replication/` **except** `src/api.rs` and `src/services.rs`.
- The **server builder** owns `crates/space3d-server/**` **plus** `crates/space3d-replication/src/api.rs` and `src/services.rs`, the player-service DTOs and HTTP client.

`space3d-net` is done (§17.1). Any further change goes to the server builder for `src/server/**` and to the replication builder for `src/ws/**`.

Root `Cargo.toml`:
- Only the server builder edits `[workspace.dependencies]`.
- The replication builder asks for additions, and only for latest-stable crates (AGENTS rule 1).
- The security dependencies (`hmac`, `sha2`, `getrandom`, `argon2`) and `postcard` are already added.

### 3.1 `crates/space3d-replication` (shared; wasm32-clean; no tokio)

**Features:**
- `server`: room-side replication, pure logic.
- `client`: `ClientReplicationPlugin` and bots.
- `api`: player-service DTOs, plus `PlayerServiceClient` when `client` is also on.
- `harness`: `NetHarness`; implies `server` and `client`.

Defaults are `server`, `client`, `api`.

**Dependencies:** `space3d-core`, `space3d-tasks`, `space3d-net` (WS client, `Http`), `serde`, `serde_json`, `postcard`, `ahash`, `smallvec`, `web-time`, `parking_lot`, `async-channel`, `tracing`, `thiserror`. With `api`, also `space3d-db` and `uuid`. `proptest` is a dev-dependency.

| module | owner | contents |
|---|---|---|
| `lib.rs` | R | `ReplicationPlugin` (built-in `Transform`/`Name`/`Parent`), `AppReplicationExt` (`replicate`, `replicate_with`, `add_input`, `add_message`, `add_sim_system`, `set_input_sampler`), `FastMap`, prelude |
| `ids.rs` | R | `PlayerId`, `RoomId`, `MatchId` (u64; hex in JSON, u64 in postcard), `Tick(u32)`, `NetId(u32)`, `NetClientId(u16)`, `LinkId(u64)` |
| `components.rs` | R | `Replicated`, `ControlledBy`, `Owner`, `NetTeam`, `NetPriority`, `NetRelevance`, `InterestSource`, `Simulated`, `NetEntity`, `Remote`, `Predicted`, `Interpolated`, `Controlled` |
| `registry.rs` | R | `ReplicationRegistry`, `ComponentRule<T>`, `NetCodec<T>`, `PostcardCodec`, `MappedCodec`, `ClientMode`, `Audience`, `SendRule`, `Interpolate`, `MessageConfig`, `Direction`, `EventTiming`, `ComponentKind`, schema hash |
| `mapping.rs` | R | `NetIdMap` (server), `EntityMap` (client), `EncodeCtx`, `DecodeCtx` |
| `quant.rs` | R | `quantize_f32`, smallest-three quaternion, `TransformCodec`, `ScaleMode` |
| `wire.rs` | R | `Writer`, `Reader` (bounded; never panics), `DecodeError`, `EncodeError` |
| `sim.rs` | R | `SimSchedule`, `SimTime`, `SimRole`, `run_sim` |
| `input.rs` | R | `NetInput`, `PlayerInput<I>`, `InputSampler<I>`, `InvalidInput` |
| `messages.rs` | R | `NetMessage`, `FromClient<M>`, `FromServer<M>`, `Target`, `InvalidMessage` |
| `time.rs` | R | `NetClock`, `SystemClock`, `ManualClock`, `NetClockHandle`, `RttEstimator`, `ServerClock`, `TickPacer`, `PacerConfig`, `PaceAction`, `InterpClock`, `InterpConfig` |
| `stats.rs` | R | `NetStats`, `RoomNetStats` |
| `protocol/mod.rs` | R | `PROTOCOL_VERSION`, `MAGIC`, `PacketKind`, `ChannelId`, `flags`, `AckField`, `DataHeader`, `DataWriter`, `decode_packet`, `peek`, `encode_handshake`, `WireLimits` |
| `protocol/handshake.rs` | R | `Hello`, `HelloAuth`, `ClientCaps`, `Platform`, `Welcome`, `Reject`, `RejectCode`, `Disconnect`, `DisconnectCode`, `ResumeToken` |
| `protocol/control.rs` | R | `ControlMsg`, `TimeMsg`, `Ping`, `Pong`, `Notice`, `NoticeLevel`, `ClientReport` |
| `protocol/input.rs` | R | `InputWindow`, `InputRun` |
| `protocol/snapshot.rs` | R | `SnapshotHeader`, `SnapshotWriter`, `SnapshotView`, `EntityRecord`, `DespawnRecord`, `flags`, `op` |
| `transport/mod.rs` | R | `Lane`, `TransportCaps`, `LinkState`, `TransportError`, `CloseCode`, `TransportStats`, `ClientTransport`, `Connector`, `PeerInfo`, `LinkTx`, `LinkEvent` |
| `transport/ws.rs` | R | `WsConnector`, `WsClientTransport` (native and web) |
| `transport/memory.rs` | R | `MemoryNetwork`, `MemoryServerEndpoint`, `MemoryConnector`, `MemoryClientTransport`, `MemoryLinkTx` |
| `transport/conditioner.rs` | R | `LinkConditions` (+ presets), `LinkProfile`, `Semantics`, `BurstLoss`, `SplitMix64`, `ConditionedQueue`, `Conditioned<T>`, `parse_netsim` |
| `server/mod.rs` | R | `ServerReplicationPlugin`, `ServerNetConfig`, `NetServer`, `NetInbox`, `RoomSender`, `room_channel`, `RoomInbound`, `JoinRequest`, `InboxFull`, `PlayerIdentity`, `PlayerJoined`, `PlayerDisconnected`, `PlayerResumed`, `PlayerLeft`, `LeaveReason`, `ClientInfo`, `SlotState`, `InterestPolicy`, `Visibility`, `StrikePolicy` |
| `server/handshake.rs` | R | `parse_hello`, `resume_route`, `reject_packet`, `ct_eq`, `SecretSource` |
| `server/{slots,change,interest,priority,inputs,receive,send}.rs` | R | internals (§8), plus the public `TokenBucket`, `VisibilityRule`, `SpatialGrid`, `ByteBudget`, `VirtualBrain`, `brain` |
| `server/full_state.rs` | R | `FullState`, `EncodedEntity`, `capture(&App)`, `restore(&mut App, &FullState)` (checkpoints) |
| `server/lagcomp.rs` | R | **Phase 2:** `LagCompensated`, `HitShape`, `LagCompensation::raycast`, `LagHit` |
| `client/mod.rs` | R | `ClientReplicationPlugin`, `ClientConfig`, `PredictionConfig`, `NetClient`, `ConnectTarget`, `ConnectionState`, `FailReason`, `ConnectionEvent`, `RemoteDespawned`, `Mispredicted` |
| `client/{connection,receive,prediction,interpolation}.rs` | R | internals (§9); `SnapshotBuffer<T>`, `PredictionHistory`, `PredictionError`, `InputHistory`, `PendingRefs` |
| `bots.rs` | R | `Bot<I>`, `install_bot`, `scripted` |
| `harness/mod.rs` | R | `NetHarness`, `HarnessConfig`, `ClientSpec`, `ClientHandle`, `ConvergenceError` |
| **`api.rs`** | **S** | PS REST DTOs (§11.1) |
| **`services.rs`** | **S** | `PlayerServiceClient`, `DeviceCredentials`, `OnlineError` |

R = replication builder, S = server builder.

The server-side logic lives in the shared crate for four reasons:
- It is pure logic with no sockets.
- The deterministic harness can then run server and clients from one crate.
- A future listen server (host plays in the browser) stays possible.
- `space3d-server` only wraps the process around it.

### 3.2 `crates/space3d-server` (native only; server builder)

- `#![cfg(not(target_arch = "wasm32"))]` sits at the crate root, and every dependency is target-gated, so the wasm32 check sees an empty crate.
- Features: `password-auth` (argon2, default on) and `testing` (`TestCluster`, which enables `space3d-replication/harness`).
- Binary: `space3d-server` (PS role, `keygen`, `check-config`, `print-default-config`, `healthcheck`).

| module | contents |
|---|---|
| `lib.rs` | `ServerMain<G>` (`from_env`, `with_config`, `unix_clock`, `build() -> App` (supervisor), `run() -> ExitCode`), `ServerError`, prelude |
| `config.rs` | `ServerConfig` and all sections, `Role`, `Env`, `LogFormat`, `CheckpointStoreKind`, `QueueConfig`, `load`, `validate`, `redacted_toml`, `overlay_env`, `parse_duration`, `parse_bytes`, serde adapters |
| `clock.rs` | `UnixClock`, `SystemUnixClock`, `ManualUnixClock` |
| `game.rs` | `GameDefinition` (`game_id`, `protocol`, `build_room`, `room_net_config`, `validate_spec`, `checkpoint_registry`, `recovery_policy`, `dev_room_spec`), `RoomSpec`, `RecoveryPolicy`, `NoGame` |
| `logging.rs` | `init(&LogConfig)` |
| `lifecycle.rs` | `Phase`, `LifecycleHandle`, `install_signal_handlers`, `LifecyclePlugin` |
| `metrics.rs` | `Metrics`, `Counter`, `Gauge`, `Histogram`, bucket constants, `GameServerMetrics`, `PlayerServiceMetrics` |
| `health.rs` | `HealthState` (shard heartbeats, draining, db, registered), `router` |
| `admin.rs` | `AdminState`, `router` (§14.3) |
| `security/token.rs` | `KeyRing`, `Secret`, `TokenKind`, `Claims`, `sign`, `verify`, `JoinClaims`, `SessionClaims`, `NonceCache`, `TokenError` |
| `security/{password,ratelimit,bans,netaddr}.rs` | argon2 via `spawn_compute`; `KeyedLimiter<K>`, `ConnectionLimiter`; `BanList`, `Ban`, `BanTarget`; `Cidr`, `resolve_client_ip` |
| `gs/mod.rs` | `GameServerPlugin`, `GameServerHandle` |
| `gs/transport_ws.rs` | `WsServerTransport` (hardened `WsServer` → `LinkEvent`), `WsLinkTx` |
| `gs/gatekeeper.rs` | sans-IO `Gatekeeper`, `GatekeeperConfig`, `GateOutcome` |
| `gs/ingress.rs` | `Ingress`, `spawn` |
| `gs/rooms.rs` | `RoomManager`, `RoomHandle`, `RoomCommand`, `RoomStatus`, `RoomDetail`, `RoomPhase`, `RoomError` |
| `gs/shard.rs` | `ShardPool`, `ShardHandle`, `ShardCmd` |
| `gs/room.rs` | `RoomBuild`, `build_room_app`, `RoomPlugin`, `RoomContext`, `RoomControl`, `RoomRequest`, `MatchResult`, `PlayerResult`, `PlayerDataLoaded` |
| `gs/checkpoint.rs` | `Checkpoint`, `CheckpointStore` (+ `LocalCheckpoints`, `RemoteCheckpoints`, `MemoryCheckpoints`, `NoCheckpoints`) |
| `gs/fleet.rs` | `FleetUp`, `FleetDown`, `ServerRegistration`, `Heartbeat`, `Capacity`, `DataWrite`, `FleetLink` (+ `WsFleetLink`, `InProcessFleetLink`, `in_process_pair`) |
| `gs/outbox.rs` | `Outbox`, `OutboxSender`, `OutboxFull` |
| `ps/mod.rs` | `PlayerService` (shared state), `PlayerServicePlugin`, `PlayerServiceHandle` |
| `ps/{store,schema,auth,matchmaker,lobbies,fleet,results,routes}.rs` | `PlayerStore` + `SqliteStore`; migrations; auth routes and `AuthedPlayer`; pure `Matchmaker`; `Lobbies`; `FleetRegistry`; Elo; routers |
| `loadtest.rs` | `LoadTestConfig`, `LoadTarget`, `Slo`, `LoadTestReport`, `run` |
| `testing.rs` | `TestCluster`, `TestClusterConfig` (feature `testing`) |
| `deploy/` | `Dockerfile`, `compose.yaml`, `server.example.toml` |

A game's server binary looks like this:

```rust
fn main() -> std::process::ExitCode {
    match space3d_server::ServerMain::from_env(arena::ArenaGame) {
        Ok(main) => main.run(),
        Err(e) => { eprintln!("{e}"); std::process::ExitCode::from(2) }
    }
}
```

### 3.3 Example games

- `examples/multiplayer_arena` is the minimal template (integration pass): one crate whose library holds the shared protocol (`CubesProtocol`: `CubeColor`, `CubeInput`, the `drive` sim system), the server rules (`CubesRules`), a deterministic `Wander` bot, WASD `controls` (feature `client`, through `space3d_platform::ActionMap`) and, native only, `server::CubesGame` + `DEV_CONFIG`. Binaries: `server` (`ServerMain::from_env_with_defaults`; all-in-one on loopback with no arguments), `client` (native + web; `--ps` guest login → quick play → ticket, or `--server` dev auth; `--headless --bot`; rejoin through the player service after a failed session; feature `client`, so `--no-default-features` builds a server without GPU dependencies) and `loadtest`.
- `examples/arena-shared` (protocol, components, input, sim systems; no render dependencies) and `examples/arena-client` (native and web) are owned by the replication builder.
- `examples/arena-server` and the arena load-test binary are owned by the server builder.

---

## 4. Threading model

```
main thread ── supervisor App (headless_runner, 10 Hz)
   LifecyclePlugin · GameServerPlugin (room directory, fleet heartbeat, outbox pump, checkpoint
   bookkeeping, drain) · PlayerServicePlugin (matchmaker tick, lobby starts, fleet leases, allocation)
tokio (space3d_tasks::runtime(), multi-thread "space3d-io")
   axum: /play (WsServerTransport), /ping, PS /v1 + /internal/v1, admin :9090, /metrics, health
   space3d-net per-socket reader/writer tasks (bounded, byte-counted)
   ingress task: WsServerTransport::next_event().await → Gatekeeper → RoomSender::try_send
   fleet link (WsFleetLink reconnect loop) · checkpoint uploads · PS handlers (await Db tasks)
shard threads × S ("space3d-shard-N"; S = available_parallelism − 1, min 1)
   own rooms; App built here (App is !Send); deadline scheduler; catch_unwind per room tick
space3d-db worker thread(s): PS store; GS outbox + local checkpoints
rayon (spawn_compute): argon2, checkpoint serialization, optional per-client packet build (>32 clients)
```

**Rules:**
- A room `World` is touched only by its shard thread.
- Everything else reaches a room through `RoomSender` (bounded `async_channel`, `try_send`) or `RoomCommand` (bounded crossbeam, `try_send`). It reads the `RoomStatus` that each room publishes at 1 Hz into a `Mutex` the room holds for microseconds.
- A room tick calls only `try_recv`, `LinkTx::send` (non-blocking), `Task::poll`, `AsyncQueue` callbacks and CPU work. `Db::blocking` and `Task::block` are banned outside tools and tests.
- Wall-clock time is read only by `SystemClock`, `SystemUnixClock` and the shard scheduler. Tests inject `ManualClock` and `ManualUnixClock`, and step rooms directly.

**Shard loop** (`gs/shard.rs`):

1. Apply `ShardCmd` (`Create`, `Remove`, `Stop`).
2. For each due room, run up to `max_catchup_ticks` rounds of `catch_unwind(app.update(step))`, one tick each, observing `tick_seconds` every round.
   - If the room is still behind, record `ticks_skipped` and set `next_at = now + step`.
   - On panic, mark the room `Crashed`, increment `room_crashes`, and rebuild it from its last checkpoint at the next epoch of its placement (`gs::room::rebuilt_epoch`: the low 16 bits count rebuilds, the high bits placements, so a stale copy's rebuilds never reach the epoch of a copy restored elsewhere from its checkpoint). After 3 crashes in 5 minutes, close it.
3. Store the heartbeat.
4. `select` on the command channel with a deadline of `min(next_at)`.

**Placement:** a new room goes to the shard with the lowest EWMA tick cost. Rooms never migrate (deferred).

**Room construction:** `RoomManager::create` creates the inbox immediately, so ingress can queue joins at once, then sends `ShardCmd::Create(RoomBuild)`. The shard calls `build_room_app` (§7.1).

---

## 5. Transport layer (`space3d_replication::transport`)

| lane | WebSocket (v1) | QUIC / WebTransport (deferred) | memory (tests) |
|---|---|---|---|
| `Reliable` | the TCP stream | one ordered stream | ordered; "loss" becomes an RTO stall with head-of-line blocking |
| `Unreliable` | same stream; the **sender skips** optional packets while backlogged; the receiver drops stale ones | datagrams (≤ ~1150 B) | loss, bursts, jitter, reorder, duplicates, bandwidth cap |

```rust
pub trait ClientTransport: Send + Sync + 'static {            // Send+Sync on every target → storable in Resources
    fn caps(&self) -> TransportCaps;  fn state(&self) -> LinkState;
    fn send(&mut self, lane: Lane, packet: Vec<u8>) -> Result<(), TransportError>;
    fn recv(&mut self) -> Option<Vec<u8>>;  fn backlog_bytes(&self) -> usize;
    fn close(&mut self, code: CloseCode, reason: &str);  fn stats(&self) -> TransportStats;
}
pub trait Connector: Send + Sync + 'static {                  // one fresh link per (re)connect attempt
    fn connect(&self, url: &str) -> Result<Box<dyn ClientTransport>, TransportError>;
}
pub trait LinkTx: Send + Sync + 'static {                     // server side, one per link
    fn caps(&self) -> TransportCaps;  fn send(&self, lane: Lane, packet: Vec<u8>) -> Result<(), TransportError>;
    fn backlog_bytes(&self) -> usize;  fn close(&self, code: CloseCode, reason: &str);  fn is_open(&self) -> bool;
}
pub enum LinkEvent { Opened { link: LinkId, peer: PeerInfo, tx: Arc<dyn LinkTx> },
                     Packet { link: LinkId, lane: Lane, data: Vec<u8> }, Closed { link: LinkId, reason: String } }
pub struct TransportCaps { pub true_datagrams: bool, pub max_unreliable: usize, pub max_reliable: usize }
```

- **`WsConnector` / `WsClientTransport`** (native and web) wrap `space3d_net::WsConnection`. They use binary messages only, with `max_message_size = 1 MiB`. They **never** use `WsConfig::reconnect`: its buffered sends would replay stale inputs before the resume `Hello`. `NetClient` runs its own reconnect loop with fresh links.
- **`WsServerTransport` / `WsLinkTx`** (server crate) wrap a hardened `WsServer`: 16 KiB inbound cap, `accept_text = false`, bounded inbox, and a byte-counted outbound backlog. `LinkId` is the WS `ClientId`.
- **`MemoryNetwork`** is deterministic on a shared `ManualClock`. Each link has two `ConditionedQueue`s, `pump()` delivers everything that is due, and `wire_digest()` hashes all delivered bytes. `partition` and `sever` support resume tests.
- **Conditioner presets:** `perfect`, `lan`, `broadband`, `mobile_4g`, `bad_wifi` (Gilbert–Elliott bursts), `terrible`. `parse_netsim("latency=80ms,loss=2%")` feeds debug menus, and `Conditioned<T>` can wrap a real transport.
- **QUIC decision:** not in v1. WS covers every target with one code path, while UDP adds certificates, UDP load balancing and connection-ID routing. The protocol is already datagram-ready (seq/acks, redundancy, stale-drop, per-packet MTU splitting) and exercised through `Semantics::Datagram`. Phase 6 adds `quinn 0.11.12` for native clients, then WebTransport (`wtransport 0.7.2`, browser behind `web_sys_unstable_apis`). The harness measures the benefit first, for example misprediction bursts at 3 % loss.

---

## 6. Protocol and wire format (`space3d_replication::protocol`)

One transport message (WS binary message, datagram, or stream frame) is one packet. Integers are little-endian, varints are LEB128 (`Reader` rejects over-long encodings), and signed values are zigzag-encoded.

```
Packet     := u8 header · body
header     := bits 0-2 kind: 1 Hello · 2 Welcome · 3 Reject · 4 Data · 5 Disconnect
              bits 3-7 flags (Data only; must be 0 otherwise)
Hello/Welcome/Reject/Disconnect := postcard(struct), ≤ MAX_HANDSHAKE_BYTES (4096)
Data       := [HAS_SEQ] u16 seq · [HAS_ACK] u16 ack · u32 ack_bits · varu32 tick · Block*
Data flags := bit3 RELIABLE (has reliable-lane blocks) · bit4 HAS_SEQ · bit5 HAS_ACK · bits 6-7 reserved(0)
Block      := u8 channel · varu32 len · payload[len]
```

The `tick` field is the server tick of the state for S→C packets and the client's predicted tick for C→S packets. `seq` and `ack` appear only on datagram transports: S→C unreliable packets carry `seq`, and C→S packets ack them with `AckField{ack, bits}` (a 33-packet window).

| ch | name | lane | dir | payload |
|---|---|---|---|---|
| 0 | Control | reliable | ⇄ | postcard `ControlMsg` (Notice, Kick, Leave, SetRate, InterestHint, Report) |
| 1 | Time | unreliable | ⇄ | postcard `TimeMsg` (`Ping{id,t_us}` / `Pong{id, client_time_us, server_tick, tick_frac, hold_us}`) |
| 2 | Snapshot | unreliable | S→C | §6.3 |
| 3 | Input | unreliable | C→S | §6.4 |
| 4 | MsgReliable | reliable | ⇄ | `varu16 message kind · postcard(M)` |
| 5 | MsgUnreliable | unreliable | ⇄ | same |
| 6–255 | reserved | | | unknown channel → strike |

On streams, all blocks for a client in one tick go into **one** Data packet. On datagrams, reliable blocks travel on the reliable stream and unreliable blocks in datagrams no larger than `max_unreliable`, split at entity granularity.

### 6.1 Handshake (`protocol/handshake.rs`)

```rust
pub struct Hello { magic: [u8;4] /* b"S3DN" */, protocol: u16, game_id: String /* ≤32 */, game_version: String /* ≤64 */,
                   schema_hash: u64, auth: HelloAuth, caps: ClientCaps, client_time_us: u64 }
pub enum HelloAuth { Ticket(String /* ≤1024 */), Resume(Vec<u8> /* ResumeToken::encode */), Dev { name: String /* ≤32 */ } }
pub struct Welcome { client: NetClientId, player: PlayerId, room: RoomId, room_epoch: u32, tick_hz: u16, send_interval: u8,
                     server_tick: Tick, resume: Vec<u8>, resume_window_s: u16, limits: WireLimits,
                     down_bytes_per_sec: u32, resumed: bool, echo_client_time_us: u64 }
pub enum RejectCode { VersionMismatch=1, SchemaMismatch, WrongGame, BadTicket, TicketExpired, TicketReused, WrongServer,
                      Banned, RoomFull, RoomNotFound, RoomClosed, ResumeExpired, ServerDraining, RateLimited,
                      DevAuthDisabled, Malformed, Internal }
pub struct Reject { code: RejectCode, message: String /* ≤128 */, retry_after_ms: Option<u32> }
pub struct Disconnect { code: DisconnectCode, message: String, resume_allowed: bool }
pub struct ResumeToken { room: RoomId, client: NetClientId, epoch: u32, secret: [u8;16] }   // fixed 30-byte encoding
```

**Schema hash:** FNV-1a 64 over, in order:
- `PROTOCOL_VERSION`;
- each component `name|codec schema_id|mode|audience`, sorted by **explicit name** (never `type_name`);
- the input name;
- each message `name|lane|direction`.

It is computed when the registry freezes at the first `Startup`. A mismatch yields `Reject(SchemaMismatch)`, and `GET /admin/v1/schema` prints `manifest()` for diffing.

**Flow:** an unauthenticated link may send exactly one Hello (≤ 4 KiB) within `handshake_timeout` (5 s). Anything else closes it with an IP strike. After `Welcome` the client is `Syncing`: it sends 8 pings at 100 ms, then pings at 4 Hz, and becomes `Connected` on the first snapshot flagged `SYNC_COMPLETE`.

### 6.2 `WireLimits` (sent in `Welcome`; server defaults)

| limit | default | enforced |
|---|---|---|
| inbound packet (= WS message and frame cap) | 16 KiB | WS upgrade (close 1009) and `decode_packet` |
| blocks per packet | 64 | `decode_packet` |
| input runs per block / bytes per input | 32 / 256 | `InputWindow::decode` |
| default message cap | 1 KiB (per type: `MessageConfig::max_bytes`) | receive |
| input packets/s per link | 90 inputs + 30 other = 120/s, burst 60 (inputs never under 1.5 × tick rate) | `TokenBucket` |
| packets/s per link into the room inbox | 4 × `input_packets_per_sec` (burst a third); excess dropped at the gateway, sustained excess closes the link + IP strike | gatekeeper |
| messages/s per link | 30 (burst 60; per type `rate_limit`) | `TokenBucket` |
| input tick window | `[T, T + max_input_ahead(32)]` (older are late and dropped) | `InputBuffer` |
| handshakes per IP per minute / concurrent per IP | 20 / 8 | gatekeeper |
| outbound backlog per link | skip snapshots above 256 KiB; kick `SlowConsumer` after 5 s above 1 MiB | send |
| room inbox | 8192 messages (overflow: drop unreliable-only, close otherwise) | ingress |

**Strikes:** a decode error, unknown channel, out-of-window input, oversize message or failed `validate` each count as a strike. `strikes.max` (5) within 10 s leads to `Disconnect(ProtocolViolation)`. Rate-limit overflow (packets, bytes and messages, per type too) is dropped first and counted per 1 s window (`OverflowWindows`): drops in 3 consecutive windows followed by another drop, or more than 10 s worth of the packet allowance within one window, lead to `Disconnect(RateLimited)`. (A bucket refills continuously, so "no packet passed for 3 s" never happens under a flood; windows do.)

**Hostile input:** decoders never panic and never allocate beyond the packet size. This is enforced by proptest fuzzing of every reader.

### 6.3 Snapshot block (`protocol/snapshot.rs`)

```
Snapshot   := SnapshotHeader · varu32 entity_count · Entity* · [HAS_DESPAWNS] Despawns
SnapshotHeader := u8 flags (bit0 FULL_RESET · bit1 SYNC_COMPLETE · bit2 HAS_DESPAWNS)
                · varu32 input_ack_delta   // tick − newest input tick consumed for this client (u32::MAX = none)
                · i8 input_slack           // ticks early the input for `tick` arrived (i8::MIN = missing)
Entity     := varu32 net_id_delta (ascending; first absolute)
            · u8 op (bit0 SPAWN · bit1 HAS_REMOVED · bit2 CONTROLLED · bit3 OWNED · bit4 DEFERRED)
            · varu64 present_mask · [HAS_REMOVED] varu64 removed_mask
            · for each set bit ascending: varu32 len · codec bytes
Despawns   := varu32 n · n × varu32 (net_id_delta << 1 | out_of_scope)
```

**Invariant:** an entity written for a client includes **every** component whose `changed_tick` is newer than that client's ack tick for the entity (datagram records too are written against the acked baseline, never an unacked one, so each record supersedes every older in-flight one). Presence follows the same rule: `removed_mask` covers every component the client may hold (the newest acked record's, plus whatever a record still in flight may have inserted), and every allowed component the client may lack (missing from the acked record or from one still in flight) is written, so a client that applied a record holds exactly its present set whichever in-flight records it applied before (`server::send::ClientEntity::{maybe_present, surely_present}`). Loss and reordering are therefore harmless. `DEFERRED` marks a record whose changes the byte budget held back for at least one earlier snapshot (the client must not read the gap as rest; §9.4). Protocol version 2 added it. The client skips a record whose tick is at or below the entity's `last_applied_tick`. A client's own predicted entities are written in **every** snapshot with all their `Predict*` components, because reconciliation needs a confirmation even when nothing changed (for example when the server rejected a move).

**Example cost:** one moving player with the default `TransformCodec` (1 mm, 10-bit smallest-three, scale omitted) costs about 1 B id + 1 B op + 1 B mask + 1 B len + 12–14 B = **16–18 B**.

### 6.4 Input block (`protocol/input.rs`)

```
InputWindow := varu32 newest_tick · u8 run_count · Run* (newest → oldest) · varu32 view_delay_q8
Run         := varu16 repeat · varu32 len · postcard(I)
```

- Identical consecutive inputs collapse into one run.
- On streams the window holds only the newest input.
- On datagrams it holds every input the server has not acknowledged (from `input_ack_delta`), capped at `ClientConfig::input_redundancy` (8).
- `view_delay_q8` is the client's interpolation delay in 1/256 ticks, used for lag compensation (phase 2).

---

## 7. Authoritative simulation

### 7.1 Room composition (`gs/room.rs::build_room_app`, on the shard thread)

1. `App::new().with_tickrate(tick_hz)`.
2. Insert resources: `NetInbox`, `SecretSource::from_fn(getrandom)`, `NetClockHandle`, `Metrics`, `RoomContext`, `RoomControl`.
3. `game.protocol(app)`, the shared protocol plugin (the same one the client adds).
4. `ServerReplicationPlugin { config: game.room_net_config(spec, base) }`.
5. `RoomPlugin`.
6. `game.build_room(app, spec)` for server rules and level spawning.
7. If restoring: `full_state::restore` plus server-only scene restore, at the first epoch of the next placement (`gs::room::restored_epoch`), and the tick continues from the checkpoint.

**One `app.update(step)` is exactly one fixed tick**, the same as `step_ticks(1)`. The room tick is `NetServer::tick()`, owned by the plugin, not `Time.tick`, so restored rooms continue their numbering.

### 7.2 Shared simulation: `SimSchedule` (`sim.rs`)

The core schedule runs systems in registration order, and a system cannot re-run `FixedUpdate`. So predicted gameplay is registered once with `app.add_sim_system(name, system)` and executed by the replication drivers:
- **Server:** once per tick inside `replication::server_tick`, after inputs are written.
- **Client:** once per predicted tick, plus once per replayed tick (`SimTime::resimulating = true`).

`run_sim(ctx, SimTime{tick, dt, role, resimulating})` inserts `SimTime`, pins `Time::fixed_delta` to the nominal step (the client dilates `TickRate::hz`), runs the systems in order, and restores `fixed_delta`.

Rules for sim systems:
- Query `With<&Simulated>`. On the server every simulated entity has it; on a client only its predicted entities do, so interpolated remote entities are never stepped.
- Read `SimTime`, not `Time`.
- Read inputs from `PlayerInput<I>`, never from `Events`.
- Guard cosmetic effects with `!resimulating`.
- `space3d_physics::physics_step` may be a server sim system. Clients predict with kinematic controllers only (no rapier rollback in v1).

### 7.3 Inputs and validation

- **`NetInput`** (`Serialize + DeserializeOwned + Clone + Default + PartialEq`) carries **state, not edges**: axes, absolute view angles, a held-button mask, and wrapping counters for discrete actions. Redundancy and repetition are therefore idempotent. Games latch per-frame edges inside their `InputSampler`.
- **`sanitize(&mut self) -> Result<(), InvalidInput>`** clamps NaN, ranges and unknown bits. `Err` counts as a strike and the neutral input is used.
- **Missing inputs:** for tick T the server uses `buffer[T]`. If it is missing, the server uses `last.repeat()` (counted as missing). While the player is disconnected it uses `NetInput::neutral()`. The input for such a tick that arrives afterwards is counted once as *late* (`space3d_input_total{result="late"}`, `NetStats::input_late`); redundant copies of on-time inputs are not.
- **Client controls:** `NetClient::set_rate` (`ControlMsg::SetRate`, clamped to `[4096, client_bandwidth_max]`) and `NetClient::set_interest_hint` (`ControlMsg::InterestHint`, an extra interest centre such as a free camera, radius ≤ 1000 m).
- **Slack:** `input_slack` is the EWMA of (arrival tick − T) and is reported in every snapshot.
- **Beyond the wire:** clients send intent only. Cooldowns, ammo and reach are simulated server-side, and commands go through `NetMessage::validate` plus per-type size and rate limits.
- **Server-side bots:** `NetServer::add_virtual_client(identity, brain(|world, tick| input))` fills a slot with no link, for backfill and load.

### 7.4 Join, leave, reconnect, resume

| state | behaviour |
|---|---|
| `Syncing` → `Connected` | on `SYNC_COMPLETE` |
| link lost / idle 10 s | `Disconnected{until = now + resume_window(20 s)}`; entity kept; neutral input; `PlayerDisconnected` event |
| `Hello{Resume(token)}` within the window, epoch matches, secret `ct_eq` | same slot and entities; epoch + 1; fresh token in `Welcome{resumed: true}`; next snapshot `FULL_RESET` (the client reuses entities for `NetId`s it already maps, so nothing pops); `PlayerResumed` |
| window expires | `PlayerLeft{Timeout}`: the game despawns the pawn or hands it to AI |
| same `PlayerId` joins with a fresh ticket | old link closed with `Superseded`; slot rebound |
| `ControlMsg::Leave` / kick / room close | `PlayerLeft{Left / Kicked / RoomClosed}` |

**Resume tokens** are room-issued, single-use, and **process-local** by design. After a server loss, a failed resume (`ResumeExpired` or a connect error) makes the client call `POST /v1/sessions/rejoin`. That returns a ticket for the restored room. The client stops resuming once the resume window (`Welcome::resume_window_s`, whole seconds rounded up, counted from the loss) is over — its last attempt starts just before it ends — and fails with `FailReason::Unreachable`: a slot kept no longer cannot be resumed, and a lost server's resume state died with it. (It used to retry for its whole reconnect schedule, 10 attempts over ~56 s, although every attempt was refused at once.)

**Resume secrets** come from `SecretSource`: `getrandom` in the server, seeded in the harness.

### 7.5 Lag compensation (phase 2, `server/lagcomp.rs`)

- Hitboxes of `LagCompensated` entities are recorded each tick in PostUpdate into a ring covering 200 ms.
- `LagCompensation::raycast(shooter, tick, origin, dir, max, exclude)` rewinds to `tick − view_delay(shooter)`, clamped to that 200 ms, and interpolates between the two bracketing ticks exactly as the client rendered them.
- The wire field (`view_delay_q8`) ships in v1.

---

## 8. Replication

### 8.1 Registration (`registry.rs`, `lib.rs`)

```rust
app.add_plugin(ReplicationPlugin::default())        // core.transform (TransformCodec, PredictAndInterpolate), core.name, core.parent (mapped)
   .replicate::<Health>("arena.health")                                    // postcard, Apply
   .replicate_with::<Ammo>("arena.ammo", ComponentRule::serde().audience(Audience::Owner))
   .replicate_with::<Target>("arena.target", ComponentRule::mapped(to_wire, from_wire))
   .add_input::<Move>("arena.move")
   .add_message::<Chat>("arena.chat", MessageConfig::reliable(Direction::ServerToClient))
   .add_message::<Say>("arena.say", MessageConfig::reliable(Direction::ClientToServer).rate_limit(2, 5).max_bytes(512))
   .add_sim_system("arena::walk", walk);
```

**`ComponentRule<T>`** is a builder over the codec, `ClientMode`, `Audience`, `SendRule`, priority, `quantize_authoritative` and `rollback_when`:
- `ClientMode`: `Apply | Delayed | Interpolate | Predict | PredictAndInterpolate`.
- `Audience`: `All | Owner | ExceptOwner | Team | Custom(fn)`.
- `SendRule`: `OnChange | Always | Interval(n)`.

**`NetCodec<T>`** has `encode(&T, &mut Writer, &EncodeCtx)`, `decode(&mut Reader, &mut DecodeCtx)` and `schema_id()`. Encoding must be deterministic.

**Freezing:** the registry freezes at the first `Startup`, assigning kinds by **sorted name** so plugin order never matters. Registering afterwards panics, and at most 64 components are allowed. The input type is type-erased inside the registry, so `ServerReplicationPlugin` and `ClientReplicationPlugin` are not generic.

**`space3d_scene::ComponentRegistry` is not used on the wire** because it goes through `serde_json::Value`. It is used for server-only checkpoint state.

### 8.2 Identity (`mapping.rs`)

- **`NetId(u32)`** comes from a per-room counter and is **never reused within an epoch**; varints keep it at 1–3 B.
- **Server:** `NetIdMap` (`Entity ↔ NetId`). **Client:** `EntityMap` (`NetId ↔ Entity`), plus a `NetEntity` component.
- **Entity references** encode through `EncodeCtx::net_id` and decode through `DecodeCtx::entity`. The client keeps the **newest value of every component that holds references** (`PendingRefs`; a `DecodeCtx::tracking` records which `NetId`s the decoder asked for, so any codec is covered, `MappedCodec` or hand-written) and decodes it again whenever a snapshot spawns one of those entities:
  - a value whose target is unknown is **parked**, however long the target takes (a target outside the client's scope is simply not there yet); it is dropped only by a newer value, a removal of the component, the despawn of the holder, or (with a warning) because its decoder asked for no entity at all, which nothing can ever resolve;
  - an applied value is **bound** to the targets it resolved. When a target leaves the client's scope its local entity is despawned and the component still names it; when the target returns as a new local entity, the value is decoded again and written (the value does not change on the server, so nothing is resent). Buffered interpolation / delayed samples of that component are dropped first, and a predicted component of a predicting client is left to the server's per-snapshot confirmation;
  - `NetClient::reference_stats` counts both kinds (bounded by holders in view; nothing outlives its holder). Cost: parked and bound values share one `(target, holder, kind)` index (a parked value is indexed by the ids its decoder asked for, which stops at the first unknown entity), so a snapshot that spawns entities decodes exactly the kept values that asked for one of them, once each; applying a newer value and despawning a holder touch only that holder's values. Nothing is proportional to the number of values kept (before the index every spawn-carrying snapshot decoded every parked value again, and applying a value scanned them all), and no cap is needed: the memory bound is holders in view times reference components.
  - *Why on the client, not a server resend when the target's scope epoch changes* (the earlier plan): the client is the only party that knows what it can resolve. A server-side resend needs, per client and holder, a model of which stay of each target the client's copy was decoded against; under datagram loss the ack of the resent holder record says nothing about whether the target's spawn had arrived, so the resend would have to wait for the target's acked spawn and repeat on every flap, costs bytes per flap, and does not cover a target that arrives later than the parking window. The client-side rule depends only on applied state (already loss- and reorder-safe through `last_applied` and the despawn tombstones), changes no wire format and works against old servers.
- **Hierarchy:** children inherit their root's interest, so `Parent` never dangles.
- **`StableId`** (scene) remains the persistent identity in checkpoints.

### 8.3 Change detection: encode once (`server/change.rs`)

On send ticks (`tick % send_interval == 0`), `server_collect` runs in `Stage::Render`, after all game PostUpdate systems and transform propagation:
1. Iterate the `Replicated` entities. Assign `NetId`s to new ones. Entities that are gone (or lost `Replicated`) become despawn tombstones.
2. Encode each present component with its codec, compare with the cached bytes, and update `changed_tick` on a difference.
3. A presence difference sets `structure_tick` and the `removed` mask.

The result is an `EntityRecord { entity, net_id, present, comps: [CompSlot{kind, changed_tick, bytes}], owner, controller, team, pos, priority }`.

The bytes are copied into every recipient's packet, so there is no per-client re-encoding. Comparing quantized bytes suppresses sub-quantum jitter. `SendRule::Interval(n)` skips encoding between intervals.

**`quantize_authoritative`** (on by default for `Predict*` modes) writes the decoded value back into the server component every tick. The client does the same after each predicted tick. Both sides then simulate identical values, and mispredictions are exact byte differences.

### 8.4 Per-client state, baselines and acks (`server/send.rs`)

`ClientEntity { spawned, ack_tick, last_sent_tick, acc, misses, leaving_since }` is tracked for each visible entity. A component is due for a client when `changed_tick > ack_tick` and the audience allows it. **All due components of an entity ship together**, which is why one baseline tick per entity is correct.

- **Streams (WS): optimistic.** The baseline advances when the packet is handed to the link. If the link dies, resume sends `FULL_RESET` anyway.
- **Datagrams: acked.** A `SentRecord{seq, tick, entities, despawns, lost}` ring (256 entries) is kept, and an ack raises `ack_tick`. Records are written against `ack_tick` (in-flight changes are repeated until acked), so a reordered older record never carries something a newer one lacks. A packet unacked after the RTO (1 s before the first RTT sample, RFC 6298; the client's reported RTT seeds it) is presumed lost: an unacked spawn may be resent (`last_sent_tick = ack_tick`), and the record stays in the ring marked `lost`, so a late ack still credits it and samples the RTT (a long path raises the RTO instead of looking like permanent loss).
- **Spawns and despawns are state.** A spawn record repeats until acked (datagrams) or is sent once (streams). On datagrams a despawn follows every entity that was ever put on the wire, including one whose spawn was presumed lost (its datagram may only be late: without the despawn the late spawn created a ghost, since the client had no tombstone). Despawn tombstones persist until every client has acked them or they age past the resume window. The client keeps its own tombstones (`NetId` → despawn tick, 30 s, cleared on a new epoch): a spawn record older than a despawn it already applied (a reordered or duplicated datagram, or a despawn that overtook its spawn) is ignored, so a short-lived entity never comes back as a ghost; `NetId`s are never reused within an epoch, so re-entering scope (a newer spawn) still applies.
- **Stays in scope fence acks (datagrams).** Every entry into a client's view starts a new stay (`ClientEntity::scope`, from `ClientView::next_scope`); records and despawns carry the stay they were written in, and acks and loss detection only apply to the same stay. (A late ack of a spawn sent before the entity left and came back used to mark the new stay's spawn delivered although it was lost — the client never saw the entity again; likewise a late ack of an earlier despawn confirmed a later, lost one and left a ghost.)

### 8.5 Interest (`server/interest.rs`)

- Rooms are the hard partition.
- Inside a room the policy is `InterestPolicy::RoomWide` (the default) or `Spatial{cell, radius, hysteresis: 1.2}`. `Spatial` queries a 2D (XZ) `SpatialGrid` around each client's `InterestSource`s. It is updated incrementally and re-evaluated every `interest_every` send ticks.
- **Precedence:** own entities > `NetServer::set_visibility` override > `NetRelevance` (`Global | Spatial{radius} | OwnerOnly | Manual`) > `VisibilityRule`s (fog of war) > spatial.
- Entering scope sends a full spawn. Leaving (past `radius × hysteresis`) sends `Despawn(out_of_scope)`, and the client fires `RemoteDespawned{out_of_scope: true}`.

### 8.6 Priority, budget and flow control (`server/priority.rs`, `send.rs`)

Per client, per send tick:

1. **Flow control.** If `LinkTx::backlog_bytes() > backlog_skip_bytes` (256 KiB), skip this client's snapshot and halve its snapshot rate until the backlog drains. Above `backlog_kick_bytes` (1 MiB) for 5 s, send `Disconnect(SlowConsumer, resume_allowed)`. **This is what prevents TCP bufferbloat.**
2. **Budget.** `ByteBudget` is a token bucket at `client_bandwidth` (48 KiB/s default, clamped by `ClientCaps` and `SetRate`), with a 2× burst, and 4× until `SYNC_COMPLETE`.
3. **Always first:** despawns, removals, control, reliable messages, pongs, and the client's controlled (predicted) entities. The client's other own entities compete for the budget with a 4× priority boost (an RTS player owning hundreds of units stays within budget and still sees everything else).
4. **Accumulate.** Each other relevant entity with due changes gets `acc += rule_priority × NetPriority × 1/(1 + d/ref) × Δticks`.
5. **Write.** Sort by `acc`, and write while the budget allows, trying up to 8 smaller entries after the first miss. Written entities reset `acc` to 0 and unwritten ones keep growing. Nothing starves: a full bucket lets the top entry through even when it does not fit (the budget goes into debt, capped at one burst), a top entry deferred 8 snapshots in a row goes out whatever the bucket holds (so a controlled set larger than the budget cannot shut everything else out), and a top entry deferred more than 3 snapshots in a row holds lower priorities back until it goes. While the bucket is in debt, the controlled entities' *unchanged* predicted components are re-confirmed every 4 snapshots instead of every snapshot (changes still go every snapshot); controlled entities that change every tick are always sent, so a controlled set larger than the budget exceeds `client_bandwidth` by design. Also, so an entity larger than a snapshot's budget (or the whole burst) is still delivered and never blocks `SYNC_COMPLETE`. Stream packets are cut at 16 KiB; a single larger entity record goes alone up to the link's cap (1 MiB server → client), and a record over the cap is dropped with an error and **not** marked delivered.
6. **Stats.** `NetStats::{budget_utilization, starved_entities}` feed metrics and tests.

### 8.7 Messages / RPC (`messages.rs`)

- **Server → client.** `NetServer::send(Target, &M)`, where `Target` is `All | Client | Player | AllExcept | Players | ObserversOf(entity) | Team`. The message is encoded once and arrives as `Events<FromServer<M>>{tick, message}`. `EventTiming::Interpolated` holds it until the client's render tick reaches `tick`, so an explosion lines up with interpolated poses.
- **Client → server.** `NetClient::send(&M)` passes the size cap, rate limit and `NetMessage::validate`, then arrives as `Events<FromClient<M>>{client, player, tick, message}`. Read these in `Update`, not in the sim.
- **Ordering.** On streams, messages and snapshots share one ordered stream, so a message referencing a `NetEntity` decodes after its spawn. On datagrams, unresolved references wait up to 1 s.

---

## 9. Client side (wasm32-compatible; `client/`)

```rust
app.add_plugin(ArenaProtocol)
   .add_plugin(ClientReplicationPlugin { config: ClientConfig { game_id: "arena".into(), ..Default::default() } })
   .set_input_sampler(|ctx| sample_move(ctx));
app.resources.expect_mut::<NetClient>().connect(ConnectTarget::Ticket { url, ticket });   // or Dev { url, name }
```

**`NetClient` API:**
- `connect`, `disconnect`, `state() -> &ConnectionState`, `is_connected`;
- `client_id`, `player_id`;
- `send<M>`;
- `entity(NetId)`, `net_id(Entity)`;
- `server_tick_estimate`, `predicted_tick`, `render_tick`, `stats`.

**Events:**
- `ConnectionEvent`: `Connecting | Connected{client, player, room, resumed} | Synced | Reconnecting{attempt} | Rejected(Reject) | Disconnected{code, reason, will_reconnect} | Notice`.
- `RemoteDespawned{net_id, entity, out_of_scope}`, fired before the despawn.
- `Mispredicted{tick, replayed}`.

**Markers on replicated entities:** `Remote`, `Predicted`, `Interpolated`, `Controlled`, `Simulated` (predicted only), `NetEntity`, and `PredictionError` (render-only correction).

### 9.1 Connection state machine (`client/connection.rs`)

`Idle → Connecting{attempt} → Handshaking → Syncing → Connected`, with `Reconnecting{attempt}` and `Failed(FailReason)`.

- **Every attempt opens a fresh transport.** Reconnects use `ClientConfig::reconnect` (`space3d_net::ReconnectPolicy`: 250 ms → 10 s, 10 attempts) and try `HelloAuth::Resume` when a token is held.
- **Failed resume.** `ResumeExpired` falls back to the original credentials only for `Dev`, since tickets are single-use. Otherwise the client moves to `Failed`, and the game asks the PS for `/v1/sessions/rejoin`.
- **`Disconnect{resume_allowed: false}`** moves the client to `Failed(Disconnected)`.

### 9.2 Time sync and tick pacing (`time.rs`)

- **`RttEstimator`:** RFC 6298 (α = 1/8, β = 1/4) with a windowed minimum. `rtt = now − echo − hold`.
- **`ServerClock`** keeps the last 16 pong samples, `server_tick + tick_frac + (rtt/2)·hz`. It uses the median of the lower-RTT half, slews at most 1 tick/s, and snaps when the error exceeds 5 ticks.
- **Path RTT and jitter for pacing and interpolation** come from the `ServerClock` window, not from the RFC 6298 estimator: `path_rtt` = median RTT of the lowest-RTT half, `jitter` = median change between consecutive samples / √2. A head-of-line stall on a stream adds a burst of high samples that these ignore, where `srtt` jumps several-fold (it would pull the render clock back and fight the pacer). A lasting RTT rise is followed early: once the last 3 samples have all stayed above `path_rtt` by more than 3·jitter (at least 5 ms) for two ping intervals plus one path RTT — longer than a stall's burst lasts before the first fresh sample — their lowest is used (≈ 1.25 s after the change instead of ≈ 3 s). The consecutive-sample jitter counts a level shift once, where the spread around the window's median counted it as jitter for as long as the window straddled it (together with the stale path RTT that made the clock-based resync jump forward and back after a step, and inputs went missing again ~3 s later). `RttEstimator` still drives RTOs and `NetStats::rtt`.
- **Predicted tick target:** `est_server_tick + (path_rtt/2)·hz + target_slack`.
- **`TickPacer`** is a PI controller on the server-reported `input_slack` relative to `target_slack_ticks` (1.5, plus 2·jitter, raised after a head-of-line stall is detected).
  - `PaceAction::Scale(s)` with `s ∈ [0.95, 1.05]` sets `TickRate::hz = nominal × s`.
  - `PaceAction::Resync(n)` fires when the error exceeds 8 ticks: jump the tick and clear history.
  - *Implementation notes (replication builder):* the jitter term `2·jitter` is capped at 6 ticks (`PacerConfig::max_jitter_margin_ticks`), because downstream queueing also inflates the RTT variance; a missing input (`i8::MIN`) only speeds up and raises the stall boost, never jumps; after a resync to tick T, slack feedback is ignored until the server has consumed an input for T or later (its EWMA still describes the old timeline). A backward resync on feedback (inputs arrive too early: the uplink got faster) waits longer — until the server consumed `SLACK_SETTLE_TICKS` (24) inputs of the new timeline, counted from the one after the newest input already sent (the client sends nothing new before its prediction passes it), because the server's slack EWMA (α = 0.1) needs that many samples to forget the old timeline — and holds the clock-based resync (`|target − next| > 1.5 × 8` ticks, meant for stalls, hitches and clock jumps) for 8 ping intervals (≥ 2 s) unless the server reports a missing input meanwhile: the clock's path RTT and offset lag a path change by ≈ 1.25 s. (Without both, a large drop of the uplink latency made the stale feedback pull back again and again while the clock jumped forward to the old target: `[-7, +6, -9, +11, -9, -5, +6, -4]`, then missing and late inputs.) A forward resync on feedback (late inputs, typically after a stall) gets neither: there the clock is right and may undo it at once. Ticks predicted again after a backward resync keep the inputs already sent for them (the server simulates those) instead of sampling new ones that would never be sent. The server drops inputs beyond `T + max_input_ahead` without a strike (the slack is still reported so the client pulls back); only ticks beyond `4 × max_input_ahead` strike.
  - `SimTime.dt` and `Time::fixed_delta` inside `run_sim` stay nominal, so **no `space3d-core` change is needed**.

### 9.3 Prediction and reconciliation (`client/prediction.rs`)

**Each predicted tick T:**
1. If `rollback_from = S` is set:
   1. Write the confirmed values at S.
   2. For t in S+1..T: apply `InputHistory[t]`, `run_sim(resimulating)`, and rewrite the history.
   3. Store `PredictionError += old_visual − new_visual`.
   4. Emit `Mispredicted`.
   - If `T − S > max_rollback_ticks` (32), or S is older than the history, snap instead of replaying.
2. `i = InputSampler::sample(ctx)`, stored in `InputHistory[T]` and written as `PlayerInput<I>` on `Controlled` entities.
3. `run_sim(T)`, then quantize the predicted components and record `PredictionHistory[T]` (encoded bytes).
4. Queue the INPUT window.

**When a snapshot for S arrives:** for each `Predicted` entity, compare the confirmed bytes with `history[S]` (or apply `rollback_when`). On a difference, set `rollback_from = min(rollback_from, S)`.

**Replay start (integration pass):** only the newest confirmed values are kept, so the replay always starts from the newest confirmed tick, whatever older tick first disagreed. When several snapshots arrive together after a stall, an older mismatch is superseded by the newest confirmation; if the prediction for that newest tick was right, nothing is replayed at all. (Starting at the oldest mismatch wrote the newest values and then re-applied the inputs in between: a wrong state and a second correction, or a snap when the span exceeded `max_rollback_ticks`.) Ticks with no recorded input (skipped by a forward resync) replay the last earlier input `repeat()`ed, exactly as the server filled them, and a resync keeps the input history, so a resync no longer forces a snap.

**Behind the confirmed state:** when the newest confirmation S is at or past the next predicted tick T (a frame hitch on a low-latency link), the confirmed state already includes ticks T..=S: the timeline fast-forwards to S + 1 (a forward resync) instead of simulating those ticks again on top of it.

**Resyncs rebuild:** a resync (the clock-based hard resync or `PaceAction::Resync`) moves the next predicted tick while the world still holds the old timeline's state, so the next `client_tick` always rebuilds the state before the new tick from the newest confirmation (replaying recorded inputs, repeating the last one over skipped ticks), even when the prediction for that confirmation was right. (Skipping the rebuild left the pawn behind by the skipped ticks after a forward jump and applied inputs a second time after a backward one.) The clock-based resync also drops the fixed-step backlog of the long frame that caused it (`Time::discard_backlog`, keeping one step): the jump already covers that time, and running up to `max_steps_per_frame` catch-up steps on top of it overshot the lead by as many ticks and forced a later backward resync.

**Snaps:** when a rollback cannot be replayed (longer than `max_rollback_ticks`, or inputs missing after a resync), the confirmed values are written and the recorded history before the current predicted tick T is dropped; confirmations older than T then only update the confirmed values, so one snap is followed by at most one ordinary replay. While the prediction lead itself exceeds `max_rollback_ticks` every correction snaps, so keep it above the expected lead (default 32 ticks ≈ 530 ms).

**Visuals:** `PredictionError` decays with `smoothing_half_life` (60 ms), and errors above `snap_distance` (2 m) snap instead. It is applied to the rendered transform only and never simulated.

**Predicted spawns** (a projectile keyed by `(tick, counter)` and matched on the server spawn) are phase 3.

### 9.4 Interpolation and jitter buffer (`client/interpolation.rs`)

- **`InterpClock`:** `render_tick = (server_estimate − (path_rtt/2)·hz) − delay`, i.e. the delay trails the newest tick a snapshot can have arrived for, not the server's current tick. The delay is `clamp(send_interval + jitter_k·jitter + ½ tick, 35 ms, 250 ms)` (stall-resistant `jitter`, see §9.2), slewed by ±5 % playback speed. The render tick never runs backwards: more than 10 ticks ahead of the target it slows down (to half speed at most); more than 10 ticks behind it snaps forward.
- **`SnapshotBuffer<T>`** holds 32 samples per `Interpolate`/`PredictAndInterpolate` component of each non-predicted entity. `sample(render_tick, lerp, max_extrapolate)` holds or extrapolates for up to 100 ms, then freezes. Extrapolation only applies to entities present in the newest snapshot received: an entity missing from newer snapshots is unchanged (or deferred by the budget), so it holds its last value instead of overshooting a stop or a teleport.
- **Gap fix:** a sample arriving more than `send_interval` after the previous one gets a synthetic copy of the previous value inserted at the newest snapshot received in between — which did not carry the entity, so it was resting — and resting entities do not slide. A gap that no received snapshot explains (lost datagrams, snapshots the sender skipped) is interpolated across: holding there stepped a moving entity back at every lost snapshot. Records with the `DEFERRED` bit skip it (the gap was motion held back by the budget), and the entity's buffer grows a per-entity render `lag` (its observed update gap beyond the send interval, decaying once updates arrive at the nominal rate): budget-deferred movers are drawn further in the past but move smoothly instead of freezing and jumping.
- **`Delayed` components** and `Interpolated` events release when `render_tick` reaches their tick.

---

## 10. Per-tick data flow

### 10.1 Room (one frame = one tick; 60 Hz tick, 30 Hz snapshots)

```
PreUpdate
 1 tasks::drain_completed          AsyncQueue callbacks (player data loaded, checkpoint written)
 2 replication::server_receive     NetInbox.try_recv ≤ max_inbound_per_tick: Join → checks → Welcome/Reject,
                                   PlayerJoined/Resumed; Packet → limits → decode (strike on error) → acks,
                                   Pong queue, InputBuffer, Control, Events<FromClient<M>>; Closed → Disconnected
 3 room::commands                  RoomCommand (kick, broadcast, close, drain, checkpoint, joinable, inspect)
FixedUpdate
 4 replication::server_tick        tick += 1; PlayerInput<I> ← buffer/repeat/neutral; run_sim(Server);
                                   quantize-authoritative write-back; resume-window expiry → PlayerLeft
 5 game FixedUpdate systems        non-predicted rules (AI, spawners, scoring)
Update                             game logic (reads PlayerJoined / FromClient<M>, NetServer::send)
PostUpdate                         core::propagate_transforms; (phase 2) lag-comp record
Render
 6 replication::server_collect     send ticks: NetIds, tombstones, encode-once, change ticks
 7 replication::server_send        send ticks: per client flow control → interest → priority/budget →
                                   Data packet(s) → LinkTx::send (rayon above 32 clients)
Last
 8 room::ops                       RoomControl → outbox; 1 Hz RoomStatus; checkpoint cadence; empty TTL
 9 events swap (core)
```

### 10.2 Client (variable frame rate)

```
PreUpdate   replication::client_receive    connection machine, pings; decode: Pong → ServerClock; Snapshot →
                                           EntityMap spawn/despawn, Apply now, Interpolate/Delayed → buffers,
                                           Predict* → confirm (rollback_from); messages → Events / timed queue
            replication::client_pace       TickPacer → TickRate::hz (or Resync)
            game input systems             ActionState, edge latching
FixedUpdate replication::client_tick       (0..n per frame) rollback+replay once; sample; PlayerInput; run_sim;
                                           history; queue INPUT
Update      replication::client_interpolate  render tick; buffers → components; Delayed/timed events; correction
            game Update systems            camera, UI, cosmetics
PostUpdate  core::propagate_transforms; replication::client_send (one packet per frame)
Render      renderer
```

Game plugins go **after** `ServerReplicationPlugin` and `ClientReplicationPlugin`, because stage order is registration order.

---

## 11. Player service

### 11.1 Public REST API (`api.rs` DTOs; JSON; `Authorization: Bearer <access>`)

| route | body → response |
|---|---|
| `POST /v1/auth/guest` | `GuestLoginRequest{device_id, device_secret, display_name?}` → `SessionTokens` (creates on first use; per-IP limited) |
| `POST /v1/auth/register`, `/login` | `RegisterRequest` / `PasswordLoginRequest` → `SessionTokens` (feature `password-auth`; argon2id) |
| `POST /v1/auth/refresh`, `/logout` | `RefreshRequest` → `SessionTokens` / 204 (rotation; reuse revokes the family) |
| `GET/PATCH /v1/me` | → `Profile` / `UpdateProfileRequest` → `Profile` |
| `GET/PUT /v1/me/data/{key}` | → `PlayerDataEntry` / `PutPlayerDataRequest` (only `client_writable_keys`) |
| `POST /v1/matchmaking/tickets`, `GET …/{id}?wait=20`, `DELETE …/{id}` | `MatchmakeRequest` → `MatchmakeResponse`; → `TicketStatus` (`searching` / `found{assignment}` / `expired` / `cancelled`) |
| `POST /v1/play/quick` | `QuickPlayRequest` → `Assignment` (best open room with seats) |
| `POST /v1/sessions/rejoin` | → `Assignment` or 404 (after a server loss; restores the room if needed) |
| `POST /v1/lobbies`, `/v1/lobbies/join`, `GET /v1/lobbies/{id}?wait=20`, `POST …/{id}/ready\|start\|leave` | → `LobbyState` |
| `GET /v1/leaderboards/{board}?top=50`, `…/me` | → `Vec<LeaderboardRow>` / `Option<LeaderboardRow>` |
| `GET /v1/status` | → `ServiceStatus{protocol, version, motd, regions}` |

- Errors return `ApiError{code, message, retry_after_ms}`.
- The HTTP body limit is 64 KiB, CORS uses an allowlist, the request rate is limited per IP, and login attempts are limited per IP and per username.
- **Client:** `services::PlayerServiceClient` (on `space3d_net::Http`, native and web) exposes `login_guest`, `login_password`, `refresh`, `logout`, `me`, `matchmake`, `poll_ticket`, `cancel_ticket`, `quick_play`, `rejoin`, `create_lobby`, `join_lobby` and `leaderboard`. Each returns a pollable `Task`.
- **Device credentials:** `DeviceCredentials` is persisted with `space3d_db::KvStore` (localStorage on the web).

**Internal endpoints** (fleet token):
- `WS /internal/v1/fleet`;
- `PUT|GET /internal/v1/rooms/{id}/checkpoint` (≤ 16 MiB), `DELETE …/checkpoint?through_epoch=N` (deletes only a checkpoint of epoch ≤ N: a stale room copy closing never deletes the live copy's checkpoint; absent = any).

### 11.2 Fleet (`gs/fleet.rs`, `ps/fleet.rs`)

- **`FleetUp`:** `Register(ServerRegistration{token, server_id, region, public_url, game_id, build, protocol, schema_hash, capacity, modes})`, `Heartbeat{seq, players, tick_p99_us, draining, rooms: Vec<RoomStatus>}`, `RoomOpened`, `RoomClosed{restorable}`, `RoomMoving` (the room is about to move: drain handoff or restorable shutdown, §2.4; answered by `RoomMovingAck`), `Deregister`, `PlayerJoined`, `PlayerLeft`, `MatchResult{op_id}`, `PlayerDataWrite{op_id}`, `SubmitScore{op_id}`, `FetchPlayerData`.
- **`FleetDown`:** `Registered{lease_ttl_ms, bans}`, `PrepareRoom{spec}`, `CloseRoom`, `RoomSuperseded` (stale copy, below), `Reregister`, `Kick`, `Ban`, `Unban`, `Broadcast`, `Drain`, `Ack{op_id}`, `PlayerData`, `RoomMovingAck` (the PS holds the moving room's rejoins now).
- **Link close:** a stopping game server delivers what is still queued (its `Deregister`) and closes the fleet WebSocket with a closing handshake (`FleetLink::shutdown`, bounded, from the process's shutdown guard); the PS handles a link's last messages before its close (a `Deregister` that arrived in the same supervisor tick as the close used to be dropped as coming from an unregistered link) and logs the close of a server that left the fleet at INFO, any other at WARN. The game server sends `Deregister` in the same supervisor tick whose drain report lets the lifecycle stop, never later.
- **Seats (integration pass):** a room's used seats are `max(reported players, presence) + reservations bound to the room`, and a server's are `max(heartbeat players, presence) + reservations`. `PlayerJoined`/`PlayerLeft` presence is immediate while room status and heartbeats lag by up to 2 s, and a join releases its reservation, so counting only the heartbeat oversubscribed rooms during quick-play bursts (`Reject(RoomFull)`). `allocate_room` binds the seat it reserves to the new room; a resume of a present player is not a new seat.
- **Leases:** the heartbeat runs every 2 s and the lease lasts 10 s. When a lease expires the server is marked `Dead` and its rooms are orphaned (eligible for rejoin and restore).
- **Partitions (split brain):** a lease also expires when only the fleet link is cut: the game server keeps running its rooms (a PS outage must not end matches, §12), so a player who rejoins meanwhile gets the room restored on another server. The restored copy is authoritative (its epoch is newer and only its checkpoints are stored). The PS never lets a copy on another server take a room's entry back: a heartbeat or `RoomOpened` that reports a room whose entry now names another server gets `FleetDown::RoomSuperseded`, and that copy's presence events are ignored. The stale copy closes without uploading or deleting a checkpoint, its clients get `Disconnect(ServerShutdown)` ("room moved to another server; rejoin") and their rejoin brings them to the live copy; its progress since the partition is lost (the price of the lapsed lease). That includes its outbox: every result, score and player-data op carries its origin (room, epoch, tick), the live copy reports the point it was restored from (`RoomStatus::origin`), and the PS acknowledges without applying every op another copy made after that point (ops made before it are part of the live history and apply; until the live copy reports, such ops wait for the outbox's resend). The PS keeps this fence for 24 h after the move, beyond the room entry. Results are also idempotent per match (`op_id = "<room>:match:<match_id>"`), whichever server reports them. (Before, the stale copy's result, scores and progress were applied when the link healed: Elo twice, with opposite winners.) The fence tracks the latest move only, and lives in the PS's memory. A heartbeat from a server the PS already forgot (lease expired while the link stayed up) gets `FleetDown::Reregister`, answered with `Register` and an immediate heartbeat.
- **Allocation** filters on region, `game_id`, `schema_hash`, `Ready`, not draining, and enough free seats. It picks the lowest `players/capacity + tick_p99/budget` and reserves seats for the ticket TTL. Clients declare their build in quick play, matchmaking and lobby requests (`game_id`, `schema_hash`; empty / 0 = any; `PlayerServiceClient::with_build`); the matchmaker only matches players of one build and open rooms are offered only to their build. Each `RoomEntry` records the build of the server that created it, and a rejoin/restore allocates only on that build (its checkpoint's schema must load).

### 11.3 Matchmaking and lobbies

- **`Matchmaker` is pure** (`tick(now_ms) -> Vec<MatchFormed>`) and is driven by the supervisor every `matchmaking.interval` (250 ms).
- Tickets are matched oldest first, within a rating window that widens over time (`rating_window` + `rating_window_growth`/s) and a region window (`region_ping_ms`, widening to `region_ping_max_ms` after 30 s).
- After `max_wait` it starts short with at least `min_players`, or the ticket expires.
- `backfill` fills open rooms first, and failed allocation requeues players at the front.
- **Lobbies** use 6-character codes, and the host starts the game. **Ratings** use Elo with K = 32 and team averages (Glicko-2 deferred).
- Parties are deferred.

### 11.4 Storage (`ps/schema.rs`, `ps/store.rs`)

- **Tables:** `accounts`, `refresh_tokens` (hashed; `family`; `rotated`, `revoked`), `player_data` (JSON value, optimistic `version`), `applied_ops` (idempotency), `matches`, `match_players`, `room_checkpoints`, `bans`, `audit_log`. Leaderboards use `space3d_db::Leaderboard`.
- **Every game-server op** runs in one transaction: check `applied_ops`, apply, insert `op_id`. Retries are therefore exactly-once in effect.
- **Scale envelope:** SQLite in WAL mode on SSD handles thousands of small transactions per second, which covers tens of thousands of CCU. Beyond that, the path is a Postgres `PlayerStore`, a leader-elected matcher and leases in the DB (deferred).
- **Backups:** `POST /admin/v1/backup` runs `VACUUM INTO`.

---

## 12. Persistence and recovery

| data | where | how |
|---|---|---|
| accounts, profiles, player data, bans, leaderboards, match history | PS SQLite | per request / per op, transactional |
| player data written by rooms | `RoomControl::write_player_data` → outbox → `FleetUp::PlayerDataWrite{op_id}` → PS | at-least-once, idempotent |
| results / scores | `RoomControl::end_match` / `submit_score` → outbox | same |
| player data read by rooms | `RoomControl::load_player_data` → `FleetUp::FetchPlayerData` → `Events<PlayerDataLoaded>` | async; the slot plays meanwhile (small data such as rating travels in the ticket) |
| room checkpoints (`FullState` keeps `NetId`s and the room-side markers `ControlledBy`, `Owner`, `NetTeam`, `NetPriority`, `NetRelevance`, `InterestSource`, `Simulated`) | `Checkpoint{version, room_id, epoch, saved_ms, spec, state: FullState, server_only, game_blob}` (format 2: `epoch` + `saved_ms` form a header readable without decoding the state) → `CheckpointStore` (`player-service` default, `local`, `none`) | every 10 s, on phase change, drain, request; serialization on the compute pool; ≤ 1 upload in flight per room; a room's pending upload is abandoned when it is dropped or rebuilt, and a closed room's checkpoint is deleted after its uploads in flight settle — including those its earlier incarnations started before a crash rebuild replaced them — (also when the crash-loop guard or a failed rebuild closes it: that state must not come back through a join ticket that is still valid). A delete only removes a checkpoint of the deleting copy's epoch or older (`CheckpointStore::delete(room, through_epoch)`), so a stale copy that closes normally behind a fleet partition leaves the live copy's checkpoint alone. Rooms that end any other way (orphans nobody rejoins, abandoned rooms, handed-off or shut-down rooms nobody comes back to) keep theirs until garbage collection: once a minute the PS store, and the GS's local and rescue stores, delete checkpoints captured more than `recovery_max_age` + 60 s ago (they can no longer be restored). **Every store keeps the checkpoint with the greatest `(epoch, saved_ms)` of the blob** (never the upload time) and refuses older ones, so a slow upload, a previous incarnation's upload (crash rebuild, restore elsewhere) or a draining server's late one never replaces newer state |

**Failure matrix:**

| failure | detection | recovery |
|---|---|---|
| client network blip | close / idle 10 s | resume within 20 s: same slot, `FULL_RESET`, no pop-in |
| room panic | `catch_unwind` (`--profile server`) | rebuild from the last checkpoint at the next epoch of its placement (`gs::room::rebuilt_epoch`), `Disconnect(RoomCrashed)` (clients rejoin); the rebuilt room continues the drain it was in (joinability, deadline, handoff, move notice, pending close: `gs::room::{take_for_rebuild, adopt_carried}`); crash-loop guard (3 in 5 min → close; `gs::shard::recover_crashed`, shared by the shards and `TestCluster`) |
| shard stall | heartbeat older than 5 s → `/healthz` 503 | orchestrator restart → server-loss path |
| GS crash or node loss | PS lease expiry (10 s) | rooms orphaned; clients' resume fails → `POST /v1/sessions/rejoin` → PS allocates a new GS of the room's build with `restore_from` checkpoint (no server with room: `503 no_capacity` with `retry_after_ms`). Clients stop resuming when the resume window (`Welcome::resume_window_s`) is over, not after the whole reconnect schedule; the example client asks the PS every 2 s while a reconnect keeps failing and follows the room to its new server, about `lease_ttl` + 2 s after the loss. A quick play into the mode of a lost persistent room restores it the same way. `RecoveryPolicy::Abandon` (reported in `RoomStatus::abandon_on_loss`): the PS voids the room (audit `match_void`, `space3d_matches_void_total`) and rejoin answers 404, so clients queue again |
| PS unavailable | fleet link down | rooms keep running; outbox buffers; new logins and matchmaking unavailable; GS `/readyz` stays ready (registration latch; `space3d_fleet_connected` = 0) |
| GS ↔ PS partition longer than the lease (PS up) | PS lease expiry | the GS keeps its rooms running; a rejoin meanwhile restores a room on another server (the authoritative copy from then on). When the GS is heard from again, the PS sends `RoomSuperseded` for every room restored elsewhere: that copy closes (`Disconnect(ServerShutdown)`, no checkpoint written or deleted) and its players rejoin the live copy, losing what they did in the stale one. A forgotten GS is asked to `Reregister` |
| second SIGTERM / SIGINT | lifecycle | rooms checkpointed into the GS's local DB, clients `Disconnect(ServerShutdown)`, exit 1; the next start uploads the checkpoints |
| PS crash | restart; WAL durability | fleet state rebuilt from `Register`s on reconnect; clients re-queue on 404 |

---

## 13. Security

- **Tokens** (`security/token.rs`) have the form `s3.<kind>.<kid>.<b64url(postcard(claims))>.<b64url(mac)>`.
  - The MAC is HMAC-SHA256 with `K_kind = HMAC(master[kid], "space3d/"+kind)`, so a Session token can never pass as a Join ticket.
  - Verification uses constant-time `verify_slice`, with a 5 s leeway on `exp`/`iat`.
  - `KeyRing` verifies any listed `kid` and signs with `signing_kid`. Rotation: add k2 everywhere, flip the signing key on the PS, remove k1 after the longest token TTL.
  - Randomness (keys, `jti`, refresh tokens, session ids, resume secrets) comes from `getrandom 0.4.3`.
- **Join tickets** live 60 s, are single-use (`NonceCache` of `jti` until expiry), and are bound to `server_id` and `room`. They travel **in the Hello, never in the URL**.
- **Sessions:** the access token is stateless and lives 1 h. Revocation is checked through a 30 s cache. Refresh tokens are opaque, stored hashed, rotated on each use, and reuse revokes the whole family.
- **Passwords:** argon2id (`argon2 0.6.0`, default parameters) on `spawn_compute`.
- **Edge:**
  - `WsServerConfig`: 16 KiB messages and frames, bounded inbox, outbound byte cap, `max_connections`, text rejected, and an `accept` hook for per-IP concurrency and rate, IP/CIDR bans and the browser `Origin` allowlist.
  - `X-Forwarded-For` is honoured only from `trusted_proxies`.
  - Handshake deadline of 5 s.
  - Per-address limits and strikes key IPv4 addresses as is and IPv6 addresses by /64; `KeyedLimiter` evicts least recently used keys in O(log n).
  - Admitted links have a per-link packet budget into the shared room inbox (dropped at the gateway; sustained flooding closes the link), so one client cannot fill the inbox of everyone in its room. A resume refused as busy carries a retry delay and the client retries it.
  - IP/CIDR bans added at runtime also close links already open from the banned range (`Disconnect(Banned)`).
- **Join ticket nonces:** the cache never evicts an unexpired `jti` (that ticket could be replayed); when it is full of live nonces new tickets are refused with a retryable `RateLimited`.
  - Every HTTP listener (game port, PS, admin) runs its own accept loop (`space3d_net::ConnectionLimits`): a header-read timeout (`limits.http_header_timeout`, 10 s; also closes idle keep-alive connections), and caps on connections that have not completed a request or a WebSocket upgrade, per client IP (`limits.http_connections_per_ip`, 64; IPv6 per /64; trusted proxies exempt) and in total (`limits.http_max_connections`), refused at accept time. The PS reads request bodies within `limits.http_body_timeout` (10 s → 408).
  - `X-Forwarded-For`: every header line counts, joined in order (RFC 9110), before the right-most-untrusted rule, so a proxy that adds its own line cannot leave the client's line in charge.
- **Runtime:** per-link token buckets, strikes, the input tick window, `sanitize`/`validate`, and backpressure disconnects. Server authority means clients send intent only. Interest and `Audience::Owner` limit what can leak (a wallhack sees only in-scope entities). Account bans are enforced by every room a player is in (on the ban-list change and on every join, resume or supersede: `Disconnect(Banned)`); an admin kick reaches every room of the server.
- **Abuse of the PS:** quick play holds at most one unused seat reservation per account (a new quick play releases the previous one), is limited per account (1/s, burst 10), may open at most 3 rooms at once then 6 per minute per account, and only for a mode that a configured queue or a server (`GameDefinition::modes`, sent at registration) hosts; a room a game server refuses to build (`PrepareRoom` fails) releases its reservations at once (`RoomClosed`). Lobbies: one open lobby per host (a new one closes the previous), mode 1–32 of `[A-Za-z0-9_.-]`, closed after 30 min without activity, codes indexed. Password logins charge the per-username budget only on failure, per (username, address prefix) first, with a 4× username-wide backstop, so other addresses' wrong guesses do not lock the owner out. Every attempt reserves a token from both budgets before the (awaited) password check and a success refunds them, so a burst of concurrent guesses cannot all pass the check before the first failure is counted.
- **Admin:** a separate listener (loopback or cluster-internal), bearer tokens compared constant-time against SHA-256 hashes (production requires every token to be ≥ 16 characters), and every action audit-logged under the token-derived actor. Without tokens on a loopback bind (dev) the API is open, but only to requests a web page cannot forge: a loopback `Host` (no DNS rebinding) and, if present, a loopback `Origin`.
- **Fleet:** the fleet token is checked at the WebSocket upgrade (`Authorization: Bearer`, constant time; no socket without it) and again in `Register`; at most 1024 fleet connections, and a connection that does not register within 10 s is closed. `/internal/v1/*` checks the per-IP rate and the fleet token in a middleware before any body is read, and bounds a checkpoint upload to 30 s.
- **Secrets** come only from env or files, and are redacted in logs and config dumps.
- **Dev auth** is only allowed with a loopback bind. Metrics labels never contain player ids, room ids or IPs.
- **TLS** terminates at the ingress (`wss://` is required on https pages). In-process TLS is deferred.

---

## 14. Observability and operations

### 14.1 Metrics (`GET /metrics` on the admin port, Prometheus text 0.0.4; in-house registry; bounded labels)

**GS:**
- Rooms and players: `space3d_rooms{phase}`, `space3d_players{state}`.
- Tick health: `space3d_tick_seconds` (histogram, 0.25–50 ms), `space3d_ticks_skipped_total`, `space3d_room_crashes_total`, `space3d_shard_load{shard}`.
- Traffic: `space3d_bytes_total{dir,channel="all"}` (no per-channel breakdown yet), `space3d_packets_total{dir}`, `space3d_snapshot_bytes` (histogram), `space3d_snapshots_skipped_total{reason}`, `space3d_rtt_seconds` (histogram).
- Inputs and connections: `space3d_input_total{result=ok|late|missing|invalid}`, `space3d_handshakes_total{result}`, `space3d_rate_limited_total{kind=packets|messages|link_packets|room_inbox}`, `space3d_strikes_total`, `space3d_disconnects_total{reason}` (every `DisconnectCode::label`, plus `link_lost` and `room_inbox_full`; from `RoomNetStats::disconnects`), `space3d_fleet_connected`.
- Persistence: `space3d_outbox_pending`, `space3d_checkpoint_seconds`, `space3d_checkpoint_bytes`.

**PS:**
- Sessions: `space3d_logins_total{provider,result}`, `space3d_sessions_active` (refresh families with a live token, counted every 10 s).
- Matchmaking: `space3d_mm_queue_size{queue,region}`, `space3d_mm_wait_seconds{queue}`, `space3d_matches_total{queue}`, `space3d_matches_void_total`.
- Fleet: `space3d_fleet_servers{region,state}`, `space3d_fleet_free_seats{region}`.
- Infrastructure: `space3d_db_queue_depth` (`Db::queue_depth`), `space3d_http_requests_total{route,status}`, `space3d_http_seconds{route}`.

Room stats: `NetServer::room_stats()` returns cumulative `RoomNetStats` counters, and the room plugin exports their deltas at 1 Hz. Per-room detail appears only in the admin JSON API.

### 14.2 Logs

`tracing-subscriber` writes JSON (production) or pretty output (dev) to stdout, with an `EnvFilter`.
- **Spans:** the process runs in a `server` span (`server_id`, `role`), entered by `ServerMain` and inherited by the listeners' connection tasks, the ingress task, the fleet link and the shard threads; each room tick runs in a fresh `room` span (`server_id`, `room_id`, `epoch`, `tick`; `logging::room_tick_span`, shared by the shards and `TestCluster`), so replication warnings that do not name the room carry it. It is never one long-lived span with the tick `record`ed on it: the text formatter appends every recorded value to the span's stored fields, so the span and each line logged in it would grow by ` tick=N` every tick. Shard threads also log to the subscriber of the thread that started them (a scoped one in tests). Both are `ERROR`-level spans so a `warn` filter keeps them. JSON lines include the current span's fields.
- **Other fields:** `client`, `player_id`, `link`.
- **Security events:** logged at `warn` with `event=`.
- **Audit:** admin actions go to the `space3d::audit` target.

### 14.3 Admin API (`admin.rs`)

- **Read:** `GET /admin/v1/status | rooms | rooms/{id} | players | config | schema`.
- **Room actions:** `POST /admin/v1/rooms/{id}/close | rooms/{id}/checkpoint | players/{id}/kick`.
- **Bans:** `POST /admin/v1/bans` and `DELETE /admin/v1/bans/{id}` (the PS persists the ban and pushes it to GSs).
- **Server actions:** `POST /admin/v1/broadcast | drain | backup`.
- **Probes:** `GET /metrics`, `/healthz`, `/readyz`.

---

## 15. Testability

### 15.1 `NetHarness` (`space3d-replication`, feature `harness`)

```rust
let mut h = NetHarness::new(HarnessConfig { seed: 7, semantics: Semantics::Stream, link: LinkProfile::symmetric(LinkConditions::mobile_4g()), ..Default::default() },
                            |app| { app.add_plugin(ArenaProtocol).add_plugin(ArenaRules); });
let a = h.add_client(ClientSpec::named("alice"), |app| { app.add_plugin(ArenaProtocol); install_bot(app, scripted(|t| Move::forward(t))); });
assert!(h.run_until(600, |h| h.client_net(a).is_connected()));
h.step_ticks(120);
h.assert_converged(a).unwrap();
h.partition(a, Duration::from_secs(3)); h.step_ticks(300);          // resume path
assert_eq!(h.wire_digest(), run_again_with_same_seed());            // byte-level determinism
```

- A single thread runs with one `ManualClock`.
- Clients can have their own `frame_dt` and phase, which exposes aliasing bugs.
- `Semantics::Stream` emulates WS head-of-line stalls; `Datagram` gives real loss and reordering.
- The harness plays the gateway (`parse_hello`, then `RoomInbound::Join`) with dev identities and `SecretSource::seeded`.

### 15.2 `TestCluster` (`space3d-server`, feature `testing`)

`TestCluster` runs the PS, `Gatekeeper`, rooms (stepped directly with no shard threads) and clients over `MemoryNetwork`, using `ManualClock` + `ManualUnixClock` and an in-memory `Db`. It covers:
- `guest`, `matchmake`, `connect`, `step`, `run_until`;
- `crash_room` (catch_unwind and restore, through the shards' crash-loop guard);
- `kill_game_server` (lease expiry, rejoin, restore);
- `set_fleet_outage` / `set_fleet_swallow` (fleet link down; answers lost after a request went out);
- `TestClusterConfig::supervisor_every` (run the supervisors every N steps: `6` reproduces production's 10 Hz supervisors against 60 Hz rooms, so fleet messages take as long to cross as for real while clients react to their room at once).

Checkpoint uploads, database writes and the PS's async answers run in real time while cluster time can race ahead of them: tests that wait for them give background work real time per tick (see `run_until_bg` in `tests/cluster.rs`).

### 15.3 Test plan (gates per PR: fmt, clippy `-D warnings`, tests, wasm check)

| area | tests |
|---|---|
| wire / codec | proptest round-trips (varint, zigzag, quantizers; smallest-three error < 0.1°); fuzz every decoder (no panic, no over-allocation); reserved bits and unknown channels rejected |
| registry | schema hash independent of registration order; mismatch → `Reject(SchemaMismatch)`; duplicate / late registration panics |
| conditioner | same seed gives the same trace; loss within ±1 % over 100k packets; bandwidth cap honoured; the stream never reorders |
| replication (harness) | spawn / insert / remove / despawn converge under `bad_wifi` in both semantics; `Parent` resolves when the child arrives first; `Audience::Owner` never leaks (byte-level); interest enter/leave without flicker at the hysteresis edge; budget ≤ cap + 5 % and no starvation with 2000 movers; flow control keeps backlog bounded on a 128 KiB/s stream |
| prediction | deterministic sim at 100 ms RTT: 0 mispredictions over 600 ticks; a server-only impulse gives exactly one `Mispredicted` and converges within RTT + 2 ticks; `max_rollback` snaps |
| time | clock converges to ±1 tick in 3 s under 100 ± 30 ms jitter and 300 ppm drift; the pacer holds `input_slack` near target with 0 missing inputs on `broadband`; hard resync after a 500 ms client stall |
| interpolation | render tick is monotonic; bounded error; no slide across rest → move gaps |
| sessions | resume keeps entity and `NetId`, with no duplicates; window expiry → `PlayerLeft{Timeout}`; supersede; epoch bump forces a full reset |
| determinism | `wire_digest` identical across two runs with the same seed |
| security (server) | token sign/verify/tamper/expiry/kind confusion/kid rotation/`jti` replay; gatekeeper rejects (version, schema, ticket, wrong server, banned, draining, timeout); per-IP limits with an injected clock; strikes → kick |
| server | config overlay precedence and validation; matchmaker; fleet leases and allocation; outbox idempotency; checkpoint round trip and `(epoch, saved_ms)` ordering; room panic contained and crash-loop close (`server-profile` CI job runs them with `--profile server`); drain handoff of persistent rooms (frozen at the final capture, rejoin waits for it, a slow or unreachable store, same-millisecond captures); rejoin during drain; stale copies after a fleet partition and their fenced outbox ops; checkpoint deletion and garbage collection; player-data load / fetch timeout / scores; quick-play, lobby and login abuse; slow / idle sockets; XFF lines; admin cross-site requests; room log spans |
| net (done) | 17 KiB → 1009 on server and client; slow consumer → 1008; accept hook; bounded inbox |
| e2e sockets | all-in-one on port 0: PS login → quick play → 3 WS clients see each other; drain writes checkpoints and closes with 1001 (`pump_until` style, 5 s deadline) |
| wasm | `cargo check -p space3d-replication --target wasm32-unknown-unknown` (default features and `--no-default-features --features client`) |
| load | CI smoke: 50 bots in process for 600 ticks, asserting bytes and allocations (not timing). Nightly (`.github/workflows/nightly.yml`): `loadtest` with 200 clients over real sockets and SLO gates (`examples/multiplayer_arena/tests/load.rs`, `#[ignore]`, release build) |
| acceptance (integration pass) | `crates/space3d-server/tests/acceptance.rs`: 4 clients through guest login → quick play → ticket on a 100 ms RTT, ±20 ms jitter, 5 % loss stream link converge (remote p95 0, final 1 mm), predict exactly after reconciliation whenever the server had their inputs, resume the same slot after a 3 s blackout, stay under bandwidth budgets (4 movers ≈ 1.7 KB/s down; 300 NPCs held to a 12 KiB/s budget); separate player-service and game-server Apps over localhost with ticket → connect and single-use tickets. `examples/multiplayer_arena/tests/processes.rs`: the shipped binaries over localhost. Numbers: `docs/server.md` §7 |

### 15.4 Bots and load test

- `bots::Bot<I>` (`think(&World, Tick) -> I`), `install_bot`, `scripted`. Server-side bots use `NetServer::add_virtual_client` + `server::inputs::brain`.
- `loadtest::run(config, protocol, make_bot)` runs N headless client `App`s over real WS on `clients / bots_per_thread` threads. Clients connect via dev auth or through the PS.
- It reports connect success and handshake percentiles, RTT, down/up bytes/s, snapshot rate, rollbacks/s, disconnect reasons, and scraped server tick p99.
- The SLO gates set the exit code.
- Acceptance target: 200 clients across 10 rooms at 60 Hz on 4 cores, with server tick p99 < 4 ms and no skipped ticks in a release build (the nightly job; the test tolerates ≤ 0.1 % skipped ticks for scheduler noise when the bots share the machine with an unoptimised server).

---

## 16. Budgets (design targets)

These are targets, not measured guarantees. What is measured: the byte budgets (harness scenarios and `acceptance.rs` assert downstream bytes per client), and the whole-server tick p99 of the 200-client / 10-room load test (≤ 4 ms SLO, nightly). The per-room CPU figures (16-client room < 1 ms, one 200-client room with 1000 movers < 5 ms p99) and the wasm client figures have no benchmark yet; they are budgets for future benches.

- **Downstream:**
  - 16-player arena at 30 Hz snapshots: about 16 × 17 B plus about 12 B header ≈ 285 B per snapshot, or **8.5 KB/s**, plus WS/TLS framing of about 2.2 KB/s.
  - 100 visible movers ≈ 51 KB/s, where the 48 KiB/s default budget makes priority decide.
- **Upstream:** 60 input packets/s × (about 8 B data + 6 B header + about 70 B WS/TLS/TCP) ≈ 5 KB/s.
- **Server CPU, 16-client room:** collect about 20 µs per 60 entities, packet build about 15–30 µs per client, for a target replication cost under 1 ms per tick. A 200-client room with 1000 movers stays under 5 ms p99 on one core, with rayon packet build above 32 clients.
- **Wasm client:** receive and interpolation for 300 entities ≤ 1 ms per frame; an 8-tick rollback of 4 predicted entities ≤ 0.5 ms.

---

## 17. Changes to existing crates

### 17.1 `space3d-net`: implemented in this change

- **`WsServerConfig`** with builder methods:

  | field | default | behaviour |
  |---|---|---|
  | `max_message_size` / `max_frame_size` | 1 MiB (was tungstenite's 64 MiB) | an oversize message closes with **1009** |
  | `outbound_limit_bytes` | 16 MiB per client, byte-counted | exceeding it closes with **1008 slow consumer** |
  | `inbound_queue` | 65 536 messages, shared | overflow drops the message and closes the sender with 1008 |
  | `max_connections` | | excess upgrades get 503 |
  | `accept_text` | | when false, a text frame closes with 1003 |
  | `accept: Option<AcceptHook>` | | runs on `ConnectMeta{addr, origin, forwarded_for, user_agent}` and returns an HTTP status on rejection |

- **New `WsServer` API:** `WsServer::with_config`, `config`, `disconnect_with(client, code, reason)`, `client_meta`, `backlog_bytes`, `inbound_len`, and `async recv()` for the ingress task.
- **Client:** `WsConfig::max_message_size(n)`. Natively it sets the tungstenite `WebSocketConfig` message and frame size. On the web, an oversize message closes with 1009 and is never handed to the game. `WsConnection::queued_outgoing()` is also new.
- **Tests:** `tests/net/ws.rs` gained 5 tests (oversize on server and client, slow consumer, accept hook plus metadata, async recv plus inbound bound). `postcard` moved to `[workspace.dependencies]`.

### 17.2 `space3d-core`: none

Designs A and B each asked for a clock-scale field. Instead, pacing scales `TickRate::hz`, and `run_sim` pins `fixed_delta`. `App: !Send` is handled by building rooms on their shard.

### 17.3 `space3d-physics`: none required

Games add `physics_step` as a server sim system and do not add `PhysicsPlugin`'s own `FixedUpdate` step in rooms. A `PhysicsPlugin::manual()` convenience can come later.

### 17.4 Workspace

- `[workspace.dependencies]` gained `postcard 1.1.3`, `hmac 0.13.0`, `sha2 0.11.0`, `getrandom 0.4.3` and `argon2 0.6.0` (all latest stable, verified with `cargo search`), plus `space3d-replication` and `space3d-server`.
- `[profile.server] inherits = "release", panic = "unwind"` exists because the release profile aborts on panic.
- The server crate enables `tokio/signal` and `tracing-subscriber/json` locally; features are additive.

---

## 18. Frozen contract and work plan

### 18.1 Cross-builder contract (do not change without both builders agreeing and updating this doc)

The server crate uses these items from `space3d-replication`:

- **Transport:** `transport::{Lane, TransportCaps, LinkState, TransportError, CloseCode, TransportStats, ClientTransport, Connector, PeerInfo, LinkTx, LinkEvent}`; `transport::memory::{MemoryNetwork, MemoryServerEndpoint, MemoryConnector}`; `transport::conditioner::{LinkConditions, LinkProfile, Semantics}`.
- **Protocol:** `protocol::{PROTOCOL_VERSION, MAGIC, MAX_HANDSHAKE_BYTES, PacketKind, WireLimits, encode_handshake, peek}`; `protocol::handshake::*`; `protocol::control::{Notice, NoticeLevel}`.
- **Server (room API):**
  - `ServerReplicationPlugin{config}`, `ServerNetConfig` (fields), `NetServer::{tick, set_tick, send, notice, kick, disconnect_all, set_joinable, is_joinable, clients, client_of, connected_count, stats, room_stats, net_id, entity, set_visibility, add_visibility_rule, add_virtual_client}`.
  - Inbox: `NetInbox::try_recv`, `RoomSender::{try_send, len, is_closed}`, `room_channel(cap)`, `RoomInbound::{Join(Box<JoinRequest>), Packet{link, bytes}, Closed{link, reason}}`, `JoinRequest{link, tx, peer, hello, identity}`, `InboxFull`.
  - Identity and events: `PlayerIdentity{player, display_name, team, is_bot, extra}` and `PlayerIdentity::dev(name)`; `PlayerJoined`, `PlayerDisconnected`, `PlayerResumed`, `PlayerLeft{reason: LeaveReason}`.
  - Policy types: `ClientInfo`, `SlotState`, `InterestPolicy`, `Visibility`, `StrikePolicy`.
  - Helpers: `server::handshake::{parse_hello, resume_route, reject_packet, ct_eq, SecretSource::{from_fn, seeded}}`; `server::full_state::{FullState, capture, restore}`; `server::receive::TokenBucket`; `server::inputs::{VirtualBrain, brain}`.
- **Client:** `ClientReplicationPlugin`, `ClientConfig`, `NetClient` (all pub methods), `ConnectTarget`, `ConnectionState`, `ConnectionEvent`; `bots::{Bot, install_bot, scripted}`; `harness::{NetHarness, HarnessConfig, ClientSpec, ClientHandle}` (for `TestCluster`).
- **Shared:** `time::{NetClock, NetClockHandle, ManualClock, SystemClock}`; `stats::{NetStats, RoomNetStats}`; ids; `ReplicationRegistry::{freeze, schema_hash, manifest}`; `AppReplicationExt`; `ReplicationPlugin`; `FastMap`.

The replication builder may freely change everything else: registry internals, `server::{change, interest internals, priority, send, slots}`, `client::{connection, receive, prediction, interpolation}` internals, `wire`, `quant`, `mapping` internals, and the snapshot and input block encodings (both ends are theirs; update §6 if the format changes).

`api.rs` and `services.rs` belong to the server builder, which may change them as long as `api::{Assignment, PlayerDataEntry}` keep their fields, because the replication harness docs and clients use them.

### 18.2 Phases

| phase | replication builder | server builder | exit criteria |
|---|---|---|---|
| **P1** | `wire`, `quant`, `protocol/*` (encode/decode), registry and schema hash, `mapping`, transports (memory, conditioner, ws), `time` estimators, `sim::run_sim` | `config`, `clock`, `logging`, `metrics`, `health`, `security/*` (tokens, limits, bans, cidr, password), `transport_ws`, `gatekeeper` (sans-IO, unit-tested with fake `LinkTx`), `ps::matchmaker`, `ps::results` | unit tests and proptests green; wasm check |
| **P2** | server plugin (receive, slots, change, RoomWide interest, priority/budget, flow control, send, full_state); client plugin (connection, receive, interpolation, input send); `NetHarness`; bots | `ps` store/schema/auth/routes/lobbies/fleet registry, `services.rs` client, `rooms`/`shard`/`room`/`ingress`, `fleet` links, `outbox`, `checkpoint` stores, `lifecycle`, `admin`, `ServerMain` | harness convergence and determinism tests; PS API tests; gatekeeper and room e2e once P2-R lands |
| **P3** | prediction and rollback, `TickPacer`, spatial interest, `Audience`, messages timing, `examples/arena-shared` + `arena-client` | `TestCluster`, e2e sockets, drain, recovery, `examples/arena-server`, Docker/compose | prediction and time tests; e2e all-in-one and split; recovery tests |
| **P4** | lag compensation, predicted spawns, datagram packet splitting under `Semantics::Datagram` | `loadtest` + arena load-test bin, nightly SLO job | 200-client acceptance |
| **P5 (deferred)** | QUIC transport (native) | QUIC listener, Postgres store | — |

Each builder removes the crate-level `#![allow(dead_code, unused_variables, unused_imports, clippy::todo, clippy::ptr_arg)]` once its crate is implemented.

### 18.3 Implementation notes (server builder)

All changes are additive; the §18.1 contract is unchanged.

- **`TestCluster` runs the split topology.** The player service and each game server (`gs-1`, `gs-2`, ...) are separate supervisor `App`s joined by in-process fleet links. Each game server has its own `MemoryNetwork`, and clients reach the player service over loopback HTTP through `PlayerServiceClient`. Rooms are built and ticked on the test thread. Faults: `crash_room` (injected panic, then `gs::shard::rebuild_crashed`) and `kill_game_server` (rooms, supervisor and links vanish, connections are refused, and the lease expires at the player service).
- **Embedding hooks:**
  - `BuildOptions::{fleet_link, checkpoints}` inject a fleet link and a checkpoint store for the game-server role.
  - `ServerMain::with_shared_game(Arc<G>, config)` lets several servers in one process share a game definition.
  - `ps::FleetHub::add_local(peer)` attaches more in-process game servers.
  - `gs::shard::rebuild_crashed` is shared by the shard threads and `TestCluster`.
- **Fleet registry:** a `Register` from a known `server_id` orphans that server's reported rooms until its next heartbeat lists them again. A restarted pod with a stable id therefore leaves its lost rooms eligible for rejoin and restore, while a link blip is healed by the next heartbeat. Orphaned rooms are never forgotten because a heartbeat omits them.
- **`services.rs`:** the client refreshes tokens automatically when the access token expires within 30 s. A rejected refresh ends the local session. Beyond the listed calls it also offers `register`, `update_profile`, `get_data`/`put_data`, `poll_lobby`, `lobby_action`, `leaderboard_me`, `status`, `set_session`, and `OnlineError::{status, code, retry_after_ms, is_retryable}`.
- **`loadtest`:** `LoadTestConfig::{from_args, try_from_args}` (clap) plus `parse_histogram` and `histogram_quantile`, which compute the tick p99 from `/metrics` scrapes taken before and after the run.
- **Restored state reaches clients (resolved in the integration pass):** `full_state::restore` pre-maps `NetId`s; `server::change` now adopts pre-mapped entities on its first collect (records re-sorted by `NetId`, pre-mapped entities despawned before that collect are forgotten). The crash and server-loss tests in `tests/cluster.rs` check what clients see and are no longer ignored.

### 18.4 Integration notes

All additive to the §18.1 contract.

- **Replication:** `ChangeCache::collect` adopts restored entities (above); `NetIdMap::retain`; `full_state` checkpoints the `Simulated` marker (`META_SIMULATED`), so a restored room keeps simulating its pawns; client replay starts from the newest confirmation and fills resync gaps with repeated inputs (§9.3). New scenario: `snapshot_burst_after_a_stall_replays_from_the_newest_confirmation`.
- **Server:** `ServerConfig::load_layered` + `config::merge_tables`, `ServerMain::from_env_with_defaults` (game-supplied defaults under file and env; `--print-default-config` shows them); fleet seat counting with presence and room-bound reservations (§11.2), `ServerRecord::{present, players()}`; `PlayerService::quick_play_room` finds an open room or allocates one under a single fleet lock (concurrent quick plays used to open a room each); the load-test report gained `server: Option<ServerSample>` (tick mean / p50 / p99, ticks, skipped ticks, rooms, server bytes/s from two `/metrics` scrapes) and `loadtest::metric_value`; the Dockerfile takes `CARGO_ARGS` (package / features).
- **Examples and tests:** `examples/multiplayer_arena` (§3.3); `crates/space3d-server/tests/acceptance.rs` (§15.3).
- **Still deferred:** predicted spawns, rayon packet building above 32 clients, the datagram `loss` stat, `select_nth_unstable` priority selection, Postgres, QUIC (`what it would take`: §19).

### 18.5 Review fixes

All additive to the §18.1 contract except the snapshot op bit (protocol version 2).

- **Replication:** parked unresolved references are dropped when a newer value or a removal of the component arrives; rate-limit overflow is judged per 1 s window (message drops count) and kicks with `RateLimited`; datagram records use acked baselines, a conservative initial RTO and late acks of presumed-lost records; the budget covers owned entities, never starves large entities, and marks deferred records (`DEFERRED`), which clients interpolate across with a per-entity lag; oversized stream records are never marked delivered; per-player visibility overrides are forgotten on despawn / leave; `RoomNetStats` counts disconnects per code, lost links, rate-limit drops and oversize drops; debug checks for plugin order and sim discipline (§20); transient rejects of resumes are retried.
- **Server:** forced stop rescue checkpoints (§2.4); readiness latch (§12); build-aware allocation and restore (§11.2); `RecoveryPolicy::Abandon` voids orphaned rooms (§12); per-link gateway budget, runtime IP-ban enforcement, IPv6 /64 rate keys, LRU limiter, fail-closed nonce cache, strict production admin tokens, fleet upgrade authentication and registration deadline, internal-route guard (§13); per-reason disconnect / rate-limit metrics, `space3d_sessions_active`, `space3d_db_queue_depth` (§14.1); whole-Hz `tick_hz` (the rate travels as an integer in `Welcome`).
- **Net / db:** `ConnectMeta::authorization`, `WsConfig::headers` (native), lock-free `ClientSender` (`WsLinkTx` never takes the server-wide client lock), `Db::queue_depth`.

**Second review round** (additive to §18.1 except the checkpoint format, version 2, and `FleetUp::{RoomClosed.restorable, Deregister}` / `ServerRegistration::modes` with serde defaults):

- **Replication:** fast-forward instead of re-simulating ticks the confirmed state already includes (§9.3); client despawn tombstones against reordered spawns (§8.4); budget debt capped, guaranteed progress for starved candidates, thinned re-confirmations while in debt (§8.6); stall-resistant path RTT / jitter for pacing and interpolation, a render clock that never runs backwards (§9.2, §9.4); late inputs counted per missed tick (§7.3); `NetClient::{set_rate, set_interest_hint, last_applied}`; `ClientSpec::with_team` in the harness; feature tests for audiences, send rules, relevance overrides, visibility rules, message targets, `rollback_when`, `NetPriority` and input sanitizing (`tests/features.rs`).
- **Server:** checkpoints ordered by `(epoch, saved_ms)` in every store (§12); persistent-room handoff on drain, rejoin never forks a running room, deregistration (§2.4); one crash-loop guard for shards and `TestCluster`; player-data fetch timeout (`gs::FETCH_TIMEOUT`); bans enforced by every room (joins, resumes, ban-list changes) and kicks reaching every room; quick-play / lobby / login limits, `GameDefinition::modes` (§13); `server` / `room` log spans (§14.2).
- **Net:** `HttpServer` runs its own HTTP/1.1 accept loop with `ConnectionLimits` (header-read timeout, total and per-peer caps) and `ConnectionStats`; `joined_header` for list-valued headers; the HTTP client drops idle pooled connections after 5 s.
- **CI:** wasm check of `space3d-replication` (default and client-only), a `server-profile` job, and the nightly load job (§15.3).

**Fourth review round** (additive; `origin` on outbox ops and `RoomStatus`, both `serde(default)`):

- **Replication:** after a backward resync on feedback the pacer waits for the server to consume `SLACK_SETTLE_TICKS` inputs of the new timeline, and the clock-based resync does not undo it for a hold; ticks predicted again after a backward resync reuse the inputs already sent (§9.2); lost snapshots are not taken for rest by the interpolation gap fix (§9.4); `NetStats` documents which side fills each field.
- **Server:** a frozen room closes only once its freeze capture is stored, never after the shutdown wait, and a stop keeps that capture in the rescue store; capture times strictly increase per room (§2.4); ops of a stale room copy after the live copy's restore point are dropped, results are idempotent per match (§11.2); crash-looped rooms' checkpoints are deleted and expired checkpoints garbage-collected (§12).

**Third review round** (additive; new fleet messages `FleetUp::RoomMoving`, `FleetDown::{RoomSuperseded, Reregister}`):

- **Replication:** resyncs always rebuild the timeline from the newest confirmation, and the clock-based resync drops the long frame's fixed-step backlog (`space3d_core::Time::{pending_steps, discard_backlog}`) (§9.3); datagram despawns follow presumed-lost spawns (§8.4); `path_rtt` follows a lasting rise in ≈ 1.25 s and `jitter` comes from consecutive samples (§9.2).
- **Server:** stale room copies after a fleet partition are superseded, forgotten servers re-register (§11.2); the drain handoff freezes the room at its final capture and rejoins wait for it (§2.4); login budgets are reserved before the password check (§13); a fresh `room` span per tick (§14.2).

**Fifth review round** (additive; `FleetDown::RoomMovingAck`, `RoomStatus::moves_on_drain` with `serde(default)`, `?through_epoch=` on the internal checkpoint delete):

- **Replication:** datagram acks and losses are fenced by the stay in scope they were sent in (§8.4); `Reject(ServerDraining)` with a retry delay is transient (`Reject::is_transient`; reusable credentials retry, a spent ticket fails with `FailReason::retry_after` for the game's next rejoin).
- **Server:** a moving room keeps its clients until the PS acknowledged `RoomMoving`, the PS also holds rejoins into a draining server's `moves_on_drain` rooms and gives up on stalled moves; a room that panics while handing off continues the handoff (§2.4); crash-loop deletes wait for every incarnation's uploads, deletes are epoch-fenced (§12); the fleet link closes with a handshake after the `Deregister`, which the PS no longer drops when the close follows at once (§11.2); `TestClusterConfig::supervisor_every` (§15.2).
- **Example and tests:** the arena client rejoins with backoff honouring retry hints (`Reject.retry_after_ms`, `ApiError.retry_after_ms`), resetting its budget once connected; `examples/multiplayer_arena/tests/processes.rs` runs the split drain handoff with the real binaries (player service, two game servers, two bots, SIGTERM mid-play) and captures every process's stdout and stderr (tracing goes to stdout) to assert no ERROR line.

**Sixth review round** (additive: `NetServer::{refuse_joins, join_refusal}`, `QuickPlayRefusal::RoomMoving`, `PlayerService::restore_room`, `FleetRegistry::displaced_room_for`):

- **Replication:** a room closed to newcomers refuses them with the refusal it was closed with (`NetServer::refuse_joins`; `set_joinable(false)` is still `RoomClosed`); a client stops resuming when the server's resume window is over (§7.4), which `Welcome::resume_window_s` rounds up (a sub-second window no longer reads as none).
- **Server:** a room that moves on drain refuses newcomers as transient (`ServerDraining` with a retry delay), the draining heartbeat goes out at once, and quick play waits for a moving persistent room or restores a handed-off / lost one instead of opening a second copy of the world (§2.4); `503 no_capacity` of a rejoin carries `retry_after_ms` like quick play's.
- **Example:** the arena client retries quick play and rejoin on retryable errors (5xx, 429, transport) with the hint or backoff, and while a reconnect keeps failing asks the PS every 2 s where the room is (rejoin for a session, quick play for a ticket that never got in), following it to another server. `examples/multiplayer_arena/tests/processes.rs` adds real-process runs of both: bots that quick play while the world moves end up in it (one room), and after a SIGKILL of the hosting game server (4 s lease) both bots are back in the restored room within 12 s.

---

## 19. Deferred

- **Transports and encoding:** QUIC (`quinn 0.11.12`) and WebTransport (`wtransport 0.7.2`); raw UDP with custom ARQ; compression (`lz4_flex` / zstd); bit-packing and field-level deltas (measure first).
- **Physics:** rapier rollback.
- **Simulation models:** GGPO or lockstep modes; listen server or host migration.
- **Rooms and fleet:** room migration between shards and servers; work stealing (needs `Runner: Send` in core); Agones or k8s operator.
- **Player service:** Postgres `PlayerStore` plus an HA matchmaker; OAuth and platform auth (Steam, Apple, Google, Discord) via an `ExternalAuthVerifier`; email verification; parties, friends, presence, chat; Glicko-2; WS push for matchmaking.
- **Other:** in-process TLS; replay recording (inputs plus checkpoints); a proc-macro `#[derive(NetCodec)]`.
- **CLI and facade:** `space3d-cli` integration (`space3d server run|docker`) and `space3d` facade re-exports, because those crates are being built elsewhere. Planned: `space3d::net` re-exports `space3d_replication::prelude`.

### What it would take

Three deferred items are large enough to need the owner's go-ahead. What each would involve, from the code as it is:

**Predicted spawns** (a projectile that appears at once on the firing client). Today a client only draws an entity once a snapshot `Spawn` record maps its `NetId` (`client/receive.rs`, `apply_snapshot` spawns `(NetEntity, Remote)`); `Predicted` covers components of entities the server already replicated (`client/prediction.rs`, `PredictionHistory` keyed by `NetId`).
1. *A spawn key both sides derive.* The sim system runs on the server and in prediction, so it can spawn through a helper (working name `spawn_keyed`, next to the `SimTime` handling in `sim.rs`) that computes `(owner slot, tick, counter within the tick)`. Roughly 150 lines.
2. *Wire.* The server writes the key into the `Spawn` record for the owner (a new `op` bit and one varint triple, so `PROTOCOL_VERSION` 2 → 3; the doc's additive rule cannot cover an op bit), and `full_state.rs` does the same for resyncs. Other clients get the plain record.
3. *Client adoption.* The client keeps `PredictedSpawns { key → local entity }`. A keyed spawn record binds the `NetId` to that entity (`EntityMap::insert`) instead of spawning a new one; the entity's `PredictionHistory` moves from the temporary id to the `NetId`. A key that the acked tick passes with no record despawns the entity and emits a `SpawnRejected` event; a rollback replay that spawns the same key again reuses the entity, and one that no longer spawns it despawns it.
4. *Edge cases that need their own tests:* despawn before confirmation, a reordered or lost spawn record on datagrams (the `spawned` bookkeeping in `apply_snapshot` and the despawn tombstones already fence duplicates), interest (the owner must not also draw the plain record), and lag compensation (`server/lagcomp.rs` records the confirmed entity, not the predicted one).
5. *Tests:* `NetHarness` scenarios for adopt, reject, replay and resync over `Stream` and `Datagram`, plus a proptest next to `prop_references_follow_their_targets_through_scope_changes`. About 1 200 lines with tests; the arena example is the template.

**QUIC / WebTransport.** The netcode already runs over true datagrams: `Semantics::Datagram` in `transport/conditioner.rs` exercises acks, baselines, presumed-lost records and packet splitting. What is missing is I/O.
1. *Client transport.* `transport::{ClientTransport, Connector}` are the seam (`Connector::connect(url)`; `NetClient` never reconnects on its own). Add `transport/quic.rs` (native, `quinn`, `TransportCaps::datagram(≈1150, …)`) and `transport/webtransport.rs` (wasm, `web-sys` `WebTransport`, which is behind `web_sys_unstable_apis`), and let `NetClient` choose the connector by URL scheme. UDP is blocked on some networks, so a `ConnectTarget` with a WebSocket fallback list is needed as well.
2. *Server transport.* `gs/transport_ws.rs` turns a `WsServer` into `LinkEvent`s; `gs/ingress.rs` holds a concrete `WsServerTransport`, so it becomes a merge of several event streams (or an enum). A `gs/transport_quic.rs` with the same event shape is added; `PeerInfo::transport` already names `"quic"`. The gatekeeper is transport-agnostic and needs no change, but the WebSocket-only guards (`AdmissionPolicy` origin checks, `X-Forwarded-For`) have QUIC counterparts: per-IP caps on the UDP socket, retry tokens against amplification, 0-RTT off.
3. *Config and discovery.* `game.quic_bind`, `game.tls_cert` / `tls_key` (files, reloadable), ALPN, and an additive `public_urls` list in `ServerRegistration` (sent in `FleetUp::Register`) and in the assignment (`serde(default)`; today there is a single `public_url`).
4. *Ops.* Ingress controllers do not carry UDP: a per-pod `LoadBalancer` or `hostPort` UDP Service in `deploy/k8s/`, UDP in the compose file and NetworkPolicies, a `transport` label on `space3d_bytes_total`, and TLS certificates for WebTransport (a CA-issued certificate or `serverCertificateHashes`).
5. *Build.* `quinn` needs a rustls crypto provider; the graph already builds `aws-lc-sys` for `reqwest`'s rustls, which is why the Dockerfile installs `cmake`, `clang` and `perl`. Every dependency is pinned to its newest stable release first (AGENTS.md rule 1). Native loopback tests through the `Connector` trait, the existing harness for loss, `cargo check --target wasm32-unknown-unknown` for the web transport. About 2 000 lines with tests.

**Postgres.** The seam is `ps::store::PlayerStore` (28 methods returning `space3d_tasks::Task`, `ps/store.rs`); only `PlayerService::open` names `SqliteStore` (`ps/mod.rs`).
1. *`PostgresStore`* behind a cargo feature (`tokio-postgres` with `deadpool-postgres`, or `sqlx`), translating each method; `schema.rs` becomes a second migration set (`INTEGER PRIMARY KEY` ids as `BIGINT`, blobs as `BYTEA`, `WITHOUT ROWID` dropped, `INSERT … ON CONFLICT` for the idempotent `applied_ops`, `SELECT … FOR UPDATE` where SQLite relies on its single writer: `rotate_refresh`, `apply_writes`, `put_checkpoint`'s newer-key check). `backup()` (`VACUUM INTO`) becomes `pg_dump` guidance and returns `Unsupported`.
2. *Coupling to remove.* `PlayerService` also holds a `space3d_db::Db` for backups and `db_queue_depth`, `lib.rs` opens one SQLite file that the all-in-one role shares with the game server (migrations 100–199 and 200–299), and `space3d_db::Leaderboard` is SQLite-only. The game server's outbox and rescue checkpoints stay local SQLite on purpose (`gs/outbox.rs`, `gs/checkpoint.rs`: they must survive a player-service outage).
3. *Config and readiness.* `player_service.database_url` (a secret: env or file), pool sizes, `db_ready` / `space3d_db_queue_depth` from the pool, and the compose and k8s manifests gain a database.
4. *Not solved by Postgres alone.* The fleet registry, matchmaker queues, lobbies, rate limiters and revocation cache are in-process state of the single PS, so a second PS instance still needs leader election or that state moved into the database. That is the HA matchmaker item.
5. *Tests:* the store's existing test suite (`ps/store.rs`) run against both backends (a `testcontainers`-style Postgres in CI), and a migration test that applies 101–102 to an empty database. Roughly 1 800 lines including migrations.

## 20. Risks

1. **TCP head-of-line blocking on lossy mobile links.** Mitigations: flow control, latest-wins snapshots, and adaptive slack and interpolation delay. The real fix is datagrams in phase 5. The harness's `Stream` semantics quantify the effect early.
2. **Byte-compare change detection costs O(entities × components) per send tick.** Mitigations: only on send ticks, `SendRule::Interval`, archetype presence caching. A `NetStatic` marker or core change ticks can come later.
3. **Float determinism across wasm, x86 and ARM.** Authoritative quantization plus `rollback_when` tolerate small differences; the server stays authoritative.
4. **Single PS instance** is both a ceiling and a single point of failure. Rooms keep running thanks to the outbox; logins and matchmaking pause. The HA path is deferred.
5. **`SimSchedule` discipline.** Games must put predicted logic in `add_sim_system` and query `With<Simulated>`. The arena example is the template. Debug builds check it: before each predicted tick the client compares every predicted component with what the sim recorded for the previous tick, warns once per session and counts changes made outside the sim (`NetClient::sim_discipline_violations`).
6. **Registration order.** Replication plugins must come before game plugins. Debug builds check it with `App::system_names`: both replication plugins warn about game systems already registered in `PreUpdate` / `FixedUpdate` (`space3d_replication::systems_before_replication`).

---

## Appendix: design scores and rationale

Each design was scored 1–10 on each criterion, and each score was checked against the actual crate sources in this worktree.

| criterion | A: snapshot-interp (Quake/Overwatch) | B: ECS-native (replicon/lightyear) | C: ops and scale |
|---|---|---|---|
| Correctness of netcode | **9** | 8 | 8 |
| Fit with existing engine APIs | 7 | 6 | **9** |
| Wasm client compatibility | 8 | 6 | **9** |
| Security | 8 | 8 | **9** |
| Operability / scale-out | 7 | 8 | **10** |
| Testability | 9 | 9 | 9 |
| Implementability | 6 | 7 | 7 |
| **Total (/70)** | **54** | **52** | **61** |

**A: snapshot-interp (54).**

*Strengths:*
- The most rigorous netcode: per-entity acked baselines, field-level quantized deltas, `quantize_authoritative` so mispredictions are exact lane comparisons, input slack plus a PI time-dilation controller, lag compensation, and a render-synced event queue.
- "Reliability from the transport": no hand-written ARQ, and seq/acks only for baselines. This is the simplest correct choice and was adopted.

*Weaknesses:*
- Requires a `space3d-core` change (`TickRate::clock_scale`) that C shows is unnecessary.
- The bit-packed lane codec (`Field` lists, `LaneCodec`) is a large and error-prone surface to build and keep wasm-portable.
- The HTTP-only fleet needs PS→GS calls into each game server.
- Quarantining freed `NetId`s is more complex than never reusing within an epoch.

**B: ECS-native (52).**

*Strengths:*
- The best developer experience and registration API (`ComponentRule`-style builder, `ClientMode`, `Audience`, `replicate_mapped` mirroring `space3d-scene`).
- Encode-once, fan-out bytes with byte comparison as change detection. It correctly rejects `hecs::ChangeTracker`.
- Ack-based flow control against WS bufferbloat.
- Found `panic = "abort"` in the release profile and `App: !Send`.
- All adopted.

*Weaknesses:*
- `Transport: MaybeSend`, meaning `!Send` on wasm, **cannot be stored in `Resources`**, which require `Send + Sync`. That breaks the client on web.
- Requires core (`Time::relative_speed`) and physics changes.
- A separate reliable ACTIONS channel with `action_seq` gating adds ordering machinery that state-based spawn/despawn makes unnecessary.
- The large new `WsAcceptor`/`WsLink` net API is more than was needed; per-client metadata and backlog on `WsServer` suffice.

**C: ops and scale (61), the winner.**

*Strengths:*
- Complete deployment story: roles, fleet link with leases, tickets that carry the `RoomSpec` (no RPC on the match-start path), checkpoints stored on the PS so any GS restores, durable outbox with idempotent ops, a failure matrix, k8s specifics.
- Sans-IO gatekeeper and rooms, driven identically by the harness and production.
- **No core changes**: pacing via `TickRate::hz` with `fixed_delta` pinned in the sim, and replication in `Stage::Render` after PostUpdate.
- Caught a real `space3d-net` pitfall (`WsConfig::reconnect` replays stale buffered inputs before a resume).
- Traits that are `Send + Sync` on every target.

*Weaknesses (fixed in the synthesis):*
- Its own reliability and fragmentation layer over datagrams (resends, windows, fragments) duplicates what QUIC streams give for free. Replaced by A's reliability-from-transport.
- The simpler component-level model lacked B's flow-control detail and DX builder, and A's slack feedback. Grafted in.

**Synthesis:**
- C's architecture and ops, including the fleet link, lazy rooms from tickets, checkpoints, outbox, sans-IO gatekeeper, `Stage::Render` placement and no core change.
- B's registration API, encode-once change detection, flow control and `SimSchedule` naming.
- A's reliability-from-transport, per-entity baseline invariant (C's formulation), input-slack pacer, `quantize_authoritative` and lag compensation.
- Resume tokens are room-issued, process-local secrets (A and C), so no HMAC is needed inside the shared crate and the harness can resume without crypto.

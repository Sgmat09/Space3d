# Space3d dedicated game server, player service and replication

**Status:** final spec. The skeleton crates are committed and compile, and the contract is frozen (§18).
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

**Load order:** defaults ← file (`--config` / `SPACE3D_CONFIG`) ← env `SPACE3D__SECTION__KEY=value` (split on `__`, lowercased, value parsed as a TOML scalar, else a string) ← CLI flags.

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
  - Build stage: `rust:1.98-slim-bookworm`, `cargo build --locked --profile server --bin ${BIN}`.
  - Runtime stage: `gcr.io/distroless/cc-debian12:nonroot`.
  - `EXPOSE 7777 8080 9090`, `VOLUME /data`, `STOPSIGNAL SIGTERM`.
  - Distroless has no curl, so probes use `server healthcheck [--ready]`.
- **`deploy/compose.yaml`:** one PS and two GSs for split mode.
- **Kubernetes (documented; manifests come later):**
  - PS: `StatefulSet` with 1 replica and a PVC; ingress with TLS; `/internal/*` blocked at the ingress.
  - GS: `StatefulSet` so names are stable (`server_id = POD_NAME`) and the outbox and checkpoint PVC is durable. Add a headless Service plus a per-pod ingress host (`gs-N.play.example.com`) or `hostPort`.
  - Set `terminationGracePeriodSeconds ≥ drain_timeout + 30`, add `preStop: sleep 5`, and a PDB with `maxUnavailable: 1`.
  - `space3d_fleet_free_seats{region}` drives HPA or KEDA scaling.

### 2.4 Health, readiness and graceful shutdown

- **`/healthz`** returns 200 while the supervisor ticks and every shard heartbeat is less than 5 s old. A stuck tick fails liveness.
- **`/readyz`** returns 200 when the server is not draining, the DB is ready, it is registered with the PS (GS role), and its shards are alive.
- **Lifecycle** (`lifecycle.rs`): `Starting → Ready → Draining → Stopping → Stopped`.
  1. **Drain trigger:** SIGTERM or SIGINT, `POST /admin/v1/drain`, or `FleetDown::Drain`. `/readyz` returns 503, the heartbeat reports `draining`, new tickets get `Reject(ServerDraining)` while resumes still work, and every room gets `RoomCommand::Drain{deadline}`, which sends `Notice::ShuttingDown`.
  2. **During drain:** match rooms finish naturally within `drain_timeout`. Persistent rooms checkpoint immediately and send `Notice::Reconnect`, so clients rejoin through the PS and are restored elsewhere.
  3. **Deadline:** checkpoint all rooms, then await uploads and the outbox flush (up to 20 s). Send `Disconnect(ServerShutdown)`, stop listeners, deregister, drop `Db` (its worker drains the queue), and exit 0.
  4. **Second signal:** checkpoint to local storage only, then exit 1.

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

### 3.3 Example game (after both builders land their P1)

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
   - On panic, mark the room `Crashed`, increment `room_crashes`, and rebuild it from its last checkpoint (epoch + 1). After 3 crashes in 5 minutes, close it.
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
| input packets/s per link | 90 (burst 30) | `TokenBucket` |
| messages/s per link | 30 (burst 60; per type `rate_limit`) | `TokenBucket` |
| input tick window | `[T, T + max_input_ahead(32)]` (older are late and dropped) | `InputBuffer` |
| handshakes per IP per minute / concurrent per IP | 20 / 8 | gatekeeper |
| outbound backlog per link | skip snapshots above 256 KiB; kick `SlowConsumer` after 5 s above 1 MiB | send |
| room inbox | 8192 messages (overflow: drop unreliable-only, close otherwise) | ingress |

**Strikes:** a decode error, unknown channel, out-of-window input, oversize message or failed `validate` each count as a strike. `strikes.max` (5) within 10 s leads to `Disconnect(ProtocolViolation)`. Rate-limit overflow drops packets first and kicks after 3 s of sustained overflow.

**Hostile input:** decoders never panic and never allocate beyond the packet size. This is enforced by proptest fuzzing of every reader.

### 6.3 Snapshot block (`protocol/snapshot.rs`)

```
Snapshot   := SnapshotHeader · varu32 entity_count · Entity* · [HAS_DESPAWNS] Despawns
SnapshotHeader := u8 flags (bit0 FULL_RESET · bit1 SYNC_COMPLETE · bit2 HAS_DESPAWNS)
                · varu32 input_ack_delta   // tick − newest input tick consumed for this client (u32::MAX = none)
                · i8 input_slack           // ticks early the input for `tick` arrived (i8::MIN = missing)
Entity     := varu32 net_id_delta (ascending; first absolute)
            · u8 op (bit0 SPAWN · bit1 HAS_REMOVED · bit2 CONTROLLED · bit3 OWNED)
            · varu64 present_mask · [HAS_REMOVED] varu64 removed_mask
            · for each set bit ascending: varu32 len · codec bytes
Despawns   := varu32 n · n × varu32 (net_id_delta << 1 | out_of_scope)
```

**Invariant:** an entity written for a client includes **every** component whose `changed_tick` is newer than that client's ack tick for the entity. Loss and reordering are therefore harmless. The client skips a record whose tick is at or below the entity's `last_applied_tick`. A client's own predicted entities are written in **every** snapshot with all their `Predict*` components, because reconciliation needs a confirmation even when nothing changed (for example when the server rejected a move).

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
7. If restoring: `full_state::restore` plus server-only scene restore, with `room_epoch + 1`, and the tick continues from the checkpoint.

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
- **Missing inputs:** for tick T the server uses `buffer[T]`. If it is missing, the server uses `last.repeat()` (counted as missing). While the player is disconnected it uses `NetInput::neutral()`.
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

**Resume tokens** are room-issued, single-use, and **process-local** by design. After a server loss, a failed resume (`ResumeExpired` or a connect error) makes the client call `POST /v1/sessions/rejoin`. That returns a ticket for the restored room.

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
- **Entity references** encode through `EncodeCtx::net_id` and decode through `DecodeCtx::entity`. An unknown target makes the value **pending** (`PendingRefs`): it is retried after each snapshot and dropped after 1 s or when the holder despawns.
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

`ClientEntity { spawned, ack_tick, last_sent_tick, acc, leaving_since }` is tracked for each visible entity. A component is due for a client when `changed_tick > max(ack_tick, last_sent_tick)` and the audience allows it. **All due components of an entity ship together**, which is why one baseline tick per entity is correct.

- **Streams (WS): optimistic.** The baseline advances when the packet is handed to the link. If the link dies, resume sends `FULL_RESET` anyway.
- **Datagrams: acked.** A `SentRecord{seq, tick, entities, despawns}` ring (256 entries) is kept, and an ack raises `ack_tick`. A packet unacked after `rto` is presumed lost: its entities fall back to `last_sent_tick = ack_tick` and are resent with **current** values, never stale bytes.
- **Spawns and despawns are state.** A spawn record repeats until acked (datagrams) or is sent once (streams). Despawn tombstones persist until every client has acked them or they age past the resume window.

### 8.5 Interest (`server/interest.rs`)

- Rooms are the hard partition.
- Inside a room the policy is `InterestPolicy::RoomWide` (the default) or `Spatial{cell, radius, hysteresis: 1.2}`. `Spatial` queries a 2D (XZ) `SpatialGrid` around each client's `InterestSource`s. It is updated incrementally and re-evaluated every `interest_every` send ticks.
- **Precedence:** own entities > `NetServer::set_visibility` override > `NetRelevance` (`Global | Spatial{radius} | OwnerOnly | Manual`) > `VisibilityRule`s (fog of war) > spatial.
- Entering scope sends a full spawn. Leaving (past `radius × hysteresis`) sends `Despawn(out_of_scope)`, and the client fires `RemoteDespawned{out_of_scope: true}`.

### 8.6 Priority, budget and flow control (`server/priority.rs`, `send.rs`)

Per client, per send tick:

1. **Flow control.** If `LinkTx::backlog_bytes() > backlog_skip_bytes` (256 KiB), skip this client's snapshot and halve its snapshot rate until the backlog drains. Above `backlog_kick_bytes` (1 MiB) for 5 s, send `Disconnect(SlowConsumer, resume_allowed)`. **This is what prevents TCP bufferbloat.**
2. **Budget.** `ByteBudget` is a token bucket at `client_bandwidth` (48 KiB/s default, clamped by `ClientCaps` and `SetRate`), with a 2× burst, and 4× until `SYNC_COMPLETE`.
3. **Always first:** despawns, removals, control, reliable messages, pongs, and the client's own entities.
4. **Accumulate.** Each other relevant entity with due changes gets `acc += rule_priority × NetPriority × 1/(1 + d/ref) × Δticks`.
5. **Write.** Select the top entries with `select_nth_unstable`, and write while the budget allows, trying up to 8 smaller entries after the first miss. Written entities reset `acc` to 0 and unwritten ones keep growing, so nothing starves.
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
- **Predicted tick target:** `est_server_tick + (rtt/2)·hz + target_slack`.
- **`TickPacer`** is a PI controller on the server-reported `input_slack` relative to `target_slack_ticks` (1.5, plus 2·jitter, raised after a head-of-line stall is detected).
  - `PaceAction::Scale(s)` with `s ∈ [0.95, 1.05]` sets `TickRate::hz = nominal × s`.
  - `PaceAction::Resync(n)` fires when the error exceeds 8 ticks: jump the tick and clear history.
  - *Implementation notes (replication builder):* the jitter term `2·jitter` is capped at 6 ticks (`PacerConfig::max_jitter_margin_ticks`), because downstream queueing also inflates the RTT variance; a missing input (`i8::MIN`) only speeds up and raises the stall boost, never jumps; after a resync to tick T, slack feedback is ignored until the server has consumed an input for T or later (its EWMA still describes the old timeline). The server drops inputs beyond `T + max_input_ahead` without a strike (the slack is still reported so the client pulls back); only ticks beyond `4 × max_input_ahead` strike.
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

**Snaps:** when a rollback cannot be replayed (longer than `max_rollback_ticks`, or inputs missing after a resync), the confirmed values are written and the recorded history before the current predicted tick T is dropped; confirmations older than T then only update the confirmed values, so one snap is followed by at most one ordinary replay. While the prediction lead itself exceeds `max_rollback_ticks` every correction snaps, so keep it above the expected lead (default 32 ticks ≈ 530 ms).

**Visuals:** `PredictionError` decays with `smoothing_half_life` (60 ms), and errors above `snap_distance` (2 m) snap instead. It is applied to the rendered transform only and never simulated.

**Predicted spawns** (a projectile keyed by `(tick, counter)` and matched on the server spawn) are phase 3.

### 9.4 Interpolation and jitter buffer (`client/interpolation.rs`)

- **`InterpClock`:** `render_tick = (server_estimate − (rtt/2)·hz) − delay`, i.e. the delay trails the newest tick a snapshot can have arrived for, not the server's current tick. The delay is `clamp(send_interval + jitter_k·jitter + ½ tick, 35 ms, 250 ms)`, slewed by ±5 % playback speed, and snaps beyond 10 ticks.
- **`SnapshotBuffer<T>`** holds 32 samples per `Interpolate`/`PredictAndInterpolate` component of each non-predicted entity. `sample(render_tick, lerp, max_extrapolate)` holds or extrapolates for up to 100 ms, then freezes.
- **Gap fix:** a sample arriving more than `send_interval` after the previous one gets a synthetic copy of the previous value inserted at `tick − send_interval`, so resting entities do not slide.
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
- `PUT|GET /internal/v1/rooms/{id}/checkpoint` (≤ 16 MiB).

### 11.2 Fleet (`gs/fleet.rs`, `ps/fleet.rs`)

- **`FleetUp`:** `Register(ServerRegistration{token, server_id, region, public_url, game_id, build, protocol, schema_hash, capacity})`, `Heartbeat{seq, players, tick_p99_us, draining, rooms: Vec<RoomStatus>}`, `RoomOpened`, `RoomClosed`, `PlayerJoined`, `PlayerLeft`, `MatchResult{op_id}`, `PlayerDataWrite{op_id}`, `SubmitScore{op_id}`, `FetchPlayerData`.
- **`FleetDown`:** `Registered{lease_ttl_ms, bans}`, `PrepareRoom{spec}`, `CloseRoom`, `Kick`, `Ban`, `Unban`, `Broadcast`, `Drain`, `Ack{op_id}`, `PlayerData`.
- **Leases:** the heartbeat runs every 2 s and the lease lasts 10 s. When a lease expires the server is marked `Dead` and its rooms are orphaned (eligible for rejoin and restore).
- **Allocation** filters on region, `game_id`, `schema_hash`, `Ready`, not draining, and enough free seats. It picks the lowest `players/capacity + tick_p99/budget` and reserves seats for the ticket TTL.

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
| room checkpoints | `Checkpoint{version, room_id, spec, state: FullState, server_only, game_blob, saved_ms}` → `CheckpointStore` (`player-service` default, `local`, `none`) | every 10 s, on phase change, drain, request; serialization via `AsyncQueue::compute_then`; ≤ 1 upload in flight per room |

**Failure matrix:**

| failure | detection | recovery |
|---|---|---|
| client network blip | close / idle 10 s | resume within 20 s: same slot, `FULL_RESET`, no pop-in |
| room panic | `catch_unwind` (`--profile server`) | rebuild from the last checkpoint, epoch + 1, `Notice::Reconnect`; crash-loop guard (3 in 5 min → close) |
| shard stall | heartbeat older than 5 s → `/healthz` 503 | orchestrator restart → server-loss path |
| GS crash or node loss | PS lease expiry (10 s) | rooms orphaned; clients' resume fails → `POST /v1/sessions/rejoin` → PS allocates a new GS with `restore_from` checkpoint (or `RecoveryPolicy::Abandon`: void and requeue) |
| PS unavailable | fleet link down | rooms keep running; outbox buffers; new logins and matchmaking unavailable; GS `/readyz` stays ready |
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
- **Runtime:** per-link token buckets, strikes, the input tick window, `sanitize`/`validate`, and backpressure disconnects. Server authority means clients send intent only. Interest and `Audience::Owner` limit what can leak (a wallhack sees only in-scope entities).
- **Admin:** a separate listener (loopback or cluster-internal), bearer tokens compared constant-time against SHA-256 hashes, and every action audit-logged.
- **Fleet:** the bearer token travels in `Register` over the in-cluster link. Tear down on mismatch.
- **Secrets** come only from env or files, and are redacted in logs and config dumps.
- **Dev auth** is only allowed with a loopback bind. Metrics labels never contain player ids, room ids or IPs.
- **TLS** terminates at the ingress (`wss://` is required on https pages). In-process TLS is deferred.

---

## 14. Observability and operations

### 14.1 Metrics (`GET /metrics` on the admin port, Prometheus text 0.0.4; in-house registry; bounded labels)

**GS:**
- Rooms and players: `space3d_rooms{phase}`, `space3d_players{state}`.
- Tick health: `space3d_tick_seconds` (histogram, 0.25–50 ms), `space3d_ticks_skipped_total`, `space3d_room_crashes_total`, `space3d_shard_load{shard}`.
- Traffic: `space3d_bytes_total{dir,channel}`, `space3d_packets_total{dir}`, `space3d_snapshot_bytes` (histogram), `space3d_snapshots_skipped_total{reason}`, `space3d_rtt_seconds` (histogram).
- Inputs and connections: `space3d_input_total{result=ok|late|missing|invalid}`, `space3d_handshakes_total{result}`, `space3d_rate_limited_total{kind}`, `space3d_strikes_total`, `space3d_disconnects_total{reason}`.
- Persistence: `space3d_outbox_pending`, `space3d_checkpoint_seconds`, `space3d_checkpoint_bytes`.

**PS:**
- Sessions: `space3d_logins_total{provider,result}`, `space3d_sessions_active`.
- Matchmaking: `space3d_mm_queue_size{queue,region}`, `space3d_mm_wait_seconds{queue}`, `space3d_matches_total{queue}`.
- Fleet: `space3d_fleet_servers{region,state}`, `space3d_fleet_free_seats{region}`.
- Infrastructure: `space3d_db_queue_depth`, `space3d_http_requests_total{route,status}`, `space3d_http_seconds{route}`.

Room stats: `NetServer::room_stats()` returns cumulative `RoomNetStats` counters, and the room plugin exports their deltas at 1 Hz. Per-room detail appears only in the admin JSON API.

### 14.2 Logs

`tracing-subscriber` writes JSON (production) or pretty output (dev) to stdout, with an `EnvFilter`.
- **Fields:** `server_id`, `role`, `room_id`, `client`, `player_id`, `link`, `tick`.
- **Spans:** each room tick runs in a `room` span.
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
- `crash_room` (catch_unwind and restore);
- `kill_game_server` (lease expiry, rejoin, restore).

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
| server | config overlay precedence and validation; matchmaker; fleet leases and allocation; outbox idempotency; checkpoint round trip; room panic contained (`--profile server` CI job) |
| net (done) | 17 KiB → 1009 on server and client; slow consumer → 1008; accept hook; bounded inbox |
| e2e sockets | all-in-one on port 0: PS login → quick play → 3 WS clients see each other; drain writes checkpoints and closes with 1001 (`pump_until` style, 5 s deadline) |
| wasm | `cargo check -p space3d-replication --target wasm32-unknown-unknown` (default features and `--no-default-features --features client`) |
| load | CI smoke: 50 bots in process for 600 ticks, asserting bytes and allocations (not timing). Nightly: `loadtest` with 200 clients over real sockets and SLO gates |

### 15.4 Bots and load test

- `bots::Bot<I>` (`think(&World, Tick) -> I`), `install_bot`, `scripted`. Server-side bots use `NetServer::add_virtual_client` + `server::inputs::brain`.
- `loadtest::run(config, protocol, make_bot)` runs N headless client `App`s over real WS on `clients / bots_per_thread` threads. Clients connect via dev auth or through the PS.
- It reports connect success and handshake percentiles, RTT, down/up bytes/s, snapshot rate, rollbacks/s, disconnect reasons, and scraped server tick p99.
- The SLO gates set the exit code.
- Acceptance target: 200 clients across 10 rooms at 60 Hz on 4 cores, with server tick p99 < 4 ms and no skipped ticks.

---

## 16. Budgets (targets verified by harness and load tests)

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

---

## 19. Deferred

- **Transports and encoding:** QUIC (`quinn 0.11.12`) and WebTransport (`wtransport 0.7.2`); raw UDP with custom ARQ; compression (`lz4_flex` / zstd); bit-packing and field-level deltas (measure first).
- **Physics:** rapier rollback.
- **Simulation models:** GGPO or lockstep modes; listen server or host migration.
- **Rooms and fleet:** room migration between shards and servers; work stealing (needs `Runner: Send` in core); Agones or k8s operator.
- **Player service:** Postgres `PlayerStore` plus an HA matchmaker; OAuth and platform auth (Steam, Apple, Google, Discord) via an `ExternalAuthVerifier`; email verification; parties, friends, presence, chat; Glicko-2; WS push for matchmaking.
- **Other:** in-process TLS; replay recording (inputs plus checkpoints); a proc-macro `#[derive(NetCodec)]`.
- **CLI and facade:** `space3d-cli` integration (`space3d server run|docker`) and `space3d` facade re-exports, because those crates are being built elsewhere. Planned: `space3d::net` re-exports `space3d_replication::prelude`.

## 20. Risks

1. **TCP head-of-line blocking on lossy mobile links.** Mitigations: flow control, latest-wins snapshots, and adaptive slack and interpolation delay. The real fix is datagrams in phase 5. The harness's `Stream` semantics quantify the effect early.
2. **Byte-compare change detection costs O(entities × components) per send tick.** Mitigations: only on send ticks, `SendRule::Interval`, archetype presence caching. A `NetStatic` marker or core change ticks can come later.
3. **Float determinism across wasm, x86 and ARM.** Authoritative quantization plus `rollback_when` tolerate small differences; the server stays authoritative.
4. **Single PS instance** is both a ceiling and a single point of failure. Rooms keep running thanks to the outbox; logins and matchmaking pause. The HA path is deferred.
5. **`SimSchedule` discipline.** Games must put predicted logic in `add_sim_system` and query `With<Simulated>`. The arena example is the template, and a debug assert warns when a `Predicted` component changes outside the sim.
6. **Registration order.** Replication plugins must come before game plugins. `App::system_names` makes this checkable in a debug assert.

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

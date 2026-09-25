# Dedicated server guide

Space3d games can run a **separate, authoritative server** that owns the game state and the
players. Native, mobile and browser clients connect to it; clients only send input and never
send state. This guide covers how to run it, deploy it, configure it and scale it.

- **Design and internals:** [`docs/design/game-server.md`](design/game-server.md).
- **Crates:**
  - `space3d-server` is native only: the process, the game server and the player service.
  - `space3d-replication` is shared by server and client, and compiles for `wasm32`: protocol,
    replication, prediction and interpolation.
- **Example:** `examples/multiplayer_arena` is the smallest complete game.
  - `server` binary: the dedicated server.
  - `client` binary: a native or web client, with a `--headless --bot` mode.
  - `loadtest` binary: headless bots.
  - `examples/arena-*` is a larger game with shooting, lag compensation, matches and
    leaderboards.

---

## 1. What runs where

The server has two roles. Both are in one library, and one binary can run either or both.

| role | owns | clients talk to it over |
|---|---|---|
| **Player service (PS)** | accounts, sessions, profiles, player data, leaderboards, matchmaking, lobbies, the fleet registry of game servers, room checkpoints | HTTPS JSON (`/v1/*`) |
| **Game server (GS)** | rooms. A room is an isolated simulation of 1–N players, ticking at a fixed rate on a shard thread. | WebSocket (`/play`) |

The `role` config key picks what a process runs:

| `role` | runs | typical use |
|---|---|---|
| `all-in-one` | PS + GS, joined by an in-process fleet link. They can share one port. | development, small games, a single VM |
| `player-service` | PS only. The stock `space3d-server` binary has no game code. | scale-out |
| `game-server` | GS only. It registers with the PS over the fleet WebSocket. | scale-out, one per node or pod |

### 1.1 Topologies

**All-in-one (one process, one port):**

```
  native / web client
        │  POST /v1/auth/guest, POST /v1/play/quick  ──►┐
        │  WS /play  (Hello{Ticket}) ──────────────────►│  127.0.0.1:7777
        ▼                                                ▼
 ┌──────────────────────── server (role = all-in-one) ────────────────────────┐
 │ player service ──in-process fleet link──► game server                      │
 │ SQLite: accounts, player data, outbox,    ingress + gatekeeper → rooms     │
 │         checkpoints                       (shard threads, 60 Hz ticks)     │
 │ admin / metrics / health :9090 (loopback)                                  │
 └────────────────────────────────────────────────────────────────────────────┘
```

**Split (one player service, N game servers):**

```
                       ┌──────────── player service (1 instance) ───────────┐
 client ──HTTPS /v1──► │ accounts · sessions · matchmaking · lobbies         │
   │                   │ fleet registry (leases) · checkpoints · SQLite      │
   │                   └──────▲──────────────────────────▲──────────────────┘
   │ Assignment               │ fleet WS: register,       │ checkpoint upload
   │ {server_url, room_id,    │ heartbeat, presence,      │ (HTTP PUT)
   │  join_ticket}            │ results (outbox)          │
   ▼                          │                           │
 ┌── game server gs-1 ───────┴──┐   ┌── game server gs-2 ──┴──┐   …
 │ WS :7777/play                │   │ WS :7777/play           │
 │ shards × S → rooms × k       │   │                         │
 │ admin :9090 (private)        │   │                         │
 │ local SQLite: outbox         │   │                         │
 └──────────────────────────────┘   └─────────────────────────┘
```

A game server needs no inbound internal port. It dials the player service. The player
service hands out URLs, and clients connect to the game server directly.

**Join sequence:**

```
client                 player service                        game server
  │ POST /v1/auth/guest ──►│ access 1 h / refresh 30 d
  │ POST /v1/play/quick ──►│ pick an open room or allocate one;
  │                        │ sign JoinClaims{player, server_id, room spec, jti, exp=60 s}
  │ ◄── Assignment ────────│
  │ WS /play  Hello{v, game_id, schema_hash, Ticket} ─────────────────►│ verify HMAC offline,
  │                                                                    │ single-use jti, bans,
  │                                                                    │ create room lazily
  │ ◄──────────────────────── Welcome{slot, tick, resume token} ───────│
  │ ◄──── snapshots 30 Hz ── inputs 60 Hz ──►  pings 4 Hz ─────────────│
```

The ticket carries the room spec, so starting a match needs no RPC between the services. A
dropped link resumes the same slot within 20 s using the resume token. After a room crash
or a server loss, the client calls `POST /v1/sessions/rejoin`. The player service may then
restore the room on another game server from its last checkpoint.

---

## 2. Quickstart: run it locally

```sh
# Terminal 1: all-in-one dev server on 127.0.0.1:7777 (admin on 127.0.0.1:9090)
cargo run -p multiplayer_arena --bin server

# Terminal 2 and 3: windowed clients through the player service (WASD / arrows to move)
cargo run -p multiplayer_arena --bin client -- --ps http://127.0.0.1:7777 --name alice
cargo run -p multiplayer_arena --bin client -- --ps http://127.0.0.1:7777 --name bob

# Or headless bots (exit after 10 s; exit code 0 only if they were connected)
cargo run -p multiplayer_arena --bin client -- --headless --bot --ps http://127.0.0.1:7777 --name bot-1 --seconds 10

# Dev auth straight to the game server, with no player service (loopback only)
cargo run -p multiplayer_arena --bin client -- --server ws://127.0.0.1:7777/play --name carol
```

With no arguments the server runs its **built-in dev config**
(`examples/multiplayer_arena/server.toml`):

- all-in-one, loopback binds, one shared port for `/v1` and `/play`;
- dev auth on;
- relaxed per-IP limits, so local bots can connect;
- data under `target/multiplayer_arena/`;
- the public dev signing key. It logs a warning, and `env = "production"` refuses it.

Look inside the running server:

```sh
curl -s 127.0.0.1:9090/readyz                       # ready
curl -s 127.0.0.1:9090/admin/v1/rooms | jq          # rooms, players, tick cost (open on loopback in dev)
curl -s 127.0.0.1:9090/metrics | grep space3d_tick_seconds
```

Stop it with Ctrl-C (SIGINT) or SIGTERM:

1. Clients get a `ShuttingDown` notice.
2. Rooms write a final checkpoint.
3. Clients are disconnected with `ServerShutdown`.
4. The process exits 0.

A second signal forces exit 1 without waiting: every room is checkpointed synchronously into
the game server's own database, clients are disconnected with `ServerShutdown`, and the next
start moves those checkpoints into the configured store so rejoining players are restored.

### 2.1 Split mode on one machine

```sh
export SPACE3D_KEYS="$(cargo run -q -p space3d-server -- keygen)"   # shared signing key
export SPACE3D_FLEET_TOKEN=local-fleet-token-0123456789

# Player service (stock binary, no game code)
SPACE3D__ROLE=player-service SPACE3D__PLAYER_SERVICE__BIND=127.0.0.1:8080 \
SPACE3D__ADMIN__BIND=127.0.0.1:9080 SPACE3D__PLAYER_SERVICE__DB_PATH=target/split/players.db \
  cargo run -p space3d-server

# A game server (the game's binary), registering over the fleet WebSocket
SPACE3D__SERVER_ID=gs-1 SPACE3D__GAME__BIND=127.0.0.1:7777 SPACE3D__GAME__PUBLIC_URL=ws://127.0.0.1:7777/play \
SPACE3D__ADMIN__BIND=127.0.0.1:9091 SPACE3D__PERSISTENCE__DATA_DIR=target/split/gs-1 \
SPACE3D__FLEET__PLAYER_SERVICE_URL=ws://127.0.0.1:8080/internal/v1/fleet \
SPACE3D__FLEET__PLAYER_SERVICE_HTTP=http://127.0.0.1:8080 \
  cargo run -p multiplayer_arena --bin server -- --role game-server

cargo run -p multiplayer_arena --bin client -- --ps http://127.0.0.1:8080 --name alice
```

For a second game server, repeat with `gs-2`, other ports and another data dir. Quick play,
matchmaking and rejoin spread players over every registered server.

### 2.2 Your own game

The server and every client add the **same protocol plugin**. It registers the replicated
components, the input type, the messages and the predicted simulation, and it is hashed into
the schema hash.

```rust
// shared (native + wasm)
impl Plugin for CubesProtocol {
    fn build(&self, app: &mut App) {
        app.add_plugin(ReplicationPlugin::default())            // Transform (predicted / interpolated), Name
            .replicate::<CubeColor>("cubes.color")
            .add_input::<CubeInput>("cubes.input")
            .add_sim_system("cubes::drive", drive);             // runs on the server and in client prediction
    }
}

// server binary
impl GameDefinition for CubesGame {
    fn game_id(&self) -> &str { "multiplayer_arena" }
    fn protocol(&self, app: &mut App) { app.add_plugin(CubesProtocol); }
    fn build_room(&self, app: &mut App, _spec: &RoomSpec) { app.add_plugin(CubesRules); } // spawn a cube per PlayerJoined
}
fn main() -> ExitCode {
    match ServerMain::from_env_with_defaults(CubesGame, include_str!("../server.toml")) {
        Ok(main) => main.run(),
        Err(e) => { eprintln!("{e}"); ExitCode::from(2) }
    }
}

// client
app.add_plugin(ClientReplicationPlugin { config: ClientConfig { game_id: "multiplayer_arena".into(), ..Default::default() } })
   .add_plugin(CubesProtocol);
app.set_input_sampler(|ctx| CubeInput::from_axes(/* ActionState axes */));
app.resources.expect_mut::<NetClient>().connect(ConnectTarget::Ticket { url, ticket });
```

Rules that keep prediction exact:

- Put predicted gameplay in `add_sim_system`.
- Query `With<&Simulated>` and read `SimTime`, not wall time.
- Inputs carry **state, not edges**.
- Add the replication plugin before game plugins.

Also:

- Anything that takes time on the client, such as player-service calls, returns a
  `space3d_tasks::Task`. Poll it from a system and never block the frame. The example client's
  `online_flow` shows how.
- `ServerMain::from_env_with_defaults` layers the game's defaults under `--config` and the env
  overlay. `ServerMain::from_env` starts from the library defaults.
- List the modes your game hosts in `GameDefinition::modes` (and reject others in
  `validate_spec`): quick play then opens rooms only of those modes.

---

## 3. Docker

The image is distroless, runs as non-root, stops on SIGTERM, and uses a `/data` volume. Build
from the workspace root:

```sh
# the example's dedicated server (without the windowed client and its GPU dependencies)
docker build -f crates/space3d-server/deploy/Dockerfile \
  --build-arg BIN=server --build-arg CARGO_ARGS="-p multiplayer_arena --no-default-features" \
  -t multiplayer-arena-server .

# the stock player service
docker build -f crates/space3d-server/deploy/Dockerfile --build-arg BIN=space3d-server -t space3d-player-service .
```

The image starts `server --config /etc/space3d/server.toml`, which is a copy of
`deploy/server.example.toml`, with `SPACE3D__ENV=production` and JSON logs. Production
refuses to start without real secrets:

```sh
docker run --rm -p 7777:7777 -p 8080:8080 -v arena-data:/data \
  -e SPACE3D_KEYS="k1:$(openssl rand -base64 32)" \
  -e SPACE3D_ADMIN_TOKENS="$(openssl rand -hex 24)" \
  -e SPACE3D__SERVER_ID=arena-1 \
  -e SPACE3D__GAME__PUBLIC_URL=wss://arena-1.play.example.com/play \
  multiplayer-arena-server
```

- **Probes:** the image has no shell and no curl. Use
  `server healthcheck` for liveness and `server healthcheck --ready` for readiness. The
  compose file already does this.
- **Split stack:** one player service and two game servers.
  ```sh
  SPACE3D_KEYS=k1:$(openssl rand -base64 32) SPACE3D_FLEET_TOKEN=$(openssl rand -hex 24) \
    SPACE3D_ADMIN_TOKENS=$(openssl rand -hex 24) \
    docker compose -f crates/space3d-server/deploy/compose.yaml up --build
  ```
  `compose.yaml` builds the arena game server. Set `BIN: server` and `CARGO_ARGS` in its
  build args to use the example instead.
- **TLS:** terminate TLS at your ingress or load balancer. Browsers on an https page need
  `wss://`.

---

## 4. Configuration reference

Configuration is loaded in this order, where later layers win:

1. library defaults;
2. the game binary's built-in TOML (`from_env_with_defaults`);
3. the `--config` file or `SPACE3D_CONFIG`;
4. environment variables `SPACE3D__SECTION__KEY=value`;
5. `--role`.

Merging rules:

- Tables merge key by key.
- Arrays, such as `[[matchmaking.queues]]`, replace the whole array.
- Env values are parsed as TOML, so `SPACE3D__MATCHMAKING__QUEUES='[{ name = "arena", mode = "arena", teams = 1, team_size = 20 }]'` works.

Value formats:

- `${VAR}` expands inside strings.
- `{server_id}` expands inside `game.public_url`.
- Durations are strings: `"250ms"`, `"30s"`, `"15m"`, `"24h"`, `"30d"`.
- Sizes are integers or strings: `"16KiB"`, `"1MiB"`.

Tools:

- `--check-config` validates the config and the secrets and prints the redacted effective
  config.
- `--print-default-config` prints the commented example.

### Top level

| key | default | meaning |
|---|---|---|
| `role` | `"all-in-one"` | `all-in-one` \| `player-service` \| `game-server` |
| `env` | `"dev"` | `production` enforces real secrets. It rejects missing keys, keys under 32 bytes, the dev key, the dev fleet token and short admin tokens. |
| `server_id` | `"local-1"` | Stable id, such as the pod name. Join tickets are bound to it, so keep it stable across restarts. |
| `region` | `"local"` | Used in allocation. `region_pings` from clients choose among regions. |

### `[game]`: game-server role

| key | default | meaning |
|---|---|---|
| `bind` | `0.0.0.0:7777` | `/play` WebSocket and `/ping` |
| `path` | `"/play"` | WebSocket path |
| `public_url` | `ws://127.0.0.1:7777/play` | URL written into assignments, e.g. `wss://{server_id}.play.example.com/play` |
| `shards` | `0` | Room threads. 0 means cores − 1, minimum 1. |
| `max_rooms` / `max_players` | 128 / 1024 | Capacity reported to the fleet |
| `tick_hz` | 60 | Room tick rate, whole Hz (clients receive it as an integer) |
| `send_interval` | 2 | Snapshot every N ticks (30 Hz) |
| `resume_window` | `"20s"` | How long a dropped player's slot and pawn are kept |
| `empty_room_ttl` | `"30s"` | Empty non-persistent rooms close after this |
| `drain_timeout` | `"15m"` | Budget for matches to finish on SIGTERM |
| `max_catchup_ticks` | 3 | Late ticks a room may run back to back before it skips |
| `allow_dev_auth` | false | `Hello{Dev}` without a ticket. Refused unless `bind` is loopback. |
| `allowed_origins` | `[]` | Browser `Origin` allowlist. Empty means any. |
| `room_inbox` | 8192 | Messages queued per room |

### `[limits]`: per connection

| key | default | meaning |
|---|---|---|
| `max_message_bytes` | 16 KiB | Inbound WebSocket message and frame cap (close 1009) |
| `max_connections` | 10000 | All links |
| `max_connections_per_ip` / `handshakes_per_ip_per_min` | 8 / 20 | Raise these for local load tests |
| `handshake_timeout` / `idle_timeout` | 5 s / 10 s | |
| `client_bandwidth` | 48 KiB | Default downstream snapshot budget per client, in bytes/s. Priority decides what fits. |
| `outbound_backlog_bytes` | 1 MiB | Above this for 5 s, the link is kicked as `SlowConsumer`. Snapshots are skipped above 256 KiB. |
| `http_connections_per_ip` / `http_max_connections` | 64 / 16384 | Every HTTP listener (game port, player service, admin): connections that have not completed a request or a WebSocket upgrade, per client IP (IPv6 per /64; `trusted_proxies` exempt) and in total. Extra sockets are closed at accept. Raise the per-IP cap for local load tests. |
| `http_header_timeout` / `http_body_timeout` | 10 s / 10 s | Every listener closes a connection that does not complete a request head in time (idle keep-alive connections too); the player service answers 408 to a body that does not arrive in time. |
| `input_packets_per_sec` / `messages_per_sec` | 90 / 30 | Token buckets. The packet bucket adds 30/s for pings and control (120/s, burst 60, never under 1.5 × `tick_hz` for inputs). Overflow is dropped; drops in 3 consecutive seconds (or 10 s worth of the allowance within one second) kick with `RateLimited`. The gateway also caps each link at 4 × `input_packets_per_sec` into the shared room inbox and closes sustained flooders. |

### `[player_service]`

| key | default | meaning |
|---|---|---|
| `bind` | `0.0.0.0:8080` | `/v1/*` and `/internal/v1/*`. In all-in-one mode, the same address as `game.bind` shares one listener. |
| `db_path` | `data/players.db` | SQLite (WAL) |
| `session_ttl` / `refresh_ttl` / `join_ticket_ttl` | 1 h / 30 d / 60 s | Refresh tokens rotate, and reusing one revokes the whole family |
| `guest_auth` / `password_auth` | true / false | Password auth needs the `password-auth` feature, which uses argon2id |
| `client_writable_keys` | `["settings"]` | Player-data keys that clients may `PUT` |
| `max_body_bytes` | 64 KiB | |
| `cors_origins` | `[]` | Allowlist for browser clients |
| `http_rps_per_ip` / `login_attempts_per_min` | 20 / 5 | |
| `motd` | none | Returned by `GET /v1/status` |

### `[fleet]`: game server to player service

| key | default | meaning |
|---|---|---|
| `player_service_url` | `ws://127.0.0.1:8080/internal/v1/fleet` | Fleet WebSocket |
| `player_service_http` | `http://127.0.0.1:8080` | Checkpoint upload and download |
| `heartbeat` / `lease_ttl` | 2 s / 10 s | A server silent for `lease_ttl` is dead. Its rooms are orphaned and can be restored elsewhere. |

### `[admin]`, `[security]`, `[persistence]`, `[matchmaking]`

| key | default | meaning |
|---|---|---|
| `admin.bind` | `127.0.0.1:9090` | Admin API, `/metrics`, `/healthz`, `/readyz`. Keep it private. |
| `security.signing_kid` | `"k1"` | Key that signs tokens. Every listed key verifies. |
| `security.keys_env` / `keys_file` | `SPACE3D_KEYS` / none | `k2:<b64>,k1:<b64>` or a TOML file `{ k1 = "<b64>" }`. Rotate by adding k2 everywhere, switching `signing_kid`, and dropping k1 after 30 days. |
| `security.fleet_token_env` / `_file` | `SPACE3D_FLEET_TOKEN` | Shared by the PS and the GSs. The fleet WebSocket requires it in the upgrade (`Authorization: Bearer …`) and closes connections that do not register within 10 s; `/internal/v1/*` checks it (and the per-IP rate) before reading a request body. |
| `security.admin_token_env` / `_file` | `SPACE3D_ADMIN_TOKENS` | Comma-separated. With no tokens the admin API is open on a loopback bind (only to requests with a loopback `Host` and, if any, a loopback `Origin`: web pages cannot drive it) and locked otherwise. In production every token must be ≥ 16 characters. |
| `security.trusted_proxies` | `[]` | CIDRs whose `X-Forwarded-For` is honoured (every header line, joined in order; the right-most address that is not a trusted proxy is the client). Per-address limits and strikes key IPv4 addresses as is and IPv6 addresses by /64. |
| `persistence.data_dir` | `data` | Game-server outbox and local checkpoints |
| `persistence.checkpoint_interval` | 10 s | Rooms also checkpoint on phase change, drain and admin request |
| `persistence.checkpoint_store` | `player-service` | `none` \| `local` \| `player-service`. With `player-service`, any GS can restore. |
| `persistence.recovery_max_age` | 5 min | Older checkpoints are not restored |
| `matchmaking.interval` | 250 ms | Matcher period |
| `[[matchmaking.queues]]` | none | `name`, `mode`, `teams`, `team_size`, `min_players`, `max_wait`, `rating_window` (+`_growth`), `region_ping_ms` (+`_max_ms`), `backfill`, `params`. The queue whose `mode` matches also sets quick-play room capacity to `teams × team_size`. The default is 16. |

**Quick play and lobbies.** Quick play opens rooms only for a mode that a queue configures or
that a game server hosts: list them in `GameDefinition::modes` (sent at registration; a game
that lists none accepts any mode `validate_spec` allows). An account holds one unused
quick-play seat at a time (asking again releases the previous one), makes at most 1 quick play
per second (burst 10) and opens at most 3 rooms at once, then 6 per minute. A host has one open
lobby at a time; idle lobbies close after 30 minutes. Password logins count failures per
(username, address) and per username (4× the per-address budget), and only failures.

---

## 5. Admin API, metrics and health

These live on `admin.bind`.

**Auth:**

- Send `Authorization: Bearer <token>`. Tokens are compared in constant time against SHA-256
  hashes.
- Every action is written to the audit log.
- `/metrics`, `/healthz` and `/readyz` need no token.

| method and path | what it does |
|---|---|
| `GET /healthz` | 200 while the supervisor ticks and every shard heartbeat is under 5 s old |
| `GET /readyz` | 200 when the server is not draining, the DB is up, it has registered with the PS once (GS role; a later PS outage keeps it ready), and its shards are alive |
| `GET /metrics` | Prometheus text: `space3d_tick_seconds`, `space3d_ticks_skipped_total`, `space3d_rooms{phase}`, `space3d_players{state}`, `space3d_bytes_total{dir,channel="all"}`, `space3d_input_total{result}`, `space3d_handshakes_total{result}`, `space3d_disconnects_total{reason}` (every `DisconnectCode` label plus `link_lost` and `room_inbox_full`), `space3d_rate_limited_total{kind}` (`packets`, `messages`, `link_packets`, `room_inbox`), `space3d_fleet_connected`, `space3d_outbox_pending`, `space3d_checkpoint_seconds`, and on the PS `space3d_logins_total`, `space3d_sessions_active`, `space3d_db_queue_depth`, `space3d_matches_void_total`, `space3d_mm_*`, `space3d_fleet_*`, `space3d_http_*` |
| `GET /admin/v1/status` | server id, role, version, schema hash, phase, registration, fleet link state |
| `GET /admin/v1/rooms`, `GET /admin/v1/rooms/{id}` | room list and detail: phase, players, tick cost, entities |
| `POST /admin/v1/rooms/{id}/close` | Close a room. Body: `{"reason": "…"}`, optional. |
| `POST /admin/v1/rooms/{id}/checkpoint` | Checkpoint now. Returns 202. |
| `GET /admin/v1/players` | connected players |
| `POST /admin/v1/players/{id}/kick` | Kick a player from every room of the server |
| `POST /admin/v1/bans`, `GET /admin/v1/bans`, `DELETE /admin/v1/bans/{id}` | Body: `{"player": "<id>"}` or `{"cidr": "10.0.0.0/8"}`, plus `"reason"` and an optional `"duration_s"`. The PS persists bans and pushes them to every GS. A player ban removes the player from every room they are in and refuses their resumes (`Disconnect(Banned)`); an IP/CIDR ban also closes connections already open from that range. |
| `POST /admin/v1/broadcast` | Body: `{"text": "…", "room": "<id>"?, "level": "info\|warn"?}` |
| `POST /admin/v1/drain` | Same as SIGTERM: not ready, no new rooms, matches finish, final checkpoints, then the process exits 0 once the drain completes (idle, or `drain_timeout`) |
| `GET /admin/v1/config`, `GET /admin/v1/schema` | Redacted effective config; the replication manifest, for diffing client builds |
| `POST /admin/v1/backup` | PS: `VACUUM INTO` a timestamped copy under `data_dir` |

---

## 6. Scaling

**Measured capacity.** These numbers come from the load test in §7.3:

- **Setup:** one Apple M4 Max machine (16 cores) runs the server and all bots on loopback.
  The server uses `--profile server`, is all-in-one, and has 15 shard threads.
- **Server tick time:** 200 players in 10 rooms of 20 moving cubes. Mean tick 0.084 ms, p99
  ≤ 0.5 ms, no skipped ticks.
- **Server resources:** about 20–30 % of one core and 55 MB RSS.
- **Traffic:** each client receives about 7.8 KB/s and sends about 0.8 KB/s of application
  data.

Size real games by measuring them. Tick cost grows with game logic, entities per room and
clients per room.

**Vertical: one game server.**

- Rooms spread over `shards` threads. A new room goes to the least-loaded shard, and rooms
  never migrate.
- A room is single-threaded. Keep one room's tick well under the tick budget: 16.7 ms at
  60 Hz, and the design target is < 5 ms p99 for 200 clients.
- `space3d_tick_seconds` and `space3d_ticks_skipped_total` tell you when a room is too big.
- Bandwidth per client is capped by `limits.client_bandwidth`. Priority and interest decide
  what fits. For large worlds use `InterestPolicy::Spatial`, so each client only receives
  nearby entities.

**Horizontal: more game servers.**

- Run N `game-server` processes behind one `player-service`.
- Each game server needs a stable `server_id` and its own public URL. For example, a
  Kubernetes StatefulSet with `gs-N.play.example.com` per pod, or `hostPort`.
- Allocation picks the least-loaded ready server in the requested region: players plus
  reserved seats over capacity, plus tick p99 over the budget. Only servers of the client's
  build qualify: quick play, matchmaking and lobby requests carry `game_id` and
  `schema_hash` (`PlayerServiceClient::with_build`), matchmaking only matches players of one
  build, and a restore after a server loss only goes to a server of the room's build. During a
  rolling deploy that changes the replication schema, old and new clients each land on their
  own servers.
- Seats count both the players in each heartbeat and the join / leave events between
  heartbeats. Quick play finds or opens a room atomically. So bursts of quick plays neither
  oversubscribe a room nor scatter over half-empty ones.
- Scale on `space3d_fleet_free_seats{region}`, which the PS exports.

**Draining and upgrades.**

- Draining a game server removes it from allocation.
- Match rooms run to completion within `drain_timeout`. A player who rejoins meanwhile goes back
  to the same room on the draining server (a match is never forked onto a second server).
- Persistent rooms warn clients (`Notice::Reconnect`) and freeze: clients get
  `Disconnect(ServerShutdown)` and the final checkpoint is captured in the same tick, so it
  holds everything the players saw. Clients rejoin through the PS
  (`POST /v1/sessions/rejoin`); the rejoin waits (up to 10 s) until that checkpoint is stored,
  then the room is restored on another server from exactly that state. The example client
  rejoins on any disconnect.
- When the drain completes the server deregisters (the PS stops counting it at once).
- Set `terminationGracePeriodSeconds ≥ drain_timeout + 30 s`.

**Failures.**

| failure | what happens |
|---|---|
| Link blip | Resume within `resume_window`: same slot and pawn, with a full resync |
| Room panic | Build with `--profile server`, which uses `panic = "unwind"`. The room is rebuilt from its last checkpoint and players rejoin. Three crashes in 5 minutes close the room. |
| Game server lost | The lease expires in `lease_ttl`. Rejoin restores the room from the PS-stored checkpoint on another server of the same game and schema hash. With `RecoveryPolicy::Abandon` the room is voided instead (audit `match_void`); rejoin answers 404 and players queue again. |
| Player service down | Rooms keep playing, results queue in the game servers' durable outbox, and `/readyz` stays ready (`space3d_fleet_connected` shows the link). Logins and matchmaking pause until the PS returns; the outbox then flushes. |
| Game server cut off from the PS (PS up) for longer than `lease_ttl` | The game server keeps playing its rooms, but the PS counts it as lost: a player who rejoins meanwhile gets the room restored elsewhere, and that copy is the one that counts. When the link is back, the PS closes the stale copy (`Disconnect(ServerShutdown)`); its players rejoin the live copy and lose what they did in the stale one. |

**Limits of v1.**

- There is a single player-service instance. SQLite in WAL mode handles thousands of writes
  per second. A Postgres `PlayerStore` and an HA matchmaker are deferred.
- WebSocket (TCP) is the only transport, so loss shows up as head-of-line stalls (§7.1).
  QUIC and WebTransport datagrams are deferred.

---

## 7. Testing and measured behaviour

| what | where | run |
|---|---|---|
| Replication unit tests and proptests, harness scenarios (prediction, interpolation, interest, budget, resume, bursts after stalls) | `crates/space3d-replication/{src,tests}` | `cargo test -p space3d-replication` |
| Full stack in process: PS, GSs, gatekeeper, tickets, rooms and clients over the conditioned memory network, with manual clocks (`TestCluster`) | `crates/space3d-server/tests/{cluster,acceptance}.rs` | `cargo test -p space3d-server` |
| Real sockets: all-in-one; split with lease expiry and restore; separate PS and GS Apps with ticket then connect | `crates/space3d-server/tests/{e2e,split,acceptance}.rs` | same |
| Abuse over real sockets: idle / slow / excess sockets, trickled bodies, `X-Forwarded-For` lines, login lockout, cross-site admin requests, lobby limits, audit actor; the deployment files' documented commands | `crates/space3d-server/tests/{abuse,deploy}.rs` | same |
| Example: WASD through the `ActionMap` in the harness; the shipped `server` plus 2 `client --headless --bot` processes | `examples/multiplayer_arena/tests/{netcode,processes}.rs` | `cargo test -p multiplayer_arena` |
| Load: 200 bots | `examples/multiplayer_arena/tests/load.rs` (`#[ignore]`), `loadtest` binary | see §7.3 |

### 7.1 Lossy network acceptance

This is `tests/acceptance.rs`:

- **Setup:** 4 clients join through guest login, quick play and a ticket. The link has
  100 ms RTT, ±20 ms jitter in each direction and 5 % loss in each direction, with
  WebSocket (stream) semantics. On a stream, every lost segment stalls everything behind it
  for a 200 ms retransmission timeout.
- **Measurement:** 10 s of play with pawns moving at 4 m/s. Bots change direction every
  0.75 s. The run is deterministic (seeded).

| measurement | result |
|---|---|
| Remote pawns: drawn position vs the server's position at the client's render tick | p50 0 mm, p95 0 mm, p99 0.49 m, max 1.9 m. The tail is TCP stalls: at most 100 ms of extrapolation, then hold. |
| Own pawn: predicted vs authoritative, all ticks | 94.2 % exact, p99 1.85 m. Mispredictions happen only when an input stalled behind a lost segment and the server repeated the previous one. This affected 7.5 % of client ticks (171 of 2295). |
| Own pawn after reconciliation (the server had the inputs) | **100 % exact** (1356 samples), no snaps |
| What the player sees (smoothed correction) | p95 0.07 m |
| After input stops | every client within 1 mm of the server |
| Session resume after a 3 s blackout | same slot, same pawn and `NetId`, on a fresh link |
| Bandwidth, 4 movers | about 1.7 KB/s down and 0.85 KB/s up per client (application bytes) |
| Bandwidth, 300 moving NPCs with a 12 KiB/s budget | 11.9–12.0 KB/s down per client, and every client knows every NPC |

### 7.2 Local smoke

`examples/multiplayer_arena/tests/processes.rs` runs the real binaries:

1. Start `server` and two `client --headless --bot --ps …` processes over localhost
   WebSockets for 4 s.
2. Both bots connect and see both cubes, with about 1.2 KB/s down and 0.65 KB/s up at
   around 23 ms loopback RTT. The RTT is frame-quantized.
3. SIGTERM drains the server, which exits 0.

The whole test takes about 8 s.

### 7.3 Load test

Run it against a server you started:

```sh
cargo build --profile server -p multiplayer_arena --no-default-features --bin server --bin loadtest
SPACE3D__MATCHMAKING__QUEUES='[{ name = "arena", mode = "arena", teams = 1, team_size = 20, min_players = 1, backfill = true }]' \
  target/server/server &
target/server/loadtest --ps http://127.0.0.1:7777 --queue arena --quick-play --clients 200 --ramp 50 \
  --duration 20 --metrics http://127.0.0.1:9090/metrics --slo-tick-p99-ms 4
```

Or run it as a test, which starts the server itself:
`cargo test -p multiplayer_arena --release --test load -- --ignored --nocapture`
(`LOAD_CLIENTS`, `LOAD_SECS`).

The load test prints a JSON report. Exit codes: 0 means every SLO held, 1 means an SLO
failed, 2 means a setup error.

Recorded run: 200 clients, 20 s after a 4 s ramp. The server was built with
`--profile server` on an M4 Max, with the bots on the same machine.

| | result |
|---|---|
| connected | 200 / 200, 10 rooms, no disconnects |
| handshake (connect → synced) | p50 36 ms, p99 53 ms |
| RTT (loopback) | p50 17 ms, p99 24 ms |
| per client down / up | p50 7.8 / 0.81 KB/s, p99 8.2 / 0.85 KB/s |
| snapshot rate | 100 % of nominal (30 Hz) |
| rollbacks | 0.06 per client per second |
| server room tick | mean 0.084 ms, p50 ≤ 0.25 ms, p99 ≤ 0.5 ms (histogram buckets), 13 336 ticks, 0 skipped |
| server replication traffic | 1.39 MB/s out, 0.15 MB/s in |
| server process | 55 MB RSS; `ps` reported 29 % CPU (average over the process lifetime) |

The unoptimised test profile (`cargo test` without `--release`) showed a tick mean of
0.19 ms, a p99 ≤ 1 ms, and 3 skipped ticks out of 13 335.

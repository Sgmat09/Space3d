# Networking

`space3d-net` (facade feature `net`, on by default) provides HTTP and WebSocket clients on every
platform and HTTP/WebSocket servers natively. All I/O runs on the background runtime from
`space3d-tasks`; engine systems only poll tasks and channels, so **a slow network never stalls a
frame**.

| piece | native | web |
|---|---|---|
| `Http` client | reqwest + rustls | `fetch` |
| `WsClient` / `WsConnection` | tokio-tungstenite + rustls | browser `WebSocket` |
| `HttpServer`, `WsServer` | axum + tower-http | — |
| `Channel<T>`, `Codec`, `Frame` | ✓ | ✓ |

`NetPlugin` (part of `DefaultPlugins`) inserts `Http` and `WsClient`, runs HTTP callbacks and emits
`Events<WsEvent>` in `PreUpdate`, and — when a `WsServer` resource exists — `Events<ServerEvent>`.

## HTTP

```rust
#[derive(serde::Deserialize)]
struct Motd { text: String }

// Callback style: runs on the main thread with full SystemCtx access.
ctx.resources.expect::<Http>().get_json_then("https://example.com/motd.json", |ctx, r: Result<Motd, NetError>| {
    if let Ok(motd) = r { ctx.resources.insert(motd.text); }
});

// Task style: poll it from a system.
let mut task = http.get("https://example.com/level/3.json");
if let Some(result) = task.poll() { /* … */ }
```

Requests support headers, JSON bodies, timeouts and retry policies (`HttpRequest::…`).
`Http::fetch` exposes the raw future for composing inside other async code — e.g. an asset source
that loads over HTTP (see `examples/async_loading`).

## WebSocket client

```rust
let chat = ctx.resources.expect_mut::<WsClient>().open_with(
    "wss://example.com/chat",
    WsConfig::default().reconnect(ReconnectPolicy::default()),
);
ctx.resources.expect::<WsClient>().send(chat, "hello")?;   // queued until the socket opens

// Every frame:
let mut reader = EventReader::default();
app.add_system(Stage::Update, move |ctx| {
    for event in reader.read(ctx.resources.expect::<Events<WsEvent>>()) {
        match &event.kind {
            WsEventKind::Opened => { /* (re)send hello */ }
            WsEventKind::Message(m) => { /* m.as_text() / m.as_bytes() */ }
            WsEventKind::Closed { reason, reconnecting } => { /* … */ }
        }
    }
});
```

After a reconnect you get another `Opened` — send your handshake there, not only at start-up.

## Servers (native)

```rust
// Static files + JSON API + WebSockets on one port.
let ws = WsServer::new();
let handle = HttpServer::new()
    .bind(([0, 0, 0, 0], 8080))
    .serve_spa("dist/web")
    .get_json("/api/status", || serde_json::json!({ "ok": true }))
    .ws("/ws", &ws)
    .cross_origin_isolated()          // COOP/COEP for SharedArrayBuffer
    .start()?;                        // binds synchronously; serves on the background runtime
app.insert_resource(ws);              // NetPlugin → Events<ServerEvent>
app.insert_resource(handle);          // dropping the handle shuts the server down
```

A bare WebSocket server: `WsServer::new().listen(addr)?`. `WsServer` has `send`, `broadcast`,
`broadcast_except`, `disconnect`, `clients`. `ServerEvent::{Connected, Message, Disconnected}`
arrive in order per client.

`space3d serve` is built on the same `HttpServer` (`serve_spa` + `cross_origin_isolated` +
`no_cache`).

## Multiplayer architecture

The `multiplayer` template (`space3d new party --template multiplayer`) is a complete,
server-authoritative example:

* **Protocol**: JSON text frames — `ClientMsg::{Hello, Input}` up, `ServerMsg::{Welcome,
  Snapshot}` down; a pure `step(position, input, dt)` movement rule shared by everyone.
* **Server** (`ServerPlugin`): keeps players in a resource keyed by `ClientId`, simulates them on
  `FixedUpdate`, broadcasts a snapshot every 3 ticks (20 Hz at 60 Hz).
* **Client** (`ClientPlugin`): sends input when it changes (plus a keep-alive), applies snapshots
  to replica entities and eases them towards the latest state; re-sends `Hello` after every
  (re)connect.
* **Modes**: `cargo run` = listen server (server + client in one process); `cargo run --bin
  server` = dedicated headless server (`MinimalPlugins`, no window); `--connect ws://host:7777`
  joins. The web build is client-only.
* **Tests**: a real server and client in one headless `TestApp`, stepping with short sleeps until
  the key press has moved the player on the server and the replica followed.

For typed, sequenced, tick-stamped messages over any transport, see `space3d_net::channel`
(`Channel<T>` with a `Codec`, including a snapshot/input replication example in its docs).

`examples/multiplayer_chat` shows the smallest possible server + clients (`default-features =
false, features = ["net"]` — no window or renderer compiled in).

//! WebSocket chat: a [`WsServer`] and [`WsClient`]s in one binary, all driven by engine
//! systems that only poll (the frame never waits on the network).
//!
//! ```text
//! cargo run -p multiplayer_chat                                   # demo: server + 2 bots in-process, exits when done
//! cargo run -p multiplayer_chat -- --server --port 9001          # dedicated server
//! cargo run -p multiplayer_chat -- --client ws://127.0.0.1:9001 --name alice   # type lines on stdin
//! ```
//!
//! Protocol (text frames): a client first sends `/nick NAME`, then chat lines. The server
//! broadcasts `[NAME] text`, plus `* NAME joined` / `* NAME left` notices.
//!
//! This example only enables the `net` feature of `space3d` (`default-features = false`):
//! no window, renderer or audio is compiled in.

use space3d::prelude::*;
use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::mpsc;
use std::time::Duration;

/// What to run.
#[derive(Clone, Debug, PartialEq)]
enum Mode {
    Demo,
    Server { port: u16 },
    Client { url: String, name: String },
}

fn parse_args(args: &[String]) -> Mode {
    let value = |flag: &str| {
        args.iter()
            .position(|a| a == flag)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    if args.iter().any(|a| a == "--server") {
        Mode::Server {
            port: value("--port").and_then(|p| p.parse().ok()).unwrap_or(9001),
        }
    } else if args.iter().any(|a| a == "--client") {
        let url = value("--client")
            .filter(|u| u.starts_with("ws"))
            .unwrap_or_else(|| "ws://127.0.0.1:9001".into());
        Mode::Client {
            url,
            name: value("--name").unwrap_or_else(|| "guest".into()),
        }
    } else {
        Mode::Demo
    }
}

fn build(app: &mut App) {
    let args: Vec<String> = std::env::args().skip(1).collect();
    app.add_plugin(MinimalPlugins::new().tick_rate(60.0));
    app.add_plugin(NetPlugin::default());
    match parse_args(&args) {
        Mode::Demo => {
            let url = start_server(app, 0);
            add_bot(app, &url, "alice", &["hi bob!", "the frame loop never blocks", "bye"]);
            add_bot(app, &url, "bob", &["hey alice", "neither does mine", "see you"]);
            app.add_system(Stage::Update, demo_referee);
        }
        Mode::Server { port } => {
            start_server(app, port);
        }
        Mode::Client { url, name } => add_stdin_client(app, &url, &name),
    }
}

// ------------------------------------------------------------------------------------ server

/// Keeps the server alive (dropping the handle shuts it down) and tracks nicknames.
struct ChatServer {
    _handle: ServerHandle,
    names: HashMap<ClientId, String>,
}

/// Bind the WebSocket server; returns its `ws://` URL.
fn start_server(app: &mut App, port: u16) -> String {
    let server = WsServer::new();
    let handle = server
        .listen(([127, 0, 0, 1], port))
        .unwrap_or_else(|e| panic!("cannot listen on port {port}: {e}"));
    let url = handle.ws_url("/");
    println!("server listening on {url}");
    // NetPlugin turns a `WsServer` resource's inbox into `Events<ServerEvent>` every PreUpdate.
    app.insert_resource(server);
    app.insert_resource(ChatServer {
        _handle: handle,
        names: HashMap::new(),
    });
    let mut reader = EventReader::default();
    app.add_system(Stage::Update, move |ctx| {
        let events: Vec<ServerEvent> = reader
            .read(ctx.resources.expect::<Events<ServerEvent>>())
            .cloned()
            .collect();
        for event in events {
            handle_server_event(ctx, event);
        }
    });
    url
}

fn handle_server_event(ctx: &mut SystemCtx, event: ServerEvent) {
    let server = ctx.resources.expect::<WsServer>().clone();
    let chat = ctx.resources.expect_mut::<ChatServer>();
    match event {
        ServerEvent::Connected { client, addr } => {
            println!("[server] client {} connected from {addr}", client.0);
        }
        ServerEvent::Message { client, message } => {
            let Some(text) = message.as_text() else { return };
            if let Some(nick) = text.strip_prefix("/nick ") {
                let nick = nick.trim().to_owned();
                server.broadcast(format!("* {nick} joined"));
                chat.names.insert(client, nick);
            } else {
                let name = chat
                    .names
                    .get(&client)
                    .cloned()
                    .unwrap_or_else(|| format!("#{}", client.0));
                server.broadcast(format!("[{name}] {text}"));
            }
        }
        ServerEvent::Disconnected { client, reason } => {
            let name = chat.names.remove(&client).unwrap_or_else(|| format!("#{}", client.0));
            println!("[server] {name} left ({reason})");
            server.broadcast_except(client, format!("* {name} left"));
        }
    }
}

// ------------------------------------------------------------------------------------ clients

/// A scripted client used by the demo.
struct Bot {
    name: String,
    connection: ConnectionId,
    script: Vec<String>,
    next_line: usize,
    received: Vec<String>,
    open: bool,
}

/// All demo bots.
#[derive(Default)]
struct Bots(Vec<Bot>);

fn add_bot(app: &mut App, url: &str, name: &str, script: &[&str]) {
    let connection = app.resources.expect_mut::<WsClient>().open(url);
    // Queued until the socket opens.
    let _ = app
        .resources
        .expect::<WsClient>()
        .send(connection, format!("/nick {name}"));
    let bots = app.resources.init::<Bots>();
    bots.0.push(Bot {
        name: name.into(),
        connection,
        script: script.iter().map(|s| s.to_string()).collect(),
        next_line: 0,
        received: Vec::new(),
        open: false,
    });
    if bots.0.len() == 1 {
        let mut reader = EventReader::default();
        app.add_system(Stage::Update, move |ctx| {
            let events: Vec<WsEvent> = reader
                .read(ctx.resources.expect::<Events<WsEvent>>())
                .cloned()
                .collect();
            let bots = ctx.resources.expect_mut::<Bots>();
            for event in events {
                let Some(bot) = bots.0.iter_mut().find(|b| b.connection == event.connection) else {
                    continue;
                };
                match event.kind {
                    WsEventKind::Opened => bot.open = true,
                    WsEventKind::Message(m) => bot.received.extend(m.as_text().map(str::to_owned)),
                    WsEventKind::Closed { reason, .. } => println!("[{}] disconnected: {reason}", bot.name),
                }
            }
        });
        // Every half second each bot says its next line.
        app.add_system(Stage::FixedUpdate, |ctx| {
            if !ctx.time().tick.is_multiple_of(30) {
                return;
            }
            let mut outgoing = Vec::new();
            for bot in ctx.resources.expect_mut::<Bots>().0.iter_mut().filter(|b| b.open) {
                if let Some(line) = bot.script.get(bot.next_line) {
                    outgoing.push((bot.connection, line.clone()));
                    bot.next_line += 1;
                }
            }
            let client = ctx.resources.expect::<WsClient>();
            for (connection, line) in outgoing {
                let _ = client.send(connection, line);
            }
        });
    }
}

/// Demo: exit once every bot has seen every chat line (or give up after 15 s).
fn demo_referee(ctx: &mut SystemCtx) {
    let elapsed = ctx.time().elapsed;
    let bots = ctx.resources.expect::<Bots>();
    let total_lines: usize = bots.0.iter().map(|b| b.script.len()).sum();
    let done = bots
        .0
        .iter()
        .all(|b| b.received.iter().filter(|l| l.starts_with('[')).count() >= total_lines);
    if done || elapsed > Duration::from_secs(15) {
        for bot in &bots.0 {
            println!("--- {} saw:", bot.name);
            for line in &bot.received {
                println!("    {line}");
            }
        }
        if done {
            println!("demo finished: {total_lines} chat lines delivered to every client");
        } else {
            eprintln!("demo timed out");
        }
        ctx.send(AppExit);
    }
}

/// Interactive client: lines typed on stdin are sent; received lines are printed.
fn add_stdin_client(app: &mut App, url: &str, name: &str) {
    let connection = app
        .resources
        .expect_mut::<WsClient>()
        .open_with(url, WsConfig::default().reconnect(ReconnectPolicy::default()));
    let _ = app
        .resources
        .expect::<WsClient>()
        .send(connection, format!("/nick {name}"));
    println!("connecting to {url} as {name}; type a message and press enter (Ctrl-D quits)");

    // Blocking stdin reads live on their own thread; the engine only polls the channel.
    let (tx, rx) = mpsc::channel::<Option<String>>();
    std::thread::spawn(move || {
        for line in std::io::stdin().lines() {
            let Ok(line) = line else { break };
            if tx.send(Some(line)).is_err() {
                return;
            }
        }
        let _ = tx.send(None);
    });
    let rx = Mutex::new(rx);
    let mut reader = EventReader::default();
    app.add_system(Stage::Update, move |ctx| {
        for event in reader.read(ctx.resources.expect::<Events<WsEvent>>()) {
            match &event.kind {
                WsEventKind::Opened => println!("* connected"),
                WsEventKind::Message(m) => println!("{}", m.as_text().unwrap_or("<binary>")),
                WsEventKind::Closed { reason, reconnecting } => {
                    println!(
                        "* disconnected: {reason}{}",
                        if *reconnecting { " (reconnecting)" } else { "" }
                    )
                }
            }
        }
        let mut quit = false;
        while let Ok(line) = rx.lock().expect("stdin channel").try_recv() {
            match line {
                Some(line) if !line.trim().is_empty() => {
                    let _ = ctx.resources.expect::<WsClient>().send(connection, line);
                }
                Some(_) => {}
                None => quit = true,
            }
        }
        if quit {
            ctx.send(AppExit);
        }
    });
}

space3d::main!(build);

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn modes() {
        assert_eq!(parse_args(&[]), Mode::Demo);
        assert_eq!(parse_args(&args("--server --port 7000")), Mode::Server { port: 7000 });
        assert_eq!(
            parse_args(&args("--client ws://h:1 --name zed")),
            Mode::Client {
                url: "ws://h:1".into(),
                name: "zed".into()
            }
        );
        assert_eq!(
            parse_args(&args("--client --name x")),
            Mode::Client {
                url: "ws://127.0.0.1:9001".into(),
                name: "x".into()
            }
        );
    }

    #[test]
    fn demo_delivers_every_line() {
        let mut app = App::new();
        app.add_plugin(MinimalPlugins::new());
        app.add_plugin(NetPlugin::default());
        let url = start_server(&mut app, 0);
        add_bot(&mut app, &url, "a", &["one", "two"]);
        add_bot(&mut app, &url, "b", &["three"]);
        app.add_system(Stage::Update, demo_referee);
        for _ in 0..3000 {
            app.step_ticks(1);
            if app.should_exit() {
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        let bots = app.resources.expect::<Bots>();
        for bot in &bots.0 {
            let chat: Vec<&String> = bot.received.iter().filter(|l| l.starts_with('[')).collect();
            assert_eq!(chat.len(), 3, "{} saw {:?}", bot.name, bot.received);
        }
    }
}

use crate::{pump_until, wait_until};
use space3d_core::prelude::*;
use space3d_net::prelude::*;
use std::net::SocketAddr;
use std::time::Duration;

const LOCALHOST_ANY_PORT: ([u8; 4], u16) = ([127, 0, 0, 1], 0);

/// Server-side engine: `WsServer` as a resource, echo every message from an `Update` system.
fn echo_server_app(ws: &WsServer) -> App {
    let mut app = App::new();
    app.add_plugin(NetPlugin::default());
    app.insert_resource(ws.clone());
    let mut reader = EventReader::default();
    app.add_system(Stage::Update, move |ctx| {
        let events: Vec<ServerEvent> = reader
            .read(ctx.resources.expect::<Events<ServerEvent>>())
            .cloned()
            .collect();
        let ws = ctx.resources.expect::<WsServer>();
        for e in events {
            if let ServerEvent::Message { client, message } = e {
                ws.send(client, message).unwrap();
            }
        }
    });
    app
}

#[test]
fn echo_roundtrip_text_and_binary() {
    let ws = WsServer::new();
    let handle = ws.listen(LOCALHOST_ANY_PORT).unwrap();
    let mut server = echo_server_app(&ws);

    let conn = WsClient::connect(handle.ws_url("/game"));
    assert_eq!(conn.state(), WsState::Connecting);
    // Queued while connecting, delivered once open.
    conn.send("hello").unwrap();
    conn.send(vec![1u8, 2, 3]).unwrap();

    let mut got = Vec::new();
    pump_until(&mut server, "echo", |_| {
        got.extend(conn.drain());
        got.len() >= 2
    });
    assert_eq!(got, vec![Message::Text("hello".into()), Message::Binary(vec![1, 2, 3])]);
    assert!(conn.is_open());
    assert_eq!(ws.client_count(), 1);

    conn.close();
    pump_until(&mut server, "server sees disconnect", |_| ws.client_count() == 0);
    wait_until("client closed", || conn.is_closed());
    assert_eq!(conn.send("late"), Err(NetError::Closed));
}

#[test]
fn broadcast_to_three_clients() {
    let ws = WsServer::new();
    let handle = ws.listen(LOCALHOST_ANY_PORT).unwrap();
    let clients: Vec<WsConnection> = (0..3).map(|_| WsClient::connect(handle.ws_url("/"))).collect();
    wait_until("3 clients connected", || {
        ws.client_count() == 3 && clients.iter().all(WsConnection::is_open)
    });

    let connected: Vec<ClientId> = ws
        .drain()
        .into_iter()
        .filter_map(|e| match e {
            ServerEvent::Connected { client, addr } => {
                assert!(addr.ip().is_loopback());
                Some(client)
            }
            _ => None,
        })
        .collect();
    assert_eq!(connected, ws.clients());

    let mut inbox: Vec<Vec<Message>> = vec![Vec::new(); 3];
    let collect = |inbox: &mut Vec<Vec<Message>>, per_client: usize| {
        wait_until("messages delivered", || {
            for (i, c) in clients.iter().enumerate() {
                inbox[i].extend(c.drain());
            }
            inbox.iter().map(Vec::len).sum::<usize>() == per_client
        });
    };

    assert_eq!(ws.broadcast("tick 1"), 3);
    collect(&mut inbox, 3);
    assert!(inbox.iter().all(|m| m == &[Message::Text("tick 1".into())]));

    let target = connected[0];
    assert_eq!(ws.broadcast_except(target, "not you"), 2);
    ws.send(target, "just you").unwrap();
    collect(&mut inbox, 6);
    let just: Vec<_> = inbox
        .iter()
        .filter(|m| m.contains(&Message::Text("just you".into())))
        .collect();
    assert_eq!(just.len(), 1);
    assert!(!just[0].contains(&Message::Text("not you".into())));
    assert_eq!(
        inbox
            .iter()
            .filter(|m| m.contains(&Message::Text("not you".into())))
            .count(),
        2
    );

    assert_eq!(ws.send(ClientId(u64::MAX), "x"), Err(NetError::UnknownPeer(u64::MAX)));
    ws.disconnect_all("bye");
    wait_until("all kicked", || {
        ws.client_count() == 0 && clients.iter().all(WsConnection::is_closed)
    });
}

fn listen_retrying(ws: &WsServer, addr: SocketAddr) -> ServerHandle {
    for _ in 0..200 {
        if let Ok(h) = ws.listen(addr) {
            return h;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("could not rebind {addr}");
}

#[test]
fn client_reconnects_after_server_restart() {
    let ws = WsServer::new();
    let handle = ws.listen(LOCALHOST_ANY_PORT).unwrap();
    let addr = handle.local_addr();
    let policy = ReconnectPolicy::new(Backoff::new(Duration::from_millis(10), Duration::from_millis(40)));
    let conn = WsClient::connect_with(handle.ws_url("/"), WsConfig::default().reconnect(policy));
    wait_until("first open", || conn.is_open());

    handle.shutdown();
    handle.wait().unwrap();
    wait_until("client notices the outage", || !conn.is_open());
    assert_eq!(conn.state(), WsState::Connecting, "reconnect policy keeps it alive");

    let _restarted = listen_retrying(&ws, addr);
    wait_until("reconnected", || conn.is_open() && ws.client_count() == 1);

    conn.send("again").unwrap();
    wait_until("message after reconnect", || {
        ws.drain()
            .iter()
            .any(|e| matches!(e, ServerEvent::Message { message, .. } if message.as_text() == Some("again")))
    });

    let events: Vec<WsEventKind> = std::iter::from_fn(|| conn.poll_event()).collect();
    assert_eq!(events.len(), 3, "{events:?}");
    assert_eq!(events[0], WsEventKind::Opened);
    assert!(matches!(&events[1], WsEventKind::Closed { reconnecting: true, reason } if reason.contains("1001")));
    assert_eq!(events[2], WsEventKind::Opened);
}

#[test]
fn ws_events_system_tags_each_connection() {
    let ws = WsServer::new();
    let handle = ws.listen(LOCALHOST_ANY_PORT).unwrap();
    let mut server = echo_server_app(&ws);

    #[derive(Default)]
    struct Seen(Vec<WsEvent>);
    let mut app = App::new();
    app.add_plugin(NetPlugin::default());
    app.init_resource::<Seen>();
    let mut reader = EventReader::default();
    app.add_system(Stage::Update, move |ctx| {
        let evs: Vec<WsEvent> = reader
            .read(ctx.resources.expect::<Events<WsEvent>>())
            .cloned()
            .collect();
        ctx.resources.expect_mut::<Seen>().0.extend(evs);
    });

    let client = app.resources.expect_mut::<WsClient>();
    let a = client.open(handle.ws_url("/a"));
    let b = client.open(handle.ws_url("/b"));
    assert_ne!(a, b);
    client.send(a, "from a").unwrap();
    client.send(b, "from b").unwrap();
    assert!(client.send(ConnectionId(u64::MAX), "x").is_err());

    let messages = |app: &App| {
        app.resources
            .expect::<Seen>()
            .0
            .iter()
            .filter(|e| matches!(e.kind, WsEventKind::Message(_)))
            .count()
    };
    pump_until(&mut app, "both echoes", |app| {
        server.step_ticks(1);
        messages(app) == 2
    });

    let seen = &app.resources.expect::<Seen>().0;
    for (id, text) in [(a, "from a"), (b, "from b")] {
        let kinds: Vec<&WsEventKind> = seen.iter().filter(|e| e.connection == id).map(|e| &e.kind).collect();
        assert_eq!(
            kinds,
            vec![&WsEventKind::Opened, &WsEventKind::Message(Message::Text(text.into()))]
        );
    }
}

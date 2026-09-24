//! Server-authoritative input/snapshot loop over typed channels, both codecs.

use crate::pump_until;
use serde::{Deserialize, Serialize};
use space3d_core::prelude::*;
use space3d_net::prelude::*;
use std::collections::BTreeMap;

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Default)]
struct Input {
    dx: i32,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
struct Snapshot {
    pos: i32,
}

struct Sim {
    codec: Codec,
    pos: BTreeMap<ClientId, i32>,
    inputs: BTreeMap<ClientId, Channel<Input>>,
    snapshots: BTreeMap<ClientId, Channel<Snapshot>>,
    /// (seq, tick) of every input frame the server decoded.
    received: Vec<(u64, u64)>,
}

fn server_app(ws: &WsServer, codec: Codec) -> App {
    let mut app = App::new();
    app.add_plugin(NetPlugin::default());
    app.insert_resource(ws.clone());
    app.insert_resource(Sim {
        codec,
        pos: BTreeMap::new(),
        inputs: BTreeMap::new(),
        snapshots: BTreeMap::new(),
        received: Vec::new(),
    });
    let mut reader = EventReader::default();
    app.add_system(Stage::FixedUpdate, move |ctx| {
        let tick = ctx.time().tick;
        let events: Vec<ServerEvent> = reader
            .read(ctx.resources.expect::<Events<ServerEvent>>())
            .cloned()
            .collect();
        ctx.resources.scope::<Sim, _>(|res, sim| {
            let ws = res.expect::<WsServer>();
            for ev in events {
                match ev {
                    ServerEvent::Connected { client, .. } => {
                        sim.pos.insert(client, 0);
                        sim.inputs.insert(client, Channel::new(sim.codec));
                        sim.snapshots.insert(client, Channel::new(sim.codec));
                    }
                    ServerEvent::Message { client, message } => {
                        let frame = sim.inputs.get_mut(&client).unwrap().decode(&message).unwrap();
                        sim.received.push((frame.seq, frame.tick));
                        *sim.pos.get_mut(&client).unwrap() += frame.payload.dx;
                    }
                    ServerEvent::Disconnected { client, .. } => {
                        sim.pos.remove(&client);
                        sim.inputs.remove(&client);
                        sim.snapshots.remove(&client);
                    }
                }
            }
            for (client, ch) in sim.snapshots.iter_mut() {
                ch.set_ack(sim.inputs[client].last_received());
                ch.send_to(ws, *client, tick, &Snapshot { pos: sim.pos[client] })
                    .unwrap();
            }
        });
    });
    app
}

struct Link {
    conn: WsConnection,
    inputs: Channel<Input>,
    snapshots: Channel<Snapshot>,
    frames: Vec<Frame<Snapshot>>,
}

fn client_app(url: String, codec: Codec) -> App {
    let mut app = App::new();
    app.add_plugin(NetPlugin::default());
    app.insert_resource(Link {
        conn: WsClient::connect(url),
        inputs: Channel::new(codec),
        snapshots: Channel::new(codec),
        frames: Vec::new(),
    });
    app.add_system(Stage::FixedUpdate, |ctx| {
        let tick = ctx.time().tick;
        let link = ctx.resources.expect_mut::<Link>();
        if tick < 5 {
            let seq = link.inputs.send(&link.conn, tick, &Input { dx: 1 }).unwrap();
            assert_eq!(seq, tick + 1);
        }
        let new = link.snapshots.drain(&link.conn);
        link.frames.extend(new);
    });
    app
}

fn authoritative_loop(codec: Codec) {
    let ws = WsServer::new();
    let handle = ws.listen(([127, 0, 0, 1], 0)).unwrap();
    let mut server = server_app(&ws, codec);
    let mut client = client_app(handle.ws_url("/play"), codec);

    pump_until(&mut client, "all inputs acked", |client| {
        server.step_ticks(1);
        client
            .resources
            .expect::<Link>()
            .frames
            .last()
            .is_some_and(|f| f.ack == 5)
    });

    let link = client.resources.expect::<Link>();
    let last = link.frames.last().unwrap();
    assert_eq!(last.payload, Snapshot { pos: 5 }, "server applied all 5 inputs");
    // Sequenced, gap-free, tick-stamped by the server.
    for (i, f) in link.frames.iter().enumerate() {
        assert_eq!(f.seq, i as u64 + 1);
    }
    assert!(link.frames.windows(2).all(|w| w[1].tick > w[0].tick));
    assert!(link.frames.windows(2).all(|w| w[1].ack >= w[0].ack));
    assert_eq!(link.snapshots.last_received(), last.seq);

    let sim = server.resources.expect::<Sim>();
    assert_eq!(sim.received, (1..=5).map(|s| (s, s - 1)).collect::<Vec<_>>());
}

#[test]
fn typed_channel_json_roundtrip() {
    authoritative_loop(Codec::Json);
}

#[test]
fn typed_channel_binary_roundtrip() {
    authoritative_loop(Codec::Binary);
}

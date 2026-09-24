//! Typed game-netcode channels over WebSockets.
//!
//! * [`Codec`] — stateless serde encoding of any value into a [`Message`]:
//!   JSON text (debuggable, works with any serde type) or compact binary
//!   ([postcard](https://docs.rs/postcard); smaller and faster, but no
//!   self-describing features such as `#[serde(flatten)]`, untagged enums or
//!   `serde_json::Value`).
//! * [`Frame`] — the wire envelope: sequence number, ack of the peer's latest
//!   sequence number, simulation tick ([`space3d_core::Time::tick`]) and payload.
//! * [`Channel`] — per-peer, per-direction bookkeeping of `seq`/`ack` on top.
//!
//! Use one `Channel<T>` per peer and message type (use an enum `T` to multiplex
//! several kinds of message over one connection). After a reconnect the peer's
//! sequence restarts, so call [`Channel::reset`] on [`crate::WsEventKind::Opened`].
//!
//! # Pattern: server-authoritative tick loop with input/snapshot replication
//!
//! Clients never simulate authoritatively; they send their **inputs** stamped
//! with the tick they were sampled on, and the server runs the simulation in
//! `FixedUpdate` and broadcasts **snapshots** of the resulting state. A
//! snapshot's `ack` tells the client which of its inputs have been applied, so
//! it can drop them from its prediction buffer and re-simulate the rest.
//!
//! ```no_run
//! use serde::{Deserialize, Serialize};
//! use space3d_core::prelude::*;
//! use space3d_net::prelude::*;
//! use std::collections::BTreeMap;
//!
//! #[derive(Serialize, Deserialize, Clone, Copy, Default, Debug)]
//! struct Input { dx: f32, dy: f32 }
//!
//! #[derive(Serialize, Deserialize, Clone, Default, Debug)]
//! struct Snapshot { players: Vec<(u64, [f32; 2])> }
//!
//! // ---------------- server ----------------
//! #[derive(Default)]
//! struct Sim {
//!     players: BTreeMap<ClientId, ([f32; 2], Input)>,
//!     inputs: BTreeMap<ClientId, Channel<Input>>,
//!     snapshots: BTreeMap<ClientId, Channel<Snapshot>>,
//! }
//!
//! let ws = WsServer::new();
//! let _handle = ws.listen(([0, 0, 0, 0], 7777)).unwrap();
//! let mut server = App::new().with_tickrate(30.0);
//! server.add_plugin(NetPlugin::default());
//! server.insert_resource(ws); // → Events<ServerEvent> every PreUpdate
//! server.init_resource::<Sim>();
//! let mut reader = EventReader::default();
//! server.add_system(Stage::FixedUpdate, move |ctx| {
//!     let tick = ctx.time().tick;
//!     let events: Vec<ServerEvent> = reader.read(ctx.resources.expect::<Events<ServerEvent>>()).cloned().collect();
//!     ctx.resources.scope::<Sim, _>(|res, sim| {
//!         let ws = res.expect::<WsServer>();
//!         // 1. apply the latest input of every client
//!         for ev in events {
//!             match ev {
//!                 ServerEvent::Connected { client, .. } => {
//!                     sim.players.insert(client, ([0.0; 2], Input::default()));
//!                     sim.inputs.insert(client, Channel::binary());
//!                     sim.snapshots.insert(client, Channel::binary());
//!                 }
//!                 ServerEvent::Message { client, message } => {
//!                     let (Some(ch), Some(p)) = (sim.inputs.get_mut(&client), sim.players.get_mut(&client)) else { continue };
//!                     if let Ok(frame) = ch.decode(&message) {
//!                         p.1 = frame.payload;
//!                     }
//!                 }
//!                 ServerEvent::Disconnected { client, .. } => {
//!                     sim.players.remove(&client);
//!                     sim.inputs.remove(&client);
//!                     sim.snapshots.remove(&client);
//!                 }
//!             }
//!         }
//!         // 2. step the authoritative simulation
//!         for (pos, input) in sim.players.values_mut() {
//!             pos[0] += input.dx;
//!             pos[1] += input.dy;
//!         }
//!         // 3. replicate: one snapshot per client so each carries that client's input ack
//!         let snap = Snapshot { players: sim.players.iter().map(|(id, (p, _))| (id.0, *p)).collect() };
//!         for (client, ch) in sim.snapshots.iter_mut() {
//!             ch.set_ack(sim.inputs[client].last_received());
//!             let _ = ch.send_to(ws, *client, tick, &snap);
//!         }
//!     });
//! });
//!
//! // ---------------- client ----------------
//! struct Link { conn: WsConnection, inputs: Channel<Input>, snapshots: Channel<Snapshot>, latest: Option<Frame<Snapshot>> }
//! let mut client = App::new().with_tickrate(30.0);
//! client.add_plugin(NetPlugin::default());
//! client.insert_resource(Link {
//!     conn: WsClient::connect_with("ws://127.0.0.1:7777/", WsConfig::default().reconnect(ReconnectPolicy::default())),
//!     inputs: Channel::binary(),
//!     snapshots: Channel::binary(),
//!     latest: None,
//! });
//! client.add_system(Stage::FixedUpdate, |ctx| {
//!     let tick = ctx.time().tick;
//!     let link = ctx.resources.expect_mut::<Link>();
//!     // sample + send this tick's input (buffered while connecting)
//!     let _ = link.inputs.send(&link.conn, tick, &Input { dx: 1.0, dy: 0.0 });
//!     // keep only the newest authoritative state; `frame.ack` = last input the server applied
//!     for frame in link.snapshots.drain(&link.conn) {
//!         link.latest = Some(frame);
//!     }
//! });
//! ```

use crate::{Message, NetError, WsConnection};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::marker::PhantomData;

/// Wire format for [`Channel`]s (and any standalone serde value).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Codec {
    /// `serde_json` in a text message.
    #[default]
    Json,
    /// `postcard` in a binary message (compact, not self-describing).
    Binary,
}

impl Codec {
    /// Serialize `value` into a message of this codec's kind.
    pub fn encode<T: Serialize + ?Sized>(self, value: &T) -> Result<Message, NetError> {
        match self {
            Codec::Json => serde_json::to_string(value)
                .map(Message::Text)
                .map_err(|e| NetError::Encode(e.to_string())),
            Codec::Binary => postcard::to_stdvec(value)
                .map(Message::Binary)
                .map_err(|e| NetError::Encode(e.to_string())),
        }
    }

    /// Deserialize a message. JSON accepts text or binary frames; binary requires a binary frame.
    pub fn decode<T: DeserializeOwned>(self, msg: &Message) -> Result<T, NetError> {
        match (self, msg) {
            (Codec::Json, Message::Text(s)) => serde_json::from_str(s).map_err(|e| NetError::Decode(e.to_string())),
            (Codec::Json, Message::Binary(b)) => serde_json::from_slice(b).map_err(|e| NetError::Decode(e.to_string())),
            (Codec::Binary, Message::Binary(b)) => postcard::from_bytes(b).map_err(|e| NetError::Decode(e.to_string())),
            (Codec::Binary, Message::Text(_)) => Err(NetError::Decode("binary codec received a text message".into())),
        }
    }
}

/// Envelope for every message sent through a [`Channel`].
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Frame<T> {
    /// Sender's sequence number on this channel, starting at 1.
    pub seq: u64,
    /// Highest `seq` the sender had received from us at send time (0 = none yet).
    pub ack: u64,
    /// Sender's simulation tick ([`space3d_core::Time::tick`]) the payload belongs to.
    pub tick: u64,
    /// The message.
    pub payload: T,
}

/// Typed, sequenced message stream to/from one peer.
///
/// `Channel` is plain data (no I/O of its own), so it is `Send + Sync` for any
/// `T` and can live in a resource or a per-client map.
pub struct Channel<T> {
    codec: Codec,
    next_seq: u64,
    last_received: u64,
    last_tick: Option<u64>,
    _marker: PhantomData<fn(T) -> T>,
}

impl<T> std::fmt::Debug for Channel<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Channel")
            .field("type", &std::any::type_name::<T>())
            .field("codec", &self.codec)
            .field("next_seq", &self.next_seq)
            .field("last_received", &self.last_received)
            .finish()
    }
}

impl<T> Default for Channel<T> {
    fn default() -> Self {
        Self::new(Codec::default())
    }
}

impl<T> Channel<T> {
    /// A fresh channel using `codec`.
    pub fn new(codec: Codec) -> Self {
        Self {
            codec,
            next_seq: 1,
            last_received: 0,
            last_tick: None,
            _marker: PhantomData,
        }
    }
    /// JSON text frames.
    pub fn json() -> Self {
        Self::new(Codec::Json)
    }
    /// Compact binary (postcard) frames.
    pub fn binary() -> Self {
        Self::new(Codec::Binary)
    }
    /// The codec in use.
    pub fn codec(&self) -> Codec {
        self.codec
    }
    /// Sequence number the next sent frame will carry.
    pub fn next_seq(&self) -> u64 {
        self.next_seq
    }
    /// Highest sequence number received from the peer (0 = none). Sent back as `ack`.
    pub fn last_received(&self) -> u64 {
        self.last_received
    }
    /// Tick of the most recently decoded frame.
    pub fn last_tick(&self) -> Option<u64> {
        self.last_tick
    }
    /// Override the `ack` sent with the next frames — useful when the acknowledged
    /// stream arrives on a different channel (e.g. snapshots acking inputs).
    pub fn set_ack(&mut self, ack: u64) {
        self.last_received = ack;
    }
    /// Forget all sequence state (call after a reconnect).
    pub fn reset(&mut self) {
        self.next_seq = 1;
        self.last_received = 0;
        self.last_tick = None;
    }
}

impl<T: Serialize + DeserializeOwned> Channel<T> {
    /// Wrap `payload` in the next [`Frame`] and encode it.
    pub fn encode(&mut self, tick: u64, payload: &T) -> Result<Message, NetError> {
        #[derive(Serialize)]
        struct FrameRef<'a, P> {
            seq: u64,
            ack: u64,
            tick: u64,
            payload: &'a P,
        }
        let msg = self.codec.encode(&FrameRef {
            seq: self.next_seq,
            ack: self.last_received,
            tick,
            payload,
        })?;
        self.next_seq += 1;
        Ok(msg)
    }

    /// Decode a received message and record its `seq` for our acks.
    ///
    /// Frames are returned even if older than a previously seen one (never
    /// happens within one WebSocket connection); compare `seq` with
    /// [`Self::last_received`] *before* decoding if you need to filter.
    pub fn decode(&mut self, msg: &Message) -> Result<Frame<T>, NetError> {
        let frame: Frame<T> = self.codec.decode(msg)?;
        self.last_received = self.last_received.max(frame.seq);
        self.last_tick = Some(frame.tick);
        Ok(frame)
    }

    /// Encode and queue on a client connection. Returns the frame's `seq`.
    pub fn send(&mut self, conn: &WsConnection, tick: u64, payload: &T) -> Result<u64, NetError> {
        let seq = self.next_seq;
        conn.send(self.encode(tick, payload)?)?;
        Ok(seq)
    }

    /// Next frame received on `conn` (standalone connections; engine-managed ones
    /// deliver messages via `Events<WsEvent>` — use [`Self::decode`] there).
    pub fn recv(&mut self, conn: &WsConnection) -> Option<Result<Frame<T>, NetError>> {
        conn.try_recv().map(|m| self.decode(&m))
    }

    /// Every frame received so far on `conn`; undecodable messages are logged and skipped.
    pub fn drain(&mut self, conn: &WsConnection) -> Vec<Frame<T>> {
        let mut out = Vec::new();
        while let Some(r) = self.recv(conn) {
            match r {
                Ok(f) => out.push(f),
                Err(e) => {
                    tracing::warn!(error = %e, channel = std::any::type_name::<T>(), "dropping undecodable frame")
                }
            }
        }
        out
    }

    /// Encode and queue for one server client. Returns the frame's `seq`.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn send_to(
        &mut self,
        server: &crate::WsServer,
        client: crate::ClientId,
        tick: u64,
        payload: &T,
    ) -> Result<u64, NetError> {
        let seq = self.next_seq;
        server.send(client, self.encode(tick, payload)?)?;
        Ok(seq)
    }

    /// Encode once and queue for every server client (shared `seq`). Returns the recipient count.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn broadcast(&mut self, server: &crate::WsServer, tick: u64, payload: &T) -> Result<usize, NetError> {
        Ok(server.broadcast(self.encode(tick, payload)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
    struct Input {
        dx: f32,
        jump: bool,
        name: String,
    }

    fn sample() -> Input {
        Input {
            dx: 0.5,
            jump: true,
            name: "p1".into(),
        }
    }

    #[test]
    fn codec_roundtrip_both_formats() {
        for codec in [Codec::Json, Codec::Binary] {
            let msg = codec.encode(&sample()).unwrap();
            assert_eq!(msg.is_text(), codec == Codec::Json);
            assert_eq!(codec.decode::<Input>(&msg).unwrap(), sample());
        }
        assert!(Codec::Binary.decode::<Input>(&Message::Text("{}".into())).is_err());
    }

    #[test]
    fn binary_is_smaller_than_json() {
        let j = Codec::Json.encode(&sample()).unwrap();
        let b = Codec::Binary.encode(&sample()).unwrap();
        assert!(b.len() < j.len(), "{} vs {}", b.len(), j.len());
    }

    #[test]
    fn channel_sequences_and_acks() {
        for codec in [Codec::Json, Codec::Binary] {
            let mut a = Channel::<Input>::new(codec);
            let mut b = Channel::<Input>::new(codec);
            let m1 = a.encode(10, &sample()).unwrap();
            let m2 = a.encode(11, &sample()).unwrap();
            let f1 = b.decode(&m1).unwrap();
            let f2 = b.decode(&m2).unwrap();
            assert_eq!((f1.seq, f1.ack, f1.tick), (1, 0, 10));
            assert_eq!((f2.seq, f2.tick), (2, 11));
            assert_eq!(b.last_received(), 2);
            assert_eq!(b.last_tick(), Some(11));
            // b replies; its ack covers a's second frame
            let reply = b.encode(12, &sample()).unwrap();
            let r = a.decode(&reply).unwrap();
            assert_eq!((r.seq, r.ack), (1, 2));
            a.reset();
            assert_eq!((a.next_seq(), a.last_received()), (1, 0));
        }
    }

    #[test]
    fn channel_is_send_sync_for_any_t() {
        fn assert_send_sync<X: Send + Sync>() {}
        assert_send_sync::<Channel<std::rc::Rc<u8>>>();
    }
}

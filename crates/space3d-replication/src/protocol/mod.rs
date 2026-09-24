//! Wire protocol v1 (normative description: `docs/design/game-server.md` §6).
//!
//! One transport message (WebSocket binary message / datagram / stream frame) = one packet:
//!
//! ```text
//! Packet     := u8 header · body
//! header     := bits 0-2 kind (1 Hello, 2 Welcome, 3 Reject, 4 Data, 5 Disconnect)
//!               bits 3-7 flags (Data only; must be 0 otherwise)
//! Hello/Welcome/Reject/Disconnect body := postcard(struct), ≤ MAX_HANDSHAKE_BYTES
//! Data body  := [HAS_SEQ] u16 seq LE · [HAS_ACK] u16 ack LE · u32 ack_bits LE
//!               · varu32 tick · Block*
//! Block      := u8 channel · varu32 len · payload[len]
//! Data flags := bit3 RELIABLE (contains reliable-lane blocks) · bit4 HAS_SEQ · bit5 HAS_ACK
//! ```
//!
//! `tick` is the server tick of the state (S→C) or the client's predicted tick (C→S).
//! `seq`/`ack` exist only on datagram transports (`TransportCaps::true_datagrams`): S→C
//! unreliable packets carry `seq`, C→S packets ack them (32-packet window) so the server can
//! advance per-entity baselines. On stream transports (WebSocket) both are omitted and the
//! server acks optimistically on send. Reliability itself always comes from the transport.

pub mod control;
pub mod handshake;
pub mod input;
pub mod snapshot;

use crate::Tick;
use crate::transport::Lane;
use crate::wire::{DecodeError, Writer};
use handshake::{Disconnect, Hello, Reject, Welcome};
use serde::Serialize;
use smallvec::SmallVec;

/// Protocol version carried in `Hello`; bump on any wire-incompatible change.
pub const PROTOCOL_VERSION: u16 = 1;

/// `Hello::magic`.
pub const MAGIC: [u8; 4] = *b"S3DN";

/// Largest Hello/Welcome/Reject/Disconnect packet.
pub const MAX_HANDSHAKE_BYTES: usize = 4096;

/// Packet kind (low 3 bits of the header byte).
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PacketKind {
    /// C→S, first packet, postcard [`Hello`].
    Hello = 1,
    /// S→C, postcard [`Welcome`].
    Welcome = 2,
    /// S→C, postcard [`Reject`], then close.
    Reject = 3,
    /// Both directions, header + blocks.
    Data = 4,
    /// Both directions, postcard [`Disconnect`], then close.
    Disconnect = 5,
}

/// Block channel id.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ChannelId {
    /// Reliable, both: postcard `control::ControlMsg`.
    Control = 0,
    /// Unreliable, both: postcard `control::TimeMsg` (Ping/Pong).
    Time = 1,
    /// Unreliable, S→C: `snapshot` format.
    Snapshot = 2,
    /// Unreliable, C→S: `input::InputWindow`.
    Input = 3,
    /// Reliable, both: `varu16 message kind · postcard(M)`.
    MsgReliable = 4,
    /// Unreliable, both: `varu16 message kind · postcard(M)`.
    MsgUnreliable = 5,
}

impl ChannelId {
    /// Lane the channel travels on.
    pub fn lane(self) -> Lane {
        match self {
            ChannelId::Control | ChannelId::MsgReliable => Lane::Reliable,
            ChannelId::Time | ChannelId::Snapshot | ChannelId::Input | ChannelId::MsgUnreliable => Lane::Unreliable,
        }
    }

    /// Parse a channel byte (ids ≥ 6 are reserved → `None`).
    pub fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            0 => ChannelId::Control,
            1 => ChannelId::Time,
            2 => ChannelId::Snapshot,
            3 => ChannelId::Input,
            4 => ChannelId::MsgReliable,
            5 => ChannelId::MsgUnreliable,
            _ => return None,
        })
    }
}

/// Data header flag bits.
pub mod flags {
    /// The packet contains at least one reliable-lane block (routing: never drop it silently).
    pub const RELIABLE: u8 = 0x08;
    /// `u16 seq` present.
    pub const HAS_SEQ: u8 = 0x10;
    /// `u16 ack · u32 ack_bits` present.
    pub const HAS_ACK: u8 = 0x20;
    /// Bits that must be zero.
    pub const RESERVED: u8 = 0xC0;
}

/// Ack of the peer's unreliable packets: `ack` plus the 32 before it (`bits` bit i = `ack-1-i`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AckField {
    /// Newest received sequence number.
    pub ack: u16,
    /// Bitfield of the 32 sequence numbers before `ack`.
    pub bits: u32,
}

impl AckField {
    /// Whether `seq` is acknowledged by this field.
    pub fn acks(&self, seq: u16) -> bool {
        let d = self.ack.wrapping_sub(seq);
        d == 0 || (1..=32).contains(&d) && self.bits & (1 << (d - 1)) != 0
    }
}

/// Parsed Data header.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DataHeader {
    /// RELIABLE flag.
    pub reliable: bool,
    /// Present on S→C packets over datagram transports.
    pub seq: Option<u16>,
    /// Present on C→S packets over datagram transports.
    pub ack: Option<AckField>,
    /// Server tick (S→C) / client predicted tick (C→S).
    pub tick: Tick,
}

/// One block of a Data packet, borrowed from the input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Block<'a> {
    /// Channel.
    pub channel: ChannelId,
    /// Payload bytes.
    pub payload: &'a [u8],
}

/// A decoded Data packet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DataPacket<'a> {
    /// Header.
    pub header: DataHeader,
    /// Blocks in wire order.
    pub blocks: SmallVec<[Block<'a>; 8]>,
}

/// Any decoded packet.
#[derive(Clone, Debug, PartialEq)]
pub enum Packet<'a> {
    /// Client hello.
    Hello(Hello),
    /// Server welcome.
    Welcome(Welcome),
    /// Server reject.
    Reject(Reject),
    /// Data.
    Data(DataPacket<'a>),
    /// Orderly disconnect.
    Disconnect(Disconnect),
}

/// Decoder/limiter settings. The server's values are sent to the client in `Welcome::limits`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WireLimits {
    /// Largest inbound packet (server default 16 KiB = the gateway's WS cap; client 1 MiB).
    pub max_packet: u32,
    /// Blocks per Data packet (default 64).
    pub max_blocks: u16,
    /// Input runs per INPUT block (default 32).
    pub max_input_runs: u8,
    /// Encoded size of one input (default 256).
    pub max_input_bytes: u32,
    /// Default per-message cap for messages without their own (default 1 KiB).
    pub max_message_bytes: u32,
    /// Input packets per second per client (token bucket; default 90, burst 30).
    pub input_packets_per_sec: u32,
    /// Messages per second per client over all types (default 30, burst 60).
    pub messages_per_sec: u32,
}

impl Default for WireLimits {
    fn default() -> Self {
        Self {
            max_packet: 16 * 1024,
            max_blocks: 64,
            max_input_runs: 32,
            max_input_bytes: 256,
            max_message_bytes: 1024,
            input_packets_per_sec: 90,
            messages_per_sec: 30,
        }
    }
}

/// Peek at a packet's kind and flags without decoding (gateway routing). `None` for an empty
/// packet or unknown kind.
pub fn peek(bytes: &[u8]) -> Option<(PacketKind, u8)> {
    let b = *bytes.first()?;
    let kind = match b & 0x07 {
        1 => PacketKind::Hello,
        2 => PacketKind::Welcome,
        3 => PacketKind::Reject,
        4 => PacketKind::Data,
        5 => PacketKind::Disconnect,
        _ => return None,
    };
    Some((kind, b & 0xF8))
}

/// Encode a handshake-family packet (`Hello`, `Welcome`, `Reject`, `Disconnect`).
pub fn encode_handshake<T: Serialize>(kind: PacketKind, value: &T) -> Vec<u8> {
    todo!()
}

/// Incrementally builds one Data packet.
#[derive(Debug)]
pub struct DataWriter {
    w: Writer,
    reliable: bool,
    blocks: u16,
}

impl DataWriter {
    /// Start a packet with the given header fields (`reliable` is derived from the blocks).
    pub fn new(seq: Option<u16>, ack: Option<AckField>, tick: Tick) -> Self {
        todo!()
    }

    /// Append a block whose payload is produced by `f`.
    pub fn block(&mut self, channel: ChannelId, f: impl FnOnce(&mut Writer)) {
        todo!()
    }

    /// Bytes so far (to respect size budgets).
    pub fn len(&self) -> usize {
        self.w.len()
    }

    /// Whether no block was added.
    pub fn is_empty(&self) -> bool {
        self.blocks == 0
    }

    /// Finish (patches the RELIABLE flag) and return the packet bytes.
    pub fn finish(self) -> Vec<u8> {
        todo!()
    }
}

/// Decode any packet, enforcing `limits` (size, block count, reserved bits, unknown channels).
pub fn decode_packet<'a>(bytes: &'a [u8], limits: &WireLimits) -> Result<Packet<'a>, DecodeError> {
    todo!()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ack_field_window() {
        let a = AckField { ack: 10, bits: 0b101 };
        assert!(a.acks(10));
        assert!(a.acks(9));
        assert!(!a.acks(8));
        assert!(a.acks(7));
        assert!(!a.acks(11));
        let wrap = AckField { ack: 1, bits: 0b1 };
        assert!(wrap.acks(0));
    }

    #[test]
    fn channel_lanes() {
        assert_eq!(ChannelId::Control.lane(), Lane::Reliable);
        assert_eq!(ChannelId::Snapshot.lane(), Lane::Unreliable);
        assert_eq!(ChannelId::from_u8(6), None);
        assert_eq!(
            peek(&[0x04 | flags::RELIABLE]),
            Some((PacketKind::Data, flags::RELIABLE))
        );
    }
}

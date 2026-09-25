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
use crate::wire::{DecodeError, Reader, Writer, postcard_exact};
use handshake::{Disconnect, Hello, Reject, Welcome};
use serde::Serialize;
use smallvec::SmallVec;

/// Protocol version carried in `Hello`; bump on any wire-incompatible change.
pub const PROTOCOL_VERSION: u16 = 2;

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
    /// Input packets per second per client (default 90; the room's packet bucket adds 30/s
    /// for pings / control, so 120/s with a burst of 60, and never less than 1.5 × the tick
    /// rate for inputs).
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

/// Encode a handshake-family packet (`Hello`, `Welcome`, `Reject`, `Disconnect`): the kind byte
/// followed by the postcard body. Panics if `kind` is [`PacketKind::Data`] (use [`DataWriter`]).
pub fn encode_handshake<T: Serialize>(kind: PacketKind, value: &T) -> Vec<u8> {
    assert!(
        kind != PacketKind::Data,
        "encode_handshake: Data packets use DataWriter"
    );
    let mut out = Vec::with_capacity(64);
    out.push(kind as u8);
    // Plain handshake structs never fail to serialize.
    let body = postcard::to_stdvec(value).expect("handshake encode");
    out.extend_from_slice(&body);
    out
}

/// Incrementally builds one Data packet.
#[derive(Debug)]
pub struct DataWriter {
    w: Writer,
    scratch: Writer,
    reliable: bool,
    blocks: u16,
}

impl DataWriter {
    /// Start a packet with the given header fields (`reliable` is derived from the blocks).
    pub fn new(seq: Option<u16>, ack: Option<AckField>, tick: Tick) -> Self {
        let mut w = Writer::with_capacity(256);
        let mut header = PacketKind::Data as u8;
        if seq.is_some() {
            header |= flags::HAS_SEQ;
        }
        if ack.is_some() {
            header |= flags::HAS_ACK;
        }
        w.u8(header);
        if let Some(seq) = seq {
            w.u16(seq);
        }
        if let Some(ack) = ack {
            w.u16(ack.ack);
            w.u32(ack.bits);
        }
        w.varu32(tick.0);
        Self {
            w,
            scratch: Writer::new(),
            reliable: false,
            blocks: 0,
        }
    }

    /// Append a block whose payload is produced by `f`.
    pub fn block(&mut self, channel: ChannelId, f: impl FnOnce(&mut Writer)) {
        self.scratch.clear();
        f(&mut self.scratch);
        let payload = std::mem::take(&mut self.scratch);
        self.block_bytes(channel, payload.as_slice());
        self.scratch = payload;
    }

    /// Append a block with a ready payload.
    pub fn block_bytes(&mut self, channel: ChannelId, payload: &[u8]) {
        self.w.u8(channel as u8);
        self.w.bytes(payload);
        self.blocks += 1;
        if channel.lane() == Lane::Reliable {
            self.reliable = true;
        }
    }

    /// Bytes so far (to respect size budgets).
    pub fn len(&self) -> usize {
        self.w.len()
    }

    /// Whether no block was added.
    pub fn is_empty(&self) -> bool {
        self.blocks == 0
    }

    /// Number of blocks added.
    pub fn blocks(&self) -> u16 {
        self.blocks
    }

    /// Whether a reliable-lane block was added.
    pub fn has_reliable(&self) -> bool {
        self.reliable
    }

    /// Finish (patches the RELIABLE flag) and return the packet bytes.
    pub fn finish(self) -> Vec<u8> {
        let mut out = self.w.into_vec();
        if self.reliable {
            out[0] |= flags::RELIABLE;
        }
        out
    }
}

/// Size of a block header (`u8 channel · varu32 len`) for a payload of `len` bytes.
pub fn block_overhead(len: usize) -> usize {
    1 + varint_len(len as u64)
}

/// Encoded size of a LEB128 varint.
pub fn varint_len(mut v: u64) -> usize {
    let mut n = 1;
    while v >= 0x80 {
        v >>= 7;
        n += 1;
    }
    n
}

/// Decode any packet, enforcing `limits` (size, block count, reserved bits, unknown channels).
pub fn decode_packet<'a>(bytes: &'a [u8], limits: &WireLimits) -> Result<Packet<'a>, DecodeError> {
    if bytes.len() > limits.max_packet as usize {
        return Err(DecodeError::TooLong {
            len: bytes.len(),
            max: limits.max_packet as usize,
        });
    }
    let Some(&header) = bytes.first() else {
        return Err(DecodeError::Eof);
    };
    let (kind, fl) = peek(bytes).ok_or(DecodeError::Unknown {
        what: "packet kind",
        value: u64::from(header & 0x07),
    })?;
    let body = &bytes[1..];
    if kind != PacketKind::Data {
        if fl != 0 {
            return Err(DecodeError::Invalid("flags on handshake packet"));
        }
        if bytes.len() > MAX_HANDSHAKE_BYTES {
            return Err(DecodeError::TooLong {
                len: bytes.len(),
                max: MAX_HANDSHAKE_BYTES,
            });
        }
        return Ok(match kind {
            PacketKind::Hello => Packet::Hello(postcard_exact(body)?),
            PacketKind::Welcome => Packet::Welcome(postcard_exact(body)?),
            PacketKind::Reject => Packet::Reject(postcard_exact(body)?),
            PacketKind::Disconnect => Packet::Disconnect(postcard_exact(body)?),
            PacketKind::Data => unreachable!(),
        });
    }
    if fl & flags::RESERVED != 0 {
        return Err(DecodeError::Invalid("reserved data flags"));
    }
    let mut r = Reader::new(body);
    let seq = if fl & flags::HAS_SEQ != 0 { Some(r.u16()?) } else { None };
    let ack = if fl & flags::HAS_ACK != 0 {
        Some(AckField {
            ack: r.u16()?,
            bits: r.u32()?,
        })
    } else {
        None
    };
    let tick = Tick(r.varu32()?);
    let mut blocks: SmallVec<[Block<'a>; 8]> = SmallVec::new();
    let mut reliable = false;
    while !r.is_empty() {
        if blocks.len() >= usize::from(limits.max_blocks) {
            return Err(DecodeError::TooLong {
                len: blocks.len() + 1,
                max: usize::from(limits.max_blocks),
            });
        }
        let ch = r.u8()?;
        let channel = ChannelId::from_u8(ch).ok_or(DecodeError::Unknown {
            what: "channel",
            value: u64::from(ch),
        })?;
        let payload = r.bytes(limits.max_packet as usize)?;
        reliable |= channel.lane() == Lane::Reliable;
        blocks.push(Block { channel, payload });
    }
    if reliable != (fl & flags::RELIABLE != 0) {
        return Err(DecodeError::Invalid("RELIABLE flag does not match blocks"));
    }
    Ok(Packet::Data(DataPacket {
        header: DataHeader {
            reliable,
            seq,
            ack,
            tick,
        },
        blocks,
    }))
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
    fn data_packet_roundtrip() {
        let mut dw = DataWriter::new(Some(7), Some(AckField { ack: 3, bits: 0xF0 }), Tick(1234));
        dw.block(ChannelId::Snapshot, |w| w.raw(&[1, 2, 3]));
        assert!(!dw.has_reliable());
        dw.block_bytes(ChannelId::Control, &[9]);
        let bytes = dw.finish();
        assert_eq!(
            peek(&bytes),
            Some((PacketKind::Data, flags::RELIABLE | flags::HAS_SEQ | flags::HAS_ACK))
        );
        let Packet::Data(p) = decode_packet(&bytes, &WireLimits::default()).unwrap() else {
            panic!("not data")
        };
        assert_eq!(p.header.seq, Some(7));
        assert_eq!(p.header.ack, Some(AckField { ack: 3, bits: 0xF0 }));
        assert_eq!(p.header.tick, Tick(1234));
        assert!(p.header.reliable);
        assert_eq!(p.blocks.len(), 2);
        assert_eq!(p.blocks[0].channel, ChannelId::Snapshot);
        assert_eq!(p.blocks[0].payload, &[1, 2, 3]);
        assert_eq!(p.blocks[1].payload, &[9]);
    }

    #[test]
    fn data_packet_limits() {
        let limits = WireLimits {
            max_blocks: 2,
            ..Default::default()
        };
        let mut dw = DataWriter::new(None, None, Tick(1));
        for _ in 0..3 {
            dw.block_bytes(ChannelId::Time, &[]);
        }
        assert!(decode_packet(&dw.finish(), &limits).is_err());
        // Unknown channel.
        assert!(decode_packet(&[0x04, 0x01, 0x07, 0x00], &limits).is_err());
        // Reserved bits.
        assert!(decode_packet(&[0x04 | 0x40, 0x01], &limits).is_err());
        // RELIABLE flag without reliable blocks.
        assert!(decode_packet(&[0x04 | flags::RELIABLE, 0x01], &limits).is_err());
        // Oversize.
        let big = vec![0x04; 17 * 1024];
        assert!(matches!(
            decode_packet(&big, &WireLimits::default()),
            Err(DecodeError::TooLong { .. })
        ));
        // Unknown kind / empty.
        assert!(decode_packet(&[0x07], &limits).is_err());
        assert!(decode_packet(&[], &limits).is_err());
    }

    use proptest::prelude::*;

    proptest! {
        #[test]
        fn prop_decode_packet_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..200)) {
            let _ = decode_packet(&bytes, &WireLimits::default());
        }

        #[test]
        fn prop_data_blocks_roundtrip(tick in any::<u32>(), blocks in proptest::collection::vec((0u8..6, proptest::collection::vec(any::<u8>(), 0..40)), 0..10)) {
            let mut dw = DataWriter::new(None, None, Tick(tick));
            for (ch, payload) in &blocks {
                dw.block_bytes(ChannelId::from_u8(*ch).unwrap(), payload);
            }
            let bytes = dw.finish();
            let Packet::Data(p) = decode_packet(&bytes, &WireLimits::default()).unwrap() else { panic!() };
            prop_assert_eq!(p.header.tick, Tick(tick));
            prop_assert_eq!(p.blocks.len(), blocks.len());
            for (b, (ch, payload)) in p.blocks.iter().zip(&blocks) {
                prop_assert_eq!(b.channel as u8, *ch);
                prop_assert_eq!(b.payload, payload.as_slice());
            }
        }
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

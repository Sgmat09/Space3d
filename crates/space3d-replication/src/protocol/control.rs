//! Control (reliable, `ChannelId::Control`) and time-sync (unreliable, `ChannelId::Time`)
//! messages. Both are postcard enums; one block may carry one message.

use crate::Tick;
use serde::{Deserialize, Serialize};

/// Ping (either side; the client pings 8× at 100 ms after `Welcome`, then at 4 Hz).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ping {
    /// Sequence id.
    pub id: u16,
    /// Sender clock (µs).
    pub t_us: u64,
}

/// Pong (answers a [`Ping`] in the next outgoing packet).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pong {
    /// Echoed ping id.
    pub id: u16,
    /// Echoed `Ping::t_us`.
    pub client_time_us: u64,
    /// Server tick being simulated when the pong was written.
    pub server_tick: Tick,
    /// Fraction of the current tick elapsed, in 1/65536.
    pub tick_frac: u16,
    /// Time between receiving the ping and sending the pong (µs); subtracted from the RTT.
    pub hold_us: u32,
}

/// Time channel payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TimeMsg {
    /// Ping.
    Ping(Ping),
    /// Pong.
    Pong(Pong),
}

/// Severity of a [`Notice`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum NoticeLevel {
    /// Informational.
    Info,
    /// Warning.
    Warn,
}

/// Server announcements shown to players.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Notice {
    /// Admin broadcast text.
    Text {
        /// Severity.
        level: NoticeLevel,
        /// Text (≤ 256 bytes).
        text: String,
    },
    /// The server will shut down in `in_ms`.
    ShuttingDown {
        /// Milliseconds until disconnect.
        in_ms: u32,
    },
    /// The room is moving to another server (a persistent room on a draining server). It keeps
    /// running here until its state is saved, then the link ends with
    /// `Disconnect(ServerShutdown)`: rejoin through the player service
    /// (`POST /v1/sessions/rejoin`), which restores the room elsewhere from that state. A
    /// warning only — `NetClient` surfaces it as `ConnectionEvent::Notice`; reacting to the
    /// disconnect (as the example client does) is enough.
    Reconnect,
    /// The match ended.
    MatchEnded,
}

/// Client statistics reported every 5 s (feeds server metrics; never trusted for gameplay).
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ClientReport {
    /// Frames per second.
    pub fps: f32,
    /// Interpolation delay (ms).
    pub interp_delay_ms: f32,
    /// Rollbacks since the last report.
    pub rollbacks: u32,
    /// Replayed ticks since the last report.
    pub resim_ticks: u32,
    /// Smoothed round-trip time (ms) as measured by the client.
    pub rtt_ms: f32,
}

/// Control channel payload (reliable, ordered).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ControlMsg {
    /// S→C announcement.
    Notice(Notice),
    /// S→C: you are being disconnected (a `Disconnect` packet follows).
    Kick {
        /// Reason.
        reason: String,
    },
    /// C→S: leaving on purpose (no resume grace).
    Leave,
    /// C→S: change downstream budget / snapshot rate (clamped by the server).
    SetRate {
        /// Bytes per second.
        max_down_bytes_per_sec: u32,
    },
    /// C→S: extra interest centre (camera), if the room type allows it.
    InterestHint {
        /// World-space centre.
        center: [f32; 3],
        /// Radius (clamped).
        radius: f32,
    },
    /// C→S: periodic statistics.
    Report(ClientReport),
}

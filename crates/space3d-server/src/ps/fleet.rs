//! Fleet registry (player-service side, pure logic): registrations, heartbeats, leases, room
//! directory, presence, allocation, orphan detection.
//!
//! Allocation: servers in the region (any region if none there) with matching `game_id` +
//! `schema_hash` (when given), `Ready`, not draining, enough free seats (capacity − players −
//! reserved) → lowest `players/capacity + tick_p99/tick_budget` → reserve seats for the ticket
//! TTL (released when the players show up in presence or the reservation expires).

use crate::game::RoomSpec;
use crate::gs::fleet::{FleetDown, FleetUp, Heartbeat, ServerRegistration};
use crate::gs::rooms::RoomStatus;
use space3d_replication::{PlayerId, RoomId};
use std::collections::BTreeMap;

/// Server liveness.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServerState {
    /// Heartbeating.
    Ready,
    /// Draining (no new allocations).
    Draining,
    /// Lease expired.
    Dead,
}

impl ServerState {
    /// Metric label.
    pub fn label(self) -> &'static str {
        match self {
            ServerState::Ready => "ready",
            ServerState::Draining => "draining",
            ServerState::Dead => "dead",
        }
    }
}

/// One registered game server.
#[derive(Clone, Debug, PartialEq)]
pub struct ServerRecord {
    /// Registration.
    pub registration: ServerRegistration,
    /// Last heartbeat.
    pub heartbeat: Heartbeat,
    /// Last heartbeat time (unix ms).
    pub last_seen_ms: i64,
    /// State.
    pub state: ServerState,
    /// Seats reserved by unexpired tickets.
    pub reserved: u32,
    /// Players present in its rooms per `PlayerJoined` / `PlayerLeft` (ahead of the next
    /// heartbeat's count, which lags by up to one heartbeat interval).
    pub present: u32,
    /// Sent `Deregister`: dead until it registers again (late heartbeats do not revive it).
    pub left: bool,
}

impl ServerRecord {
    /// Players on the server: the newer of the heartbeat count and the presence events.
    pub fn players(&self) -> u32 {
        self.heartbeat.players.max(self.present)
    }

    /// Free player seats.
    pub fn free_seats(&self) -> u32 {
        self.registration
            .capacity
            .max_players
            .saturating_sub(self.players())
            .saturating_sub(self.reserved)
    }

    fn allocatable(&self) -> bool {
        self.state == ServerState::Ready && !self.heartbeat.draining
    }
}

/// `(region, state) → servers` and `region → free seats` (see [`FleetRegistry::census`]).
pub type Census = (BTreeMap<(String, &'static str), usize>, BTreeMap<String, u64>);

/// A game build: which game servers can host a room (or a client). Empty `game_id` / zero
/// `schema_hash` match any server.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Build {
    /// `GameDefinition::game_id`.
    pub game_id: String,
    /// Replication schema hash.
    pub schema_hash: u64,
}

impl Build {
    /// A specific build.
    pub fn new(game_id: &str, schema_hash: u64) -> Self {
        Self {
            game_id: game_id.to_owned(),
            schema_hash,
        }
    }

    /// The build a server registered with.
    pub fn of(registration: &ServerRegistration) -> Self {
        Self::new(&registration.game_id, registration.schema_hash)
    }

    /// Whether a server registered with `registration` can host this build.
    pub fn accepts(&self, registration: &ServerRegistration) -> bool {
        (self.game_id.is_empty() || registration.game_id == self.game_id)
            && (self.schema_hash == 0 || registration.schema_hash == self.schema_hash)
    }
}

/// A room known to the player service.
#[derive(Clone, Debug, PartialEq)]
pub struct RoomEntry {
    /// Hosting server.
    pub server: String,
    /// Build of the server that created the room: a restore (rejoin after a server loss) only
    /// goes to a server of the same game and schema, which can load its checkpoint.
    pub build: Build,
    /// Spec (known for rooms the player service allocated or that reported `RoomOpened`).
    pub spec: Option<RoomSpec>,
    /// Last reported status (`None` until the server reports the room).
    pub status: Option<RoomStatus>,
    /// The hosting server died; the room awaits rejoin + restore.
    pub orphaned: bool,
    /// When the entry was created / orphaned (unix ms).
    pub since_ms: i64,
}

/// Side effects the supervisor must perform after feeding a message.
#[derive(Clone, Debug, PartialEq)]
pub enum FleetEffect {
    /// Send to a server.
    Send {
        /// Server id.
        server: String,
        /// Message.
        msg: FleetDown,
    },
    /// Apply an outbox op (store), then ack it; or answer a `FetchPlayerData`.
    Apply {
        /// Server id.
        server: String,
        /// The op.
        op: FleetUp,
    },
    /// A server died: its rooms are orphaned (eligible for rejoin + restore).
    Orphaned {
        /// Rooms.
        rooms: Vec<RoomId>,
    },
}

#[derive(Clone, Debug, PartialEq)]
struct Reservation {
    server: String,
    room: Option<RoomId>,
    seats: u32,
    expires_ms: i64,
    /// The one player a quick-play / rejoin seat was reserved for (a player holds at most
    /// one such reservation: a new one releases the previous).
    holder: Option<PlayerId>,
}

/// Tick budget used in the load score (60 Hz).
const TICK_BUDGET_US: f64 = 16_667.0;
/// Rooms allocated but never reported are forgotten after this.
const PENDING_ROOM_TTL_MS: i64 = 120_000;

/// The registry.
#[derive(Debug)]
pub struct FleetRegistry {
    servers: BTreeMap<String, ServerRecord>,
    rooms: BTreeMap<RoomId, RoomEntry>,
    presence: BTreeMap<PlayerId, RoomId>,
    reservations: Vec<Reservation>,
    lease_ttl_ms: i64,
    /// Orphaned rooms are forgotten after this (their checkpoints are too old to restore).
    orphan_ttl_ms: i64,
}

impl Default for FleetRegistry {
    fn default() -> Self {
        Self::new(10_000, 300_000)
    }
}

impl FleetRegistry {
    /// Registry with a lease TTL and an orphan retention time.
    pub fn new(lease_ttl_ms: i64, orphan_ttl_ms: i64) -> Self {
        Self {
            servers: BTreeMap::new(),
            rooms: BTreeMap::new(),
            presence: BTreeMap::new(),
            reservations: Vec::new(),
            lease_ttl_ms,
            orphan_ttl_ms,
        }
    }

    fn recount_reserved(&mut self) {
        for s in self.servers.values_mut() {
            s.reserved = 0;
            s.present = 0;
        }
        for r in &self.reservations {
            if let Some(s) = self.servers.get_mut(&r.server) {
                s.reserved += r.seats;
            }
        }
        for room in self.presence.values() {
            if let Some(s) = self.rooms.get(room).and_then(|e| self.servers.get_mut(&e.server)) {
                s.present += 1;
            }
        }
    }

    /// Players present per room (presence events; see [`ServerRecord::present`]).
    fn present_per_room(&self) -> BTreeMap<RoomId, u32> {
        let mut n = BTreeMap::new();
        for room in self.presence.values() {
            *n.entry(*room).or_insert(0) += 1;
        }
        n
    }

    /// Feed a message from server `server_id` (the connection's registered id) at `now_ms`.
    pub fn on_message(&mut self, server_id: &str, msg: FleetUp, now_ms: i64) -> Vec<FleetEffect> {
        match msg {
            FleetUp::Register(reg) => {
                let id = reg.server_id.clone();
                // A (re)registration means a fresh process or a re-established link: rooms the
                // server hosted are orphaned until its next heartbeat lists them again (a
                // restarted pod with the same id has lost them; players rejoin + restore).
                for e in self.rooms.values_mut().filter(|e| e.server == id && e.status.is_some()) {
                    e.orphaned = true;
                    e.since_ms = now_ms;
                }
                self.servers.insert(
                    id.clone(),
                    ServerRecord {
                        registration: reg,
                        heartbeat: Heartbeat::default(),
                        last_seen_ms: now_ms,
                        state: ServerState::Ready,
                        reserved: 0,
                        present: 0,
                        left: false,
                    },
                );
                self.recount_reserved();
                vec![FleetEffect::Send {
                    server: id,
                    msg: FleetDown::Registered {
                        lease_ttl_ms: self.lease_ttl_ms as u64,
                        bans: Vec::new(),
                    },
                }]
            }
            FleetUp::Heartbeat(hb) => {
                let Some(rec) = self.servers.get_mut(server_id).filter(|r| !r.left) else {
                    return Vec::new();
                };
                rec.last_seen_ms = now_ms;
                rec.state = if hb.draining {
                    ServerState::Draining
                } else {
                    ServerState::Ready
                };
                let reported: Vec<RoomId> = hb.rooms.iter().map(|r| r.room_id).collect();
                let build = Build::of(&rec.registration);
                for status in &hb.rooms {
                    let entry = self.rooms.entry(status.room_id).or_insert_with(|| RoomEntry {
                        server: server_id.to_owned(),
                        build: build.clone(),
                        spec: None,
                        status: None,
                        orphaned: false,
                        since_ms: now_ms,
                    });
                    entry.server = server_id.to_owned();
                    entry.status = Some(status.clone());
                    entry.orphaned = false;
                    for p in &status.player_ids {
                        self.presence.insert(*p, status.room_id);
                    }
                }
                // Rooms this server reported before but not now are gone — except on a draining
                // server, where a room may vanish from the heartbeat just before its
                // `RoomClosed { restorable: true }` (handoff) arrives on the same link: those
                // are settled by that message (or by the lease when the server exits).
                let draining = hb.draining;
                let gone: Vec<RoomId> = self
                    .rooms
                    .iter()
                    .filter(|(id, e)| {
                        !draining
                            && e.server == server_id
                            && e.status.is_some()
                            && !e.orphaned
                            && !reported.contains(id)
                    })
                    .map(|(id, _)| *id)
                    .collect();
                for id in gone {
                    self.forget_room(id);
                }
                if let Some(rec) = self.servers.get_mut(server_id) {
                    rec.heartbeat = hb;
                }
                self.recount_reserved();
                Vec::new()
            }
            FleetUp::RoomOpened { room_id, spec } => {
                let build = self
                    .servers
                    .get(server_id)
                    .map(|r| Build::of(&r.registration))
                    .unwrap_or_default();
                let entry = self.rooms.entry(room_id).or_insert_with(|| RoomEntry {
                    server: server_id.to_owned(),
                    build,
                    spec: None,
                    status: None,
                    orphaned: false,
                    since_ms: now_ms,
                });
                entry.server = server_id.to_owned();
                entry.spec = Some(spec);
                entry.orphaned = false;
                Vec::new()
            }
            FleetUp::RoomClosed {
                room_id, restorable, ..
            } => {
                if self.rooms.get(&room_id).is_some_and(|e| e.server == server_id) {
                    if restorable {
                        // Handed off (drain) with a fresh checkpoint: keep presence and spec;
                        // the players' rejoin restores the room on another server.
                        if let Some(e) = self.rooms.get_mut(&room_id) {
                            e.orphaned = true;
                            e.status = None;
                            e.since_ms = now_ms;
                        }
                        self.reservations.retain(|r| r.room != Some(room_id));
                        self.recount_reserved();
                    } else {
                        self.forget_room(room_id);
                    }
                }
                Vec::new()
            }
            FleetUp::Deregister { reason } => {
                tracing::info!(server = %server_id, %reason, "game server deregistered");
                if let Some(rec) = self.servers.get_mut(server_id) {
                    rec.left = true;
                }
                self.kill_server(server_id, now_ms).into_iter().collect()
            }
            FleetUp::PlayerJoined { room_id, player } => {
                // A rejoin (resume / new link) is not a new seat.
                let fresh = self.presence.insert(player, room_id) != Some(room_id);
                // The seat moves from the reservation to presence (the room's own reservation
                // first, then a server-wide one).
                let i = self
                    .reservations
                    .iter()
                    .position(|r| r.server == server_id && r.room == Some(room_id))
                    .or_else(|| {
                        self.reservations
                            .iter()
                            .position(|r| r.server == server_id && r.room.is_none())
                    });
                if let Some(i) = i.filter(|_| fresh) {
                    let r = &mut self.reservations[i];
                    r.seats = r.seats.saturating_sub(1);
                    if r.seats == 0 {
                        self.reservations.remove(i);
                    }
                }
                self.recount_reserved();
                Vec::new()
            }
            FleetUp::PlayerLeft { room_id, player } => {
                if self.presence.get(&player) == Some(&room_id) {
                    self.presence.remove(&player);
                    self.recount_reserved();
                }
                Vec::new()
            }
            op @ (FleetUp::MatchResult { .. }
            | FleetUp::PlayerDataWrite { .. }
            | FleetUp::SubmitScore { .. }
            | FleetUp::FetchPlayerData { .. }) => vec![FleetEffect::Apply {
                server: server_id.to_owned(),
                op,
            }],
        }
    }

    /// Void an orphaned room (`RecoveryPolicy::Abandon`): forget it, its presence and its
    /// reservations. Returns its entry.
    pub fn abandon_room(&mut self, id: RoomId) -> Option<RoomEntry> {
        let entry = self.rooms.get(&id).cloned()?;
        self.forget_room(id);
        Some(entry)
    }

    fn forget_room(&mut self, id: RoomId) {
        self.rooms.remove(&id);
        self.presence.retain(|_, r| *r != id);
        self.reservations.retain(|r| r.room != Some(id));
        self.recount_reserved();
    }

    /// Server `id` is gone (lease expired or deregistered): orphan its rooms (rejoin restores
    /// them elsewhere) and drop its reservations.
    fn kill_server(&mut self, id: &str, now_ms: i64) -> Option<FleetEffect> {
        let s = self.servers.get_mut(id)?;
        s.state = ServerState::Dead;
        s.heartbeat.players = 0;
        s.heartbeat.draining = false;
        let mut orphaned = Vec::new();
        for (room, e) in self.rooms.iter_mut().filter(|(_, e)| e.server == id && !e.orphaned) {
            e.orphaned = true;
            e.since_ms = now_ms;
            orphaned.push(*room);
        }
        self.reservations.retain(|r| r.server != id);
        self.recount_reserved();
        (!orphaned.is_empty()).then_some(FleetEffect::Orphaned { rooms: orphaned })
    }

    /// Expire leases, reservations and stale pending/orphaned rooms; returns effects for dead
    /// servers.
    pub fn sweep(&mut self, now_ms: i64, lease_ttl_ms: i64) -> Vec<FleetEffect> {
        let mut effects = Vec::new();
        let dead: Vec<String> = self
            .servers
            .iter()
            .filter(|(_, s)| s.state != ServerState::Dead && now_ms.saturating_sub(s.last_seen_ms) > lease_ttl_ms)
            .map(|(id, _)| id.clone())
            .collect();
        for id in dead {
            tracing::warn!(server = %id, "game server lease expired");
            effects.extend(self.kill_server(&id, now_ms));
        }
        let before = self.reservations.len();
        self.reservations.retain(|r| r.expires_ms > now_ms);
        let stale: Vec<RoomId> = self
            .rooms
            .iter()
            .filter(|(_, e)| {
                (e.orphaned && now_ms.saturating_sub(e.since_ms) > self.orphan_ttl_ms)
                    || (!e.orphaned && e.status.is_none() && now_ms.saturating_sub(e.since_ms) > PENDING_ROOM_TTL_MS)
            })
            .map(|(id, _)| *id)
            .collect();
        for id in stale {
            self.forget_room(id);
        }
        if before != self.reservations.len() {
            self.recount_reserved();
        }
        // Dead servers are forgotten once their rooms are gone.
        let rooms = &self.rooms;
        self.servers
            .retain(|id, s| s.state != ServerState::Dead || rooms.values().any(|e| &e.server == id));
        effects
    }

    /// Pick a server for `seats` new players (any region if none fits in `region`; empty
    /// `game_id` / zero `schema_hash` match any server) and reserve the seats until
    /// `now_ms + reserve_ms`.
    pub fn allocate(
        &mut self,
        region: &str,
        game_id: &str,
        schema_hash: u64,
        seats: u32,
        now_ms: i64,
        reserve_ms: i64,
    ) -> Option<ServerRecord> {
        self.allocate_mode(region, &Build::new(game_id, schema_hash), "", seats, now_ms, reserve_ms)
    }

    /// Whether a server hosts rooms of `mode` (it listed the mode at registration, or listed
    /// none). An empty `mode` matches every server.
    pub fn hosts_mode(s: &ServerRecord, mode: &str) -> bool {
        mode.is_empty() || s.registration.modes.is_empty() || s.registration.modes.iter().any(|m| m == mode)
    }

    /// Whether `mode` is unknown to the fleet: allocatable servers of `build` exist and none of
    /// them hosts `mode`. (No server at all is a capacity problem, not an unknown mode.)
    pub fn mode_unknown(&self, build: &Build, mode: &str) -> bool {
        let mut any = false;
        for s in self
            .servers
            .values()
            .filter(|s| s.allocatable() && build.accepts(&s.registration))
        {
            if Self::hosts_mode(s, mode) {
                return false;
            }
            any = true;
        }
        any
    }

    /// [`Self::allocate`] on servers that host `mode` (see [`Self::hosts_mode`]).
    pub fn allocate_mode(
        &mut self,
        region: &str,
        build: &Build,
        mode: &str,
        seats: u32,
        now_ms: i64,
        reserve_ms: i64,
    ) -> Option<ServerRecord> {
        let fits = |s: &ServerRecord, any_region: bool| {
            s.allocatable()
                && (any_region || region.is_empty() || s.registration.region == region)
                && build.accepts(&s.registration)
                && Self::hosts_mode(s, mode)
                && s.free_seats() >= seats
                && (s.registration.capacity.max_rooms == 0
                    || (s.heartbeat.rooms.len() as u32) < s.registration.capacity.max_rooms)
        };
        let score = |s: &ServerRecord| {
            let cap = s.registration.capacity.max_players.max(1) as f64;
            (s.players() + s.reserved) as f64 / cap + s.heartbeat.tick_p99_us as f64 / TICK_BUDGET_US
        };
        let pick = |any: bool| {
            self.servers
                .values()
                .filter(|s| fits(s, any))
                .min_by(|a, b| score(a).total_cmp(&score(b)))
                .map(|s| s.registration.server_id.clone())
        };
        let id = pick(false).or_else(|| pick(true))?;
        self.reservations.push(Reservation {
            server: id.clone(),
            room: None,
            seats,
            expires_ms: now_ms + reserve_ms,
            holder: None,
        });
        self.recount_reserved();
        self.servers.get(&id).cloned()
    }

    /// Record a room the player service created (tickets carry the spec; the server reports
    /// it with `RoomOpened` once built).
    pub fn note_room(&mut self, spec: &RoomSpec, server: &str, now_ms: i64) {
        // The seats `allocate` just reserved server-wide belong to this room: bind them, so
        // quick play counts them against the room's capacity.
        if let Some(r) = self
            .reservations
            .iter_mut()
            .rev()
            .find(|r| r.server == server && r.room.is_none())
        {
            r.room = Some(spec.room_id);
        }
        let build = self
            .servers
            .get(server)
            .map(|r| Build::of(&r.registration))
            .unwrap_or_default();
        self.rooms.insert(
            spec.room_id,
            RoomEntry {
                server: server.to_owned(),
                build,
                spec: Some(spec.clone()),
                status: None,
                orphaned: false,
                since_ms: now_ms,
            },
        );
    }

    /// An open room of `mode` with `seats` free (quick play / backfill): the fullest one that
    /// still fits, on an allocatable server. Reserves the seats.
    pub fn open_room(
        &mut self,
        mode: &str,
        seats: u32,
        now_ms: i64,
        reserve_ms: i64,
    ) -> Option<(ServerRecord, RoomSpec)> {
        self.open_room_for(mode, &Build::default(), seats, now_ms, reserve_ms)
    }

    /// [`Self::open_room`] on servers that can host `build` only.
    pub fn open_room_for(
        &mut self,
        mode: &str,
        build: &Build,
        seats: u32,
        now_ms: i64,
        reserve_ms: i64,
    ) -> Option<(ServerRecord, RoomSpec)> {
        let reserved_in = |room: RoomId| -> u32 {
            self.reservations
                .iter()
                .filter(|r| r.room == Some(room))
                .map(|r| r.seats)
                .sum()
        };
        let present = self.present_per_room();
        let best = self
            .rooms
            .iter()
            .filter_map(|(id, e)| {
                let spec = e.spec.as_ref()?;
                let server = self.servers.get(&e.server)?;
                if e.orphaned || spec.mode != mode || !server.allocatable() || !build.accepts(&server.registration) {
                    return None;
                }
                let (reported, joinable) = e
                    .status
                    .as_ref()
                    .map_or((0, true), |s| (u32::from(s.players), s.joinable));
                // Presence events are ahead of the 1 Hz room status; reservations of players
                // who already joined are released on their `PlayerJoined`.
                let players = reported.max(present.get(id).copied().unwrap_or(0));
                let used = players + reserved_in(*id);
                (joinable && used + seats <= u32::from(spec.max_players)).then_some((used, *id))
            })
            .max_by_key(|(used, id)| (*used, std::cmp::Reverse(*id)))?;
        let entry = self.rooms.get(&best.1)?.clone();
        self.reservations.push(Reservation {
            server: entry.server.clone(),
            room: Some(best.1),
            seats,
            expires_ms: now_ms + reserve_ms,
            holder: None,
        });
        self.recount_reserved();
        Some((self.servers.get(&entry.server)?.clone(), entry.spec?))
    }

    /// The reservation made last (by [`Self::allocate`] / [`Self::open_room_for`]) is
    /// `player`'s: every other reservation `player` holds is released, so one account holds
    /// at most one unused quick-play / rejoin seat however often it asks.
    pub fn hold_last_reservation(&mut self, player: PlayerId) {
        let Some(last) = self.reservations.len().checked_sub(1) else {
            return;
        };
        self.reservations[last].holder = Some(player);
        let mut i = 0;
        self.reservations.retain(|r| {
            let keep = i == last || r.holder != Some(player);
            i += 1;
            keep
        });
        self.recount_reserved();
    }

    /// Reservations held by `player` (see [`Self::hold_last_reservation`]).
    pub fn reservations_of(&self, player: PlayerId) -> usize {
        self.reservations.iter().filter(|r| r.holder == Some(player)).count()
    }

    /// Move an orphaned room to a new server (rejoin → restore); the caller issues tickets.
    pub fn reassign(&mut self, room: RoomId, server: &str, now_ms: i64) {
        if let Some(e) = self.rooms.get_mut(&room) {
            e.server = server.to_owned();
            e.orphaned = false;
            e.status = None;
            e.since_ms = now_ms;
        }
    }

    /// Where `player` currently plays (rejoin): `(room, server)`.
    pub fn room_of(&self, player: PlayerId) -> Option<(RoomId, String)> {
        let room = self.presence.get(&player)?;
        self.rooms.get(room).map(|e| (*room, e.server.clone()))
    }

    /// A room entry.
    pub fn room(&self, id: RoomId) -> Option<&RoomEntry> {
        self.rooms.get(&id)
    }

    /// All rooms.
    pub fn rooms(&self) -> impl Iterator<Item = (&RoomId, &RoomEntry)> {
        self.rooms.iter()
    }

    /// A server.
    pub fn server(&self, id: &str) -> Option<&ServerRecord> {
        self.servers.get(id)
    }

    /// All servers.
    pub fn servers(&self) -> Vec<ServerRecord> {
        self.servers.values().cloned().collect()
    }

    /// `(region, state) → count` and `region → free seats` (metrics / autoscaling).
    pub fn census(&self) -> Census {
        let mut states = BTreeMap::new();
        let mut free = BTreeMap::new();
        for s in self.servers.values() {
            *states
                .entry((s.registration.region.clone(), s.state.label()))
                .or_insert(0) += 1;
            let f = free.entry(s.registration.region.clone()).or_insert(0u64);
            if s.allocatable() {
                *f += u64::from(s.free_seats());
            }
        }
        (states, free)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gs::fleet::Capacity;

    fn reg(id: &str, region: &str, cap: u32) -> FleetUp {
        FleetUp::Register(ServerRegistration {
            token: "t".into(),
            server_id: id.into(),
            region: region.into(),
            public_url: format!("ws://{id}/play"),
            game_id: "arena".into(),
            build: "1".into(),
            protocol: 1,
            schema_hash: 42,
            capacity: Capacity {
                max_rooms: 10,
                max_players: cap,
            },
            modes: Vec::new(),
        })
    }

    fn hb(players: u32, rooms: Vec<RoomStatus>) -> FleetUp {
        FleetUp::Heartbeat(Heartbeat {
            seq: 1,
            players,
            tick_p99_us: 1_000,
            draining: false,
            rooms,
        })
    }

    fn spec(room: u64, mode: &str, max: u16) -> RoomSpec {
        RoomSpec {
            room_id: RoomId(room),
            mode: mode.into(),
            max_players: max,
            ..Default::default()
        }
    }

    fn reg_schema(id: &str, schema_hash: u64) -> FleetUp {
        let FleetUp::Register(mut r) = reg(id, "eu", 100) else {
            unreachable!()
        };
        r.schema_hash = schema_hash;
        FleetUp::Register(r)
    }

    /// Two builds in one fleet (a rolling deploy that changed the schema): new rooms, open rooms
    /// and restores only go to servers of the matching build.
    #[test]
    fn allocation_and_restore_respect_the_build() {
        let mut f = FleetRegistry::new(10_000, 60_000);
        f.on_message("old", reg_schema("old", 1), 0);
        f.on_message("new", reg_schema("new", 2), 0);
        // "new" is emptier, but a schema-1 client must land on "old".
        f.on_message("old", hb(80, vec![]), 100);
        f.on_message("new", hb(0, vec![]), 100);
        let s = f.allocate("eu", "arena", 1, 1, 100, 60_000).unwrap();
        assert_eq!(s.registration.server_id, "old");
        let s = f.allocate("eu", "arena", 2, 1, 100, 60_000).unwrap();
        assert_eq!(s.registration.server_id, "new");
        assert!(
            f.allocate("eu", "arena", 3, 1, 100, 60_000).is_none(),
            "no server of that build"
        );
        // Rooms remember the build of the server that created them.
        f.note_room(&spec(7, "ffa", 8), "old", 100);
        assert_eq!(f.room(RoomId(7)).unwrap().build, Build::new("arena", 1));
        // Quick play finds the open room only for its own build.
        assert!(
            f.open_room_for("ffa", &Build::new("arena", 2), 1, 100, 60_000)
                .is_none()
        );
        assert!(
            f.open_room_for("ffa", &Build::new("arena", 1), 1, 100, 60_000)
                .is_some()
        );
        // Restore target for an orphaned schema-1 room: never the (emptier) schema-2 server.
        let b = f.room(RoomId(7)).unwrap().build.clone();
        f.on_message("old2", reg_schema("old2", 1), 200);
        f.on_message("old2", hb(90, vec![]), 200);
        let target = f.allocate("eu", &b.game_id, b.schema_hash, 8, 200, 60_000).unwrap();
        assert_ne!(target.registration.server_id, "new");
    }

    #[test]
    fn register_heartbeat_allocate_least_loaded() {
        let mut f = FleetRegistry::new(10_000, 60_000);
        let eff = f.on_message("gs-1", reg("gs-1", "eu", 100), 0);
        assert!(matches!(
            &eff[0],
            FleetEffect::Send {
                msg: FleetDown::Registered {
                    lease_ttl_ms: 10_000,
                    ..
                },
                ..
            }
        ));
        f.on_message("gs-2", reg("gs-2", "eu", 100), 0);
        f.on_message("gs-3", reg("gs-3", "us", 100), 0);
        f.on_message("gs-1", hb(50, vec![]), 100);
        f.on_message("gs-2", hb(10, vec![]), 100);
        let s = f.allocate("eu", "arena", 42, 2, 100, 60_000).unwrap();
        assert_eq!(s.registration.server_id, "gs-2");
        assert_eq!(f.server("gs-2").unwrap().reserved, 2);
        assert!(f.allocate("eu", "other-game", 0, 2, 100, 60_000).is_none());
        assert!(
            f.allocate("eu", "arena", 7, 2, 100, 60_000).is_none(),
            "schema mismatch"
        );
        assert!(
            f.allocate("ap", "", 0, 2, 100, 60_000).is_some(),
            "no server in the region: falls back to any region"
        );
        // Reservations expire.
        f.sweep(70_000, 1_000_000);
        assert_eq!(f.server("gs-2").unwrap().reserved, 0);
        // Draining servers are skipped.
        f.on_message(
            "gs-3",
            FleetUp::Heartbeat(Heartbeat {
                draining: true,
                ..Default::default()
            }),
            100,
        );
        assert!(f.allocate("us", "", 0, 1, 100, 1).unwrap().registration.region != "us");
    }

    #[test]
    fn quick_play_never_oversubscribes_a_room_between_heartbeats() {
        let mut f = FleetRegistry::new(10_000, 60_000);
        f.on_message("gs-1", reg("gs-1", "eu", 100), 0);
        // `allocate_room`: a server-wide reservation for the first player, bound to the room.
        f.allocate("eu", "", 0, 1, 0, 60_000).unwrap();
        f.note_room(&spec(9, "ffa", 3), "gs-1", 0);
        assert!(f.open_room("ffa", 1, 0, 60_000).is_some(), "2nd seat");
        assert!(f.open_room("ffa", 1, 0, 60_000).is_some(), "3rd seat");
        assert!(
            f.open_room("ffa", 1, 0, 60_000).is_none(),
            "the first ticket's seat counts"
        );
        // Everyone joins before the next heartbeat: seats move from reservations to presence.
        for p in 1..=3 {
            f.on_message(
                "gs-1",
                FleetUp::PlayerJoined {
                    room_id: RoomId(9),
                    player: PlayerId(p),
                },
                10,
            );
        }
        assert_eq!(f.server("gs-1").unwrap().reserved, 0);
        assert_eq!(f.server("gs-1").unwrap().present, 3);
        assert_eq!(f.server("gs-1").unwrap().free_seats(), 97);
        assert!(f.open_room("ffa", 1, 10, 60_000).is_none(), "full per presence");
        // A resume of a present player is not a new seat; a leave frees one.
        f.on_message(
            "gs-1",
            FleetUp::PlayerJoined {
                room_id: RoomId(9),
                player: PlayerId(2),
            },
            20,
        );
        assert_eq!(f.server("gs-1").unwrap().present, 3);
        f.on_message(
            "gs-1",
            FleetUp::PlayerLeft {
                room_id: RoomId(9),
                player: PlayerId(3),
            },
            30,
        );
        assert!(f.open_room("ffa", 1, 30, 60_000).is_some());
    }

    #[test]
    fn leases_expire_and_rooms_orphan() {
        let mut f = FleetRegistry::new(10_000, 60_000);
        f.on_message("gs-1", reg("gs-1", "eu", 100), 0);
        f.on_message(
            "gs-1",
            FleetUp::RoomOpened {
                room_id: RoomId(5),
                spec: spec(5, "duel", 2),
            },
            0,
        );
        f.on_message(
            "gs-1",
            hb(
                1,
                vec![RoomStatus {
                    room_id: RoomId(5),
                    mode: "duel".into(),
                    players: 1,
                    max_players: 2,
                    joinable: true,
                    player_ids: vec![PlayerId(7)],
                    ..Default::default()
                }],
            ),
            1_000,
        );
        assert_eq!(f.room_of(PlayerId(7)), Some((RoomId(5), "gs-1".into())));
        assert!(f.sweep(5_000, 10_000).is_empty());
        let eff = f.sweep(12_000, 10_000);
        assert_eq!(eff, vec![FleetEffect::Orphaned { rooms: vec![RoomId(5)] }]);
        assert_eq!(f.server("gs-1").unwrap().state, ServerState::Dead);
        assert!(f.room(RoomId(5)).unwrap().orphaned);
        assert!(f.allocate("eu", "", 0, 1, 12_000, 1).is_none());
        // Rejoin: a new server takes the room over.
        f.on_message("gs-2", reg("gs-2", "eu", 100), 13_000);
        let s = f.allocate("eu", "", 0, 1, 13_000, 60_000).unwrap();
        f.reassign(RoomId(5), &s.registration.server_id, 13_000);
        assert_eq!(f.room_of(PlayerId(7)), Some((RoomId(5), "gs-2".into())));
        // Orphans are forgotten after the retention time.
        f.sweep(200_000, 1_000_000_000);
        assert!(
            f.room(RoomId(5)).is_none(),
            "never reported by gs-2: pending entry expired"
        );
    }

    #[test]
    fn restart_with_the_same_id_orphans_lost_rooms_but_a_relink_keeps_live_ones() {
        let status = |id: u64, player: u64| RoomStatus {
            room_id: RoomId(id),
            mode: "zone".into(),
            players: 1,
            max_players: 4,
            joinable: true,
            player_ids: vec![PlayerId(player)],
            ..Default::default()
        };
        let mut f = FleetRegistry::new(10_000, 60_000);
        f.on_message("gs-1", reg("gs-1", "eu", 100), 0);
        f.note_room(&spec(1, "zone", 4), "gs-1", 0);
        f.note_room(&spec(2, "zone", 4), "gs-1", 0);
        f.on_message("gs-1", hb(1, vec![status(1, 10), status(2, 20)]), 1_000);
        // Link blip: re-register, then the heartbeat still lists both rooms.
        f.on_message("gs-1", reg("gs-1", "eu", 100), 2_000);
        f.on_message("gs-1", hb(2, vec![status(1, 10), status(2, 20)]), 3_000);
        assert!(!f.room(RoomId(1)).unwrap().orphaned && !f.room(RoomId(2)).unwrap().orphaned);
        // Process restart: re-register, the heartbeat only lists a room created since.
        f.on_message("gs-1", reg("gs-1", "eu", 100), 4_000);
        f.note_room(&spec(3, "zone", 4), "gs-1", 4_000);
        f.on_message("gs-1", hb(1, vec![status(3, 30)]), 5_000);
        assert!(
            f.room(RoomId(1)).unwrap().orphaned,
            "lost room kept for rejoin + restore"
        );
        assert!(f.room(RoomId(2)).unwrap().orphaned);
        assert_eq!(f.room_of(PlayerId(10)), Some((RoomId(1), "gs-1".into())));
        assert!(!f.room(RoomId(3)).unwrap().orphaned);
    }

    #[test]
    fn open_rooms_presence_and_ops() {
        let mut f = FleetRegistry::new(10_000, 60_000);
        f.on_message("gs-1", reg("gs-1", "eu", 100), 0);
        f.note_room(&spec(1, "zone", 3), "gs-1", 0);
        f.note_room(&spec(2, "zone", 3), "gs-1", 0);
        f.on_message(
            "gs-1",
            hb(
                2,
                vec![
                    RoomStatus {
                        room_id: RoomId(1),
                        mode: "zone".into(),
                        players: 2,
                        max_players: 3,
                        joinable: true,
                        ..Default::default()
                    },
                    RoomStatus {
                        room_id: RoomId(2),
                        mode: "zone".into(),
                        players: 0,
                        max_players: 3,
                        joinable: true,
                        ..Default::default()
                    },
                ],
            ),
            10,
        );
        let (_, s) = f.open_room("zone", 1, 10, 60_000).unwrap();
        assert_eq!(s.room_id, RoomId(1), "fullest room first");
        let (_, s) = f.open_room("zone", 1, 10, 60_000).unwrap();
        assert_eq!(s.room_id, RoomId(2), "room 1 is now full with the reservation");
        assert!(f.open_room("duel", 1, 10, 60_000).is_none());
        f.on_message(
            "gs-1",
            FleetUp::PlayerJoined {
                room_id: RoomId(2),
                player: PlayerId(3),
            },
            20,
        );
        assert_eq!(f.room_of(PlayerId(3)).unwrap().0, RoomId(2));
        f.on_message(
            "gs-1",
            FleetUp::PlayerLeft {
                room_id: RoomId(2),
                player: PlayerId(3),
            },
            21,
        );
        assert!(f.room_of(PlayerId(3)).is_none());
        let eff = f.on_message(
            "gs-1",
            FleetUp::SubmitScore {
                op_id: "o".into(),
                board: "b".into(),
                player: PlayerId(1),
                score: 1,
            },
            22,
        );
        assert!(matches!(&eff[0], FleetEffect::Apply { server, .. } if server == "gs-1"));
        f.on_message(
            "gs-1",
            FleetUp::RoomClosed {
                room_id: RoomId(1),
                reason: "done".into(),
                restorable: false,
            },
            23,
        );
        assert!(f.room(RoomId(1)).is_none());
        let (states, free) = f.census();
        assert_eq!(states[&("eu".to_owned(), "ready")], 1);
        assert!(free["eu"] > 0);
    }
}

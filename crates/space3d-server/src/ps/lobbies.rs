//! Custom lobbies: joined only by their 6-character code (there is no lobby browser), host
//! starts, allocated like a fixed-composition match (one team with every member). Long-poll
//! friendly (`GET /v1/lobbies/{id}?wait=20` via a version counter that changes on every
//! mutation).
//!
//! Bounded: a host has at most one open lobby (creating another closes the previous one),
//! lobbies idle for [`IDLE_OPEN_TTL_MS`] are closed, and codes are indexed (create/join are
//! `O(log n)` under the lobbies lock).

use space3d_replication::PlayerId;
use space3d_replication::api::{Assignment, LobbyMember, LobbyPhase, LobbyState};
use std::collections::BTreeMap;

/// Code alphabet (no 0/O/1/I).
pub const CODE_ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";

/// A fresh random 6-character join code.
pub fn random_code() -> String {
    crate::security::random_bytes::<6>()
        .iter()
        .map(|b| CODE_ALPHABET[usize::from(*b) % CODE_ALPHABET.len()] as char)
        .collect()
}

#[derive(Debug)]
struct Lobby {
    /// Build of the host (allocation filter).
    build: crate::ps::fleet::Build,
    version: u64,
    state: LobbyState,
    assignments: BTreeMap<PlayerId, Assignment>,
    updated_ms: i64,
}

/// A lobby waiting for a room (supervisor allocation).
#[derive(Clone, Debug, PartialEq)]
pub struct StartingLobby {
    /// Lobby id.
    pub lobby_id: String,
    /// Build of the host: the room goes to a compatible game server.
    pub build: crate::ps::fleet::Build,
    /// Mode.
    pub mode: String,
    /// Capacity.
    pub max_players: u16,
    /// Members.
    pub members: Vec<LobbyMember>,
}

/// In-game lobbies are forgotten after this.
const IN_GAME_TTL_MS: i64 = 600_000;

/// Open (or starting) lobbies without any change for this long are closed.
pub const IDLE_OPEN_TTL_MS: i64 = 30 * 60_000;

/// All lobbies.
#[derive(Debug, Default)]
pub struct Lobbies {
    lobbies: BTreeMap<String, Lobby>,
    /// Code → lobby id, for every lobby that is not closed.
    codes: BTreeMap<String, String>,
    /// Host → their open (or starting) lobby.
    hosted: BTreeMap<PlayerId, String>,
}

impl Lobbies {
    fn bump(lobby: &mut Lobby, now_ms: i64) {
        lobby.version += 1;
        lobby.updated_ms = now_ms;
    }

    /// Close a lobby (host left, replaced, idle): its code is freed.
    fn close(&mut self, lobby_id: &str, now_ms: i64) {
        if let Some(lobby) = self.lobbies.get_mut(lobby_id) {
            if lobby.state.phase != LobbyPhase::Closed {
                lobby.state.phase = LobbyPhase::Closed;
                Self::bump(lobby, now_ms);
            }
            self.codes.remove(&lobby.state.code);
            if self.hosted.get(&lobby.state.host).is_some_and(|id| id == lobby_id) {
                self.hosted.remove(&lobby.state.host);
            }
        }
    }

    /// Create a lobby hosted by `host` (a fresh id; `code` must be unique among open lobbies,
    /// a new one is drawn on collision). A lobby `host` still had open is closed.
    pub fn create(
        &mut self,
        host: PlayerId,
        name: &str,
        mode: &str,
        max_players: u16,
        code: String,
        now_ms: i64,
    ) -> LobbyState {
        if let Some(previous) = self.hosted.get(&host).cloned() {
            self.close(&previous, now_ms);
        }
        let mut code = code;
        while self.codes.contains_key(&code) {
            code = random_code();
        }
        let lobby_id = format!("{:016x}", u64::from_le_bytes(crate::security::random_bytes::<8>()));
        let state = LobbyState {
            lobby_id: lobby_id.clone(),
            code,
            host,
            mode: mode.to_owned(),
            max_players: max_players.clamp(1, 256),
            members: vec![LobbyMember {
                player_id: host,
                display_name: name.to_owned(),
                ready: false,
            }],
            phase: LobbyPhase::Open,
            assignment: None,
        };
        self.codes.insert(state.code.clone(), lobby_id.clone());
        self.hosted.insert(host, lobby_id.clone());
        self.lobbies.insert(
            lobby_id,
            Lobby {
                build: Default::default(),
                version: 1,
                state: state.clone(),
                assignments: BTreeMap::new(),
                updated_ms: now_ms,
            },
        );
        state
    }

    fn by_id(&mut self, lobby_id: &str) -> Result<&mut Lobby, String> {
        self.lobbies
            .get_mut(lobby_id)
            .ok_or_else(|| "lobby not found".to_owned())
    }

    /// Join by code.
    pub fn join(&mut self, code: &str, player: PlayerId, name: &str, now_ms: i64) -> Result<LobbyState, String> {
        let code = code.trim().to_ascii_uppercase();
        let lobby = self
            .codes
            .get(&code)
            .and_then(|id| self.lobbies.get_mut(id))
            .filter(|l| l.state.phase != LobbyPhase::Closed)
            .ok_or_else(|| "lobby not found".to_owned())?;
        if lobby.state.members.iter().any(|m| m.player_id == player) {
            return Ok(lobby.state.clone());
        }
        if lobby.state.phase != LobbyPhase::Open {
            return Err("lobby already started".into());
        }
        if lobby.state.members.len() >= usize::from(lobby.state.max_players) {
            return Err("lobby full".into());
        }
        lobby.state.members.push(LobbyMember {
            player_id: player,
            display_name: name.to_owned(),
            ready: false,
        });
        Self::bump(lobby, now_ms);
        Ok(lobby.state.clone())
    }

    /// Toggle ready.
    pub fn ready(&mut self, lobby_id: &str, player: PlayerId, ready: bool, now_ms: i64) -> Result<LobbyState, String> {
        let lobby = self.by_id(lobby_id)?;
        let member = lobby
            .state
            .members
            .iter_mut()
            .find(|m| m.player_id == player)
            .ok_or_else(|| "not a member".to_owned())?;
        member.ready = ready;
        Self::bump(lobby, now_ms);
        Ok(lobby.state.clone())
    }

    /// Host starts: phase → Starting (the supervisor allocates a room).
    pub fn start(&mut self, lobby_id: &str, player: PlayerId, now_ms: i64) -> Result<LobbyState, String> {
        let lobby = self.by_id(lobby_id)?;
        if lobby.state.host != player {
            return Err("only the host can start".into());
        }
        if lobby.state.phase != LobbyPhase::Open {
            return Err("lobby already started".into());
        }
        lobby.state.phase = LobbyPhase::Starting;
        Self::bump(lobby, now_ms);
        Ok(lobby.state.clone())
    }

    /// Leave (the host leaving closes the lobby).
    pub fn leave(&mut self, lobby_id: &str, player: PlayerId, now_ms: i64) -> Result<(), String> {
        let lobby = self.by_id(lobby_id)?;
        if !lobby.state.members.iter().any(|m| m.player_id == player) {
            return Err("not a member".into());
        }
        if lobby.state.host == player {
            self.close(lobby_id, now_ms);
        } else {
            lobby.state.members.retain(|m| m.player_id != player);
            Self::bump(lobby, now_ms);
        }
        Ok(())
    }

    /// Lobby view for `player` (members only; their own assignment only).
    pub fn view(&self, lobby_id: &str, player: PlayerId) -> Option<(u64, LobbyState)> {
        let lobby = self.lobbies.get(lobby_id)?;
        if !lobby.state.members.iter().any(|m| m.player_id == player) && lobby.state.host != player {
            return None;
        }
        let mut state = lobby.state.clone();
        state.assignment = lobby.assignments.get(&player).cloned();
        Some((lobby.version, state))
    }

    /// Lobbies waiting for allocation.
    pub fn starting(&self) -> Vec<StartingLobby> {
        self.lobbies
            .values()
            .filter(|l| l.state.phase == LobbyPhase::Starting)
            .map(|l| StartingLobby {
                lobby_id: l.state.lobby_id.clone(),
                build: l.build.clone(),
                mode: l.state.mode.clone(),
                max_players: l.state.max_players,
                members: l.state.members.clone(),
            })
            .collect()
    }

    /// Record the host's build (allocation filter for the lobby's room).
    pub fn set_build(&mut self, lobby_id: &str, build: crate::ps::fleet::Build) {
        if let Some(l) = self.lobbies.get_mut(lobby_id) {
            l.build = build;
        }
    }

    /// Allocation succeeded: per-member assignments, phase → InGame.
    pub fn set_assignments(&mut self, lobby_id: &str, assignments: BTreeMap<PlayerId, Assignment>, now_ms: i64) {
        if let Ok(lobby) = self.by_id(lobby_id) {
            lobby.assignments = assignments;
            lobby.state.phase = LobbyPhase::InGame;
            Self::bump(lobby, now_ms);
            // In game: the code no longer admits anyone, and the host may open another lobby.
            let (code, host) = (lobby.state.code.clone(), lobby.state.host);
            self.codes.remove(&code);
            if self.hosted.get(&host).is_some_and(|id| id == lobby_id) {
                self.hosted.remove(&host);
            }
        }
    }

    /// Allocation failed: back to Open (the host may retry).
    pub fn fail_start(&mut self, lobby_id: &str, now_ms: i64) {
        if let Ok(lobby) = self.by_id(lobby_id)
            && lobby.state.phase == LobbyPhase::Starting
        {
            lobby.state.phase = LobbyPhase::Open;
            Self::bump(lobby, now_ms);
        }
    }

    /// Close idle open lobbies; forget closed lobbies and old in-game ones.
    pub fn prune(&mut self, now_ms: i64) {
        let idle: Vec<String> = self
            .lobbies
            .iter()
            .filter(|(_, l)| {
                matches!(l.state.phase, LobbyPhase::Open | LobbyPhase::Starting)
                    && now_ms.saturating_sub(l.updated_ms) >= IDLE_OPEN_TTL_MS
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in idle {
            self.close(&id, now_ms);
        }
        self.lobbies.retain(|_, l| match l.state.phase {
            LobbyPhase::Closed => now_ms.saturating_sub(l.updated_ms) < 60_000,
            LobbyPhase::InGame => now_ms.saturating_sub(l.updated_ms) < IN_GAME_TTL_MS,
            _ => true,
        });
    }

    /// Open (or starting) lobbies hosted by `host` (at most one).
    pub fn hosted_by(&self, host: PlayerId) -> usize {
        usize::from(self.hosted.contains_key(&host))
    }

    /// Number of lobbies.
    pub fn len(&self) -> usize {
        self.lobbies.len()
    }

    /// Whether there are none.
    pub fn is_empty(&self) -> bool {
        self.lobbies.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assignment(p: u64) -> Assignment {
        Assignment {
            server_url: "ws://gs/play".into(),
            server_id: "gs-1".into(),
            room_id: space3d_replication::RoomId(9),
            join_ticket: format!("ticket-{p}"),
            expires_ms: 60_000,
        }
    }

    #[test]
    fn lobby_lifecycle() {
        let mut l = Lobbies::default();
        let (host, guest, other) = (PlayerId(1), PlayerId(2), PlayerId(3));
        let s = l.create(host, "host", "duel", 2, "ABC234".into(), 0);
        assert_eq!(s.code, "ABC234");
        assert_eq!(l.join("abc234", guest, "guest", 1).unwrap().members.len(), 2);
        assert_eq!(l.join("ABC234", other, "o", 1).unwrap_err(), "lobby full");
        assert!(l.join("ZZZZZZ", other, "o", 1).is_err());
        l.ready(&s.lobby_id, guest, true, 2).unwrap();
        assert_eq!(l.start(&s.lobby_id, guest, 3).unwrap_err(), "only the host can start");
        let (v1, _) = l.view(&s.lobby_id, guest).unwrap();
        l.start(&s.lobby_id, host, 3).unwrap();
        let (v2, st) = l.view(&s.lobby_id, guest).unwrap();
        assert!(v2 > v1, "version changes on every mutation");
        assert_eq!(st.phase, LobbyPhase::Starting);
        assert!(l.view(&s.lobby_id, other).is_none(), "non-members see nothing");
        assert_eq!(l.starting().len(), 1);
        l.fail_start(&s.lobby_id, 4);
        l.start(&s.lobby_id, host, 5).unwrap();
        let mut a = BTreeMap::new();
        a.insert(host, assignment(1));
        a.insert(guest, assignment(2));
        l.set_assignments(&s.lobby_id, a, 6);
        let (_, st) = l.view(&s.lobby_id, guest).unwrap();
        assert_eq!(st.phase, LobbyPhase::InGame);
        assert_eq!(st.assignment.unwrap().join_ticket, "ticket-2", "own ticket only");
        l.prune(6 + IN_GAME_TTL_MS + 1);
        assert!(l.is_empty());
    }

    #[test]
    fn host_leaving_closes_and_codes_are_unique() {
        let mut l = Lobbies::default();
        let a = l.create(PlayerId(1), "a", "m", 4, "AAAAAA".into(), 0);
        let b = l.create(PlayerId(2), "b", "m", 4, "AAAAAA".into(), 0);
        assert_ne!(a.code, b.code);
        l.join(&a.code, PlayerId(3), "c", 1).unwrap();
        l.leave(&a.lobby_id, PlayerId(3), 2).unwrap();
        assert_eq!(l.view(&a.lobby_id, PlayerId(1)).unwrap().1.members.len(), 1);
        l.leave(&a.lobby_id, PlayerId(1), 3).unwrap();
        assert_eq!(l.view(&a.lobby_id, PlayerId(1)).unwrap().1.phase, LobbyPhase::Closed);
        assert!(l.join(&a.code, PlayerId(4), "d", 4).is_err());
        assert_eq!(random_code().len(), 6);
    }

    /// Regression: one guest could hold unbounded lobbies for 6 h, and create / join scanned
    /// every lobby under the lock.
    #[test]
    fn one_open_lobby_per_host_idle_expiry_and_indexed_codes() {
        let mut l = Lobbies::default();
        let host = PlayerId(1);
        let mut last = None;
        for i in 0..300 {
            last = Some(l.create(host, "h", "m", 4, random_code(), i));
        }
        let last = last.unwrap();
        assert_eq!(l.hosted_by(host), 1);
        let open = l.lobbies.values().filter(|x| x.state.phase == LobbyPhase::Open).count();
        assert_eq!(open, 1, "creating a lobby closes the host's previous one");
        assert_eq!(l.codes.len(), 1, "only open lobbies hold a code");
        // Closed ones are forgotten after a minute.
        l.prune(400 + 60_000);
        assert_eq!(l.len(), 1);
        // Codes are indexed: a join resolves through the index.
        assert_eq!(l.join(&last.code, PlayerId(2), "g", 400).unwrap().members.len(), 2);
        // Idle open lobbies close (and free their code) after IDLE_OPEN_TTL_MS.
        l.prune(400 + IDLE_OPEN_TTL_MS);
        assert!(l.join(&last.code, PlayerId(3), "x", 400 + IDLE_OPEN_TTL_MS).is_err());
        assert_eq!(l.hosted_by(host), 0);
        assert!(l.codes.is_empty());
        // Many hosts: create + join stay fast (indexed, no scans).
        let started = std::time::Instant::now();
        let mut codes = Vec::new();
        for i in 0..30_000u64 {
            codes.push(l.create(PlayerId(10 + i), "h", "m", 4, random_code(), 0).code);
        }
        for (i, code) in codes.iter().enumerate().step_by(97) {
            l.join(code, PlayerId(1_000_000 + i as u64), "j", 1).unwrap();
        }
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
    }
}

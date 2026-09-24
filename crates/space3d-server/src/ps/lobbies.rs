//! Custom/private lobbies: 6-character codes, host starts, allocated like a fixed-composition
//! match. Long-poll friendly (`GET /v1/lobbies/{id}?wait=20` via a version counter).

use space3d_replication::PlayerId;
use space3d_replication::api::LobbyState;

/// All lobbies.
#[derive(Debug, Default)]
pub struct Lobbies {
    lobbies: std::collections::BTreeMap<String, (u64 /* version */, LobbyState)>,
}

impl Lobbies {
    /// Create a lobby hosted by `host`.
    pub fn create(
        &mut self,
        host: PlayerId,
        name: &str,
        mode: &str,
        max_players: u16,
        private: bool,
        code: String,
    ) -> LobbyState {
        todo!()
    }

    /// Join by code.
    pub fn join(&mut self, code: &str, player: PlayerId, name: &str) -> Result<LobbyState, String> {
        todo!()
    }

    /// Toggle ready.
    pub fn ready(&mut self, lobby_id: &str, player: PlayerId, ready: bool) -> Result<LobbyState, String> {
        todo!()
    }

    /// Host starts: phase → Starting (the supervisor allocates a room).
    pub fn start(&mut self, lobby_id: &str, player: PlayerId) -> Result<LobbyState, String> {
        todo!()
    }

    /// Leave (host leaving closes the lobby).
    pub fn leave(&mut self, lobby_id: &str, player: PlayerId) -> Result<(), String> {
        todo!()
    }

    /// Lobby view for `player` (their own assignment only).
    pub fn view(&self, lobby_id: &str, player: PlayerId) -> Option<(u64, LobbyState)> {
        todo!()
    }
}

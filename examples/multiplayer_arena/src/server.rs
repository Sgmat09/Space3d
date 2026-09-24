//! The server side of the example (native only): [`CubesGame`] plugs the shared protocol and
//! the authoritative rules into `space3d-server`, and [`DEV_CONFIG`] holds the defaults the
//! `server` binary starts with (all-in-one on loopback).

use crate::{CubesProtocol, CubesRules, GAME_ID, MODE};
use space3d_core::App;
use space3d_server::game::{GameDefinition, RecoveryPolicy, RoomSpec};

/// Built-in config of the `server` binary (`examples/multiplayer_arena/server.toml`), layered
/// under `--config` and the `SPACE3D__*` env overlay.
pub const DEV_CONFIG: &str = include_str!("../server.toml");

/// The game's server-side definition.
#[derive(Clone, Copy, Debug, Default)]
pub struct CubesGame;

impl GameDefinition for CubesGame {
    fn game_id(&self) -> &str {
        GAME_ID
    }

    fn protocol(&self, app: &mut App) {
        app.add_plugin(CubesProtocol);
    }

    fn build_room(&self, app: &mut App, _spec: &RoomSpec) {
        app.add_plugin(CubesRules);
    }

    fn validate_spec(&self, spec: &RoomSpec) -> Result<(), String> {
        match spec.mode.as_str() {
            MODE | "dev" => Ok(()),
            other => Err(format!("unknown mode `{other}`")),
        }
    }

    fn recovery_policy(&self, _spec: &RoomSpec) -> RecoveryPolicy {
        RecoveryPolicy::Restore
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use space3d_server::ServerConfig;

    #[test]
    fn dev_config_is_valid_and_loopback() {
        let c = ServerConfig::load_layered(Some(DEV_CONFIG), None, Vec::new()).unwrap();
        assert_eq!(c.role, space3d_server::Role::AllInOne);
        assert!(c.game.allow_dev_auth && c.game.bind.ip().is_loopback());
        assert_eq!(c.player_service.bind, c.game.bind, "one listener");
        assert_eq!(c.matchmaking.queues[0].mode, MODE);
        let spec = RoomSpec {
            mode: MODE.into(),
            ..Default::default()
        };
        assert!(CubesGame.validate_spec(&spec).is_ok());
        assert!(CubesGame.validate_spec(&CubesGame.dev_room_spec()).is_ok());
        let golf = RoomSpec {
            mode: "golf".into(),
            ..Default::default()
        };
        assert!(CubesGame.validate_spec(&golf).is_err());
    }
}

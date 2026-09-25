//! The deployment files' documented commands must produce a config the production image
//! accepts (regression: compose.yaml told users to run with `SPACE3D_FLEET_TOKEN=dev-fleet`,
//! which the image's `SPACE3D__ENV=production` validation refuses, so every container exited).

use space3d_server::ServerConfig;
use space3d_server::security::Secrets;
use std::collections::BTreeMap;

/// `KEY=value` assignments of the documented command in `compose.yaml`'s header, with
/// `$(openssl rand -hex N)` / `$(openssl rand -base64 N)` expanded.
fn documented_env() -> BTreeMap<String, String> {
    let compose = include_str!("../deploy/compose.yaml");
    let line = compose
        .lines()
        .find(|l| l.starts_with('#') && l.contains("SPACE3D_FLEET_TOKEN="))
        .expect("compose.yaml documents the command");
    let mut env = BTreeMap::new();
    let mut rest = line.trim_start_matches('#').trim();
    while let Some(eq) = rest.find('=') {
        let key = rest[..eq].split_whitespace().last().unwrap_or_default().to_owned();
        let after = &rest[eq + 1..];
        let (prefix, after) = after.split_at(
            after
                .find("$(")
                .unwrap_or(0)
                .min(after.find(' ').unwrap_or(after.len())),
        );
        let (value, tail) = if let Some(cmd) = after.strip_prefix("$(") {
            let end = cmd.find(')').expect("closing paren");
            let args: Vec<&str> = cmd[..end].split_whitespace().collect();
            let n: usize = args.last().unwrap().parse().unwrap();
            let bytes: Vec<u8> = (0..n).map(|i| (i * 37 + 11) as u8).collect();
            let generated = match args[2] {
                "-hex" => bytes.iter().map(|b| format!("{b:02x}")).collect::<String>(),
                "-base64" => {
                    use base64::Engine;
                    base64::engine::general_purpose::STANDARD.encode(&bytes)
                }
                other => panic!("unexpected openssl flag {other}"),
            };
            (format!("{prefix}{generated}"), &cmd[end + 1..])
        } else {
            let end = after.find(' ').unwrap_or(after.len());
            (format!("{prefix}{}", &after[..end]), &after[end..])
        };
        if key.starts_with("SPACE3D") {
            env.insert(key, value);
        }
        rest = tail;
    }
    env
}

#[test]
fn the_compose_command_passes_production_validation() {
    let env = documented_env();
    for key in ["SPACE3D_KEYS", "SPACE3D_FLEET_TOKEN", "SPACE3D_ADMIN_TOKENS"] {
        assert!(env.contains_key(key), "{key} documented: {env:?}");
    }
    // What the image and compose set for the player service.
    let overlay = vec![
        ("SPACE3D__ENV".to_owned(), "production".to_owned()),
        ("SPACE3D__ROLE".to_owned(), "player-service".to_owned()),
    ];
    let config = ServerConfig::load_layered(Some(include_str!("../deploy/server.example.toml")), None, overlay)
        .expect("config loads");
    config.validate().expect("valid");
    let lookup = |k: &str| env.get(k).cloned();
    Secrets::load(&config, &lookup).expect("the documented secrets are accepted in production");
}

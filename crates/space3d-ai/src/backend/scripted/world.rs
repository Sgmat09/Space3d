//! `world/*`: shared JSON blackboards with dot paths, versions and subscriptions (protocol §6).

use super::params::{Params, check_id};
use crate::error::{AiError, AiErrorCode, AiResult};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, HashMap};

pub(super) const MAX_WORLDS: usize = 64;
pub(super) const MAX_SUBSCRIPTIONS: usize = 256;

/// One change: the path, the old value (absent when created) and the new one (absent when removed).
pub(super) type Change = (String, Option<Value>, Option<Value>);

/// A world method's result and the changes to notify subscribers about.
pub(super) type Handled = (Value, Option<(String, Vec<Change>)>);

struct World {
    state: Value,
    version: u64,
    created_at: String,
}

struct Subscription {
    world: String,
    path: String,
}

/// Every world and subscription of a scripted bridge.
#[derive(Default)]
pub(super) struct Worlds {
    worlds: HashMap<String, World>,
    order: Vec<String>,
    subscriptions: BTreeMap<u64, Subscription>,
    next_world: u64,
    next_subscription: u64,
}

pub(super) fn not_found(world: &str) -> AiError {
    AiError::new(AiErrorCode::WorldNotFound, format!("No world '{world}'.")).with_data("world", world)
}

fn world_error(world: &str, path: &str, message: String) -> AiError {
    AiError::new(AiErrorCode::WorldError, message)
        .with_data("world", world)
        .with_data("path", path)
}

fn segments(path: &str) -> Vec<&str> {
    if path.is_empty() {
        Vec::new()
    } else {
        path.split('.').collect()
    }
}

fn lookup<'v>(root: &'v Value, path: &str) -> Option<&'v Value> {
    segments(path).into_iter().try_fold(root, |value, key| match value {
        Value::Object(o) => o.get(key),
        Value::Array(a) => key.parse::<usize>().ok().and_then(|i| a.get(i)),
        _ => None,
    })
}

/// Whether a subscription at `watched` sees a change at `changed` (at, inside or above it).
fn overlaps(watched: &str, changed: &str) -> bool {
    watched.is_empty()
        || changed.is_empty()
        || watched == changed
        || changed.starts_with(&format!("{watched}."))
        || watched.starts_with(&format!("{changed}."))
}

fn join(base: &str, key: &str) -> String {
    if base.is_empty() {
        key.to_owned()
    } else {
        format!("{base}.{key}")
    }
}

impl World {
    /// Writes `value` at `path`, creating objects on the way; index `n` of an `n`-element list
    /// appends. Returns the old value.
    fn set(&mut self, id: &str, path: &str, value: Value) -> AiResult<Option<Value>> {
        let keys = segments(path);
        let Some((last, parents)) = keys.split_last() else {
            if !value.is_object() {
                return Err(world_error(id, path, "The world's root must be an object.".into()));
            }
            return Ok(Some(std::mem::replace(&mut self.state, value)));
        };
        let mut node = &mut self.state;
        for (depth, key) in parents.iter().enumerate() {
            let here = keys[..=depth].join(".");
            node = match node {
                Value::Object(o) => o.entry((*key).to_owned()).or_insert_with(|| Value::Object(Map::new())),
                Value::Array(a) => {
                    let index = key
                        .parse::<usize>()
                        .map_err(|_| world_error(id, path, format!("'{here}' is not a list index.")))?;
                    if index == a.len() {
                        a.push(Value::Object(Map::new()));
                    }
                    a.get_mut(index)
                        .ok_or_else(|| world_error(id, path, format!("'{here}' is out of range.")))?
                }
                _ => {
                    return Err(world_error(
                        id,
                        path,
                        format!("Cannot write through '{here}': it is not an object or list."),
                    ));
                }
            };
        }
        match node {
            Value::Object(o) => Ok(o.insert((*last).to_owned(), value)),
            Value::Array(a) => {
                let index = last
                    .parse::<usize>()
                    .map_err(|_| world_error(id, path, format!("'{path}' is not a list index.")))?;
                match index.cmp(&a.len()) {
                    std::cmp::Ordering::Less => Ok(Some(std::mem::replace(&mut a[index], value))),
                    std::cmp::Ordering::Equal => {
                        a.push(value);
                        Ok(None)
                    }
                    std::cmp::Ordering::Greater => Err(world_error(id, path, format!("'{path}' is out of range."))),
                }
            }
            _ => Err(world_error(
                id,
                path,
                format!("Cannot write '{path}': its parent is not an object or list."),
            )),
        }
    }

    fn remove(&mut self, path: &str) -> Option<Value> {
        let keys = segments(path);
        let (last, parents) = keys.split_last()?;
        let mut node = &mut self.state;
        for key in parents {
            node = match node {
                Value::Object(o) => o.get_mut(*key)?,
                Value::Array(a) => a.get_mut(key.parse::<usize>().ok()?)?,
                _ => return None,
            };
        }
        match node {
            Value::Object(o) => o.remove(*last),
            Value::Array(a) => {
                let index = last.parse::<usize>().ok().filter(|i| *i < a.len())?;
                Some(a.remove(index))
            }
            _ => None,
        }
    }

    /// Applies an RFC 7386 merge patch at `path`, recording one change per leaf.
    fn merge(&mut self, id: &str, path: &str, patch: &Value, changes: &mut Vec<Change>) -> AiResult<()> {
        match patch {
            Value::Object(members) => {
                if lookup(&self.state, path).is_some_and(|v| !v.is_object()) || lookup(&self.state, path).is_none() {
                    let old = lookup(&self.state, path).cloned();
                    self.set(id, path, Value::Object(Map::new()))?;
                    if old.is_some() {
                        changes.push((path.to_owned(), old, Some(Value::Object(Map::new()))));
                    }
                }
                for (key, value) in members {
                    let child = join(path, key);
                    if value.is_null() {
                        if let Some(old) = self.remove(&child) {
                            changes.push((child, Some(old), None));
                        }
                    } else {
                        self.merge(id, &child, value, changes)?;
                    }
                }
                Ok(())
            }
            leaf => {
                let old = self.set(id, path, leaf.clone())?;
                if old.as_ref() != Some(leaf) {
                    changes.push((path.to_owned(), old, Some(leaf.clone())));
                }
                Ok(())
            }
        }
    }
}

impl Worlds {
    pub(super) fn exists(&self, world: &str) -> bool {
        self.worlds.contains_key(world)
    }

    pub(super) fn get(&self, world: &str, path: &str) -> AiResult<Option<Value>> {
        let w = self.worlds.get(world).ok_or_else(|| not_found(world))?;
        Ok(lookup(&w.state, path).cloned())
    }

    /// Writes a value and returns the change (for notifications).
    pub(super) fn set(&mut self, world: &str, path: &str, value: Value) -> AiResult<Vec<Change>> {
        let w = self.worlds.get_mut(world).ok_or_else(|| not_found(world))?;
        let old = w.set(world, path, value.clone())?;
        w.version += 1;
        Ok(vec![(path.to_owned(), old, Some(value))])
    }

    /// `world/changed` params for every subscription that sees `changes`.
    pub(super) fn notifications(&self, world: &str, changes: &[Change]) -> Vec<Value> {
        let mut out = Vec::new();
        for (path, old, new) in changes {
            for (id, sub) in &self.subscriptions {
                if sub.world == world && overlaps(&sub.path, path) {
                    let mut params = json!({"world": world, "subscription": format!("sub{id}"), "path": path});
                    if let Some(old) = old {
                        params["oldValue"] = old.clone();
                    }
                    if let Some(new) = new {
                        params["newValue"] = new.clone();
                    }
                    out.push(params);
                }
            }
        }
        out
    }

    fn version(&self, world: &str) -> u64 {
        self.worlds.get(world).map_or(0, |w| w.version)
    }

    /// Handles a `world/*` method. Returns the result and the changes to notify about.
    pub(super) fn handle(
        &mut self,
        method: &str,
        params: &Params,
        npc_count: impl Fn(&str) -> usize,
    ) -> AiResult<Handled> {
        let path = || -> AiResult<String> { Ok(params.opt_string("path")?.unwrap_or_default()) };
        match method {
            "world/create" => {
                let id = match params.opt_string("world")? {
                    Some(id) => id,
                    None => loop {
                        self.next_world += 1;
                        let id = format!("w{}", self.next_world);
                        if !self.exists(&id) {
                            break id;
                        }
                    },
                };
                check_id(&id, "world")?;
                if self.exists(&id) {
                    return Err(
                        AiError::new(AiErrorCode::WorldExists, format!("A world '{id}' already exists."))
                            .with_data("world", id),
                    );
                }
                if self.worlds.len() >= MAX_WORLDS {
                    return Err(
                        AiError::new(AiErrorCode::LimitReached, format!("At most {MAX_WORLDS} worlds."))
                            .with_data("limit", MAX_WORLDS),
                    );
                }
                let state = params.opt_object("state")?.cloned().unwrap_or_else(|| json!({}));
                self.worlds.insert(
                    id.clone(),
                    World {
                        state,
                        version: 0,
                        created_at: super::engine::iso_now(),
                    },
                );
                self.order.push(id.clone());
                Ok((json!({"world": id, "version": 0}), None))
            }
            "world/get" => {
                let world = params.string("world")?;
                let path = path()?;
                let value = self.get(&world, &path)?;
                let result = json!({"world": world, "path": path, "exists": value.is_some(),
                    "value": value.unwrap_or(Value::Null), "version": self.version(&world)});
                Ok((result, None))
            }
            "world/set" => {
                let world = params.string("world")?;
                let path = path()?;
                if !params.has("value") {
                    return Err(AiError::invalid_params("Missing required parameter 'value'."));
                }
                let value = params.get("value").cloned().unwrap_or(Value::Null);
                let changes = self.set(&world, &path, value)?;
                Ok((
                    json!({"world": world, "path": path, "version": self.version(&world)}),
                    Some((world, changes)),
                ))
            }
            "world/merge" => {
                let world = params.string("world")?;
                let path = path()?;
                let patch = params
                    .get("patch")
                    .ok_or_else(|| AiError::invalid_params("Missing required parameter 'patch'."))?;
                let w = self.worlds.get_mut(&world).ok_or_else(|| not_found(&world))?;
                let mut changes = Vec::new();
                // All-or-nothing: merge into a copy.
                let backup = w.state.clone();
                if let Err(error) = w.merge(&world, &path, patch, &mut changes) {
                    w.state = backup;
                    return Err(error);
                }
                w.version += 1;
                Ok((
                    json!({"world": world, "path": path, "version": w.version}),
                    Some((world, changes)),
                ))
            }
            "world/remove" => {
                let world = params.string("world")?;
                let path = params.string("path")?;
                let w = self.worlds.get_mut(&world).ok_or_else(|| not_found(&world))?;
                let old = w.remove(&path);
                if old.is_some() {
                    w.version += 1;
                }
                let result = json!({"world": world, "path": path, "removed": old.is_some(),
                    "oldValue": old.clone().unwrap_or(Value::Null), "version": w.version});
                let changes = old.map(|old| vec![(path.clone(), Some(old), None)]).unwrap_or_default();
                Ok((result, Some((world, changes))))
            }
            "world/snapshot" => {
                let world = params.string("world")?;
                let w = self.worlds.get(&world).ok_or_else(|| not_found(&world))?;
                Ok((json!({"world": world, "state": w.state, "version": w.version}), None))
            }
            "world/delete" => {
                let world = params.string("world")?;
                if self.worlds.remove(&world).is_none() {
                    return Err(not_found(&world));
                }
                self.order.retain(|w| *w != world);
                let before = self.subscriptions.len();
                self.subscriptions.retain(|_, s| s.world != world);
                let ended = before - self.subscriptions.len();
                Ok((
                    json!({"world": world, "deleted": true, "endedSubscriptions": ended}),
                    None,
                ))
            }
            "world/list" => {
                let worlds: Vec<Value> = self
                    .order
                    .iter()
                    .filter_map(|id| {
                        let w = self.worlds.get(id)?;
                        let subscriptions = self.subscriptions.values().filter(|s| s.world == *id).count();
                        Some(json!({"world": id, "version": w.version, "npcs": npc_count(id),
                            "subscriptions": subscriptions, "createdAt": w.created_at}))
                    })
                    .collect();
                Ok((json!({"worlds": worlds}), None))
            }
            "world/subscribe" => {
                let world = params.string("world")?;
                let path = path()?;
                if !self.exists(&world) {
                    return Err(not_found(&world));
                }
                if self.subscriptions.len() >= MAX_SUBSCRIPTIONS {
                    return Err(AiError::new(
                        AiErrorCode::LimitReached,
                        format!("At most {MAX_SUBSCRIPTIONS} subscriptions."),
                    )
                    .with_data("limit", MAX_SUBSCRIPTIONS));
                }
                self.next_subscription += 1;
                let id = self.next_subscription;
                self.subscriptions.insert(
                    id,
                    Subscription {
                        world: world.clone(),
                        path: path.clone(),
                    },
                );
                Ok((
                    json!({"subscription": format!("sub{id}"), "world": world, "path": path}),
                    None,
                ))
            }
            "world/unsubscribe" => {
                let name = params.string("subscription")?;
                let removed = name
                    .strip_prefix("sub")
                    .and_then(|n| n.parse::<u64>().ok())
                    .and_then(|id| self.subscriptions.remove(&id));
                let Some(sub) = removed else {
                    return Err(
                        AiError::new(AiErrorCode::SubscriptionNotFound, format!("No subscription '{name}'."))
                            .with_data("subscription", name),
                    );
                };
                Ok((
                    json!({"subscription": name, "world": sub.world, "path": sub.path, "unsubscribed": true}),
                    None,
                ))
            }
            _ => Err(
                AiError::new(AiErrorCode::MethodNotFound, format!("Method '{method}' not found."))
                    .with_data("method", method),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn world() -> World {
        World {
            state: json!({"player": {"gold": 60, "items": ["axe"]}}),
            version: 0,
            created_at: String::new(),
        }
    }

    #[test]
    fn paths_read_and_write() {
        let mut w = world();
        assert_eq!(lookup(&w.state, "player.gold"), Some(&json!(60)));
        assert_eq!(lookup(&w.state, "player.items.0"), Some(&json!("axe")));
        assert_eq!(lookup(&w.state, ""), Some(&w.state.clone()));
        assert_eq!(w.set("w", "player.gold", json!(45)).unwrap(), Some(json!(60)));
        assert_eq!(w.set("w", "player.items.1", json!("bow")).unwrap(), None);
        assert!(w.set("w", "player.items.5", json!("x")).is_err());
        assert!(w.set("w", "player.gold.x", json!(1)).is_err());
        assert_eq!(w.set("w", "quest.stage", json!(2)).unwrap(), None);
        assert_eq!(w.remove("player.items.0"), Some(json!("axe")));
        assert_eq!(w.remove("nothing.here"), None);
        assert!(w.set("w", "", json!(3)).is_err());
    }

    #[test]
    fn merge_records_leaves() {
        let mut w = world();
        let mut changes = Vec::new();
        w.merge(
            "w",
            "",
            &json!({"player": {"gold": 10, "items": null}, "day": 2}),
            &mut changes,
        )
        .unwrap();
        assert_eq!(w.state, json!({"player": {"gold": 10}, "day": 2}));
        let mut paths: Vec<&str> = changes.iter().map(|c| c.0.as_str()).collect();
        paths.sort_unstable();
        assert_eq!(paths, ["day", "player.gold", "player.items"]);
        assert!(overlaps("player", "player.gold"));
        assert!(overlaps("player.gold", "player"));
        assert!(!overlaps("player.gold", "player.gold_max"));
    }
}

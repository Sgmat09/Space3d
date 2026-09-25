//! Typed access to request params, with the bridge's error wording
//! (`Missing required parameter 'x'.`, `'x' must be a string.`).

use crate::error::{AiError, AiErrorCode, AiResult};
use crate::tools::is_valid_tool_name;
use crate::types::ToolChoice;
use serde_json::{Map, Value};
use std::collections::HashSet;
use std::time::Duration;

/// The protocol's upper bound for `…TimeoutSeconds` (one day).
pub(super) const MAX_TIMEOUT_SECONDS: f64 = 86_400.0;

pub(super) struct Params<'a> {
    object: &'a Map<String, Value>,
    prefix: String,
}

static EMPTY: std::sync::LazyLock<Map<String, Value>> = std::sync::LazyLock::new(Map::new);

impl<'a> Params<'a> {
    /// Request params (absent or `null` = `{}`).
    pub(super) fn of(value: &'a Value) -> AiResult<Self> {
        Self::nested(value, "", "params")
    }

    /// A nested object; `prefix` is prepended to member names in messages (`options.`).
    pub(super) fn nested(value: &'a Value, prefix: &str, name: &str) -> AiResult<Self> {
        match value {
            Value::Object(object) => Ok(Self {
                object,
                prefix: prefix.to_owned(),
            }),
            Value::Null => Ok(Self {
                object: &EMPTY,
                prefix: prefix.to_owned(),
            }),
            _ => Err(AiError::invalid_params(format!("'{name}' must be an object."))),
        }
    }

    pub(super) fn path(&self, key: &str) -> String {
        format!("{}{key}", self.prefix)
    }

    /// A member; `null` counts as absent.
    pub(super) fn get(&self, key: &str) -> Option<&'a Value> {
        self.object.get(key).filter(|v| !v.is_null())
    }

    pub(super) fn has(&self, key: &str) -> bool {
        self.object.contains_key(key)
    }

    pub(super) fn keys(&self) -> impl Iterator<Item = &'a String> {
        self.object.keys()
    }

    fn missing(&self, key: &str) -> AiError {
        AiError::invalid_params(format!("Missing required parameter '{}'.", self.path(key)))
    }

    fn mistyped(&self, key: &str, kind: &str) -> AiError {
        AiError::invalid_params(format!("'{}' must be {kind}.", self.path(key)))
    }

    pub(super) fn string(&self, key: &str) -> AiResult<String> {
        self.opt_string(key)?.ok_or_else(|| self.missing(key))
    }

    pub(super) fn opt_string(&self, key: &str) -> AiResult<Option<String>> {
        match self.get(key) {
            None => Ok(None),
            Some(Value::String(s)) => Ok(Some(s.clone())),
            Some(_) => Err(self.mistyped(key, "a string")),
        }
    }

    /// A string, or any other JSON rendered as compact text (`situation`, `context`).
    pub(super) fn opt_text(&self, key: &str) -> Option<String> {
        self.get(key).map(|v| match v {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        })
    }

    pub(super) fn opt_bool(&self, key: &str) -> AiResult<Option<bool>> {
        match self.get(key) {
            None => Ok(None),
            Some(Value::Bool(b)) => Ok(Some(*b)),
            Some(_) => Err(self.mistyped(key, "a boolean")),
        }
    }

    pub(super) fn opt_u32(&self, key: &str, minimum: u32) -> AiResult<Option<u32>> {
        match self.get(key) {
            None => Ok(None),
            Some(v) => v
                .as_f64()
                .filter(|f| f.fract() == 0.0 && *f >= f64::from(minimum))
                .map(|f| Some(f.min(f64::from(u32::MAX)) as u32))
                .ok_or_else(|| self.mistyped(key, &format!("an integer >= {minimum}"))),
        }
    }

    pub(super) fn opt_i64(&self, key: &str) -> AiResult<Option<i64>> {
        match self.get(key) {
            None => Ok(None),
            Some(v) => v
                .as_f64()
                .filter(|f| f.fract() == 0.0 && f.is_finite())
                .map(|f| Some(f as i64))
                .ok_or_else(|| self.mistyped(key, "an integer")),
        }
    }

    /// A `…TimeoutSeconds` value: finite, 0…86400; `Some(None)` means "no limit" (0).
    pub(super) fn opt_seconds(&self, key: &str) -> AiResult<Option<Option<Duration>>> {
        match self.get(key) {
            None => Ok(None),
            Some(v) => {
                let seconds = v
                    .as_f64()
                    .filter(|s| s.is_finite() && (0.0..=MAX_TIMEOUT_SECONDS).contains(s))
                    .ok_or_else(|| self.mistyped(key, "a number of seconds from 0 to 86400"))?;
                Ok(Some((seconds > 0.0).then(|| Duration::from_secs_f64(seconds))))
            }
        }
    }

    pub(super) fn opt_strings(&self, key: &str) -> AiResult<Option<Vec<String>>> {
        match self.get(key) {
            None => Ok(None),
            Some(Value::Array(items)) => items
                .iter()
                .map(|item| item.as_str().map(str::to_owned))
                .collect::<Option<Vec<_>>>()
                .map(Some)
                .ok_or_else(|| self.mistyped(key, "an array of strings")),
            Some(_) => Err(self.mistyped(key, "an array of strings")),
        }
    }

    pub(super) fn opt_array(&self, key: &str) -> AiResult<Option<&'a Vec<Value>>> {
        match self.get(key) {
            None => Ok(None),
            Some(Value::Array(items)) => Ok(Some(items)),
            Some(_) => Err(self.mistyped(key, "an array")),
        }
    }

    pub(super) fn opt_object(&self, key: &str) -> AiResult<Option<&'a Value>> {
        match self.get(key) {
            None => Ok(None),
            Some(v @ Value::Object(_)) => Ok(Some(v)),
            Some(_) => Err(self.mistyped(key, "an object")),
        }
    }

    pub(super) fn opt_tool_choice(&self, key: &str) -> AiResult<Option<ToolChoice>> {
        match self.get(key) {
            None => Ok(None),
            Some(v) => ToolChoice::from_json(v).map(Some).ok_or_else(|| {
                AiError::invalid_params(format!(
                    "'{}' must be \"auto\", \"none\", \"required\", \"explicit\" or {{\"tool\": \"<name>\"}}; got {v}.",
                    self.path(key)
                ))
            }),
        }
    }

    /// Warns about members not in `known`.
    pub(super) fn warn_unknown(&self, known: &[&str], what: &str, warnings: &mut Vec<String>) {
        for key in self.keys() {
            if !known.contains(&key.as_str()) {
                warnings.push(format!("Unknown {what} '{}' was ignored.", self.path(key)));
            }
        }
    }
}

/// A validated id: 1–128 printable characters.
pub(super) fn check_id(id: &str, param: &str) -> AiResult<()> {
    let count = id.chars().count();
    if (1..=128).contains(&count) && id.chars().all(|c| !c.is_control()) {
        Ok(())
    } else {
        Err(AiError::invalid_params(format!(
            "'{param}' must be 1-128 printable characters."
        )))
    }
}

/// A client tool as the scripted backend needs it.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct ToolSpec {
    pub name: String,
    /// `None`: the default timeout; `Some(None)`: no limit.
    pub timeout: Option<Option<Duration>>,
    pub definition: Value,
}

/// Parses client tool definitions (`[{"name", "description", "parameters"?, "execution"?,
/// "timeoutSeconds"?}]`, OpenAI's `{"type": "function", "function": {…}}` accepted).
pub(super) fn tools(value: Option<&Value>, path: &str, warnings: &mut Vec<String>) -> AiResult<Vec<ToolSpec>> {
    let Some(value) = value.filter(|v| !v.is_null()) else {
        return Ok(Vec::new());
    };
    let items = value
        .as_array()
        .ok_or_else(|| AiError::invalid_params(format!("'{path}' must be an array of tool definitions.")))?;
    let mut names = HashSet::new();
    let mut specs = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        let item_path = format!("{path}[{index}]");
        let definition = item.get("function").filter(|f| f.is_object()).unwrap_or(item);
        let params = Params::nested(definition, &format!("{item_path}."), &item_path)?;
        let name = params.string("name")?;
        if !is_valid_tool_name(&name) {
            return Err(AiError::invalid_params(format!(
                "'{item_path}.name' must be 1-64 characters of letters, digits, '_', '-' or '.'; got '{name}'."
            )));
        }
        if !names.insert(name.clone()) {
            return Err(AiError::invalid_params(format!("Duplicate tool name '{name}'.")));
        }
        if params.opt_string("description")?.unwrap_or_default().is_empty() {
            warnings.push(format!(
                "{name}: no description; the model may not know when to call it."
            ));
        }
        if let Some(execution) = params.opt_string("execution")?
            && execution != "client"
        {
            return Err(AiError::invalid_params(format!(
                "'{item_path}.execution' must be \"client\" (the engine executes the tool via tool/call); got '{execution}'."
            )));
        }
        if let Some(schema) = params.get("parameters") {
            check_schema(schema, &format!("{item_path}.parameters"))?;
        }
        specs.push(ToolSpec {
            name,
            timeout: params.opt_seconds("timeoutSeconds")?,
            definition: definition.clone(),
        });
    }
    Ok(specs)
}

const TYPES: &[&str] = &["object", "string", "integer", "number", "boolean", "array", "null"];

/// Checks the structure of a JSON Schema: known `type`s, object `properties`, `items`.
/// Failures are `-32007 invalid_schema` with `data.path` and `data.schemaPath`.
pub(super) fn check_schema(schema: &Value, path: &str) -> AiResult<()> {
    fn walk(schema: &Value, pointer: &str, path: &str) -> AiResult<()> {
        let fail = |message: String, at: String| {
            Err(
                AiError::new(AiErrorCode::InvalidSchema, format!("{path}: {message} (at {at})"))
                    .with_data("path", path)
                    .with_data("schemaPath", at),
            )
        };
        let object = match schema {
            Value::Bool(true) => return Ok(()),
            Value::Object(object) => object,
            _ => return fail("a schema must be an object".into(), pointer.to_owned()),
        };
        match object.get("type") {
            None => {}
            Some(Value::String(t)) if TYPES.contains(&t.as_str()) => {}
            Some(Value::Array(ts)) if ts.iter().all(|t| t.as_str().is_some_and(|t| TYPES.contains(&t))) => {}
            Some(other) => return fail(format!("unsupported type {other}"), format!("{pointer}/type")),
        }
        if let Some(properties) = object.get("properties") {
            let Some(properties) = properties.as_object() else {
                return fail("'properties' must be an object".into(), format!("{pointer}/properties"));
            };
            for (key, property) in properties {
                walk(property, &format!("{pointer}/properties/{key}"), path)?;
            }
        }
        if let Some(items) = object.get("items") {
            walk(items, &format!("{pointer}/items"), path)?;
        }
        Ok(())
    }
    walk(schema, "#", path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn params_report_paths() {
        let value = json!({"name": 3, "count": 2.5, "flag": true, "list": ["a", 1], "t": 1e19});
        let params = Params::of(&value).unwrap();
        assert_eq!(
            params.string("missing").unwrap_err().message,
            "Missing required parameter 'missing'."
        );
        assert_eq!(params.string("name").unwrap_err().message, "'name' must be a string.");
        assert!(params.opt_u32("count", 0).is_err());
        assert_eq!(params.opt_bool("flag").unwrap(), Some(true));
        assert!(params.opt_strings("list").is_err());
        assert!(params.opt_seconds("t").is_err());
        let nested = json!({"x": 1});
        let options = Params::nested(&nested, "options.", "options").unwrap();
        assert_eq!(
            options.string("y").unwrap_err().message,
            "Missing required parameter 'options.y'."
        );
        assert!(Params::of(&json!([1])).is_err());
        assert!(check_id("gorm", "npc").is_ok());
        assert!(check_id("", "npc").is_err());
        assert!(check_id("a\nb", "npc").is_err());
    }

    #[test]
    fn tool_definitions_parse() {
        let mut warnings = Vec::new();
        let defs = json!([
            {"name": "open_gate", "description": "Opens.", "timeoutSeconds": 0},
            {"type": "function", "function": {"name": "look"}},
        ]);
        let specs = tools(Some(&defs), "tools", &mut warnings).unwrap();
        assert_eq!(specs[0].timeout, Some(None));
        assert_eq!(specs[1].name, "look");
        assert_eq!(
            warnings,
            ["look: no description; the model may not know when to call it."]
        );
        let duplicate = json!([{"name": "a"}, {"name": "a"}]);
        assert_eq!(
            tools(Some(&duplicate), "tools", &mut warnings).unwrap_err().message,
            "Duplicate tool name 'a'."
        );
        let bad = json!([{"name": "a", "parameters": {"type": "nope"}}]);
        let error = tools(Some(&bad), "tools", &mut warnings).unwrap_err();
        assert_eq!(error.code, AiErrorCode::InvalidSchema);
        assert_eq!(error.data.unwrap()["schemaPath"], "#/type");
    }
}

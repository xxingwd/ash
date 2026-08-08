use std::collections::HashSet;
use std::env::VarError;

use ash_core::ProtocolError;
use serde_json::{Map, Number, Value};

const ENV_VAR: &str = "ASH_MODEL_CONFIG";
const RESERVED_ROOTS: &[&str] = &[
    "input",
    "instructions",
    "messages",
    "model",
    "stream",
    "stream_options",
    "system",
    "tools",
];

pub(crate) fn apply_from_env(body: &mut Value) -> Result<(), ProtocolError> {
    match std::env::var(ENV_VAR) {
        Ok(config) => apply(body, &config),
        Err(VarError::NotPresent) => Ok(()),
        Err(VarError::NotUnicode(_)) => Err(ProtocolError::InvalidRequest(format!(
            "{ENV_VAR} must contain valid Unicode"
        ))),
    }
}

pub(crate) fn apply(body: &mut Value, config: &str) -> Result<(), ProtocolError> {
    let mut entries = Vec::new();
    let mut paths = HashSet::new();

    for (index, entry) in config.split(';').enumerate() {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        let (path, value) = parse_entry(entry, index + 1)?;
        let display_path = path.join(".");
        if !paths.insert(path.clone()) {
            return Err(ProtocolError::InvalidRequest(format!(
                "{ENV_VAR} sets '{display_path}' more than once"
            )));
        }
        if paths
            .iter()
            .any(|other| other != &path && (other.starts_with(&path) || path.starts_with(other)))
        {
            return Err(ProtocolError::InvalidRequest(format!(
                "{ENV_VAR} has conflicting paths involving '{display_path}'"
            )));
        }
        entries.push((path, value));
    }

    for (path, value) in entries {
        insert_value(body, &path, value)?;
    }
    Ok(())
}

fn parse_entry(entry: &str, index: usize) -> Result<(Vec<String>, Value), ProtocolError> {
    let (raw_path, raw_value) = entry.split_once('=').ok_or_else(|| {
        ProtocolError::InvalidRequest(format!("{ENV_VAR} entry {index} must use key=value syntax"))
    })?;
    let raw_path = raw_path.trim();
    if raw_path.is_empty() {
        return Err(ProtocolError::InvalidRequest(format!(
            "{ENV_VAR} entry {index} has an empty key"
        )));
    }

    let path = raw_path
        .split('.')
        .map(|part| {
            let part = part.trim();
            if part.is_empty() || part.chars().any(char::is_whitespace) {
                Err(ProtocolError::InvalidRequest(format!(
                    "{ENV_VAR} entry {index} has an invalid key '{raw_path}'"
                )))
            } else {
                Ok(part.to_string())
            }
        })
        .collect::<Result<Vec<_>, _>>()?;
    let root = &path[0];
    if RESERVED_ROOTS.contains(&root.as_str()) {
        return Err(ProtocolError::InvalidRequest(format!(
            "{ENV_VAR} cannot override the '{root}' request field"
        )));
    }

    Ok((path, parse_value(raw_value.trim())))
}

fn parse_value(value: &str) -> Value {
    match value {
        "true" => Value::Bool(true),
        "false" => Value::Bool(false),
        "null" => Value::Null,
        _ => {
            if let Ok(number) = value.parse::<i64>() {
                return Value::Number(number.into());
            }
            if let Ok(number) = value.parse::<u64>() {
                return Value::Number(number.into());
            }
            if let Ok(number) = value.parse::<f64>() {
                if let Some(number) = Number::from_f64(number) {
                    return Value::Number(number);
                }
            }
            Value::String(value.to_string())
        }
    }
}

fn insert_value(body: &mut Value, path: &[String], value: Value) -> Result<(), ProtocolError> {
    let root = &path[0];
    if path.len() == 1 {
        let object = body.as_object_mut().ok_or_else(|| {
            ProtocolError::InvalidRequest("request body must be a JSON object".to_string())
        })?;
        object.insert(root.clone(), value);
        return Ok(());
    }

    let object = body.as_object_mut().ok_or_else(|| {
        ProtocolError::InvalidRequest("request body must be a JSON object".to_string())
    })?;
    let child = object
        .entry(root.clone())
        .or_insert_with(|| Value::Object(Map::new()));
    if !child.is_object() {
        return Err(ProtocolError::InvalidRequest(format!(
            "{ENV_VAR} cannot set '{}': '{root}' is not an object",
            path.join(".")
        )));
    }
    insert_value(child, &path[1..], value)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn applies_nested_paths_and_scalar_values() {
        let mut body = json!({});

        apply(
            &mut body,
            "reasoning.effort=high;temperature=0.2;parallel_tool_calls=true;seed=42;note=",
        )
        .unwrap();

        assert_eq!(
            body,
            json!({
                "reasoning": {"effort": "high"},
                "temperature": 0.2,
                "parallel_tool_calls": true,
                "seed": 42,
                "note": "",
            })
        );
    }

    #[test]
    fn overrides_existing_model_parameters() {
        let mut body = json!({
            "max_tokens": 8192,
            "reasoning": {"effort": "low"},
        });

        apply(&mut body, "max_tokens=4096;reasoning.effort=high").unwrap();

        assert_eq!(body["max_tokens"], 4096);
        assert_eq!(body["reasoning"]["effort"], "high");
    }

    #[test]
    fn rejects_structural_request_fields() {
        let mut body = json!({});

        for config in [
            "model=other",
            "messages=value",
            "input=value",
            "tools=value",
            "stream=false",
            "system=replace",
            "instructions=replace",
        ] {
            let error = apply(&mut body, config).unwrap_err();
            assert!(
                error.to_string().contains("cannot override"),
                "{config}: {error}"
            );
        }
    }

    #[test]
    fn rejects_invalid_and_conflicting_entries() {
        let mut body = json!({});

        for config in [
            "reasoning",
            "reasoning..effort=high",
            "reasoning.effort=low;reasoning.effort=high",
            "reasoning=low;reasoning.effort=high",
            "reasoning.effort=high;reasoning=low",
        ] {
            assert!(apply(&mut body, config).is_err(), "{config}");
        }
    }
}

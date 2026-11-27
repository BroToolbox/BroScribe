//! Reply extraction from agent-CLI / HTTP output and a JSON Schema subset
//! check. Error messages name JSON paths and expectations only, never values
//! (they may hold transcript text).

use serde_json::{Map, Value};

/// Parses a reply that should be JSON: the whole text, a fenced ```json block,
/// the last JSON line (JSONL), or the outermost `{…}` span.
pub fn parse_json_text(text: &str) -> Option<Value> {
    let t = text.trim();
    if let Ok(v) = serde_json::from_str(t) {
        return Some(v);
    }
    if let Some(start) = t.find("```") {
        let after = &t[start + 3..];
        let body = after.strip_prefix("json").unwrap_or(after);
        if let Some(end) = body.find("```")
            && let Ok(v) = serde_json::from_str(body[..end].trim())
        {
            return Some(v);
        }
    }
    if let Some(v) = t
        .lines()
        .rev()
        .filter(|l| !l.trim().is_empty())
        .find_map(|l| serde_json::from_str::<Value>(l.trim()).ok())
        .filter(Value::is_object)
    {
        return Some(v);
    }
    let (start, end) = (t.find('{')?, t.rfind('}')?);
    if start < end {
        return serde_json::from_str(&t[start..=end]).ok();
    }
    None
}

/// The reply inside a CLI's JSON envelope: the first of `keys` holding an
/// object (or a string that parses as JSON); with no keys, the whole value.
pub fn unwrap_envelope(envelope: Value, keys: &[String]) -> Result<Value, String> {
    if keys.is_empty() {
        return Ok(envelope);
    }
    let Value::Object(mut map) = envelope else {
        return Err("output is not a JSON object".into());
    };
    for key in keys {
        match map.remove(key) {
            Some(v @ Value::Object(_)) => return Ok(v),
            Some(Value::String(s)) => {
                if let Some(v) = parse_json_text(&s) {
                    return Ok(v);
                }
            }
            _ => {}
        }
    }
    Err(format!(
        "no JSON reply under any of the keys {}",
        keys.join(", ")
    ))
}

/// Checks `value` against the JSON Schema subset BroScribe's schemas use:
/// `type` (string or list), `properties`, `required`, `additionalProperties:
/// false`, `items`, `enum`, `minimum`/`maximum`, `minLength`/`maxLength`,
/// `minItems`/`maxItems`. Other keywords are ignored.
pub fn check_schema(value: &Value, schema: &Value) -> Result<(), String> {
    check_at(value, schema, "$")
}

fn type_matches(value: &Value, ty: &str) -> bool {
    match ty {
        "null" => value.is_null(),
        "boolean" => value.is_boolean(),
        "string" => value.is_string(),
        "number" => value.is_number(),
        "integer" => value.as_i64().is_some() || value.as_u64().is_some(),
        "array" => value.is_array(),
        "object" => value.is_object(),
        _ => false,
    }
}

fn check_at(value: &Value, schema: &Value, path: &str) -> Result<(), String> {
    let Some(schema) = schema.as_object() else {
        return Ok(());
    };
    if let Some(ty) = schema.get("type") {
        let ok = match ty {
            Value::String(t) => type_matches(value, t),
            Value::Array(ts) => ts
                .iter()
                .filter_map(Value::as_str)
                .any(|t| type_matches(value, t)),
            _ => true,
        };
        if !ok {
            return Err(format!("{path}: expected type {ty}"));
        }
    }
    if let Some(Value::Array(allowed)) = schema.get("enum")
        && !allowed.contains(value)
    {
        return Err(format!("{path}: not one of the allowed values"));
    }
    match value {
        Value::Number(n) => {
            let x = n.as_f64().unwrap_or(f64::NAN);
            if let Some(min) = schema.get("minimum").and_then(Value::as_f64)
                && (x.is_nan() || x < min)
            {
                return Err(format!("{path}: below the minimum {min}"));
            }
            if let Some(max) = schema.get("maximum").and_then(Value::as_f64)
                && (x.is_nan() || x > max)
            {
                return Err(format!("{path}: above the maximum {max}"));
            }
        }
        Value::String(s) => {
            let len = s.chars().count() as u64;
            if let Some(min) = schema.get("minLength").and_then(Value::as_u64)
                && len < min
            {
                return Err(format!("{path}: shorter than {min} characters"));
            }
            if let Some(max) = schema.get("maxLength").and_then(Value::as_u64)
                && len > max
            {
                return Err(format!("{path}: longer than {max} characters"));
            }
        }
        Value::Array(items) => {
            let len = items.len() as u64;
            if let Some(min) = schema.get("minItems").and_then(Value::as_u64)
                && len < min
            {
                return Err(format!("{path}: fewer than {min} items"));
            }
            if let Some(max) = schema.get("maxItems").and_then(Value::as_u64)
                && len > max
            {
                return Err(format!("{path}: more than {max} items"));
            }
            if let Some(item_schema) = schema.get("items") {
                for (i, item) in items.iter().enumerate() {
                    check_at(item, item_schema, &format!("{path}[{i}]"))?;
                }
            }
        }
        Value::Object(map) => check_object(map, schema, path)?,
        _ => {}
    }
    Ok(())
}

fn check_object(
    map: &Map<String, Value>,
    schema: &Map<String, Value>,
    path: &str,
) -> Result<(), String> {
    let props = schema.get("properties").and_then(Value::as_object);
    if let Some(Value::Array(required)) = schema.get("required") {
        for key in required.iter().filter_map(Value::as_str) {
            if !map.contains_key(key) {
                return Err(format!("{path}: missing required property `{key}`"));
            }
        }
    }
    let closed = schema.get("additionalProperties") == Some(&Value::Bool(false));
    for (key, v) in map {
        match props.and_then(|p| p.get(key)) {
            Some(sub) => check_at(v, sub, &format!("{path}.{key}"))?,
            None if closed => {
                // Only the key's length, never its text: keys come from the model.
                return Err(format!(
                    "{path}: unexpected property ({} characters)",
                    key.chars().count()
                ));
            }
            None => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn parses_fenced_jsonl_and_embedded() {
        assert_eq!(parse_json_text(" {\"a\":1} ").unwrap(), json!({"a":1}));
        assert_eq!(
            parse_json_text("Here:\n```json\n{\"a\":2}\n```\n").unwrap(),
            json!({"a":2})
        );
        assert_eq!(
            parse_json_text("{\"type\":\"start\"}\n{\"a\":3}\n").unwrap(),
            json!({"a":3})
        );
        assert_eq!(
            parse_json_text("text {\"a\":4} more").unwrap(),
            json!({"a":4})
        );
        assert!(parse_json_text("no json here").is_none());
    }

    #[test]
    fn unwraps_envelopes() {
        let keys = vec!["structured_output".to_string(), "result".to_string()];
        let env = json!({"type":"result","structured_output":{"x":1},"result":"ignored"});
        assert_eq!(unwrap_envelope(env, &keys).unwrap(), json!({"x":1}));
        let env = json!({"type":"result","result":"```json\n{\"x\":2}\n```"});
        assert_eq!(unwrap_envelope(env, &keys).unwrap(), json!({"x":2}));
        assert!(unwrap_envelope(json!({"result":"plain words"}), &keys).is_err());
        assert_eq!(
            unwrap_envelope(json!({"y":1}), &[]).unwrap(),
            json!({"y":1})
        );
    }

    #[test]
    fn schema_subset() {
        let schema = json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["s", "n", "list"],
            "properties": {
                "s": {"type": "string", "maxLength": 5},
                "n": {"type": ["number", "null"], "minimum": 0},
                "list": {"type": "array", "items": {"enum": ["a", "b"]}},
            }
        });
        assert!(check_schema(&json!({"s":"ok","n":null,"list":["a"]}), &schema).is_ok());
        assert!(check_schema(&json!({"s":"ok","n":1.5,"list":[]}), &schema).is_ok());
        let err = check_schema(&json!({"s":"secret words","n":1,"list":[]}), &schema).unwrap_err();
        assert!(err.contains("$.s") && !err.contains("secret"), "{err}");
        assert!(check_schema(&json!({"s":"x","n":-1,"list":[]}), &schema).is_err());
        let err = check_schema(&json!({"s":"x","n":1,"list":["secret"]}), &schema).unwrap_err();
        assert!(
            err.contains("$.list[0]") && !err.contains("secret"),
            "{err}"
        );
        let err = check_schema(&json!({"s":"x","n":1,"list":[],"secret":1}), &schema).unwrap_err();
        assert!(!err.contains("secret"), "{err}");
        assert!(check_schema(&json!({"s":"x","list":[]}), &schema).is_err());
        assert!(check_schema(&json!([1]), &schema).is_err());
    }
}

use serde_json::{Map, Value};

pub fn normalize_schema_for_openai(schema: Value) -> Value {
    let Value::Object(mut obj) = schema else {
        return json_object([
            ("type", Value::String("object".into())),
            ("properties", Value::Object(Map::new())),
        ]);
    };

    if let Some(Value::Array(types)) = obj.get("type") {
        let non_null: Vec<Value> = types
            .iter()
            .filter(|item| item.as_str() != Some("null"))
            .cloned()
            .collect();
        if non_null.len() == 1 && non_null.len() != types.len() {
            obj.insert("type".into(), non_null[0].clone());
            obj.insert("nullable".into(), Value::Bool(true));
        }
    }

    for key in ["oneOf", "anyOf"] {
        if let Some((branch, nullable)) = nullable_branch(obj.get(key)) {
            obj.remove(key);
            if let Value::Object(branch_obj) = branch {
                for (k, v) in branch_obj {
                    obj.insert(k, v);
                }
                if nullable {
                    obj.insert("nullable".into(), Value::Bool(true));
                }
            }
            break;
        }
    }

    if let Some(Value::Object(properties)) = obj.get_mut("properties") {
        for value in properties.values_mut() {
            if value.is_object() {
                *value = normalize_schema_for_openai(std::mem::take(value));
            }
        }
    }

    if let Some(items) = obj.get_mut("items") {
        if items.is_object() {
            *items = normalize_schema_for_openai(std::mem::take(items));
        }
    }

    if obj.get("type").and_then(Value::as_str) == Some("object") {
        obj.entry("properties")
            .or_insert_with(|| Value::Object(Map::new()));
        obj.entry("required")
            .or_insert_with(|| Value::Array(Vec::new()));
    }

    Value::Object(obj)
}

fn nullable_branch(value: Option<&Value>) -> Option<(Value, bool)> {
    let Value::Array(options) = value? else {
        return None;
    };
    let mut saw_null = false;
    let mut non_null = Vec::new();
    for option in options {
        if option.get("type").and_then(Value::as_str) == Some("null") {
            saw_null = true;
        } else {
            non_null.push(option.clone());
        }
    }
    if saw_null && non_null.len() == 1 {
        Some((non_null.remove(0), true))
    } else {
        None
    }
}

fn json_object<const N: usize>(entries: [(&str, Value); N]) -> Value {
    let mut map = Map::new();
    for (key, value) in entries {
        map.insert(key.to_string(), value);
    }
    Value::Object(map)
}

//! Word notes for validation that a prompt's language type cannot express.
use serde_json::{Map, Value};

pub(crate) fn notes(schema: &Map<String, Value>) -> Vec<String> {
    let mut notes = Vec::new();
    if schema
        .get("oneOf")
        .and_then(Value::as_array)
        .is_some_and(|arms| arms.len() > 1)
    {
        notes.push("exactly one arm".into());
    }
    if let Some(note) = schema.get("not").and_then(simple_not) {
        notes.push(note);
    }
    if let Some(condition) = schema.get("if") {
        if let Some(condition) = condition_words(condition) {
            for (branch, when) in [
                ("then", condition.clone()),
                ("else", format!("not ({condition})")),
            ] {
                if let Some(rule) = schema.get(branch) {
                    describe(rule, &when, &mut notes);
                }
            }
        } else if schema.contains_key("then") || schema.contains_key("else") {
            notes.push("Conditional rules are validated by the tool.".into());
        }
    }
    for keyword in ["dependentRequired", "dependentSchemas", "dependencies"] {
        if let Some(dependencies) = schema.get(keyword).and_then(Value::as_object) {
            for (field, rule) in dependencies {
                let when = format!("{field} is present");
                if let Some(required) = rule.as_array() {
                    required_notes(required, &when, &mut notes);
                } else {
                    describe(rule, &when, &mut notes);
                }
            }
        }
    }
    for keyword in [
        "unevaluatedProperties",
        "unevaluatedItems",
        "$dynamicRef",
        "$recursiveRef",
    ] {
        if let Some(value) = schema.get(keyword) {
            notes.push(format!("{keyword} {value} is validated by the tool."));
        }
    }
    if schema.contains_key("prefixItems") || schema.get("items").is_some_and(Value::is_array) {
        notes.push("Tuple positions and tail are validated by the tool.".into());
    }
    if let Some(values) = schema.get("enum").and_then(Value::as_array)
        && values.iter().any(|value| {
            value.is_object()
                || value.is_array()
                || value.as_f64().is_some_and(|n| n.fract() != 0.0)
        })
    {
        notes.push(format!("one of {}", Value::Array(values.clone())));
    }
    notes
}

fn condition_words(schema: &Value) -> Option<String> {
    let map = schema.as_object()?;
    if map.keys().any(|key| {
        !matches!(
            key.as_str(),
            "properties" | "required" | "title" | "description"
        )
    }) {
        return None;
    }
    let properties = schema.get("properties")?.as_object()?;
    if schema
        .get("required")
        .and_then(Value::as_array)
        .is_some_and(|names| {
            names
                .iter()
                .filter_map(Value::as_str)
                .any(|name| !properties.contains_key(name))
        })
    {
        return None;
    }
    let mut words = Vec::new();
    for (field, constraint) in properties {
        if constraint
            .as_object()?
            .keys()
            .any(|key| !matches!(key.as_str(), "const" | "title" | "description"))
        {
            return None;
        }
        let value = constraint.get("const")?;
        let required = schema
            .get("required")
            .and_then(Value::as_array)
            .is_some_and(|required| required.iter().any(|name| name.as_str() == Some(field)));
        words.push(if required {
            format!("{field} is {value}")
        } else {
            format!("{field} is absent or is {value}")
        });
    }
    if words.is_empty() {
        None
    } else {
        Some(words.join(" and "))
    }
}

fn required_notes(required: &[Value], when: &str, notes: &mut Vec<String>) {
    for field in required.iter().filter_map(Value::as_str) {
        let note = format!("{field}: required when {when}");
        if !notes.contains(&note) {
            notes.push(note);
        }
    }
}

fn describe(schema: &Value, when: &str, notes: &mut Vec<String>) {
    if let Some(required) = schema.get("required").and_then(Value::as_array) {
        required_notes(required, when, notes);
    }
    if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
        for (field, constraint) in properties {
            let simple_type = constraint
                .as_object()
                .filter(|map| map.len() == 1)
                .and_then(|map| map.get("type"))
                .and_then(Value::as_str);
            if let Some(kind) = simple_type {
                notes.push(format!("{field}: if present, type {kind} when {when}"));
            } else if let Some(value) = constraint
                .as_object()
                .filter(|map| map.len() == 1)
                .and_then(|map| map.get("const"))
            {
                notes.push(format!("{field}: if present, must be {value} when {when}"));
            } else {
                notes.push(format!(
                    "{field}: conditional rules validated by the tool when {when}"
                ));
            }
        }
    }
    if schema.as_object().is_none_or(|map| {
        map.keys().any(|key| {
            !matches!(
                key.as_str(),
                "required" | "properties" | "title" | "description"
            )
        })
    }) {
        notes.push(format!(
            "Conditional rules when {when} are validated by the tool."
        ));
    }
}

// Keep simple presence exclusions readable and smaller than their source.
pub(crate) fn simple_not(schema: &Value) -> Option<String> {
    if let Some(required) = only_required(schema) {
        return Some(if let [field] = required.as_slice() {
            format!("omit {field}")
        } else {
            format!("not together: {}", required.join(" + "))
        });
    }
    let map = schema.as_object()?;
    if map.len() != 1 {
        return None;
    }
    let arms = map.get("anyOf")?.as_array()?;
    if arms.is_empty() {
        return None;
    }
    let required = arms.iter().map(only_required).collect::<Option<Vec<_>>>()?;
    if required.iter().any(|names| names.len() < 2) {
        return None;
    }
    Some(format!(
        "not together: {}",
        required
            .iter()
            .map(|names| names.join(" + "))
            .collect::<Vec<_>>()
            .join("; ")
    ))
}

fn only_required(schema: &Value) -> Option<Vec<&str>> {
    let map = schema.as_object()?;
    if map.len() != 1 {
        return None;
    }
    let names = map.get("required")?.as_array()?;
    if names.is_empty() {
        return None;
    }
    names.iter().map(Value::as_str).collect()
}

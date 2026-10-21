//! Strict-dialect `$ref` inlining for the OpenAI schema projector.
//!
//! The projector calls [`inline_ref_with_siblings`] at the head of every node it visits, for
//! the profiles that require strict objects.

use serde_json::Value;

use super::Path;

/// Inline a `$ref` that carries sibling keywords.
///
/// Strict OpenAI structured outputs and strict tool parameters reject a
/// `$ref` node with any sibling keyword (`$ref cannot have keywords
/// {'description'}`). Resolve the reference against the canonical root and
/// merge the siblings over the resolved definition — a sibling
/// `description` wins, per JSON Schema draft annotation semantics — then
/// let the caller project the inlined node in place. A bare `$ref` is left
/// untouched: strict mode accepts it.
pub(super) fn inline_ref_with_siblings(
    value: &mut Value,
    path: &Path,
    canonical_root: &Value,
    diagnostics: &mut Vec<String>,
    errors: &mut Vec<String>,
) {
    let Some(obj) = value.as_object() else {
        return;
    };
    let Some(reference) = obj.get("$ref").and_then(Value::as_str) else {
        return;
    };
    let reference = reference.to_string();
    let siblings = obj
        .keys()
        .filter(|key| key.as_str() != "$ref")
        .cloned()
        .collect::<Vec<_>>();
    if siblings.is_empty() {
        return;
    }

    let mut cycles = Vec::new();
    let Some(resolved) = resolve_ref_node_with_siblings(canonical_root, value, &mut cycles) else {
        errors.push(format!(
            "{path}: `$ref` `{reference}` carries sibling keywords ({}) and could not be resolved against the schema root",
            siblings.join(", ")
        ));
        return;
    };

    *value = resolved;
    diagnostics.push(format!(
        "{path}: inlined `$ref` `{reference}` carrying sibling keywords ({})",
        siblings.join(", ")
    ));
    cycles.sort();
    cycles.dedup();
    for cycle in cycles {
        diagnostics.push(format!(
            "{path}: truncated recursive `$ref` `{cycle}` while inlining `{reference}`"
        ));
    }
}

/// Inline one schema node whose `$ref` carries sibling keywords.
///
/// The reference chain is expanded against `root` and the node's own siblings
/// are merged over the resolved definition, so a sibling `description` wins
/// over the definition's, per JSON Schema draft annotation semantics. Returns
/// `None` when the reference is not a resolvable local pointer, leaving the
/// caller to report it rather than emit a schema the provider will reject.
/// References whose expansion was cut short by a cycle are reported in
/// `cycles`.
fn resolve_ref_node_with_siblings(
    root: &serde_json::Value,
    node: &serde_json::Value,
    cycles: &mut Vec<String>,
) -> Option<serde_json::Value> {
    let mut resolving = Vec::new();
    let resolved = resolve_schema_ref_value(root, node, &mut resolving, cycles);
    if resolved.get("$ref").is_some() {
        return None;
    }
    Some(resolved)
}

fn resolve_schema_ref_value(
    root: &serde_json::Value,
    schema: &serde_json::Value,
    resolving: &mut Vec<String>,
    cycles: &mut Vec<String>,
) -> serde_json::Value {
    match schema {
        serde_json::Value::Object(map) => {
            if let Some(reference) = map.get("$ref").and_then(serde_json::Value::as_str)
                && let Some(pointer) = reference.strip_prefix('#')
            {
                if resolving.iter().any(|active| active == reference) {
                    cycles.push(reference.to_string());
                    return serde_json::json!({});
                }
                if let Some(target) = root.pointer(pointer) {
                    resolving.push(reference.to_string());
                    let mut resolved = resolve_schema_ref_value(root, target, resolving, cycles);
                    resolving.pop();

                    let sibling_count = map.keys().filter(|key| key.as_str() != "$ref").count();
                    if sibling_count == 0 {
                        return resolved;
                    }
                    if let serde_json::Value::Object(resolved_map) = &mut resolved {
                        for (key, value) in map {
                            if key == "$ref" {
                                continue;
                            }
                            resolved_map.insert(
                                key.clone(),
                                resolve_schema_ref_value(root, value, resolving, cycles),
                            );
                        }
                        return resolved;
                    }
                }
            }

            serde_json::Value::Object(
                map.iter()
                    .map(|(key, value)| {
                        (
                            key.clone(),
                            resolve_schema_ref_value(root, value, resolving, cycles),
                        )
                    })
                    .collect(),
            )
        }
        serde_json::Value::Array(values) => serde_json::Value::Array(
            values
                .iter()
                .map(|value| resolve_schema_ref_value(root, value, resolving, cycles))
                .collect(),
        ),
        other => other.clone(),
    }
}

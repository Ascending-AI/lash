//! Strict-dialect `$ref` inlining for the OpenAI schema projector.
//!
//! The projector calls [`inline_ref_with_siblings`] at the head of every node it visits, for
//! the profiles that require strict objects.

use serde_json::Value;

use super::Path;
use crate::tool_contract::schema_docs::resolve_ref_node_with_siblings;

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

use lashlang::{
    WORKFLOW_GRAPH_SCHEMA_VERSION, WORKFLOW_TYPE_FACET_SCHEMA_VERSION, WorkflowGraph,
    WorkflowNodeTypeFacets,
};
use schemars::JsonSchema;
use serde::Serialize;
use serde_json::{Map, Value, json};

const GRAPH_NAME: &str = "workflow-graph";
const FACET_NAME: &str = "workflow-type-facets";
#[derive(Serialize)]
struct Document {
    shape: &'static str,
    version: u32,
    version_constant: &'static str,
    schema: Value,
}

fn main() -> Result<(), String> {
    let encoded = serde_json::to_string(&documents()?)
        .map_err(|error| format!("cannot encode generated schemas: {error}"))?;
    println!("{encoded}");
    Ok(())
}

fn documents() -> Result<Vec<Document>, String> {
    Ok(vec![
        Document {
            shape: GRAPH_NAME,
            version: WORKFLOW_GRAPH_SCHEMA_VERSION,
            version_constant: "WORKFLOW_GRAPH_SCHEMA_VERSION",
            schema: stamp_schema::<WorkflowGraph>(
                GRAPH_NAME,
                WORKFLOW_GRAPH_SCHEMA_VERSION,
                "WORKFLOW_GRAPH_SCHEMA_VERSION",
                true,
            )?,
        },
        Document {
            shape: FACET_NAME,
            version: WORKFLOW_TYPE_FACET_SCHEMA_VERSION,
            version_constant: "WORKFLOW_TYPE_FACET_SCHEMA_VERSION",
            schema: stamp_schema::<WorkflowNodeTypeFacets>(
                FACET_NAME,
                WORKFLOW_TYPE_FACET_SCHEMA_VERSION,
                "WORKFLOW_TYPE_FACET_SCHEMA_VERSION",
                false,
            )?,
        },
    ])
}

fn stamp_schema<T: JsonSchema>(
    shape: &str,
    version: u32,
    version_constant: &str,
    closed_root: bool,
) -> Result<Value, String> {
    let mut schema = serde_json::to_value(schemars::schema_for!(T))
        .map_err(|error| format!("cannot serialize {shape} schema: {error}"))?;
    let root = schema
        .as_object_mut()
        .ok_or_else(|| format!("{shape} schema root is not an object"))?;
    root.insert(
        "$id".to_string(),
        Value::String(format!("https://lash.dev/schemas/{shape}/v{version}")),
    );
    root.insert(
        "x-lash-schema-version".to_string(),
        Value::Number(version.into()),
    );
    root.insert(
        "x-lash-version-constant".to_string(),
        Value::String(version_constant.to_string()),
    );
    if closed_root {
        root.insert("additionalProperties".to_string(), Value::Bool(false));
        pin_property(root, "schema_version", version)?;
    }
    merge_container_node_schemas(root)?;
    Ok(schema)
}

/// Schemars represents `WorkflowNodeKind::Container(WorkflowContainer)` as an
/// object carrying `kind`, intersected with a second closed object carrying
/// the container fields. Draft 7 evaluates `additionalProperties` within each
/// object, so that representation rejects every serialized container. Replace
/// it with one closed object per container alternative.
fn merge_container_node_schemas(root: &mut Map<String, Value>) -> Result<(), String> {
    let Some(node_kind) = root
        .get_mut("definitions")
        .and_then(Value::as_object_mut)
        .and_then(|definitions| definitions.get_mut("WorkflowNodeKind"))
    else {
        return Ok(());
    };
    let alternatives = node_kind
        .get_mut("oneOf")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| "WorkflowNodeKind schema has no alternatives".to_string())?;
    let container_index = alternatives
        .iter()
        .position(|alternative| alternative["properties"]["kind"]["enum"] == json!(["container"]))
        .ok_or_else(|| "WorkflowNodeKind schema has no container alternative".to_string())?;
    let container = alternatives.remove(container_index);
    let variants = container
        .get("oneOf")
        .and_then(Value::as_array)
        .ok_or_else(|| "container node schema has no container variants".to_string())?;
    let mut merged = Vec::with_capacity(variants.len());
    for variant in variants {
        let mut variant = variant.clone();
        let object = variant
            .as_object_mut()
            .ok_or_else(|| "container variant schema is not an object".to_string())?;
        object
            .entry("properties")
            .or_insert_with(|| json!({}))
            .as_object_mut()
            .ok_or_else(|| "container variant properties are not an object".to_string())?
            .insert(
                "kind".to_string(),
                json!({ "enum": ["container"], "type": "string" }),
            );
        let required = object
            .entry("required")
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .ok_or_else(|| "container variant required list is not an array".to_string())?;
        required.push(Value::String("kind".to_string()));
        required.sort_by(|left, right| left.as_str().cmp(&right.as_str()));
        merged.push(variant);
    }
    alternatives.splice(container_index..container_index, merged);
    Ok(())
}

fn pin_property(root: &mut Map<String, Value>, name: &str, version: u32) -> Result<(), String> {
    let property = root
        .get_mut("properties")
        .and_then(Value::as_object_mut)
        .and_then(|properties| properties.get_mut(name))
        .and_then(Value::as_object_mut)
        .ok_or_else(|| format!("schema has no object property `{name}`"))?;
    property.insert("enum".to_string(), json!([version]));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn documents_are_stamped_with_their_owners() {
        let documents = documents().expect("schemas generate");
        assert_eq!(documents.len(), 2);
        for document in documents {
            assert_eq!(
                document.schema["x-lash-schema-version"],
                json!(document.version)
            );
            assert_eq!(
                document.schema["x-lash-version-constant"],
                json!(document.version_constant)
            );
        }
    }

    #[test]
    fn graph_schema_pins_the_decode_fence() {
        let graph = documents()
            .expect("schemas generate")
            .into_iter()
            .find(|document| document.shape == GRAPH_NAME)
            .expect("graph schema is registered");
        assert_eq!(graph.schema["additionalProperties"], json!(false));
        assert_eq!(
            graph.schema["properties"]["schema_version"]["enum"],
            json!([WORKFLOW_GRAPH_SCHEMA_VERSION])
        );
        assert!(
            graph.schema["definitions"]["WorkflowNodeTypeFacets"]
                .get("additionalProperties")
                .is_none()
        );
        let node_variants = graph.schema["definitions"]["WorkflowNodeKind"]["oneOf"]
            .as_array()
            .expect("node variants");
        let containers = node_variants
            .iter()
            .filter(|variant| variant["properties"]["kind"]["enum"] == json!(["container"]))
            .collect::<Vec<_>>();
        assert_eq!(containers.len(), 4);
        assert!(containers.iter().all(|variant| {
            variant["additionalProperties"] == json!(false)
                && variant["required"]
                    .as_array()
                    .is_some_and(|required| required.contains(&json!("kind")))
                && variant["properties"].get("container_kind").is_some()
        }));
    }
}

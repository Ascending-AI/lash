use serde_json::{Value, json};

pub(super) fn mcp_result_schema(
    structured_schema: Option<&serde_json::Map<String, Value>>,
) -> Value {
    let block = json!({
        "oneOf": [
            {"type":"object","properties":{"type":{"const":"text"},"text":{"type":"string"}},"required":["type","text"]},
            {"type":"object","properties":{"type":{"enum":["image","audio"]},"attachment":{},"mimeType":{"type":"string"}},"required":["type","attachment","mimeType"]},
            {"type":"object","properties":{"type":{"const":"resource"},"uri":{"type":"string"},"mimeType":{"type":"string"},"text":{"type":"string"},"attachment":{}},"required":["type","uri"]},
            {"type":"object","properties":{"type":{"const":"resource_link"},"uri":{"type":"string"},"name":{"type":"string"},"title":{"type":"string"},"description":{"type":"string"},"mimeType":{"type":"string"}},"required":["type","uri","name"]}
        ]
    });
    let mut properties = serde_json::Map::new();
    properties.insert("content".into(), json!({"type":"array","items":block}));
    let mut structured = structured_schema.cloned().unwrap_or_default();
    let draft = structured.get("$schema").cloned();
    let id_keyword = if draft
        .as_ref()
        .and_then(Value::as_str)
        .is_some_and(|uri| uri.contains("draft-04"))
    {
        "id"
    } else {
        "$id"
    };
    // The embedded schema remains its own resource, so root-relative refs
    // still resolve against the server schema rather than the result envelope.
    if structured_schema.is_some()
        && !structured
            .get(id_keyword)
            .and_then(Value::as_str)
            .is_some_and(|id| !id.is_empty())
    {
        structured.insert(id_keyword.into(), json!("urn:lash:mcp:structured-content"));
    }
    properties.insert("structuredContent".into(), Value::Object(structured));
    let mut envelope = json!({"type":"object","properties":properties,"required":["content"]});
    if let Some(draft) = draft {
        envelope["$schema"] = draft;
    }
    envelope
}

pub fn lashlang_type_expr_schema(ty: &lashlang::TypeExpr) -> serde_json::Value {
    match ty {
        lashlang::TypeExpr::Any
        | lashlang::TypeExpr::Dict
        | lashlang::TypeExpr::Ref(_)
        | lashlang::TypeExpr::Process(_)
        | lashlang::TypeExpr::TriggerHandle(_) => serde_json::json!({}),
        lashlang::TypeExpr::Str => serde_json::json!({ "type": "string" }),
        lashlang::TypeExpr::Int => serde_json::json!({ "type": "integer" }),
        lashlang::TypeExpr::Float => serde_json::json!({ "type": "number" }),
        lashlang::TypeExpr::Bool => serde_json::json!({ "type": "boolean" }),
        lashlang::TypeExpr::Null => serde_json::json!({ "type": "null" }),
        lashlang::TypeExpr::Enum(values) => serde_json::json!({
            "enum": values.iter().map(|value| value.as_str()).collect::<Vec<_>>()
        }),
        lashlang::TypeExpr::List(item) => serde_json::json!({
            "type": "array",
            "items": lashlang_type_expr_schema(item),
        }),
        lashlang::TypeExpr::Object(fields) => {
            let mut properties = serde_json::Map::new();
            let mut required = Vec::new();
            for field in fields {
                properties.insert(field.name.to_string(), lashlang_type_expr_schema(&field.ty));
                if !field.optional {
                    required.push(serde_json::Value::String(field.name.to_string()));
                }
            }
            let mut schema = serde_json::Map::new();
            schema.insert(
                "type".to_string(),
                serde_json::Value::String("object".to_string()),
            );
            schema.insert(
                "properties".to_string(),
                serde_json::Value::Object(properties),
            );
            if !required.is_empty() {
                schema.insert("required".to_string(), serde_json::Value::Array(required));
            }
            schema.insert(
                "additionalProperties".to_string(),
                serde_json::Value::Bool(true),
            );
            serde_json::Value::Object(schema)
        }
        lashlang::TypeExpr::Union(variants) => serde_json::json!({
            "anyOf": variants.iter().map(lashlang_type_expr_schema).collect::<Vec<_>>()
        }),
    }
}

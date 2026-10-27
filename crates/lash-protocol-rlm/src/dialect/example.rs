use lash_kernel_dialect::Spelling;
use lash_kernel_value::tool_shape::{SchemaDocument, SchemaShape, ShapeKind};
use serde_json::{Value, json};

/// Teach a finish only when its offered contract accepts the example. Complex
/// constraints can defeat the small example builder; a print-only cell still
/// teaches transport without claiming an invalid value can finish the turn.
pub(super) fn program(catalog: &lash_core::ToolCatalog, spelling: Spelling) -> String {
    let value = catalog.entries().iter().find_map(|tool| {
        // A host's replacement finish can have a different input convention.
        if tool.manifest.id.as_str() != crate::control_tools::FINISH_TOOL_ID {
            return None;
        }
        let finish = tool.manifest.declaration().controls.finish_declaration()?;
        let schema = &finish.value_schema;
        let doc = SchemaDocument::from_json_schema(schema.as_value(), 8).ok()?;
        let value = sample(&doc.roots[0], &doc, 8, &mut 64)?;
        (schema.validate(&value).is_ok() && value.to_string().len() <= 512).then_some(value)
    });
    match value {
        Some(value) => {
            let value = literal(&value, spelling);
            let end = if spelling == Spelling::TypeScript {
                ";"
            } else {
                ""
            };
            format!("await control.finish({value}){end}")
        }
        None => match spelling {
            Spelling::TypeScript => "console.log(1 + 2);".to_owned(),
            Spelling::Python => "print(1 + 2)".to_owned(),
        },
    }
}

fn sample(
    shape: &SchemaShape,
    doc: &SchemaDocument,
    depth: usize,
    remaining: &mut usize,
) -> Option<Value> {
    let depth = depth.checked_sub(1)?;
    *remaining = remaining.checked_sub(1)?;
    if let Some(value) = &shape.default {
        return Some(value.clone());
    }
    Some(match &shape.kind {
        ShapeKind::Ref(id) => return sample(&doc.definition(*id)?.shape, doc, depth, remaining),
        ShapeKind::Unknown | ShapeKind::Str => json!("Example answer"),
        ShapeKind::Null => Value::Null,
        ShapeKind::Bool => json!(true),
        ShapeKind::Int | ShapeKind::Float => shape
            .constraints
            .minimum
            .clone()
            .map(Value::Number)
            .unwrap_or(json!(0)),
        ShapeKind::Literals(values) => values.first()?.clone(),
        ShapeKind::List(item) => {
            let count = shape.constraints.min_items.unwrap_or(0);
            if count > 8 {
                return None;
            }
            Value::Array(
                (0..count)
                    .map(|_| sample(item, doc, depth, remaining))
                    .collect::<Option<_>>()?,
            )
        }
        ShapeKind::Tuple(items) => Value::Array(
            items
                .iter()
                .map(|item| sample(item, doc, depth, remaining))
                .collect::<Option<_>>()?,
        ),
        ShapeKind::Object(object) => Value::Object(
            object
                .fields
                .iter()
                .filter(|field| field.required)
                .map(|field| {
                    Some((
                        field.name.clone(),
                        sample(&field.shape, doc, depth, remaining)?,
                    ))
                })
                .collect::<Option<_>>()?,
        ),
        ShapeKind::Union(members) => {
            return members
                .iter()
                .find_map(|member| sample(member, doc, depth, remaining));
        }
        _ => return None,
    })
}

fn literal(value: &Value, spelling: Spelling) -> String {
    if spelling == Spelling::TypeScript {
        return value.to_string();
    }
    match value {
        Value::Null => "None".to_owned(),
        Value::Bool(true) => "True".to_owned(),
        Value::Bool(false) => "False".to_owned(),
        Value::Array(items) => format!(
            "[{}]",
            items
                .iter()
                .map(|item| literal(item, spelling))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Value::Object(fields) => format!(
            "{{{}}}",
            fields
                .iter()
                .map(|(key, value)| format!("{}: {}", json!(key), literal(value, spelling)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        _ => value.to_string(),
    }
}

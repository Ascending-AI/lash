use lash::rlm::{NamedDataType, TypeExpr, TypeField};

#[expect(
    clippy::expect_used,
    reason = "`ui.button.Pressed` and its enum/string fields satisfy NamedDataType::object's \
              validation"
)]
pub(super) fn button_pressed_event_type() -> NamedDataType {
    NamedDataType::object(
        "ui.button.Pressed",
        vec![
            TypeField {
                name: "button".into(),
                ty: TypeExpr::union(vec![
                    TypeExpr::Enum(vec!["Red".into()]),
                    TypeExpr::Enum(vec!["Blue".into()]),
                ]),
                optional: false,
            },
            TypeField {
                name: "message".into(),
                ty: TypeExpr::Str,
                optional: false,
            },
            TypeField {
                name: "pressed_at".into(),
                ty: TypeExpr::Str,
                optional: false,
            },
        ],
    )
    .expect("valid e2e button payload type")
}

#[expect(
    clippy::expect_used,
    reason = "this module declares the tool or payload schema and admission checks its invariant"
)]
pub(super) fn button_pressed_payload_schema() -> lash::triggers::JsonSchema {
    lash::triggers::JsonSchema::admit(serde_json::json!({
        "type": "object",
        "properties": {
            "button": { "type": "string", "enum": ["Red", "Blue"] },
            "message": { "type": "string" },
            "pressed_at": { "type": "string" }
        },
        "required": ["button", "message", "pressed_at"],
        "additionalProperties": false
    }))
    .expect("valid declared payload schema")
}

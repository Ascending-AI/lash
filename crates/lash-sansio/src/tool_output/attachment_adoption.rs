//! Adopting a guest runtime's tagged attachments as typed attachments,
//! after the caller has checked their provenance.

use serde_json::{Map, Value};

use super::{ATTACHMENT_TAG, REFERENCE_KEY, TAG_KEY, ToolValue};
use crate::AttachmentRef;

impl ToolValue {
    /// Attachment refs that untrusted subtrees name in the tagged
    /// attachment shape. Each one is a claim, not an attachment. A caller
    /// adopts only the claims whose provenance it has checked, through
    /// [`Self::adopt_attachments`]. A guest runtime that carries a tool's
    /// attachment through its own values hands it back in this shape.
    pub fn untrusted_attachment_claims(&self) -> Vec<AttachmentRef> {
        let mut claims = Vec::new();
        self.collect_untrusted_attachment_claims(&mut claims);
        claims
    }

    fn collect_untrusted_attachment_claims(&self, claims: &mut Vec<AttachmentRef>) {
        match self {
            Self::UntrustedJson(value) => collect_json_attachment_claims(value, claims),
            Self::Array(values) => {
                for value in values {
                    value.collect_untrusted_attachment_claims(claims);
                }
            }
            Self::Object(entries) => {
                for value in entries.values() {
                    value.collect_untrusted_attachment_claims(claims);
                }
            }
            Self::Null
            | Self::Bool(_)
            | Self::Number(_)
            | Self::String(_)
            | Self::Attachment(_) => {}
        }
    }

    /// Rewrites every untrusted tagged attachment whose ref is one of
    /// `adopted` into a typed [`Self::Attachment`]. Everything else stays
    /// untrusted. The projection is unchanged, because a typed attachment
    /// projects to the same tagged shape.
    pub fn adopt_attachments(self, adopted: &[AttachmentRef]) -> Self {
        if adopted.is_empty() {
            return self;
        }
        match self {
            Self::UntrustedJson(value) => adopt_json_attachments(value, adopted),
            Self::Array(values) => Self::Array(
                values
                    .into_iter()
                    .map(|value| value.adopt_attachments(adopted))
                    .collect(),
            ),
            Self::Object(entries) => Self::Object(
                entries
                    .into_iter()
                    .map(|(key, value)| (key, value.adopt_attachments(adopted)))
                    .collect(),
            ),
            other @ (Self::Null
            | Self::Bool(_)
            | Self::Number(_)
            | Self::String(_)
            | Self::Attachment(_)) => other,
        }
    }
}

/// The ref an object names when it has exactly the tagged attachment
/// shape; any other object names none.
fn tagged_attachment_claim(map: &Map<String, Value>) -> Option<AttachmentRef> {
    if map.len() != 2 || map.get(TAG_KEY)?.as_str()? != ATTACHMENT_TAG {
        return None;
    }
    serde_json::from_value(map.get(REFERENCE_KEY)?.clone()).ok()
}

fn collect_json_attachment_claims(value: &Value, claims: &mut Vec<AttachmentRef>) {
    match value {
        Value::Object(map) => match tagged_attachment_claim(map) {
            Some(source) => claims.push(source),
            None if map.contains_key(TAG_KEY) => {}
            None => {
                for value in map.values() {
                    collect_json_attachment_claims(value, claims);
                }
            }
        },
        Value::Array(values) => {
            for value in values {
                collect_json_attachment_claims(value, claims);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
}

fn names_adopted_attachment(value: &Value, adopted: &[AttachmentRef]) -> bool {
    let mut claims = Vec::new();
    collect_json_attachment_claims(value, &mut claims);
    claims.iter().any(|claim| adopted.contains(claim))
}

/// Splits an untrusted value around the adopted attachments it names. A
/// subtree that names none stays one untrusted value. An object carrying
/// the reserved tag key without being an adopted attachment is never
/// split, so a foreign tag cannot decode as a typed value.
fn adopt_json_attachments(value: Value, adopted: &[AttachmentRef]) -> ToolValue {
    if !names_adopted_attachment(&value, adopted) {
        return ToolValue::UntrustedJson(value);
    }
    match value {
        Value::Object(map) => match tagged_attachment_claim(&map) {
            Some(source) => ToolValue::Attachment(source),
            None => ToolValue::Object(
                map.into_iter()
                    .map(|(key, value)| (key, adopt_json_attachments(value, adopted)))
                    .collect(),
            ),
        },
        Value::Array(values) => ToolValue::Array(
            values
                .into_iter()
                .map(|value| adopt_json_attachments(value, adopted))
                .collect(),
        ),
        scalar => ToolValue::UntrustedJson(scalar),
    }
}

//! Recorded JSON literals and attachment slots; deliveries exist only while sending.

use super::attachment_delivery::{AttachmentPosition, Delivery, ProviderAccepts};
use super::types::{GenerationReceipt, LlmContentBlock, LlmRequest, ProviderRouteIdentity};
use crate::AttachmentRef;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SlotCodec {
    pub name: Box<str>,
    pub revision: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttachmentSlot {
    pub reference: AttachmentRef,
    pub position: AttachmentPosition,
    pub accepts: ProviderAccepts,
    pub codec: SlotCodec,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "segment", rename_all = "snake_case", deny_unknown_fields)]
pub enum RequestSegment {
    Literal { text: Arc<str> },
    Attachment { slot: Box<AttachmentSlot> },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordedRequestTemplate {
    pub route: ProviderRouteIdentity,
    pub stream: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<GenerationReceipt>,
    pub segments: Vec<RequestSegment>,
}

impl RecordedRequestTemplate {
    pub fn literal(
        route: ProviderRouteIdentity,
        stream: bool,
        generation: Option<GenerationReceipt>,
        body: impl Into<Arc<str>>,
    ) -> Self {
        Self {
            route,
            stream,
            generation,
            segments: vec![RequestSegment::Literal { text: body.into() }],
        }
    }

    pub fn builder(
        route: ProviderRouteIdentity,
        stream: bool,
        generation: Option<GenerationReceipt>,
    ) -> RequestTemplateBuilder {
        RequestTemplateBuilder {
            template: Self {
                route,
                stream,
                generation,
                segments: Vec::new(),
            },
            literal: String::new(),
        }
    }

    /// Serialize a JSON tree, replacing only the explicitly bound JSON pointers
    /// with typed slots. No searching or replacement of serialized strings.
    pub fn from_json(
        route: ProviderRouteIdentity,
        stream: bool,
        generation: Option<GenerationReceipt>,
        value: &serde_json::Value,
        slots: &[(String, AttachmentSlot)],
    ) -> Result<Self, TemplateError> {
        let mut builder = Self::builder(route, stream, generation);
        let mut used = 0;
        write_json(&mut builder, value, "", slots, &mut used)?;
        if used != slots.len() {
            return Err(TemplateError::SlotCount {
                expected: slots.len(),
                actual: used,
            });
        }
        builder.finish()
    }

    pub fn of_request(
        route: ProviderRouteIdentity,
        request: &LlmRequest,
    ) -> Result<Self, TemplateError> {
        let value =
            serde_json::to_value(request).map_err(|_| TemplateError::InvalidJson { offset: 0 })?;
        let mut slots = Vec::new();
        let mut add = |pointer: String, reference: &AttachmentRef, position| {
            let accepts = ProviderAccepts {
                bytes: true,
                url: true,
                provider_file: None,
            }
            .narrowed(request.attachment_acceptance.forms(
                &route.provider,
                &reference.media_type,
                position,
            ));
            if accepts.is_empty() {
                return Err(TemplateError::EmptyAcceptance);
            }
            slots.push((
                pointer,
                AttachmentSlot {
                    reference: reference.clone(),
                    position,
                    accepts,
                    codec: SlotCodec {
                        name: "lash.canonical".into(),
                        revision: 1,
                    },
                },
            ));
            Ok(())
        };
        for (mi, message) in request.messages.iter().enumerate() {
            for (bi, block) in message.blocks.iter().enumerate() {
                match block {
                    LlmContentBlock::Attachment { reference } => add(
                        format!("/messages/{mi}/blocks/{bi}/Attachment"),
                        reference,
                        AttachmentPosition::Message,
                    )?,
                    LlmContentBlock::ToolResult { content, .. } => {
                        for (pi, part) in content.iter().enumerate() {
                            if let Some(reference) = part.attachment() {
                                add(
                                    format!("/messages/{mi}/blocks/{bi}/ToolResult/content/{pi}"),
                                    reference,
                                    AttachmentPosition::ToolResult,
                                )?;
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        Self::from_json(route, request.stream_events.is_some(), None, &value, &slots)
    }

    pub fn validate(&self) -> Result<(), TemplateError> {
        let mut text = String::new();
        let mut literal = false;
        let mut in_string = false;
        let mut escape = false;
        for segment in &self.segments {
            match segment {
                RequestSegment::Literal { text: span } => {
                    if span.is_empty() {
                        return Err(TemplateError::EmptyLiteral);
                    }
                    if literal {
                        return Err(TemplateError::AdjacentLiterals);
                    }
                    literal = true;
                    text.push_str(span);
                    for byte in span.bytes() {
                        if escape {
                            escape = false;
                        } else if in_string && byte == b'\\' {
                            escape = true;
                        } else if byte == b'"' {
                            in_string = !in_string;
                        }
                    }
                }
                RequestSegment::Attachment { slot } => {
                    if in_string {
                        return Err(TemplateError::InvalidJson { offset: text.len() });
                    }
                    if slot.accepts.is_empty() {
                        return Err(TemplateError::EmptyAcceptance);
                    }
                    literal = false;
                    text.push_str("null");
                }
            }
        }
        serde_json::from_str::<serde_json::Value>(&text).map_err(|error| {
            TemplateError::InvalidJson {
                offset: error.column(),
            }
        })?;
        Ok(())
    }

    pub fn slots(&self) -> impl Iterator<Item = &AttachmentSlot> {
        self.segments.iter().filter_map(|segment| match segment {
            RequestSegment::Attachment { slot } => Some(slot.as_ref()),
            RequestSegment::Literal { .. } => None,
        })
    }

    pub fn redacted(&self) -> String {
        let mut text = String::new();
        for segment in &self.segments {
            match segment {
                RequestSegment::Literal { text: span } => text.push_str(span),
                RequestSegment::Attachment { slot } => text.push_str(
                    &serde_json::json!({"$lash_attachment": {
                        "id": slot.reference.id, "media_type": slot.reference.media_type,
                        "byte_len": slot.reference.byte_len, "position": slot.position
                    }})
                    .to_string(),
                ),
            }
        }
        text
    }
}

pub struct RequestTemplateBuilder {
    template: RecordedRequestTemplate,
    literal: String,
}
impl RequestTemplateBuilder {
    pub fn literal(&mut self, text: impl AsRef<str>) -> &mut Self {
        self.literal.push_str(text.as_ref());
        self
    }
    fn flush_literal(&mut self) {
        if !self.literal.is_empty() {
            self.template.segments.push(RequestSegment::Literal {
                text: Arc::from(std::mem::take(&mut self.literal)),
            });
        }
    }
    pub fn attachment(&mut self, slot: AttachmentSlot) -> &mut Self {
        self.flush_literal();
        self.template.segments.push(RequestSegment::Attachment {
            slot: Box::new(slot),
        });
        self
    }
    pub fn finish(mut self) -> Result<RecordedRequestTemplate, TemplateError> {
        self.flush_literal();
        self.template.validate()?;
        Ok(self.template)
    }
}

fn write_json(
    builder: &mut RequestTemplateBuilder,
    value: &serde_json::Value,
    pointer: &str,
    slots: &[(String, AttachmentSlot)],
    used: &mut usize,
) -> Result<(), TemplateError> {
    if let Some((_, slot)) = slots.iter().find(|(path, _)| path == pointer) {
        builder.attachment(slot.clone());
        *used += 1;
        return Ok(());
    }
    match value {
        serde_json::Value::Array(values) => {
            builder.literal("[");
            for (index, value) in values.iter().enumerate() {
                if index != 0 {
                    builder.literal(",");
                }
                write_json(builder, value, &format!("{pointer}/{index}"), slots, used)?;
            }
            builder.literal("]");
        }
        serde_json::Value::Object(values) => {
            builder.literal("{");
            for (index, (key, value)) in values.iter().enumerate() {
                if index != 0 {
                    builder.literal(",");
                }
                builder
                    .literal(serde_json::Value::String(key.clone()).to_string())
                    .literal(":");
                let escaped = key.replace('~', "~0").replace('/', "~1");
                write_json(builder, value, &format!("{pointer}/{escaped}"), slots, used)?;
            }
            builder.literal("}");
        }
        _ => {
            builder.literal(value.to_string());
        }
    }
    Ok(())
}

/// A JSON value for the live wire, retaining only its scrub patterns privately.
pub struct TransientJson {
    value: String,
    secrets: Vec<String>,
}
impl TransientJson {
    pub fn new(value: &serde_json::Value, delivery: &Delivery) -> Self {
        let mut secrets = Vec::new();
        if let Some(secret) = delivery.secret() {
            let raw = secret.expose();
            if !raw.is_empty() {
                secrets.push(raw.to_owned());
                let escaped = serde_json::Value::String(raw.to_owned()).to_string();
                secrets.push(escaped[1..escaped.len() - 1].to_owned());
            }
        }
        Self {
            value: value.to_string(),
            secrets,
        }
    }
}
impl std::fmt::Debug for TransientJson {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TransientJson(<redacted>)")
    }
}

pub struct LiveRequestBody {
    template: Arc<RecordedRequestTemplate>,
    values: Vec<TransientJson>,
}
impl LiveRequestBody {
    pub fn fill(
        template: Arc<RecordedRequestTemplate>,
        values: Vec<TransientJson>,
    ) -> Result<Self, TemplateError> {
        template.validate()?;
        let expected = template.slots().count();
        if expected != values.len() {
            return Err(TemplateError::SlotCount {
                expected,
                actual: values.len(),
            });
        }
        Ok(Self { template, values })
    }
    pub fn template(&self) -> &RecordedRequestTemplate {
        &self.template
    }
    pub fn route(&self) -> &ProviderRouteIdentity {
        &self.template.route
    }
    pub fn stream(&self) -> bool {
        self.template.stream
    }
    pub fn generation(&self) -> Option<GenerationReceipt> {
        self.template.generation
    }
    pub fn wire(&self) -> String {
        let mut text = String::new();
        let mut values = self.values.iter();
        for segment in &self.template.segments {
            match segment {
                RequestSegment::Literal { text: span } => text.push_str(span),
                RequestSegment::Attachment { .. } => {
                    if let Some(value) = values.next() {
                        text.push_str(&value.value);
                    }
                }
            }
        }
        text
    }
    pub fn redacted(&self) -> String {
        self.template.redacted()
    }
    pub fn redactor(&self) -> DeliveryRedactor {
        let mut secrets: Vec<String> = self
            .values
            .iter()
            .flat_map(|value| value.secrets.iter().cloned())
            .collect();
        secrets.sort_by_key(|secret| std::cmp::Reverse(secret.len()));
        secrets.dedup();
        DeliveryRedactor {
            secrets: Arc::new(secrets),
        }
    }
    pub fn scrubber(&self) -> Arc<dyn Fn(&str) -> String + Send + Sync> {
        let redactor = self.redactor();
        Arc::new(move |text| redactor.scrub(text))
    }
    pub fn scrub(&self, text: &str) -> String {
        self.redactor().scrub(text)
    }
    pub fn has_secrets(&self) -> bool {
        self.values.iter().any(|value| !value.secrets.is_empty())
    }
}
impl std::fmt::Debug for LiveRequestBody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.redacted())
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum TemplateError {
    #[error("a request template contains an empty literal")]
    EmptyLiteral,
    #[error("a request template contains adjacent literals")]
    AdjacentLiterals,
    #[error("a request template is not one JSON value at offset {offset}")]
    InvalidJson { offset: usize },
    #[error("a request template expects {expected} slots, received {actual}")]
    SlotCount { expected: usize, actual: usize },
    #[error("an attachment slot has empty acceptance")]
    EmptyAcceptance,
}

/// A transport may scrub captured provider text without access to secret values.
/// It has no serialized form and its Debug omits its patterns.
#[derive(Clone, PartialEq, Eq)]
pub struct DeliveryRedactor {
    secrets: Arc<Vec<String>>,
}
impl DeliveryRedactor {
    pub fn scrub(&self, text: &str) -> String {
        self.secrets.iter().fold(text.to_owned(), |text, secret| {
            text.replace(secret, "[redacted attachment delivery]")
        })
    }
}
impl std::fmt::Debug for DeliveryRedactor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DeliveryRedactor(<redacted>)")
    }
}

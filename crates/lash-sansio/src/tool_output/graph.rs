//! A tool call's output and its model presentation in the checkpoint graph
//! (FIG-5887 §3, §4).
//!
//! The typed outcome is the execution truth; its presentation is the
//! model-facing truth. A presentation is stored once, as a
//! [`PresentationRecipe`]: what the renderer makes of the outcome, slices
//! of that rendering, and literal text only where neither reproduces it.
//! The recipe records the exact digest and length of the presentation it
//! stands for, and [`PresentationRecipe::rebuild`] refuses any rebuild that
//! is not exactly it: there is no reinterpretation fallback.
//!
//! The codecs here write canonical tokens
//! ([`lash_kernel_doc::graph_tokens`]): structs are sequences of all their
//! fields, enums are sums with the explicit numbers below. Nested payloads
//! this crate does not model field by field (a failure's cause, a control's
//! frame nodes and schema, a view) are written as a canonical JSON value
//! tree ([`encode_json`]), never as JSON text.

use std::collections::BTreeMap;

use lash_kernel_doc::graph_tokens::{
    DecodeDimension, TokenCodec, TokenError, TokenReader, TokenWriter, check_ascending,
    encode_body, within,
};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Number, Value};

use super::{
    AttachmentMaterializationNotice, AttachmentMaterializationReason, CancelOrigin,
    ModelToolReturn, ModelToolReturnPart, ToolCallOutcome, ToolCallOutput, ToolCancellation,
    ToolControl, ToolFailure, ToolFailureClass, ToolFailureSource, ToolValue, TurnControl,
};
use crate::llm::attachment_delivery::AttachmentPosition;
use crate::{AttachmentId, AttachmentRef, AttachmentTypeMetadata, MediaType, RetainedOutput};

/// The deepest tool value tree a decoder reads.
pub const MAX_VALUE_DEPTH: u64 = lash_kernel_doc::MAX_DECODE_DEPTH;

pub use lash_kernel_doc::{FROZEN_JSON, FROZEN_TOOL_VALUE};

fn unsupported(variant: u32) -> TokenError {
    TokenError::UnsupportedVariant { variant }
}

fn invalid(message: impl Into<String>) -> TokenError {
    TokenError::Invalid(message.into())
}

/// A unit-variant enum written as a field-less sum, with its numbers
/// written out once: the declaration order at revision `9503e220ed`.
macro_rules! numbered {
    ($name:ident { $($variant:ident = $number:literal),* $(,)? }) => {
        impl TokenCodec for $name {
            fn encode(&self, out: &mut TokenWriter) {
                let number = match self {
                    $(Self::$variant => $number,)*
                };
                out.sum(number, 0);
            }

            fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
                let (variant, fields) = input.sum()?;
                TokenReader::expect_fields(fields, 0)?;
                match variant {
                    $($number => Ok(Self::$variant),)*
                    other => Err(unsupported(other)),
                }
            }
        }
    };
}

numbered!(ToolFailureClass {
    InvalidRequest = 0,
    Io = 1,
    Unavailable = 2,
    PermissionDenied = 3,
    Timeout = 4,
    Execution = 5,
    External = 6,
    ResourceLimit = 7,
    Internal = 8,
});

numbered!(ToolFailureSource {
    Runtime = 0,
    Tool = 1,
    Plugin = 2,
    Policy = 3,
    Cancellation = 4,
});

numbered!(CancelOrigin {
    TurnStopped = 0,
    ParentEnded = 1,
    OperatorRequested = 2,
    ModelRequested = 3,
    StartFailed = 4,
    RunClosing = 5,
});

numbered!(AttachmentPosition {
    Message = 0,
    ToolResult = 1,
});

numbered!(AttachmentMaterializationReason {
    NoProviderAcceptsMimeAndPosition = 0,
});

// --- JSON value trees ----------------------------------------------------

/// Writes `value` as the kernel's canonical JSON value tree
/// (`lash_kernel_doc`'s codec for [`Value`]).
pub fn encode_json(value: &Value, out: &mut TokenWriter) {
    value.encode(out);
}

/// Reads one JSON value tree [`encode_json`] wrote.
///
/// # Errors
///
/// Any other spelling, or a tree deeper than the kernel's decode depth.
pub fn decode_json(input: &mut TokenReader<'_>) -> Result<Value, TokenError> {
    Value::decode(input)
}

/// A tool value's number, in the JSON tree's number spelling:
/// non-negative integer 0, negative integer 1, finite float 2.
fn encode_number(number: &Number, out: &mut TokenWriter) {
    if let Some(value) = number.as_u64() {
        out.sum(0, 1);
        out.unsigned(value);
    } else if let Some(value) = number.as_i64() {
        out.sum(1, 1);
        out.signed(value);
    } else {
        out.sum(2, 1);
        out.float(number.as_f64().unwrap_or(f64::NAN));
    }
}

fn decode_number(input: &mut TokenReader<'_>) -> Result<Number, TokenError> {
    let (variant, fields) = input.sum()?;
    TokenReader::expect_fields(fields, 1)?;
    match variant {
        0 => Ok(Number::from(input.unsigned()?)),
        1 => {
            let value = input.signed()?;
            if value >= 0 {
                return Err(invalid("a non-negative integer is written as unsigned"));
            }
            Ok(Number::from(value))
        }
        2 => Number::from_f64(input.float()?).ok_or_else(|| invalid("a JSON number is finite")),
        other => Err(unsupported(other)),
    }
}

/// A JSON value as one FrozenValue body: `sum(2, 1, <json>)`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrozenJson(pub Value);

impl TokenCodec for FrozenJson {
    fn encode(&self, out: &mut TokenWriter) {
        out.sum(FROZEN_JSON, 1);
        encode_json(&self.0, out);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        let (variant, fields) = input.sum()?;
        TokenReader::expect_fields(fields, 1)?;
        if variant != FROZEN_JSON {
            return Err(unsupported(variant));
        }
        decode_json(input).map(Self)
    }
}

/// Writes a first-party nested payload as its canonical JSON value tree.
fn encode_tree<T: Serialize>(value: &T, out: &mut TokenWriter) {
    // A plain data type with string keys always has a JSON value.
    encode_json(&serde_json::to_value(value).unwrap_or(Value::Null), out);
}

/// Reads a payload [`encode_tree`] wrote, refusing a tree that is not the
/// one spelling of the value it decodes to.
fn decode_tree<T: Serialize + DeserializeOwned>(
    input: &mut TokenReader<'_>,
) -> Result<T, TokenError> {
    let tree = decode_json(input)?;
    let value: T =
        serde_json::from_value(tree.clone()).map_err(|error| invalid(error.to_string()))?;
    if serde_json::to_value(&value).ok().as_ref() != Some(&tree) {
        return Err(invalid("a payload tree is not its value's one spelling"));
    }
    Ok(value)
}

// --- Attachments ---------------------------------------------------------

fn decode_attachment_id(input: &mut TokenReader<'_>) -> Result<AttachmentId, TokenError> {
    AttachmentId::parse(input.text()?).map_err(|error| invalid(error.to_string()))
}

fn decode_media_type(input: &mut TokenReader<'_>) -> Result<MediaType, TokenError> {
    let text = input.text()?;
    let media = MediaType::parse(text).map_err(|error| invalid(error.to_string()))?;
    if media.as_str() != text {
        return Err(invalid("a media type is written normalized"));
    }
    Ok(media)
}

impl TokenCodec for AttachmentRef {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(5);
        out.text(self.id.as_str());
        out.text(self.media_type.as_str());
        out.unsigned(self.byte_len);
        match &self.type_metadata {
            None => out.none(),
            Some(AttachmentTypeMetadata::Image { width, height }) => {
                out.some();
                out.sum(0, 2);
                width.encode(out);
                height.encode(out);
            }
        }
        self.label.encode(out);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(5)?;
        let id = decode_attachment_id(input)?;
        let media_type = decode_media_type(input)?;
        let byte_len = input.unsigned()?;
        let type_metadata = if input.option()? {
            let (variant, fields) = input.sum()?;
            TokenReader::expect_fields(fields, 2)?;
            match variant {
                0 => Some(AttachmentTypeMetadata::Image {
                    width: Option::decode(input)?,
                    height: Option::decode(input)?,
                }),
                other => return Err(unsupported(other)),
            }
        } else {
            None
        };
        Ok(Self {
            id,
            media_type,
            byte_len,
            type_metadata,
            label: Option::decode(input)?,
        })
    }
}

impl TokenCodec for AttachmentMaterializationNotice {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(5);
        out.text(self.attachment_id.as_str());
        self.label.encode(out);
        out.text(self.media_type.as_str());
        self.position.encode(out);
        self.reason.encode(out);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(5)?;
        Ok(Self {
            attachment_id: decode_attachment_id(input)?,
            label: Option::decode(input)?,
            media_type: decode_media_type(input)?,
            position: AttachmentPosition::decode(input)?,
            reason: AttachmentMaterializationReason::decode(input)?,
        })
    }
}

impl TokenCodec for RetainedOutput {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(2);
        self.reference.encode(out);
        out.text(&self.witness);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(2)?;
        Ok(Self {
            reference: AttachmentRef::decode(input)?,
            witness: input.text()?.to_owned(),
        })
    }
}

// --- Tool values and outputs ---------------------------------------------

/// Tool value variants: Null 0, Bool 1, Number 2, String 3, Array 4,
/// Object 5, Attachment 6, UntrustedJson 7 (declaration order at
/// `9503e220ed`).
impl TokenCodec for ToolValue {
    fn encode(&self, out: &mut TokenWriter) {
        match self {
            Self::Null => out.sum(0, 0),
            Self::Bool(value) => {
                out.sum(1, 1);
                out.bool(*value);
            }
            Self::Number(number) => {
                out.sum(2, 1);
                encode_number(number, out);
            }
            Self::String(text) => {
                out.sum(3, 1);
                out.text(text);
            }
            Self::Array(items) => {
                out.sum(4, 1);
                items.encode(out);
            }
            Self::Object(entries) => {
                out.sum(5, 1);
                out.map(entries.len() as u64);
                for (key, value) in entries {
                    out.text(key);
                    value.encode(out);
                }
            }
            Self::Attachment(reference) => {
                out.sum(6, 1);
                reference.encode(out);
            }
            Self::UntrustedJson(value) => {
                out.sum(7, 1);
                encode_json(value, out);
            }
        }
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        decode_tool_value(input, 1)
    }
}

fn decode_tool_value(input: &mut TokenReader<'_>, depth: u64) -> Result<ToolValue, TokenError> {
    within(DecodeDimension::Depth, depth, MAX_VALUE_DEPTH)?;
    let (variant, fields) = input.sum()?;
    TokenReader::expect_fields(fields, u64::from(variant != 0))?;
    Ok(match variant {
        0 => ToolValue::Null,
        1 => ToolValue::Bool(input.bool()?),
        2 => ToolValue::Number(decode_number(input)?),
        3 => ToolValue::String(input.text()?.to_owned()),
        4 => {
            let count = input.seq()?;
            let mut items = Vec::new();
            for _ in 0..count {
                items.push(decode_tool_value(input, depth + 1)?);
            }
            ToolValue::Array(items)
        }
        5 => {
            let count = input.map()?;
            let mut keys = Vec::new();
            let mut entries = BTreeMap::new();
            for _ in 0..count {
                let key = input.text()?.to_owned();
                let value = decode_tool_value(input, depth + 1)?;
                keys.push(key.clone());
                entries.insert(key, value);
            }
            check_ascending(&keys)?;
            ToolValue::Object(entries)
        }
        6 => ToolValue::Attachment(AttachmentRef::decode(input)?),
        7 => ToolValue::UntrustedJson(decode_json(input)?),
        other => return Err(unsupported(other)),
    })
}

/// A tool value as one FrozenValue body: `sum(1, 1, <tool value>)`.
#[derive(Clone, Debug, PartialEq)]
pub struct FrozenToolValue(pub ToolValue);

impl TokenCodec for FrozenToolValue {
    fn encode(&self, out: &mut TokenWriter) {
        out.sum(FROZEN_TOOL_VALUE, 1);
        self.0.encode(out);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        let (variant, fields) = input.sum()?;
        TokenReader::expect_fields(fields, 1)?;
        if variant != FROZEN_TOOL_VALUE {
            return Err(unsupported(variant));
        }
        ToolValue::decode(input).map(Self)
    }
}

impl TokenCodec for ToolFailure {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(7);
        self.class.encode(out);
        out.text(&self.code);
        out.text(&self.message);
        self.source.encode(out);
        self.suggested_delay_ms.encode(out);
        match &self.cause {
            None => out.none(),
            Some(cause) => {
                out.some();
                encode_tree(cause.as_ref(), out);
            }
        }
        self.raw.encode(out);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(7)?;
        Ok(Self {
            class: ToolFailureClass::decode(input)?,
            code: input.text()?.to_owned(),
            message: input.text()?.to_owned(),
            source: ToolFailureSource::decode(input)?,
            suggested_delay_ms: Option::decode(input)?,
            cause: if input.option()? {
                Some(Box::new(decode_tree(input)?))
            } else {
                None
            },
            raw: Option::decode(input)?,
        })
    }
}

impl TokenCodec for ToolCancellation {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(5);
        out.text(&self.message);
        self.source.encode(out);
        self.origin.encode(out);
        self.raw.encode(out);
        out.bool(self.forced);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(5)?;
        Ok(Self {
            message: input.text()?.to_owned(),
            source: ToolFailureSource::decode(input)?,
            origin: Option::decode(input)?,
            raw: Option::decode(input)?,
            forced: input.bool()?,
        })
    }
}

/// Turn controls: Finish 0 (its value and the schema it settled under, as
/// a tree), SwitchAgentFrame 1 (frame key, initial nodes as a tree, task,
/// the follow-on's run spec as a tree).
impl TokenCodec for TurnControl {
    fn encode(&self, out: &mut TokenWriter) {
        match self {
            Self::Finish {
                value,
                value_schema,
            } => {
                out.sum(0, 2);
                value.encode(out);
                match value_schema {
                    None => out.none(),
                    Some(schema) => {
                        out.some();
                        encode_tree(schema, out);
                    }
                }
            }
            Self::SwitchAgentFrame {
                frame_key,
                initial_nodes,
                task,
                next_run,
            } => {
                out.sum(1, 4);
                encode_tree(frame_key, out);
                encode_tree(initial_nodes, out);
                out.text(task);
                encode_tree(next_run, out);
            }
        }
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        let (variant, fields) = input.sum()?;
        match variant {
            0 => {
                TokenReader::expect_fields(fields, 2)?;
                Ok(Self::Finish {
                    value: ToolValue::decode(input)?,
                    value_schema: if input.option()? {
                        Some(decode_tree(input)?)
                    } else {
                        None
                    },
                })
            }
            1 => {
                TokenReader::expect_fields(fields, 4)?;
                Ok(Self::SwitchAgentFrame {
                    frame_key: decode_tree(input)?,
                    initial_nodes: decode_tree(input)?,
                    task: input.text()?.to_owned(),
                    next_run: decode_tree(input)?,
                })
            }
            other => Err(unsupported(other)),
        }
    }
}

/// Tool controls: Turn 0, Fail 1, AbortRun 2 (its code as a tree).
impl TokenCodec for ToolControl {
    fn encode(&self, out: &mut TokenWriter) {
        match self {
            Self::Turn { control } => {
                out.sum(0, 1);
                control.encode(out);
            }
            Self::Fail { failure } => {
                out.sum(1, 1);
                failure.encode(out);
            }
            Self::AbortRun { code, message } => {
                out.sum(2, 2);
                encode_tree(code, out);
                out.text(message);
            }
        }
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        let (variant, fields) = input.sum()?;
        match variant {
            0 => {
                TokenReader::expect_fields(fields, 1)?;
                Ok(Self::Turn {
                    control: TurnControl::decode(input)?,
                })
            }
            1 => {
                TokenReader::expect_fields(fields, 1)?;
                Ok(Self::Fail {
                    failure: ToolFailure::decode(input)?,
                })
            }
            2 => {
                TokenReader::expect_fields(fields, 2)?;
                Ok(Self::AbortRun {
                    code: decode_tree(input)?,
                    message: input.text()?.to_owned(),
                })
            }
            other => Err(unsupported(other)),
        }
    }
}

/// A tool call's output with its success value written by the caller:
/// how the call outcome node stores it, its answer a separate FrozenValue.
/// Outcome Success 0 (the answer), Failure 1, Cancelled 2; then control,
/// view (as a tree) and projection value (as a tree).
pub trait AnswerSlot {
    /// Writes the success answer.
    fn write(&self, value: &ToolValue, out: &mut TokenWriter);
    /// Reads the success answer back.
    ///
    /// # Errors
    ///
    /// Any refusal of the slot's form.
    fn read(&self, input: &mut TokenReader<'_>) -> Result<ToolValue, TokenError>;
}

/// The answer written inline, as the tool value itself.
pub struct InlineAnswer;

impl AnswerSlot for InlineAnswer {
    fn write(&self, value: &ToolValue, out: &mut TokenWriter) {
        value.encode(out);
    }

    fn read(&self, input: &mut TokenReader<'_>) -> Result<ToolValue, TokenError> {
        ToolValue::decode(input)
    }
}

/// Writes `output` with its answer through `answer`.
pub fn encode_output(output: &ToolCallOutput, answer: &dyn AnswerSlot, out: &mut TokenWriter) {
    out.seq(4);
    match &output.outcome {
        ToolCallOutcome::Success(value) => {
            out.sum(0, 1);
            answer.write(value, out);
        }
        ToolCallOutcome::Failure(failure) => {
            out.sum(1, 1);
            failure.encode(out);
        }
        ToolCallOutcome::Cancelled(cancellation) => {
            out.sum(2, 1);
            cancellation.encode(out);
        }
    }
    output.control.encode(out);
    match &output.view {
        None => out.none(),
        Some(view) => {
            out.some();
            encode_tree(view, out);
        }
    }
    match &output.projection_value {
        None => out.none(),
        Some(value) => {
            out.some();
            encode_json(value, out);
        }
    }
}

/// Reads an output [`encode_output`] wrote.
///
/// # Errors
///
/// Any other spelling.
pub fn decode_output(
    input: &mut TokenReader<'_>,
    answer: &dyn AnswerSlot,
) -> Result<ToolCallOutput, TokenError> {
    input.expect_struct(4)?;
    let (variant, fields) = input.sum()?;
    TokenReader::expect_fields(fields, 1)?;
    let outcome = match variant {
        0 => ToolCallOutcome::Success(answer.read(input)?),
        1 => ToolCallOutcome::Failure(ToolFailure::decode(input)?),
        2 => ToolCallOutcome::Cancelled(ToolCancellation::decode(input)?),
        other => return Err(unsupported(other)),
    };
    Ok(ToolCallOutput {
        outcome,
        control: Option::decode(input)?,
        view: if input.option()? {
            Some(decode_tree(input)?)
        } else {
            None
        },
        projection_value: if input.option()? {
            Some(decode_json(input)?)
        } else {
            None
        },
    })
}

impl TokenCodec for ToolCallOutput {
    fn encode(&self, out: &mut TokenWriter) {
        encode_output(self, &InlineAnswer, out);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        decode_output(input, &InlineAnswer)
    }
}

// --- Model presentation --------------------------------------------------

/// Model return parts: Text 0, Attachment 1, Retained 2.
impl TokenCodec for ModelToolReturnPart {
    fn encode(&self, out: &mut TokenWriter) {
        match self {
            Self::Text { text } => {
                out.sum(0, 1);
                out.text(text);
            }
            Self::Attachment(reference) => {
                out.sum(1, 1);
                reference.encode(out);
            }
            Self::Retained(retained) => {
                out.sum(2, 1);
                retained.encode(out);
            }
        }
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        let (variant, fields) = input.sum()?;
        TokenReader::expect_fields(fields, 1)?;
        match variant {
            0 => Ok(Self::Text {
                text: input.text()?.to_owned(),
            }),
            1 => AttachmentRef::decode(input).map(Self::Attachment),
            2 => RetainedOutput::decode(input).map(Self::Retained),
            other => Err(unsupported(other)),
        }
    }
}

impl TokenCodec for ModelToolReturn {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(3);
        out.text(&self.tool_name);
        self.parts.encode(out);
        self.attachment_notices.encode(out);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(3)?;
        Ok(Self {
            tool_name: input.text()?.to_owned(),
            parts: Vec::decode(input)?,
            attachment_notices: Vec::decode(input)?,
        })
    }
}

/// The spec of the one renderer 1.0 reconstructs presentations with:
/// [`ModelToolReturn::from_output`]'s parts. Any change to what that
/// renderer makes of an output changes this spec, and so its identity: a
/// recipe recorded under another renderer is refused, never re-rendered.
pub const MODEL_RETURN_RENDERER: &str = "lash-model-tool-return/compact-json/v1";

/// A renderer's identity: the digest of its spec.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RendererId(pub [u8; 32]);

impl RendererId {
    /// The identity of [`MODEL_RETURN_RENDERER`].
    #[must_use]
    pub fn model_return() -> Self {
        Self(*blake3::hash(MODEL_RETURN_RENDERER.as_bytes()).as_bytes())
    }
}

fn encode_digest(digest: &[u8; 32], out: &mut TokenWriter) {
    out.bytes(digest);
}

fn decode_digest(input: &mut TokenReader<'_>) -> Result<[u8; 32], TokenError> {
    input
        .bytes()?
        .try_into()
        .map_err(|_| invalid("a digest is 32 bytes"))
}

/// One run of a presentation's text: literal text (0) or a byte slice of
/// the renderer's text for the outcome (1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecipeText {
    Literal(String),
    Slice { start: u64, len: u64 },
}

impl TokenCodec for RecipeText {
    fn encode(&self, out: &mut TokenWriter) {
        match self {
            Self::Literal(text) => {
                out.sum(0, 1);
                out.text(text);
            }
            Self::Slice { start, len } => {
                out.sum(1, 2);
                out.unsigned(*start);
                out.unsigned(*len);
            }
        }
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        let (variant, fields) = input.sum()?;
        match variant {
            0 => {
                TokenReader::expect_fields(fields, 1)?;
                Ok(Self::Literal(input.text()?.to_owned()))
            }
            1 => {
                TokenReader::expect_fields(fields, 2)?;
                Ok(Self::Slice {
                    start: input.unsigned()?,
                    len: input.unsigned()?,
                })
            }
            other => Err(unsupported(other)),
        }
    }
}

/// One part of a recipe: every block the renderer makes of the outcome,
/// verbatim (0); one text block (1); an attachment (2); a retained block
/// with its witness text (3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecipePart {
    Rendered,
    Text(Vec<RecipeText>),
    Attachment(AttachmentRef),
    Retained {
        reference: AttachmentRef,
        witness: Vec<RecipeText>,
    },
}

impl TokenCodec for RecipePart {
    fn encode(&self, out: &mut TokenWriter) {
        match self {
            Self::Rendered => out.sum(0, 0),
            Self::Text(runs) => {
                out.sum(1, 1);
                runs.encode(out);
            }
            Self::Attachment(reference) => {
                out.sum(2, 1);
                reference.encode(out);
            }
            Self::Retained { reference, witness } => {
                out.sum(3, 2);
                reference.encode(out);
                witness.encode(out);
            }
        }
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        let (variant, fields) = input.sum()?;
        match variant {
            0 => {
                TokenReader::expect_fields(fields, 0)?;
                Ok(Self::Rendered)
            }
            1 => {
                TokenReader::expect_fields(fields, 1)?;
                Ok(Self::Text(Vec::decode(input)?))
            }
            2 => {
                TokenReader::expect_fields(fields, 1)?;
                Ok(Self::Attachment(AttachmentRef::decode(input)?))
            }
            3 => {
                TokenReader::expect_fields(fields, 2)?;
                Ok(Self::Retained {
                    reference: AttachmentRef::decode(input)?,
                    witness: Vec::decode(input)?,
                })
            }
            other => Err(unsupported(other)),
        }
    }
}

/// How a call's model presentation is rebuilt from its typed outcome: the
/// renderer, the parts, the attachment notices, and the exact digest and
/// length of the canonical encoding of the [`ModelToolReturn`] it stands
/// for (FIG-5887 §4).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PresentationRecipe {
    pub renderer: RendererId,
    pub tool_name: String,
    pub parts: Vec<RecipePart>,
    pub notices: Vec<AttachmentMaterializationNotice>,
    pub digest: [u8; 32],
    pub bytes: u64,
}

impl TokenCodec for PresentationRecipe {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(6);
        encode_digest(&self.renderer.0, out);
        out.text(&self.tool_name);
        self.parts.encode(out);
        self.notices.encode(out);
        encode_digest(&self.digest, out);
        out.unsigned(self.bytes);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(6)?;
        Ok(Self {
            renderer: RendererId(decode_digest(input)?),
            tool_name: input.text()?.to_owned(),
            parts: Vec::decode(input)?,
            notices: Vec::decode(input)?,
            digest: decode_digest(input)?,
            bytes: input.unsigned()?,
        })
    }
}

/// Why a recipe did not rebuild its presentation.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PresentationRefusal {
    /// The recipe names a renderer this build does not hold.
    #[error("the presentation's renderer is not one this build holds")]
    RendererUnavailable,
    /// The rebuild is not the presentation the recipe recorded.
    #[error("the rebuilt presentation is not the one recorded")]
    Mismatch,
}

/// The digest and length of `presentation`'s canonical encoding.
#[must_use]
pub fn presentation_digest(presentation: &ModelToolReturn) -> ([u8; 32], u64) {
    let bytes = encode_body(presentation);
    (*blake3::hash(&bytes).as_bytes(), bytes.len() as u64)
}

/// The renderer's text: every text block it makes of `output`, joined.
fn rendered_text(rendered: &[ModelToolReturnPart]) -> String {
    rendered
        .iter()
        .filter_map(|part| match part {
            ModelToolReturnPart::Text { text } => Some(text.as_str()),
            ModelToolReturnPart::Attachment(_) | ModelToolReturnPart::Retained(_) => None,
        })
        .collect()
}

/// `text` as runs over `source`: one slice when `source` holds it whole;
/// otherwise the longest prefix `source` starts with as a slice, then the
/// rest literally.
fn runs_over(text: &str, source: &str) -> Vec<RecipeText> {
    if text.is_empty() {
        return vec![RecipeText::Literal(String::new())];
    }
    if let Some(start) = source.find(text) {
        return vec![RecipeText::Slice {
            start: start as u64,
            len: text.len() as u64,
        }];
    }
    let mut common = text
        .bytes()
        .zip(source.bytes())
        .take_while(|(left, right)| left == right)
        .count();
    while !text.is_char_boundary(common) {
        common -= 1;
    }
    let mut runs = Vec::new();
    if common > 0 {
        runs.push(RecipeText::Slice {
            start: 0,
            len: common as u64,
        });
    }
    runs.push(RecipeText::Literal(text[common..].to_owned()));
    runs
}

fn assemble(runs: &[RecipeText], source: &str) -> Result<String, PresentationRefusal> {
    let mut text = String::new();
    for run in runs {
        match run {
            RecipeText::Literal(literal) => text.push_str(literal),
            RecipeText::Slice { start, len } => {
                let start = usize::try_from(*start).map_err(|_| PresentationRefusal::Mismatch)?;
                let len = usize::try_from(*len).map_err(|_| PresentationRefusal::Mismatch)?;
                let end = start
                    .checked_add(len)
                    .ok_or(PresentationRefusal::Mismatch)?;
                text.push_str(
                    source
                        .get(start..end)
                        .ok_or(PresentationRefusal::Mismatch)?,
                );
            }
        }
    }
    Ok(text)
}

impl PresentationRecipe {
    /// The recipe of `presentation`, the model return recorded for a call
    /// whose typed outcome is `output`. A presentation the renderer makes
    /// whole is one [`RecipePart::Rendered`]; text it cut or extended is
    /// sliced from the renderer's text; anything else is literal.
    #[must_use]
    pub fn of(presentation: &ModelToolReturn, output: &ToolCallOutput) -> Self {
        let rendered = ModelToolReturn::from_output(String::new(), output).parts;
        let source = rendered_text(&rendered);
        let parts = if presentation.parts == rendered {
            vec![RecipePart::Rendered]
        } else {
            presentation
                .parts
                .iter()
                .map(|part| match part {
                    ModelToolReturnPart::Text { text } => {
                        RecipePart::Text(runs_over(text, &source))
                    }
                    ModelToolReturnPart::Attachment(reference) => {
                        RecipePart::Attachment(reference.clone())
                    }
                    ModelToolReturnPart::Retained(retained) => RecipePart::Retained {
                        reference: retained.reference.clone(),
                        witness: runs_over(&retained.witness, &source),
                    },
                })
                .collect()
        };
        let (digest, bytes) = presentation_digest(presentation);
        Self {
            renderer: RendererId::model_return(),
            tool_name: presentation.tool_name.clone(),
            parts,
            notices: presentation.attachment_notices.clone(),
            digest,
            bytes,
        }
    }

    /// The presentation this recipe recorded, rebuilt from `output`.
    ///
    /// # Errors
    ///
    /// [`PresentationRefusal::RendererUnavailable`] for another renderer,
    /// [`PresentationRefusal::Mismatch`] for a rebuild whose digest or
    /// length is not the recorded one.
    pub fn rebuild(&self, output: &ToolCallOutput) -> Result<ModelToolReturn, PresentationRefusal> {
        if self.renderer != RendererId::model_return() {
            return Err(PresentationRefusal::RendererUnavailable);
        }
        let rendered = ModelToolReturn::from_output(String::new(), output).parts;
        let source = rendered_text(&rendered);
        let mut parts = Vec::new();
        for part in &self.parts {
            match part {
                RecipePart::Rendered => parts.extend(rendered.iter().cloned()),
                RecipePart::Text(runs) => parts.push(ModelToolReturnPart::Text {
                    text: assemble(runs, &source)?,
                }),
                RecipePart::Attachment(reference) => {
                    parts.push(ModelToolReturnPart::Attachment(reference.clone()));
                }
                RecipePart::Retained { reference, witness } => {
                    parts.push(ModelToolReturnPart::Retained(RetainedOutput {
                        reference: reference.clone(),
                        witness: assemble(witness, &source)?,
                    }));
                }
            }
        }
        let presentation = ModelToolReturn {
            tool_name: self.tool_name.clone(),
            parts,
            attachment_notices: self.notices.clone(),
        };
        if presentation_digest(&presentation) != (self.digest, self.bytes) {
            return Err(PresentationRefusal::Mismatch);
        }
        Ok(presentation)
    }
}

#[cfg(test)]
#[path = "graph_tests.rs"]
mod tests;

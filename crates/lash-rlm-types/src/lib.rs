use lash_sansio::{
    CellFailure, CellFailureKind, CellOutcome, CellRecord, ExecutedCall, ExecutedCallOutcome,
    OutputValue, RetainedOutput, SchemaShape, ShapeKind, TerminationMode, TurnProtocol,
};

/// Read-only legacy protocol-owned assistant context paired with an RLM
/// trajectory entry.
///
/// New sessions persist this context as ordinary durable assistant messages so
/// provider reasoning replay metadata survives. This event must keep decoding
/// for old session histories, but producers must not write new instances.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct RlmAssistantContent {
    pub id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reasoning: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub prose: String,
}

/// One executed call as a cell's `history` shows it: the operation and how
/// it settled, with no arguments and no host identity. A view derived from
/// the cell entry's [`ExecutedCall`], never a stored shape.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
pub struct HistoryExecutedCall {
    pub operation: String,
    pub outcome: ExecutedCallOutcome,
}

impl From<&ExecutedCall> for HistoryExecutedCall {
    fn from(call: &ExecutedCall) -> Self {
        Self {
            operation: call.operation.clone(),
            outcome: call.outcome,
        }
    }
}

fn is_zero(value: &usize) -> bool {
    *value == 0
}

#[derive(
    Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum RlmHistoryRole {
    User,
    System,
    Assistant,
    Event,
}

#[derive(
    Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
pub struct RlmAttachmentRef {
    /// Attachment occurrence id within the history message.
    pub id: String,
    pub reference: lash_sansio::AttachmentRef,
}

#[derive(
    Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
pub struct RlmImageRef {
    pub id: String,
    #[schemars(with = "String")]
    pub media_type: lash_sansio::MediaType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    pub bytes: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

impl RlmImageRef {
    /// Build the model-visible image metadata from its durable attachment reference.
    pub fn from_attachment(attachment: &lash_sansio::AttachmentRef) -> Self {
        let (width, height) = match attachment.type_metadata.as_ref() {
            Some(lash_sansio::AttachmentTypeMetadata::Image { width, height }) => (*width, *height),
            None => (None, None),
        };
        Self {
            id: attachment.id.to_string(),
            media_type: attachment.media_type.clone(),
            width,
            height,
            bytes: attachment.byte_len as usize,
            label: attachment.label.clone(),
        }
    }
}

/// The key that marks a history value as Lash's own record rather than a
/// value the cell produced. The `$lash_` prefix is reserved.
pub const HISTORY_VALUE_TAG_KEY: &str = "$lash_history_value";

/// The tag of a [`HistoryValue::Retained`] record.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum HistoryValueTag {
    Retained,
}

/// A value retained out of history (FIG-1643), as a cell reads it: its
/// witness, its size, and the attachment that holds it whole — a value the
/// cell can hand to a tool that reads attachments.
#[derive(Clone, Debug, PartialEq, serde::Serialize, schemars::JsonSchema)]
pub struct RetainedHistoryValue {
    #[serde(rename = "$lash_history_value")]
    pub tag: HistoryValueTag,
    pub witness: String,
    pub byte_len: u64,
    pub attachment: serde_json::Value,
}

/// A printed or final value as a cell reads it back through `history`: the
/// value itself, or the one reserved-tag record of a value retained out of
/// history. Reading history never loads a retained value.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
#[serde(untagged)]
pub enum HistoryValue {
    Retained(RetainedHistoryValue),
    Inline(serde_json::Value),
}

impl From<&OutputValue> for HistoryValue {
    fn from(value: &OutputValue) -> Self {
        match value {
            OutputValue::Inline(value) => Self::Inline(value.clone()),
            OutputValue::Retained(retained) => Self::Retained(retained.into()),
        }
    }
}

impl From<&RetainedOutput> for RetainedHistoryValue {
    fn from(retained: &RetainedOutput) -> Self {
        Self {
            tag: HistoryValueTag::Retained,
            witness: retained.witness.clone(),
            byte_len: retained.reference.byte_len,
            attachment: lash_sansio::ToolValue::Attachment(retained.reference.clone())
                .to_json_value(),
        }
    }
}

impl schemars::JsonSchema for HistoryValue {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "HistoryValue".into()
    }

    /// Any JSON value: an inline value is whatever the cell produced, and the
    /// retained record is one such object.
    fn json_schema(_generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::Schema::default()
    }
}

/// A cell's failure as a later cell reads it: the closed kind and the
/// failure's own message. Recovery guidance is prompt text, not history.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
pub struct HistoryCellError {
    pub kind: CellFailureKind,
    pub message: String,
}

impl From<&CellFailure> for HistoryCellError {
    fn from(failure: &CellFailure) -> Self {
        Self {
            kind: failure.kind,
            message: failure.message.clone(),
        }
    }
}

/// What a step resolved to, as a later cell reads it: `error` for a failed
/// cell, `final_output` for a finished one, neither for a cell that ran to
/// its end. At most one key is ever written, and a finished cell whose value
/// is `null` writes `final_output: null`. A view derived from the cell
/// record's [`CellOutcome`], never a stored shape.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, schemars::JsonSchema)]
pub struct HistoryStepOutcome {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<HistoryCellError>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub final_output: Option<HistoryValue>,
}

impl From<&CellOutcome> for HistoryStepOutcome {
    fn from(result: &CellOutcome) -> Self {
        Self {
            error: result.failure().map(HistoryCellError::from),
            final_output: result.finish().map(HistoryValue::from),
        }
    }
}

/// One item of the `history` a cell reads. This serialized shape is the
/// model-visible one: [`history_item_shape`] derives the type a prompt
/// declares from it.
#[derive(Clone, Debug, PartialEq, serde::Serialize, schemars::JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RlmHistoryItem {
    Message {
        id: String,
        role: RlmHistoryRole,
        content: String,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        attachments: Vec<RlmAttachmentRef>,
    },
    LashVmStep {
        id: String,
        protocol_iteration: usize,
        code: String,
        output: Vec<serde_json::Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        output_archive: Option<RetainedHistoryValue>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        images: Vec<RlmImageRef>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        calls: Vec<HistoryExecutedCall>,
        #[serde(skip_serializing_if = "is_zero")]
        calls_omitted: usize,
        #[serde(flatten)]
        outcome: Box<HistoryStepOutcome>,
    },
}

impl RlmHistoryItem {
    /// The model-visible view of a committed cell record: prints as their
    /// values, the failure as its kind and message, and images as compact
    /// metadata instead of the attachment references the record holds.
    pub fn from_cell_record(record: &CellRecord) -> Self {
        Self::LashVmStep {
            id: record.id.clone(),
            protocol_iteration: record.protocol_iteration,
            code: record.code.clone(),
            output: record
                .prints
                .iter()
                .map(|print| print.value.clone())
                .collect(),
            output_archive: record
                .prints_retained
                .as_ref()
                .map(RetainedHistoryValue::from),
            images: record
                .images
                .iter()
                .map(RlmImageRef::from_attachment)
                .collect(),
            calls: record.calls.iter().map(HistoryExecutedCall::from).collect(),
            calls_omitted: record.calls_omitted,
            outcome: Box::new((&record.result).into()),
        }
    }
}

/// The type of one `history` item, read from [`RlmHistoryItem`]'s own
/// serialized shape so a prompt can never declare a key the item does not
/// write or leave out one it does. A dialect spells it through its schema
/// renderer. `images` drops the step's `images` key for a session whose
/// model is shown no images.
pub fn history_item_shape(images: bool) -> SchemaShape {
    // The serialize contract: a key the item may skip is optional.
    let schema = schemars::generate::SchemaSettings::default()
        .for_serialize()
        .into_generator()
        .into_root_schema_for::<RlmHistoryItem>();
    let mut shape = SchemaShape::from_json_schema(schema.as_value());
    if !images && let ShapeKind::Union(members) = &mut shape.kind {
        for member in members {
            if let ShapeKind::Object(object) = &mut member.kind {
                object.fields.retain(|field| field.name != "images");
            }
        }
    }
    shape
}

#[cfg(test)]
mod rlm_step_serde_tests {
    use std::collections::BTreeSet;

    use lash_sansio::{CellFailure, CellFailureKind, ShapeKind};

    use lash_sansio::{CellOutcome, CellRecord};

    use super::RlmHistoryItem;

    fn populated_entry() -> CellRecord {
        CellRecord {
            prints_retained: None,
            id: "step-1".to_string(),
            protocol_iteration: 3,
            language: "typescript".to_string(),
            code: "print('hello')".to_string(),
            prints: vec!["hello".to_string().into()],
            images: vec![lash_sansio::AttachmentRef {
                id: ("a".repeat(64)).parse().expect("valid attachment id"),
                media_type: "image/png".parse().expect("valid media type"),
                byte_len: 42,
                type_metadata: Some(lash_sansio::AttachmentTypeMetadata::image(
                    Some(640),
                    Some(480),
                )),
                label: Some("plot".to_string()),
            }],
            calls: vec![lash_sansio::ExecutedCall {
                operation: "math.add".to_string(),
                outcome: lash_sansio::ExecutedCallOutcome::Ok,
                call_id: Some(lash_sansio::ToolCallId::fixture("math-add")),
            }],
            calls_omitted: 2,
            bindings: Default::default(),
            result: finished(serde_json::json!({"answer": 42}).into()),
        }
    }

    fn finished(value: super::OutputValue) -> CellOutcome {
        CellOutcome::Controlled {
            tool_name: "finish".to_string(),
            call_id: lash_sansio::ToolCallId::fixture("finish"),
            control: lash_sansio::CellControl::Finish { value },
        }
    }

    fn retained(witness: &str) -> lash_sansio::RetainedOutput {
        lash_sansio::RetainedOutput {
            reference: lash_sansio::AttachmentRef {
                id: ("b".repeat(64)).parse().expect("valid attachment id"),
                media_type: "application/json".parse().expect("valid media type"),
                byte_len: 90_000,
                type_metadata: None,
                label: None,
            },
            witness: witness.to_string(),
        }
    }

    fn program_failure() -> CellFailure {
        CellFailure::new(
            CellFailureKind::Program,
            "ReferenceError: rows is not defined",
        )
    }

    fn history(entry: &CellRecord) -> serde_json::Value {
        serde_json::to_value(RlmHistoryItem::from_cell_record(entry)).expect("item encodes")
    }

    /// A retained value has one cell-visible spelling (FIG-4658 F66): an archive
    /// and a final value read back as the same reserved-tag record, whose
    /// `attachment` is a value a tool that reads attachments accepts.
    #[test]
    fn a_step_archive_and_retained_final_value_share_the_attachment_record() {
        let entry = CellRecord {
            prints_retained: Some(retained("[{\"value\": {\"rows\":[")),
            prints: Vec::new(),
            result: finished(super::OutputValue::Retained(retained("{\"rows\":["))),
            ..populated_entry()
        };
        let item = history(&entry);

        assert_eq!(item["output"], serde_json::json!([]));
        assert_eq!(
            item["output_archive"]["attachment"],
            item["final_output"]["attachment"]
        );
        assert!(item.get("final_output_retained").is_none());
        let record = &item["final_output"];
        assert_eq!(record[super::HISTORY_VALUE_TAG_KEY], "retained");
        assert_eq!(record["witness"], "{\"rows\":[");
        assert_eq!(record["byte_len"], 90_000);
        let adopted: lash_sansio::ToolValue = serde_json::from_value(record["attachment"].clone())
            .expect("the attachment decodes as a tool value");
        assert!(
            matches!(adopted, lash_sansio::ToolValue::Attachment(_)),
            "a tool adopts the record's attachment: {adopted:?}"
        );
    }

    #[test]
    fn a_null_finish_is_distinct_from_a_cell_that_ran_to_its_end() {
        for (result, expected) in [
            (
                finished(serde_json::Value::Null.into()),
                Some(serde_json::Value::Null),
            ),
            (CellOutcome::Completed, None),
            (
                finished(serde_json::json!({"answer": 42}).into()),
                Some(serde_json::json!({"answer": 42})),
            ),
        ] {
            let item = history(&CellRecord {
                result,
                ..populated_entry()
            });
            assert!(item.get("error").is_none());
            assert_eq!(item.get("final_output"), expected.as_ref());
        }
    }

    /// A failed step reads back as the failure's closed kind and its own
    /// message, with no rendered guidance and no host detail.
    #[test]
    fn a_failed_step_reads_back_as_its_kind_and_message() {
        let item = history(&CellRecord {
            result: CellOutcome::Failed(program_failure()),
            ..populated_entry()
        });
        assert_eq!(
            item["error"],
            serde_json::json!({
                "kind": "program",
                "message": "ReferenceError: rows is not defined",
            })
        );
        assert!(item.get("final_output").is_none());
    }

    /// Every history item this build can serialize, with every optional key
    /// present somewhere.
    fn history_exemplars() -> Vec<serde_json::Value> {
        let message = RlmHistoryItem::Message {
            id: "m1".to_string(),
            role: super::RlmHistoryRole::User,
            content: "hello".to_string(),
            attachments: vec![super::RlmAttachmentRef {
                id: "a1".to_string(),
                reference: lash_sansio::AttachmentRef::new(
                    lash_sansio::AttachmentId::parse("a".repeat(64)).expect("digest"),
                    "text/plain".parse().expect("valid media type"),
                    4,
                    None,
                    Some("notes".to_string()),
                ),
            }],
        };
        let bare_message = RlmHistoryItem::Message {
            id: "m2".to_string(),
            role: super::RlmHistoryRole::Assistant,
            content: "hi".to_string(),
            attachments: Vec::new(),
        };
        let mut items = vec![
            serde_json::to_value(message).expect("message encodes"),
            serde_json::to_value(bare_message).expect("message encodes"),
            history(&populated_entry()),
            history(&CellRecord {
                id: "step-0".to_string(),
                code: "1".to_string(),
                ..CellRecord::default()
            }),
        ];
        for result in [
            finished(serde_json::json!({"answer": 42}).into()),
            finished(super::OutputValue::Retained(retained("{\"answer\""))),
            CellOutcome::Failed(program_failure()),
            CellOutcome::Completed,
        ] {
            items.push(history(&CellRecord {
                prints_retained: Some(retained("ordered step observations")),
                prints: Vec::new(),
                result,
                ..populated_entry()
            }));
        }
        items
    }

    /// The model-visible `HistoryItem` type is read from the item's own
    /// serialized shape (FIG-4658 F66): every key an item writes is declared
    /// for its `kind`, every required key is always written, and the
    /// declaration names no key that no item writes.
    #[test]
    fn the_declared_history_item_shape_is_the_shape_items_serialize_as() {
        let shape = super::history_item_shape(true);
        let ShapeKind::Union(members) = &shape.kind else {
            panic!("a history item is one of its kinds: {shape:?}");
        };
        let exemplars = history_exemplars();
        let mut kinds = BTreeSet::new();
        for member in members {
            let fields = member.fields();
            let kind_field = fields
                .iter()
                .find(|field| field.name == "kind")
                .expect("every kind declares its tag");
            let ShapeKind::Literals(literals) = &kind_field.shape.kind else {
                panic!("the tag is a literal: {kind_field:?}");
            };
            let [kind] = literals.as_slice() else {
                panic!("one tag per kind: {literals:?}");
            };
            kinds.insert(kind.as_str().expect("a string tag").to_string());
            let of_kind = exemplars
                .iter()
                .filter(|item| &item["kind"] == kind)
                .map(|item| item.as_object().expect("an item is an object"))
                .collect::<Vec<_>>();
            assert!(!of_kind.is_empty(), "no exemplar of kind {kind}");
            let declared = fields
                .iter()
                .map(|field| field.name.as_str())
                .collect::<BTreeSet<_>>();
            let written = of_kind
                .iter()
                .flat_map(|item| item.keys().map(String::as_str))
                .collect::<BTreeSet<_>>();
            assert_eq!(declared, written, "kind {kind}");
            for field in fields.iter().filter(|field| field.required) {
                assert!(
                    of_kind.iter().all(|item| item.contains_key(&field.name)),
                    "kind {kind} declares `{}` required",
                    field.name
                );
            }
        }
        assert_eq!(
            kinds,
            exemplars
                .iter()
                .map(|item| item["kind"].as_str().expect("a string tag").to_string())
                .collect()
        );
    }

    #[test]
    fn a_session_without_images_declares_no_images_key() {
        let shape = super::history_item_shape(false);
        let ShapeKind::Union(members) = &shape.kind else {
            panic!("a history item is one of its kinds: {shape:?}");
        };
        assert!(
            members
                .iter()
                .flat_map(|member| member.fields())
                .all(|field| field.name != "images")
        );
    }
}

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct RlmGlobalsPatchPluginBody {
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub set_default: serde_json::Map<String, serde_json::Value>,
}

impl RlmGlobalsPatchPluginBody {
    pub fn is_empty(&self) -> bool {
        self.set_default.is_empty()
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq)]
pub enum RlmProtocolEvent {
    RlmAssistantContent(RlmAssistantContent),
    /// One executed cell: the record the transcript returns.
    RlmTrajectoryEntry(Box<CellRecord>),
    RlmGlobalsPatch(RlmGlobalsPatchPluginBody),
    RlmSeed(RlmSeedPluginBody),
    RlmDiagnostic(RlmDiagnosticEvent),
}

/// The protocol step an [`RlmDiagnosticEvent`] reports on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RlmDiagnosticPhase {
    /// A program extracted from a text-protocol reply.
    LlmExtraction,
    /// A program extracted from a native tool-call reply.
    NativeExtraction,
    /// The turn's no-progress budget ran out.
    NoProgressBudget,
    /// Bindings that degraded while a projection was rehydrated.
    ProjectionRehydration,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct RlmDiagnosticEvent {
    pub phase: RlmDiagnosticPhase,
    #[serde(default)]
    pub payload: serde_json::Value,
}

/// RLM protocol session config. Natural turns finish with prose-only model
/// responses or a declared control call (`control.finish`). Programmatic
/// turns can require a control call ([`TerminationMode::TerminalRequired`]).
/// Either mode can state the schema `control.finish` takes its value under.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RlmCreateExtras {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub render: Option<RlmRenderPatch>,
    /// Session-wide termination requirement. Absence is the `Natural` default.
    ///
    /// Absence is a distinct statement from an explicit `Natural`: options that
    /// say nothing about termination must leave a recorded `TerminalRequired`
    /// alone, and only a value that is genuinely stated participates in the
    /// set-if-unset guard (ADR 0066).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub termination: Option<TerminationMode>,
    /// The schema `control.finish` takes its value under: the host's
    /// final-answer schema. Absent, the value is any JSON value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finish_schema: Option<lash_sansio::JsonSchema>,
}

/// The RLM options a *single turn* may state again (FIG-1979).
///
/// There is currently no language choice to state: TypeScript is the only shipped RLM dialect
/// and nothing — a turn bag, a session bag, a create contract — names one.
///
/// These are the RLM owner's run options: the owner applies each stated
/// field over the session's recorded value, and an unstated one leaves it
/// alone. Nothing else is a field, so a payload that names a session pin
/// does not decode (FIG-4652).
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RlmTurnOptions {
    /// Termination requirement for this turn. Absence is the `Natural`
    /// default, and leaves whatever the session recorded alone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub termination: Option<TerminationMode>,
    /// The schema `control.finish` takes its value under for this turn.
    /// Absence leaves whatever the session recorded alone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finish_schema: Option<lash_sansio::JsonSchema>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub render: Option<RlmRenderPatch>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RlmRenderPatch {
    #[serde(
        default,
        skip_serializing_if = "lash_render::RenderParamsPatch::is_empty"
    )]
    pub print: lash_render::RenderParamsPatch,
    #[serde(
        default,
        skip_serializing_if = "lash_render::RenderParamsPatch::is_empty"
    )]
    pub preview: lash_render::RenderParamsPatch,
}

impl RlmTurnOptions {
    /// The termination this bag means, resolving absence to the default.
    pub fn effective_termination(&self) -> TerminationMode {
        self.termination.unwrap_or_default()
    }
}

/// The durable RLM facts a session has recorded, read as recorded.
///
/// Every field is `Option`-shaped on purpose: `None` means the session has
/// stated nothing yet, which is a different answer from the value the default
/// resolves to. A host that cannot tell those apart has to peek at raw payload
/// keys to label a fresh session honestly — the hack this type replaces.
///
/// The facts are recorded once, when the session is created (FIG-4379): no
/// config command changes them, and a turn states them again through its run's
/// protocol turn options.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RlmSessionConfig {
    pub termination: Option<TerminationMode>,
    pub finish_schema: Option<lash_sansio::JsonSchema>,
}

impl RlmSessionConfig {
    /// An empty request, stating nothing.
    pub fn new() -> Self {
        Self::default()
    }

    pub fn termination(mut self, termination: TerminationMode) -> Self {
        self.termination = Some(termination);
        self
    }

    pub fn finish_schema(mut self, schema: lash_sansio::JsonSchema) -> Self {
        self.finish_schema = Some(schema);
        self
    }

    pub fn is_empty(&self) -> bool {
        self.termination.is_none() && self.finish_schema.is_none()
    }
}

impl From<&RlmCreateExtras> for RlmSessionConfig {
    fn from(extras: &RlmCreateExtras) -> Self {
        Self {
            termination: extras.termination,
            finish_schema: extras.finish_schema.clone(),
        }
    }
}

impl From<&RlmSessionConfig> for RlmCreateExtras {
    fn from(config: &RlmSessionConfig) -> Self {
        Self {
            render: None,
            termination: config.termination,
            finish_schema: config.finish_schema.clone(),
        }
    }
}

/// One durable projected seed binding.
///
/// The explicit tag keeps the entry disjoint from ordinary JSON: materialized
/// data can spell any object keys — `kind` and `value` included — without
/// acquiring entry semantics during restore.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum RlmProjectedSeedEntry {
    Materialized(serde_json::Value),
}

/// Wire-format snapshot of a set of projected bindings. Pairs of
/// `(name, entry)` get re-projected as host bindings on the child session at
/// creation time. This is the serializable form of
/// `lash_protocol_rlm::RlmProjectedBindings`; lash-rlm-types stays free of any
/// runtime dependency on lash_vm itself.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RlmProjectedSeedSnapshot {
    pub entries: Vec<(String, RlmProjectedSeedEntry)>,
}

impl RlmProjectedSeedSnapshot {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, name: impl Into<String>, entry: RlmProjectedSeedEntry) {
        self.entries.push((name.into(), entry));
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RlmSeedPluginBody {
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub globals: serde_json::Map<String, serde_json::Value>,
    #[serde(default, skip_serializing_if = "RlmProjectedSeedSnapshot::is_empty")]
    pub projected: RlmProjectedSeedSnapshot,
    /// The saved functions the session is created with, by binding: each
    /// value is a kernel saved function as the RLM protocol stores one.
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub functions: serde_json::Map<String, serde_json::Value>,
}

impl RlmSeedPluginBody {
    pub fn is_empty(&self) -> bool {
        self.globals.is_empty() && self.projected.is_empty() && self.functions.is_empty()
    }
}

/// Reserved JSON key that encodes a projected seed entry when the model
/// passes a projected source as a tool argument:
/// `{"__projected__": <tagged seed entry>}`.
pub const PROJECTED_JSON_TAG: &str = "__projected__";

#[cfg(test)]
mod projected_seed_tests {
    use super::*;

    #[test]
    fn projected_seed_entry_serde_is_tagged_and_rejects_the_legacy_shape() {
        let entry = RlmProjectedSeedEntry::Materialized(serde_json::json!({
            "__projection_ref__": {
                "kind": "memory",
                "key": "data",
            }
        }));

        assert_eq!(
            serde_json::to_value(&entry).expect("serialize seed entry"),
            serde_json::json!({
                "kind": "materialized",
                "value": {
                    "__projection_ref__": {
                        "kind": "memory",
                        "key": "data",
                    }
                }
            })
        );
        assert!(
            serde_json::from_value::<RlmProjectedSeedEntry>(serde_json::json!({
                "__projection_ref__": {
                    "kind": "memory",
                    "key": "data",
                }
            }))
            .is_err(),
            "the untagged legacy seed entry must not decode"
        );
        assert!(
            serde_json::from_value::<RlmProjectedSeedEntry>(serde_json::json!({
                "kind": "ref",
                "value": {
                    "kind": "memory",
                    "key": "data",
                }
            }))
            .is_err(),
            "a durable projection ref must not decode"
        );
    }
}

#[derive(Clone, Debug)]
pub struct RlmTurnProtocol;

impl TurnProtocol for RlmTurnProtocol {
    type IntentOutcome = ();
    type Event = RlmProtocolEvent;
    type Termination = TerminationMode;
    type DriverState = serde_json::Value;
}

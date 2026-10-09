use std::collections::{BTreeMap, BTreeSet};

use lash_core::{
    Message, MessageRole, PartKind, RuntimeExecutionContext, facade_support::ChronologicalPayload,
};
use lash_rlm_types::{RlmAttachmentRef, RlmHistoryItem, RlmHistoryRole, RlmProtocolEvent};
use lash_vm::{ProjectedBindings, Value as FlowValue};

#[cfg(test)]
use lash_vm::State as FlowState;

use super::bindings::RlmProjectedBindings;
use super::history_provider::{HISTORY_PROJECTION, HistoryProvider};

/// Version of the RLM payload nested in a session-history protocol event.
/// version_surface = "migrate"
/// format_manifest = "RlmProtocolEvent"
/// version_guard(roots(RlmEventEnvelope))
pub const RLM_PROTOCOL_EVENT_VERSION: u32 = 1;

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RlmEventEnvelope {
    format: u32,
    event: RlmProtocolEvent,
}

#[expect(
    clippy::expect_used,
    reason = "the typed RLM history envelope serializes"
)]
pub fn rlm_protocol_event(
    event: RlmProtocolEvent,
    schema_version: u32,
) -> lash_core::ProtocolEvent {
    lash_core::ProtocolEvent::typed(
        crate::plugin::RLM_PROTOCOL_PLUGIN_ID,
        RlmEventEnvelope {
            format: schema_version,
            event,
        },
    )
    .expect("RLM protocol events serialize")
}

/// Read the RLM event a session-history protocol event carries: `None` for
/// another plugin's event, typed corruption for an unstamped, foreign-format or
/// undecodable payload. The inverse of [`rlm_protocol_event`].
pub fn decode_rlm_protocol_event(
    event: &lash_core::ProtocolEvent,
) -> Result<Option<RlmProtocolEvent>, lash_core::StoredDataCorruption> {
    if event.plugin_id != crate::plugin::RLM_PROTOCOL_PLUGIN_ID {
        return Ok(None);
    }
    let corrupt = |message: String| lash_core::StoredDataCorruption {
        record_kind: "RLM protocol event".into(),
        message,
    };
    #[derive(serde::Deserialize)]
    struct Stamp {
        format: u32,
    }
    let stamp: Stamp = serde_json::from_value(event.payload.clone())
        .map_err(|error| corrupt(error.to_string()))?;
    if !lash_core::store::upcast_chain_covers(
        lash_core::surface_format!(RLM_PROTOCOL_EVENT_VERSION),
        stamp.format,
        RLM_PROTOCOL_EVENT_VERSION,
    ) {
        return Err(corrupt(format!(
            "unsupported format {}, expected {RLM_PROTOCOL_EVENT_VERSION}",
            stamp.format
        )));
    }
    let envelope: RlmEventEnvelope = serde_json::from_value(event.payload.clone())
        .map_err(|error| corrupt(error.to_string()))?;
    Ok(Some(envelope.event))
}

#[derive(Clone, Debug)]
pub struct RlmHistoryProjection {
    history: Vec<RlmHistoryItem>,
    chronological_indices: BTreeMap<usize, usize>,
    suppressed_chronological_indices: BTreeSet<usize>,
}

impl RlmHistoryProjection {
    pub fn from_chronological(
        projection: &lash_core::facade_support::ChronologicalProjection,
    ) -> Result<Self, lash_core::StoredDataCorruption> {
        Self::from_entries(projection.entries())
    }

    /// The history of a transcript's entries; of a prefix of them, the
    /// history the transcript had when it was that long.
    pub(crate) fn from_entries(
        entries: &[lash_core::facade_support::ChronologicalEntry],
    ) -> Result<Self, lash_core::StoredDataCorruption> {
        let suppressed_chronological_indices = completed_turn_internal_indices(entries)?;
        let mut history = Vec::with_capacity(entries.len());
        let mut chronological_indices = BTreeMap::new();
        for entry in entries {
            if suppressed_chronological_indices.contains(&entry.index) {
                continue;
            }
            let item = match &entry.payload {
                ChronologicalPayload::Message(message) => history_item_from_message(message),
                ChronologicalPayload::ProtocolEvent(event) => {
                    match decode_rlm_protocol_event(event)? {
                        Some(RlmProtocolEvent::RlmAssistantContent(content)) => {
                            Some(RlmHistoryItem::Message {
                                id: content.id,
                                role: RlmHistoryRole::Assistant,
                                content: content.prose,
                                attachments: Vec::new(),
                            })
                        }
                        Some(RlmProtocolEvent::RlmTrajectoryEntry(step)) => {
                            Some(history_item_from_lash_vm_step(&step))
                        }
                        _ => None,
                    }
                }
            };
            if let Some(item) = item {
                chronological_indices.insert(entry.index, history.len());
                history.push(item);
            }
        }
        Ok(Self {
            history,
            chronological_indices,
            suppressed_chronological_indices,
        })
    }

    /// Return the compact semantic `history[N]` index for a retained source
    /// entry. Protocol-internal entries suppressed by completed-turn
    /// precedence do not consume an index.
    pub(crate) fn projected_index_for_chronological(&self, index: usize) -> Option<usize> {
        self.chronological_indices.get(&index).copied()
    }

    pub(crate) fn suppresses_chronological(&self, index: usize) -> bool {
        self.suppressed_chronological_indices.contains(&index)
    }

    pub fn history(&self) -> &[RlmHistoryItem] {
        self.history.as_slice()
    }

    pub fn len(&self) -> usize {
        self.history.len()
    }

    pub fn is_empty(&self) -> bool {
        self.history.is_empty()
    }

    pub fn item(&self, index: usize) -> Option<RlmHistoryItem> {
        self.history.get(index).cloned()
    }

    pub fn value(&self) -> serde_json::Value {
        serde_json::to_value(&self.history).unwrap_or_else(|_| serde_json::Value::Array(vec![]))
    }
}

/// The relationship is derived from event provenance: protocol entries and a later assistant
/// message before the next user/event turn boundary share one completed turn.
/// Content is never compared.
/// Intermediate trajectory entries remain available, while their prose is represented by the
/// canonical transcript.
/// A terminal step with an archive stays addressable; its printed values
/// are independent of the canonical assistant answer. Without a committed
/// message, every terminal step remains unchanged.
fn completed_turn_internal_indices(
    entries: &[lash_core::facade_support::ChronologicalEntry],
) -> Result<BTreeSet<usize>, lash_core::StoredDataCorruption> {
    let mut suppressed = BTreeSet::new();
    let mut assistant_content_indices = Vec::new();
    let mut terminal_step = None;

    for entry in entries {
        match &entry.payload {
            ChronologicalPayload::Message(message) => match message.role {
                // A refused call's results are protocol output inside the
                // turn, not the next turn's input.
                MessageRole::User
                    if crate::native::transport::is_exchange_message(
                        message.origin.as_ref(),
                        &message.parts,
                    ) => {}
                MessageRole::User | MessageRole::Event => {
                    assistant_content_indices.clear();
                    terminal_step = None;
                }
                MessageRole::Assistant => {
                    if is_rlm_protocol_output(message.origin.as_ref()) {
                        assistant_content_indices.push(entry.index);
                    } else if history_item_from_message(message).is_some() {
                        if let Some(step_index) = terminal_step.take() {
                            suppressed.insert(step_index);
                        }
                        suppressed.extend(assistant_content_indices.drain(..));
                    }
                }
                MessageRole::System => {}
            },
            ChronologicalPayload::ProtocolEvent(event) => match decode_rlm_protocol_event(event)? {
                Some(RlmProtocolEvent::RlmAssistantContent(_)) => {
                    assistant_content_indices.push(entry.index);
                }
                Some(RlmProtocolEvent::RlmTrajectoryEntry(step)) => {
                    terminal_step = step
                        .result
                        .finish()
                        .is_some()
                        .then_some(entry.index)
                        .filter(|_| step.prints_retained.is_none());
                }
                _ => {}
            },
        }
    }

    Ok(suppressed)
}

/// Whether a message is the RLM protocol's own durable output, judged by its
/// typed origin alone: the one classifier every RLM projection, prompt-side
/// and render-side, asks. A host or another plugin writing on the same
/// channel carries its own provenance and is never classified as internal.
pub fn is_rlm_protocol_output(origin: Option<&lash_core::MessageOrigin>) -> bool {
    match origin {
        Some(lash_core::MessageOrigin::Plugin {
            plugin_id,
            transient: false,
        })
        | Some(lash_core::MessageOrigin::TurnOutput {
            source: lash_core::TurnOutputSource::Plugin { plugin_id },
            ..
        }) => plugin_id == crate::plugin::RLM_PROTOCOL_PLUGIN_ID,
        _ => false,
    }
}

pub fn rlm_history_projection(
    projection: &lash_core::facade_support::ChronologicalProjection,
) -> Result<RlmHistoryProjection, lash_core::StoredDataCorruption> {
    RlmHistoryProjection::from_chronological(projection)
}

/// A cell's projected bindings, and the provider its `history` binding reads
/// through: `history` is a resource of the session's transcript at the
/// revision the cell starts from (ADR 0132 §9).
pub(crate) fn projected_bindings(
    ctx: &RuntimeExecutionContext<'_>,
    session_bindings: RlmProjectedBindings,
) -> Result<(ProjectedBindings, HistoryProvider), String> {
    let scope = ctx.session_scope().map_err(|error| error.to_string())?;
    let history = HistoryProvider::new(
        scope.session_id.to_string(),
        scope
            .agent_frame_id
            .as_ref()
            .map_or("", |frame| frame.as_str()),
        ctx.chronological_projection(),
    );
    history
        .history(&history.current())
        .map_err(|error| error.message)?;
    let mut bindings = ProjectedBindings::new();
    bindings
        .try_insert(HISTORY_PROJECTION, history.binding())
        .map_err(|err| format!("`{}` is reserved as an RLM built-in binding", err.name()))?;
    insert_projected_bindings(&mut bindings, session_bindings)?;
    Ok((bindings, history))
}

#[expect(
    clippy::expect_used,
    reason = "each name is collected from the same projected-binding map it is read from, so get is always Some"
)]
fn insert_projected_bindings(
    target: &mut ProjectedBindings,
    bindings: RlmProjectedBindings,
) -> Result<(), String> {
    let host_bindings = bindings.into_projected_bindings();
    for name in host_bindings.names().collect::<Vec<_>>() {
        let value = host_bindings
            .get(&name)
            .expect("name came from projected bindings");
        target.try_insert(name, value).map_err(|err| {
            format!(
                "`{}` is already bound as an RLM projected binding",
                err.name()
            )
        })?;
    }
    Ok(())
}

pub(crate) fn projected_index(index: &FlowValue, len: usize) -> Result<Option<usize>, ()> {
    let FlowValue::Number(index) = index else {
        return Err(());
    };
    if !index.is_finite() || index.fract() != 0.0 {
        return Err(());
    }
    let len = len as isize;
    let index = *index as isize;
    let normalized = if index < 0 { len + index } else { index };
    if normalized < 0 || normalized >= len {
        return Ok(None);
    }
    Ok(Some(normalized as usize))
}

#[cfg(test)]
pub(crate) fn prune_reserved_projected_bindings(rlm: &mut FlowState) {
    prune_protected_bindings(rlm, &BTreeSet::new());
}

#[cfg(test)]
pub(crate) fn prune_protected_bindings(rlm: &mut FlowState, protected_names: &BTreeSet<String>) {
    prune_projected_binding_names(
        rlm,
        std::iter::once(HISTORY_PROJECTION).chain(protected_names.iter().map(String::as_str)),
    );
}

/// Removes the named bindings in one transaction.
///
/// Pruning is one heap copy and one collection for the whole set rather than
/// one of each per name.
#[cfg(test)]
pub(crate) fn prune_projected_binding_names<'a>(
    rlm: &mut FlowState,
    names: impl IntoIterator<Item = &'a str>,
) {
    rlm.patch_globals(names.into_iter().map(|name| lash_vm::GlobalPatch::Remove {
        name: name.to_string(),
    }))
    .expect("removing bindings cannot exceed the heap bound");
}

fn history_item_from_message(message: &Message) -> Option<RlmHistoryItem> {
    // A native provider exchange is replayed as its call/result pair and is
    // the cell's own step in `history`, never a message beside it.
    if crate::native::transport::is_exchange_message(message.origin.as_ref(), &message.parts) {
        return None;
    }
    let content = message_history_text(message);
    let attachments = message
        .parts
        .iter()
        .flat_map(|part| {
            part.identified_attachments()
                .into_iter()
                .map(|(id, attachment)| RlmAttachmentRef {
                    id,
                    reference: attachment.clone(),
                })
        })
        .collect::<Vec<_>>();
    if content.is_empty() && attachments.is_empty() {
        return None;
    }
    Some(RlmHistoryItem::Message {
        id: message.id.clone(),
        role: history_role(message.role),
        content,
        attachments,
    })
}

fn history_item_from_lash_vm_step(entry: &lash_core::CellRecord) -> RlmHistoryItem {
    RlmHistoryItem::from_cell_record(entry)
}

fn message_history_text(message: &Message) -> String {
    let chunks = message
        .parts
        .iter()
        .filter(|part| matches!(part.kind(), PartKind::Text | PartKind::Prose))
        .filter_map(|part| part.text_content())
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    chunks.join("\n\n")
}

fn history_role(role: MessageRole) -> RlmHistoryRole {
    match role {
        MessageRole::User => RlmHistoryRole::User,
        MessageRole::System => RlmHistoryRole::System,
        MessageRole::Assistant => RlmHistoryRole::Assistant,
        MessageRole::Event => RlmHistoryRole::Event,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::projection::history_provider::answer;
    use lash_core::CellRecord;
    use lash_vm::{ProjectedReadRequest, ProjectedReadResponse, ProjectionProvider};
    use std::sync::Arc;

    #[test]
    fn corrupt_rlm_history_refuses_projection_and_transcript() {
        use lash_core::transcript::TranscriptDecoderPlugin as _;
        let foreign = lash_core::ProtocolEvent {
            plugin_id: "foreign".into(),
            payload: serde_json::json!(null),
        };
        assert!(
            decode_rlm_protocol_event(&foreign)
                .expect("foreign event")
                .is_none()
        );
        for payload in [
            serde_json::json!({}),
            serde_json::json!({"format": u32::MAX}),
            serde_json::json!({"format": RLM_PROTOCOL_EVENT_VERSION, "event": null}),
        ] {
            let event = lash_core::ProtocolEvent {
                plugin_id: crate::plugin::RLM_PROTOCOL_PLUGIN_ID.into(),
                payload,
            };
            let error = decode_rlm_protocol_event(&event).expect_err("our corrupt event refuses");
            assert_eq!(error.record_kind, "RLM protocol event");
            let events = [lash_core::SessionHistoryRecord::Protocol(event.clone())];
            let projection = lash_core::facade_support::ChronologicalProjection::from_turn_view(
                &events,
                &Default::default(),
            );
            assert!(rlm_history_projection(&projection).is_err());
            assert!(
                crate::projection::transcript::RlmTranscriptDecoder
                    .decode_event(&event)
                    .is_err()
            );
        }
    }

    fn message(id: &str, role: MessageRole, text: &str) -> Message {
        Message {
            id: id.to_string(),
            role,
            parts: lash_core::facade_support::shared_parts(vec![lash_core::Part::text(
                format!("{id}.p0"),
                text.to_string(),
                None,
            )]),
            origin: None,
            reply_marker: None,
        }
    }

    fn step_projection(output: &str) -> lash_core::facade_support::ChronologicalProjection {
        let entry = CellRecord {
            language: "typescript".to_string(),
            prints_retained: None,
            id: "lash_vm_step_0".to_string(),
            protocol_iteration: 0,
            code: "print big".to_string(),
            prints: vec![output.to_string().into()],
            images: Vec::new(),
            calls: Vec::new(),
            calls_omitted: 0,
            result: lash_core::CellOutcome::Completed,
        };
        let events = [lash_core::SessionHistoryRecord::Protocol(
            rlm_protocol_event(
                RlmProtocolEvent::RlmTrajectoryEntry(Box::new(entry)),
                lash_core::FleetFormat::current().writer_version(lash_core::surface_format!(
                    crate::RLM_PROTOCOL_EVENT_VERSION
                )),
            ),
        )];
        lash_core::facade_support::ChronologicalProjection::from_turn_view(
            &events,
            &lash_core::facade_support::MessageSequence::default(),
        )
    }

    async fn read_index(history: &RlmHistoryProjection, index: i64) -> FlowValue {
        match answer(
            history,
            ProjectedReadRequest::Index(FlowValue::Number(index as f64)),
        ) {
            Some(ProjectedReadResponse::Value(value)) => value,
            other => panic!("expected indexed value, got {other:?}"),
        }
    }

    // A step whose outputs were retained shows the model a bounded witness;
    // indexing the real `history` projection hands back the reference to the
    // FULL, untruncated value the prompt only previewed.
    #[tokio::test]
    async fn history_step_output_resolves_full_untruncated_value() {
        let full = "Xé🙂".repeat(50_000);
        let host = crate::testing::DurableHost::open(crate::testing::default_cell_scope()).await;
        let attachments = lash_core::facade_support::RuntimeAttachmentStore::ephemeral(
            host.backend().attachment_store(),
            lash_core::facade_support::AttachmentPolicy::standard(),
        );
        let observations = vec![lash_core::CellPrint {
            text: "bounded preview".to_string(),
            value: serde_json::json!(full),
            projection: Default::default(),
        }];
        let bytes = serde_json::to_vec(&observations).expect("encode archive");
        let reference = attachments
            .put(
                bytes.clone(),
                lash_core::AttachmentCreateMeta::new(
                    "application/json".parse().expect("media type"),
                    None,
                    Some("step archive".to_string()),
                ),
            )
            .await
            .expect("store archive");
        let entry = CellRecord {
            id: "archived-step".to_string(),
            prints_retained: Some(lash_core::RetainedOutput {
                reference: reference.clone(),
                witness: "bounded preview".to_string(),
            }),
            ..Default::default()
        };
        let projection = lash_core::facade_support::ChronologicalProjection::from_turn_view(
            &[lash_core::SessionHistoryRecord::Protocol(
                rlm_protocol_event(
                    RlmProtocolEvent::RlmTrajectoryEntry(Box::new(entry)),
                    lash_core::FleetFormat::current().writer_version(lash_core::surface_format!(
                        crate::RLM_PROTOCOL_EVENT_VERSION
                    )),
                ),
            )],
            &Default::default(),
        );
        let history = rlm_history_projection(&projection).expect("valid history fixture");
        let FlowValue::Record(step) = read_index(&history, 0).await else {
            panic!("history step");
        };
        let projected = step
            .get("output_archive")
            .expect("history exposes the archive");
        let record = crate::projection::flow_to_json_value(projected);
        let adopted: lash_core::ToolValue =
            serde_json::from_value(record["attachment"].clone()).expect("typed attachment");
        let lash_core::ToolValue::Attachment(attachment_ref) = adopted else {
            panic!("stored reference");
        };
        assert_eq!(attachment_ref, reference);
        let fetched = attachments
            .read(&attachment_ref)
            .await
            .expect("explicit fetch");
        assert_eq!(fetched, bytes);
        let outputs = crate::control_tools::decode_output_archive(&fetched).expect("exact values");
        assert_eq!(outputs, vec![serde_json::json!(full)]);
    }

    /// A host whose only ability is finishing, so a cell's `finish(...)` is the
    /// observable result.
    struct FinishOnlyHost;

    impl lash_vm::ExecutionHost for FinishOnlyHost {
        async fn perform(
            &self,
            op: lash_vm::AbilityOp,
        ) -> Result<lash_vm::AbilityOutcome, lash_vm::ExecutionHostError> {
            match op {
                lash_vm::AbilityOp::Finish(value) | lash_vm::AbilityOp::Fail(value) => {
                    Ok(lash_vm::AbilityOutcome::Value(value))
                }
                _ => Err(lash_vm::ExecutionHostError::new("unsupported host ability")),
            }
        }
    }

    /// Cell source is authored in TypeScript (FIG-3015).
    async fn run_history_cell(
        source: &str,
        history: &lash_core::facade_support::ChronologicalProjection,
    ) -> Result<FlowValue, lash_vm::RuntimeError> {
        let provider = HistoryProvider::new("session", "frame", Arc::new(history.clone()));
        let mut bindings = ProjectedBindings::new();
        bindings.insert("history", provider.binding());
        let mut providers = lash_vm::ProjectionCatalog::new();
        providers
            .register(Arc::new(provider))
            .expect("one history provider");
        let bindings = bindings.with_reader(Arc::new(lash_vm::testing::projection::CatalogReader(
            providers,
        )));
        let globals = BTreeSet::from(["history".to_string()]);
        let parsed = lash_typescript::parse_with_globals(source, &globals)
            .unwrap_or_else(|error| panic!("`{source}` should parse: {error}"));
        let compiled = lash_vm::testing::harness::try_compile_program(&parsed)
            .unwrap_or_else(|error| panic!("`{source}` should compile: {error}"));
        let env =
            lash_vm::ExecutionEnvironment::new(&FinishOnlyHost).with_projected_bindings(bindings);
        let mut state = lash_vm::State::new();
        match lash_vm::execute(&compiled, &mut state, &env).await? {
            lash_vm::ExecutionOutcome::Finished(value) => Ok(value),
            other => panic!("`{source}` should finish, got {other:?}"),
        }
    }

    /// `history.length` used to reach the blanket `Missing` default and widen
    /// into whatever each consumer guessed. The descriptor now answers `Len`
    /// explicitly, so the cell reads the real entry count (FIG-2863).
    #[tokio::test]
    async fn history_length_reads_the_real_entry_count() {
        let projection = step_projection("only");
        assert_eq!(
            run_history_cell("finish(history.length);", &projection)
                .await
                .expect("`history.length` should answer"),
            FlowValue::Number(1.0)
        );
    }

    #[tokio::test]
    async fn history_index_reads_the_typed_print_value_in_a_typescript_cell() {
        let projection = step_projection("typed print");
        assert_eq!(
            run_history_cell("finish(history[0].output[0]);", &projection)
                .await
                .expect("indexed history output"),
            FlowValue::String("typed print".into())
        );
    }

    /// `contains(history, history[0])` must agree with what `history[0]` hands
    /// back: the descriptor answers `Contains` against the same projected shape
    /// it answers `Index` with. Pinned at the descriptor seam because the
    /// TypeScript surface's `includes` is JavaScript's SameValueZero, which
    /// compares objects by reference and is false for any two built records.
    #[tokio::test]
    async fn history_contains_its_own_first_entry() {
        let history =
            rlm_history_projection(&step_projection("only")).expect("valid history fixture");
        let first = read_index(&history, 0).await;
        assert!(matches!(
            answer(&history, ProjectedReadRequest::Contains(first)),
            Some(ProjectedReadResponse::Bool(true))
        ));
        assert!(matches!(
            answer(
                &history,
                ProjectedReadRequest::Contains(FlowValue::String("absent".into()))
            ),
            Some(ProjectedReadResponse::Bool(false))
        ));
    }

    /// A list is truthy at any length, matching the dialect's reading of a
    /// `Value::List`, so a cell can guard on `history` without materializing it.
    #[tokio::test]
    async fn history_answers_truthiness_without_materializing() {
        let projection = step_projection("only");
        assert_eq!(
            run_history_cell(r#"finish(history ? "yes" : "no");"#, &projection)
                .await
                .expect("truthiness should answer"),
            FlowValue::String("yes".into())
        );
    }

    /// The TypeScript surface has no `empty(...)`, so `Empty` is pinned at the
    /// descriptor seam: it answers, rather than falling through to a refusal or
    /// to materializing the whole history.
    #[tokio::test]
    async fn history_answers_empty_at_the_descriptor_seam() {
        let populated =
            rlm_history_projection(&step_projection("only")).expect("valid history fixture");
        assert!(matches!(
            answer(&populated, ProjectedReadRequest::Empty),
            Some(ProjectedReadResponse::Bool(false))
        ));

        let empty = RlmHistoryProjection {
            history: Vec::new(),
            chronological_indices: BTreeMap::new(),
            suppressed_chronological_indices: BTreeSet::new(),
        };
        assert!(matches!(
            answer(&empty, ProjectedReadRequest::Empty),
            Some(ProjectedReadResponse::Bool(true))
        ));
    }

    /// A field this descriptor does not answer is the dialect's absent value,
    /// not a refusal and not a hardcoded `null`: a list view genuinely has no
    /// such property, and TypeScript reads that as `undefined` (FIG-2863).
    ///
    /// `??` alone would not pin this -- it fires on `null` and `undefined`
    /// alike, so the pre-fix `Value::Null` passes it. The identity comparisons
    /// are what separate the two, and `typeof` names which one arrived.
    #[tokio::test]
    async fn an_unanswered_history_field_reads_as_the_dialects_absent_value() {
        let projection = step_projection("only");
        assert_eq!(
            run_history_cell("finish(history.nonexistent === undefined);", &projection)
                .await
                .expect("an unanswered field is absent, not a failure"),
            FlowValue::Bool(true),
            "an unanswered field must be `undefined` under the TypeScript dialect"
        );
        assert_eq!(
            run_history_cell("finish(history.nonexistent === null);", &projection)
                .await
                .expect("an unanswered field is absent, not a failure"),
            FlowValue::Bool(false),
            "`null` is the lash_vm surface's absent value, not TypeScript's"
        );
        assert_eq!(
            run_history_cell("finish(typeof history.nonexistent);", &projection)
                .await
                .expect("an unanswered field is absent, not a failure"),
            FlowValue::String("undefined".into())
        );
        assert_eq!(
            run_history_cell(r#"finish(history.nonexistent ?? "fallback");"#, &projection)
                .await
                .expect("an unanswered field is absent, not a failure"),
            FlowValue::String("fallback".into())
        );
    }

    fn transcript(texts: &[&str]) -> Arc<lash_core::facade_support::ChronologicalProjection> {
        let messages = texts
            .iter()
            .enumerate()
            .map(|(index, text)| message(&format!("m{index}"), MessageRole::User, text))
            .collect::<Vec<_>>();
        Arc::new(
            lash_core::facade_support::ChronologicalProjection::from_turn_view(
                &[],
                &messages.into(),
            ),
        )
    }

    /// Provider purity (ADR 0132 §9): a `history` read is `Repeatable` at its
    /// pinned revision. The same request answers the same twice, and a
    /// provider built afresh over the transcript after it grew, as another
    /// node builds one, answers the pinned revision exactly as before. A
    /// revision of another frame is refused, never answered from a different
    /// transcript.
    #[tokio::test]
    async fn history_reads_repeat_at_their_pinned_revision() {
        let first = HistoryProvider::new("session", "frame", transcript(&["one"]));
        let pinned = first.current();
        let once = first
            .read(&pinned, ProjectedReadRequest::Materialize)
            .await
            .expect("pinned read");
        let twice = first
            .read(&pinned, ProjectedReadRequest::Materialize)
            .await
            .expect("pinned read again");
        assert_eq!(once, twice, "the same read answers the same");

        let grown = HistoryProvider::new("session", "frame", transcript(&["one", "two"]));
        assert_eq!(
            grown
                .read_range(
                    &pinned,
                    vec![ProjectedReadRequest::Materialize, ProjectedReadRequest::Len],
                )
                .await
                .expect("the earlier revision is retained"),
            vec![once, Some(ProjectedReadResponse::Len(1))],
            "a pinned revision answers what it answered before the transcript grew"
        );
        assert_eq!(
            grown
                .read(&grown.current(), ProjectedReadRequest::Len)
                .await
                .expect("current read"),
            Some(ProjectedReadResponse::Len(2))
        );

        let compacted = HistoryProvider::new("session", "next-frame", transcript(&["one"]));
        assert!(
            compacted
                .read(&pinned, ProjectedReadRequest::Len)
                .await
                .is_err(),
            "another frame's revision is refused"
        );
    }

    #[test]
    fn completed_turn_projection_keeps_only_transcript_and_compacts_indices() {
        let terminal = CellRecord {
            language: "typescript".to_string(),
            prints_retained: None,
            id: "terminal".to_string(),
            protocol_iteration: 1,
            code: "finish { answer: 42 }".to_string(),
            prints: vec!["terminal output".to_string().into()],
            images: Vec::new(),
            calls: Vec::new(),
            calls_omitted: 0,
            result: lash_core::CellOutcome::Finished(serde_json::json!({ "answer": 42 }).into()),
        };
        let retained = CellRecord {
            language: "typescript".to_string(),
            prints_retained: None,
            id: "retained".to_string(),
            protocol_iteration: 0,
            code: "print \"next\"".to_string(),
            prints: vec!["next".to_string().into()],
            images: Vec::new(),
            calls: Vec::new(),
            calls_omitted: 0,
            result: lash_core::CellOutcome::Completed,
        };
        let events = [
            lash_core::SessionHistoryRecord::Conversation(
                lash_core::facade_support::ConversationRecord::from_message(message(
                    "u1",
                    MessageRole::User,
                    "first",
                )),
            ),
            lash_core::SessionHistoryRecord::Protocol(rlm_protocol_event(
                RlmProtocolEvent::RlmAssistantContent(lash_rlm_types::RlmAssistantContent {
                    id: "terminal-content".to_string(),
                    reasoning: String::new(),
                    prose: "terminal prose".to_string(),
                }),
                lash_core::FleetFormat::current().writer_version(lash_core::surface_format!(
                    crate::RLM_PROTOCOL_EVENT_VERSION
                )),
            )),
            lash_core::SessionHistoryRecord::Protocol(rlm_protocol_event(
                RlmProtocolEvent::RlmTrajectoryEntry(Box::new(terminal)),
                lash_core::FleetFormat::current().writer_version(lash_core::surface_format!(
                    crate::RLM_PROTOCOL_EVENT_VERSION
                )),
            )),
            lash_core::SessionHistoryRecord::Conversation(
                lash_core::facade_support::ConversationRecord::from_message(message(
                    "a1",
                    MessageRole::Assistant,
                    "committed answer",
                )),
            ),
            lash_core::SessionHistoryRecord::Conversation(
                lash_core::facade_support::ConversationRecord::from_message(message(
                    "u2",
                    MessageRole::User,
                    "second",
                )),
            ),
            lash_core::SessionHistoryRecord::Protocol(rlm_protocol_event(
                RlmProtocolEvent::RlmTrajectoryEntry(Box::new(retained)),
                lash_core::FleetFormat::current().writer_version(lash_core::surface_format!(
                    crate::RLM_PROTOCOL_EVENT_VERSION
                )),
            )),
        ];
        let chronological = lash_core::facade_support::ChronologicalProjection::from_turn_view(
            &events,
            &lash_core::facade_support::MessageSequence::default(),
        );
        let projection = rlm_history_projection(&chronological).expect("valid history fixture");

        assert_eq!(projection.len(), 4);
        assert!(projection.suppresses_chronological(1));
        assert!(projection.suppresses_chronological(2));
        assert_eq!(projection.projected_index_for_chronological(3), Some(1));
        assert_eq!(projection.projected_index_for_chronological(5), Some(3));
        assert!(matches!(
            &projection.history()[1],
            RlmHistoryItem::Message { content, .. } if content == "committed answer"
        ));
        assert!(matches!(
            &projection.history()[3],
            RlmHistoryItem::LashVmStep { id, .. } if id == "retained"
        ));
    }

    #[test]
    fn completed_turn_projection_keeps_intermediate_steps_without_duplicate_prose() {
        let intermediate = CellRecord {
            language: "typescript".to_string(),
            prints_retained: None,
            id: "intermediate".to_string(),
            protocol_iteration: 0,
            code: "missing_name".to_string(),
            prints: Vec::new(),
            images: Vec::new(),
            calls: Vec::new(),
            calls_omitted: 0,
            result: lash_core::CellOutcome::Failed(lash_core::CellFailure::new(
                lash_core::CellFailureKind::Program,
                "unknown name",
            )),
        };
        let terminal = CellRecord {
            language: "typescript".to_string(),
            prints_retained: None,
            id: "terminal".to_string(),
            protocol_iteration: 1,
            code: "finish \"done\"".to_string(),
            prints: Vec::new(),
            images: Vec::new(),
            calls: Vec::new(),
            calls_omitted: 0,
            result: lash_core::CellOutcome::Finished(serde_json::json!("done").into()),
        };
        let events = [
            lash_core::SessionHistoryRecord::Conversation(
                lash_core::facade_support::ConversationRecord::from_message(message(
                    "u1",
                    MessageRole::User,
                    "start",
                )),
            ),
            lash_core::SessionHistoryRecord::Protocol(rlm_protocol_event(
                RlmProtocolEvent::RlmAssistantContent(lash_rlm_types::RlmAssistantContent {
                    id: "intermediate-content".to_string(),
                    reasoning: String::new(),
                    prose: "surviving prose".to_string(),
                }),
                lash_core::FleetFormat::current().writer_version(lash_core::surface_format!(
                    crate::RLM_PROTOCOL_EVENT_VERSION
                )),
            )),
            lash_core::SessionHistoryRecord::Protocol(rlm_protocol_event(
                RlmProtocolEvent::RlmTrajectoryEntry(Box::new(intermediate)),
                lash_core::FleetFormat::current().writer_version(lash_core::surface_format!(
                    crate::RLM_PROTOCOL_EVENT_VERSION
                )),
            )),
            lash_core::SessionHistoryRecord::Protocol(rlm_protocol_event(
                RlmProtocolEvent::RlmTrajectoryEntry(Box::new(terminal)),
                lash_core::FleetFormat::current().writer_version(lash_core::surface_format!(
                    crate::RLM_PROTOCOL_EVENT_VERSION
                )),
            )),
            lash_core::SessionHistoryRecord::Conversation(
                lash_core::facade_support::ConversationRecord::from_message(message(
                    "a1",
                    MessageRole::Assistant,
                    "surviving prose\n\ndone",
                )),
            ),
        ];
        let chronological = lash_core::facade_support::ChronologicalProjection::from_turn_view(
            &events,
            &lash_core::facade_support::MessageSequence::default(),
        );
        let projection = rlm_history_projection(&chronological).expect("valid history fixture");

        assert!(projection.suppresses_chronological(1));
        assert!(!projection.suppresses_chronological(2));
        assert!(projection.suppresses_chronological(3));
        assert_eq!(projection.len(), 3);
        assert!(matches!(
            &projection.history()[1],
            RlmHistoryItem::LashVmStep { id, outcome, .. }
                if id == "intermediate"
                    && outcome.error.as_ref().map(|error| error.message.as_str()) == Some("unknown name")
        ));
        assert!(matches!(
            &projection.history()[2],
            RlmHistoryItem::Message { content, .. }
                if content == "surviving prose\n\ndone"
        ));
    }

    #[test]
    fn prose_only_completion_suppresses_internal_assistant_record() {
        let mut internal = message("internal", MessageRole::Assistant, "natural completion");
        internal.origin = Some(lash_core::MessageOrigin::Plugin {
            plugin_id: crate::plugin::RLM_PROTOCOL_PLUGIN_ID.to_string(),
            transient: false,
        });
        let events = [
            lash_core::SessionHistoryRecord::Conversation(
                lash_core::facade_support::ConversationRecord::from_message(message(
                    "u1",
                    MessageRole::User,
                    "answer naturally",
                )),
            ),
            lash_core::SessionHistoryRecord::Conversation(
                lash_core::facade_support::ConversationRecord::from_message(internal),
            ),
            lash_core::SessionHistoryRecord::Conversation(
                lash_core::facade_support::ConversationRecord::from_message(message(
                    "a1",
                    MessageRole::Assistant,
                    "natural completion",
                )),
            ),
        ];
        let chronological = lash_core::facade_support::ChronologicalProjection::from_turn_view(
            &events,
            &lash_core::facade_support::MessageSequence::default(),
        );
        let projection = rlm_history_projection(&chronological).expect("valid history fixture");

        assert!(projection.suppresses_chronological(1));
        assert_eq!(projection.len(), 2);
        assert!(matches!(
            &projection.history()[1],
            RlmHistoryItem::Message { content, .. } if content == "natural completion"
        ));
    }
}

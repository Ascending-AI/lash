//! A stopped physical turn's partial output (ADR 0114 §1).
//!
//! The value is sealed and committed with the stopped turn and returned to the
//! caller. It never enters the conversation graph, `AssistantOutput` or
//! `SessionHistoryRecord`: a host that wants it in context resubmits it as
//! ordinary input.

use crate::{SessionId, TurnId};

/// A stopped physical turn's uncommitted tail, sealed and committed with the
/// turn. It is data returned to the caller. It never enters the graph,
/// `AssistantOutput` or `SessionHistoryRecord`.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct StoppedPartial {
    pub id: StoppedPartialId,
    /// BLAKE3, domain `lash-stopped-partial/v1`, over the canonical JSON of
    /// every other field of this value.
    pub digest: StoppedPartialDigest,
    pub reason: StopReason,
    /// Set independently of `reason` (ADR 0114 §1.3).
    pub recovered_after_process_loss: bool,
    pub coverage: CaptureCoverage,
    /// In order of each item's first capture frame.
    pub items: Vec<PartialItem>,
}

#[derive(
    Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
pub struct StoppedPartialId {
    pub session_id: SessionId,
    /// The logical root the stopped physical turn ran under.
    pub root: TurnId,
    /// The physical turn that stopped.
    pub turn_id: TurnId,
    /// Checkpoints the turn recorded before the tail began.
    pub base: CaptureBase,
    /// The last store-assigned capture sequence the seal included.
    pub sealed_through: u64,
}

/// The number of checkpoints a physical turn has recorded.
#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
    serde::Serialize,
    serde::Deserialize,
    schemars::JsonSchema,
)]
pub struct CaptureBase(pub u32);

#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    Hash,
    serde::Serialize,
    serde::Deserialize,
    schemars::JsonSchema,
)]
pub struct StoppedPartialDigest(pub [u8; 32]);

impl StoppedPartialDigest {
    /// Lowercase hexadecimal spelling, for diagnostics and store columns.
    pub fn to_hex(&self) -> String {
        use std::fmt::Write as _;
        self.0
            .iter()
            .fold(String::with_capacity(64), |mut hex, byte| {
                let _ = write!(hex, "{byte:02x}");
                hex
            })
    }
}

/// `"{invocation}/{attempt_epoch}/{kind}/{key}"`. `key` is the provider's
/// block id or call id, or `#{ordinal}` when the provider minted none.
#[derive(
    Clone,
    Debug,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
    serde::Serialize,
    serde::Deserialize,
    schemars::JsonSchema,
)]
pub struct PartialItemId(pub String);

impl PartialItemId {
    pub fn new(invocation: &str, attempt_epoch: u32, kind: PartialItemKey<'_>) -> Self {
        let (kind, key) = match kind {
            PartialItemKey::Text(key) => ("text", key),
            PartialItemKey::Reasoning(key) => ("reasoning", key),
            PartialItemKey::Tool(key) => ("tool", key),
        };
        Self(format!("{invocation}/{attempt_epoch}/{kind}/{key}"))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The kind and provider key a [`PartialItemId`] is minted from. A tool call
/// and the fragment it would have been share the `tool` kind: one provider
/// call is exactly one of the two.
#[derive(Clone, Copy, Debug)]
pub enum PartialItemKey<'a> {
    Text(&'a str),
    Reasoning(&'a str),
    Tool(&'a str),
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
// justification: a partial is built once per stopped turn and read by hosts by pattern; its items stay inline so the shape is the ADR's.
#[allow(clippy::large_enum_variant)]
pub enum PartialItem {
    /// Post-plugin assistant text: exactly what the forwarder sent.
    Text {
        id: PartialItemId,
        state: CutState,
        text: String,
    },
    /// The visible reasoning summary. Opaque replay material (signatures,
    /// encrypted content, provider item state) is never captured.
    Reasoning {
        id: PartialItemId,
        state: CutState,
        summary: String,
    },
    /// A complete call: the provider closed its arguments and they parsed.
    ToolCall {
        id: PartialItemId,
        call: CompleteToolCall,
        execution: ToolExecutionState,
    },
    /// Arguments the provider never closed, or closed but unparseable.
    /// Never prefix-parsed into a call.
    ArgumentFragment {
        id: PartialItemId,
        call_id: Option<String>,
        tool_name: Option<String>,
        /// The exact argument text the provider streamed.
        raw_arguments: String,
        state: FragmentState,
    },
}

impl PartialItem {
    pub fn id(&self) -> &PartialItemId {
        match self {
            Self::Text { id, .. }
            | Self::Reasoning { id, .. }
            | Self::ToolCall { id, .. }
            | Self::ArgumentFragment { id, .. } => id,
        }
    }
}

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum CutState {
    Complete,
    Interrupted,
}

#[derive(
    Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum FragmentState {
    /// The stop landed before the provider closed the arguments.
    Interrupted,
    /// The provider closed them, and the protocol's parse refused them.
    Invalid { parse_error: String },
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct CompleteToolCall {
    pub call_id: String,
    pub tool_name: String,
    pub arguments: serde_json::Value,
}

/// "Complete call" never means "tool succeeded".
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(tag = "state", rename_all = "snake_case")]
// justification: built once per stopped call and matched by hosts; the settled output stays inline so the shape is the ADR's.
#[allow(clippy::large_enum_variant)]
pub enum ToolExecutionState {
    /// No attempt started before the cutoff.
    NotStarted,
    /// An attempt started and did not settle before the cutoff.
    Running(RunningTool),
    /// The attempt settled before the cutoff. Its result is kept.
    Settled {
        /// `ToolCallOutput` has a hand-written wire form with no derived
        /// schema; the schema names it as any JSON value.
        #[schemars(with = "serde_json::Value")]
        output: crate::ToolCallOutput,
    },
}

/// A tool the stop interrupted. Whatever it did outside lash is unknown, and
/// lash never re-runs it.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct RunningTool {
    pub output: ToolOutputCapture,
    pub outcome: InterruptedToolOutcome,
}

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum InterruptedToolOutcome {
    OutcomeUnknown,
}

/// Captured output past which a call's chunks are counted, not kept.
pub const TOOL_OUTPUT_CAPTURE_MAX_BYTES: u64 = 1024 * 1024;

#[derive(
    Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(tag = "capture", rename_all = "snake_case")]
pub enum ToolOutputCapture {
    /// The tool has no progress channel. This is not empty output.
    Unavailable,
    /// The chunks it reported before the cutoff, in order. Past the per-call
    /// cap (1 MiB) chunks are counted in `omitted_bytes`, never silently lost.
    Captured {
        chunks: Vec<ToolOutputChunk>,
        omitted_bytes: u64,
    },
}

impl ToolOutputCapture {
    /// Folds one reported chunk in under the per-call cap: the first report
    /// flips `Unavailable` to `Captured`, and a chunk that would pass the cap
    /// is counted whole in `omitted_bytes`.
    pub fn push(&mut self, chunk: ToolOutputChunk) {
        if matches!(self, Self::Unavailable) {
            *self = Self::Captured {
                chunks: Vec::new(),
                omitted_bytes: 0,
            };
        }
        let Self::Captured {
            chunks,
            omitted_bytes,
        } = self
        else {
            return;
        };
        let kept: u64 = chunks.iter().map(|chunk| chunk.text.len() as u64).sum();
        let len = chunk.text.len() as u64;
        if *omitted_bytes == 0 && kept.saturating_add(len) <= TOOL_OUTPUT_CAPTURE_MAX_BYTES {
            chunks.push(chunk);
        } else {
            *omitted_bytes = omitted_bytes.saturating_add(len);
        }
    }
}

#[derive(
    Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
pub struct ToolOutputChunk {
    pub text: String,
}

#[derive(
    Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StopReason {
    UserCancel,
    ProcessLoss,
    Other { cause: OtherStopCause },
}

impl StopReason {
    /// The reason a committed `Stopped` terminal names (ADR 0114 §1.3).
    pub fn of_stop(stop: &crate::TurnStop) -> Self {
        use crate::TurnStop;
        match stop {
            TurnStop::Cancelled { .. } => Self::UserCancel,
            TurnStop::ProviderError => Self::Other {
                cause: OtherStopCause::ProviderFailure,
            },
            TurnStop::PluginAbort => Self::Other {
                cause: OtherStopCause::PluginAbort,
            },
            TurnStop::RuntimeError => Self::Other {
                cause: OtherStopCause::RuntimeFailure,
            },
            TurnStop::Incomplete
            | TurnStop::InvalidInput
            | TurnStop::MaxTurns
            | TurnStop::ToolFailure
            | TurnStop::ContextOverflow
            | TurnStop::SubmittedError { .. }
            | TurnStop::ToolError { .. } => Self::Other {
                cause: OtherStopCause::ProtocolStop,
            },
        }
    }
}

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum OtherStopCause {
    ProviderFailure,
    PluginAbort,
    RuntimeFailure,
    OperatorCancellation,
    /// The turn's protocol ended it: `Incomplete`, `InvalidInput`,
    /// `MaxTurns`, `ToolFailure`, `ContextOverflow`, `SubmittedError`,
    /// `ToolError`. The exact stop stays on the report's `TurnOutcome`.
    ProtocolStop,
}

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum CaptureCoverage {
    /// Every frame the turn emitted before the seal is included.
    Complete,
    /// Recovery rebuilt the prefix a lost worker acknowledged. Output that
    /// worker produced after its last acknowledged batch is not promised.
    AcknowledgedPrefix,
}

#[derive(
    Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(tag = "eligibility", rename_all = "snake_case")]
pub enum ResubmissionEligibility {
    /// Every item has a structurally valid default in `build_resubmission`.
    Ready,
    /// The host must choose, item by item, before resubmitting.
    NeedsSelection { reasons: Vec<SelectionReason> },
    /// No items.
    Empty,
}

#[derive(
    Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum SelectionReason {
    ArgumentFragment { item: PartialItemId },
    Reasoning { item: PartialItemId },
    AcknowledgedPrefixOnly,
}

/// A partial whose recorded digest is not the digest of its content.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoppedPartialDigestMismatch {
    pub recorded: StoppedPartialDigest,
    /// `None` when the content could not be encoded at all.
    pub computed: Option<StoppedPartialDigest>,
}

impl std::fmt::Display for StoppedPartialDigestMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.computed {
            Some(computed) => write!(
                f,
                "stopped partial digest {} does not match its content ({})",
                self.recorded.to_hex(),
                computed.to_hex()
            ),
            None => write!(
                f,
                "stopped partial digest {} cannot be checked: its content does not encode",
                self.recorded.to_hex()
            ),
        }
    }
}

impl std::error::Error for StoppedPartialDigestMismatch {}

/// Every field of a [`StoppedPartial`] but its digest: what the digest
/// covers. Encoded through `serde_json::Value`, whose maps sort their keys,
/// so the bytes are canonical.
#[derive(serde::Serialize)]
struct DigestedContent<'a> {
    id: &'a StoppedPartialId,
    reason: &'a StopReason,
    recovered_after_process_loss: bool,
    coverage: CaptureCoverage,
    items: &'a [PartialItem],
}

fn content_digest(
    content: &DigestedContent<'_>,
) -> Result<StoppedPartialDigest, serde_json::Error> {
    let canonical = serde_json::to_vec(&serde_json::to_value(content)?)?;
    Ok(StoppedPartialDigest(
        crate::core_support::blake3_domain_hash("lash-stopped-partial/v1", canonical),
    ))
}

impl StoppedPartial {
    /// Builds the value and computes its digest.
    pub fn seal(
        id: StoppedPartialId,
        reason: StopReason,
        recovered_after_process_loss: bool,
        coverage: CaptureCoverage,
        items: Vec<PartialItem>,
    ) -> Result<Self, serde_json::Error> {
        let digest = content_digest(&DigestedContent {
            id: &id,
            reason: &reason,
            recovered_after_process_loss,
            coverage,
            items: &items,
        })?;
        Ok(Self {
            id,
            digest,
            reason,
            recovered_after_process_loss,
            coverage,
            items,
        })
    }

    /// A fragment exists, or a call is `Running`.
    pub fn cut_mid_tool_call(&self) -> bool {
        self.items.iter().any(|item| {
            matches!(
                item,
                PartialItem::ArgumentFragment { .. }
                    | PartialItem::ToolCall {
                        execution: ToolExecutionState::Running(_),
                        ..
                    }
            )
        })
    }

    /// A call is `Running`: its outcome is `OutcomeUnknown`.
    pub fn tool_outcome_unknown(&self) -> bool {
        self.items.iter().any(|item| {
            matches!(
                item,
                PartialItem::ToolCall {
                    execution: ToolExecutionState::Running(_),
                    ..
                }
            )
        })
    }

    pub fn eligibility(&self) -> ResubmissionEligibility {
        if self.items.is_empty() {
            return ResubmissionEligibility::Empty;
        }
        let mut reasons = self
            .items
            .iter()
            .filter_map(|item| match item {
                PartialItem::ArgumentFragment { id, .. } => {
                    Some(SelectionReason::ArgumentFragment { item: id.clone() })
                }
                PartialItem::Reasoning { id, .. } => {
                    Some(SelectionReason::Reasoning { item: id.clone() })
                }
                PartialItem::Text { .. } | PartialItem::ToolCall { .. } => None,
            })
            .collect::<Vec<_>>();
        if self.coverage == CaptureCoverage::AcknowledgedPrefix {
            reasons.push(SelectionReason::AcknowledgedPrefixOnly);
        }
        if reasons.is_empty() {
            ResubmissionEligibility::Ready
        } else {
            ResubmissionEligibility::NeedsSelection { reasons }
        }
    }

    /// `true` only for `Ready`.
    pub fn safe_to_resubmit(&self) -> bool {
        self.eligibility() == ResubmissionEligibility::Ready
    }

    pub fn summary(&self) -> StoppedPartialSummary {
        StoppedPartialSummary {
            id: self.id.clone(),
            digest: self.digest,
            reason: self.reason.clone(),
            recovered_after_process_loss: self.recovered_after_process_loss,
            coverage: self.coverage,
            eligibility: self.eligibility(),
            cut_mid_tool_call: self.cut_mid_tool_call(),
            tool_outcome_unknown: self.tool_outcome_unknown(),
            item_count: u32::try_from(self.items.len()).unwrap_or(u32::MAX),
        }
    }

    /// Recomputes the digest and compares it.
    pub fn verify_digest(&self) -> Result<(), StoppedPartialDigestMismatch> {
        let computed = content_digest(&DigestedContent {
            id: &self.id,
            reason: &self.reason,
            recovered_after_process_loss: self.recovered_after_process_loss,
            coverage: self.coverage,
            items: &self.items,
        })
        .ok();
        if computed == Some(self.digest) {
            Ok(())
        } else {
            Err(StoppedPartialDigestMismatch {
                recorded: self.digest,
                computed,
            })
        }
    }
}

/// What the observation carries: identity and facts, never payload.
#[derive(
    Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
pub struct StoppedPartialSummary {
    pub id: StoppedPartialId,
    pub digest: StoppedPartialDigest,
    pub reason: StopReason,
    pub recovered_after_process_loss: bool,
    pub coverage: CaptureCoverage,
    pub eligibility: ResubmissionEligibility,
    pub cut_mid_tool_call: bool,
    pub tool_outcome_unknown: bool,
    pub item_count: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id() -> StoppedPartialId {
        StoppedPartialId {
            session_id: SessionId::from("session-1"),
            root: TurnId::from("root-1"),
            turn_id: TurnId::from("turn-1"),
            base: CaptureBase(2),
            sealed_through: 9,
        }
    }

    fn item_id(key: &str) -> PartialItemId {
        PartialItemId::new("llm", 1, PartialItemKey::Text(key))
    }

    fn text(key: &str) -> PartialItem {
        PartialItem::Text {
            id: item_id(key),
            state: CutState::Interrupted,
            text: "half an answ".to_string(),
        }
    }

    fn seal(coverage: CaptureCoverage, items: Vec<PartialItem>) -> StoppedPartial {
        match StoppedPartial::seal(id(), StopReason::UserCancel, false, coverage, items) {
            Ok(partial) => partial,
            Err(error) => panic!("seal: {error}"),
        }
    }

    #[test]
    fn item_ids_spell_invocation_epoch_kind_and_key() {
        assert_eq!(
            PartialItemId::new("inv", 3, PartialItemKey::Tool("#0")).as_str(),
            "inv/3/tool/#0"
        );
    }

    #[test]
    fn eligibility_follows_items_and_coverage() {
        assert_eq!(
            seal(CaptureCoverage::Complete, Vec::new()).eligibility(),
            ResubmissionEligibility::Empty
        );
        assert_eq!(
            seal(CaptureCoverage::AcknowledgedPrefix, Vec::new()).eligibility(),
            ResubmissionEligibility::Empty
        );
        let ready = seal(CaptureCoverage::Complete, vec![text("a")]);
        assert_eq!(ready.eligibility(), ResubmissionEligibility::Ready);
        assert!(ready.safe_to_resubmit());

        let reasoning = PartialItem::Reasoning {
            id: item_id("r"),
            state: CutState::Complete,
            summary: "thinking".to_string(),
        };
        let fragment = PartialItem::ArgumentFragment {
            id: item_id("f"),
            call_id: None,
            tool_name: Some("read".to_string()),
            raw_arguments: r#"{"path":"READ"#.to_string(),
            state: FragmentState::Interrupted,
        };
        let partial = seal(
            CaptureCoverage::AcknowledgedPrefix,
            vec![text("a"), reasoning, fragment],
        );
        assert_eq!(
            partial.eligibility(),
            ResubmissionEligibility::NeedsSelection {
                reasons: vec![
                    SelectionReason::Reasoning { item: item_id("r") },
                    SelectionReason::ArgumentFragment { item: item_id("f") },
                    SelectionReason::AcknowledgedPrefixOnly,
                ],
            }
        );
        assert!(!partial.safe_to_resubmit());
        assert!(partial.cut_mid_tool_call());
        assert!(!partial.tool_outcome_unknown());
    }

    #[test]
    fn a_running_call_is_cut_mid_tool_call_with_unknown_outcome() {
        let running = PartialItem::ToolCall {
            id: item_id("c"),
            call: CompleteToolCall {
                call_id: "call-1".to_string(),
                tool_name: "read".to_string(),
                arguments: serde_json::json!({"path": "README"}),
            },
            execution: ToolExecutionState::Running(RunningTool {
                output: ToolOutputCapture::Unavailable,
                outcome: InterruptedToolOutcome::OutcomeUnknown,
            }),
        };
        let partial = seal(CaptureCoverage::Complete, vec![running]);
        assert!(partial.cut_mid_tool_call());
        assert!(partial.tool_outcome_unknown());
        assert!(partial.safe_to_resubmit());
        let summary = partial.summary();
        assert_eq!(summary.item_count, 1);
        assert!(summary.tool_outcome_unknown);
        assert_eq!(summary.digest, partial.digest);
    }

    #[test]
    fn the_digest_covers_every_other_field() {
        let partial = seal(CaptureCoverage::Complete, vec![text("a")]);
        assert_eq!(partial.verify_digest(), Ok(()));
        let resealed = seal(CaptureCoverage::Complete, vec![text("a")]);
        assert_eq!(resealed.digest, partial.digest);

        let mut tampered = partial.clone();
        tampered.recovered_after_process_loss = true;
        assert!(tampered.verify_digest().is_err());
        let mut tampered = partial.clone();
        tampered.items.clear();
        assert!(tampered.verify_digest().is_err());
        let mut tampered = partial;
        tampered.id.sealed_through += 1;
        assert!(tampered.verify_digest().is_err());
    }

    #[test]
    fn the_value_round_trips_through_json() {
        let partial = seal(CaptureCoverage::Complete, vec![text("a")]);
        let encoded = match serde_json::to_string(&partial) {
            Ok(encoded) => encoded,
            Err(error) => panic!("encode: {error}"),
        };
        let decoded: StoppedPartial = match serde_json::from_str(&encoded) {
            Ok(decoded) => decoded,
            Err(error) => panic!("decode: {error}"),
        };
        assert_eq!(decoded, partial);
        assert_eq!(decoded.verify_digest(), Ok(()));
    }

    #[test]
    fn tool_output_capture_counts_what_passes_the_cap() {
        let mut capture = ToolOutputCapture::Unavailable;
        capture.push(ToolOutputChunk {
            text: "a".repeat(TOOL_OUTPUT_CAPTURE_MAX_BYTES as usize - 1),
        });
        capture.push(ToolOutputChunk {
            text: "bb".to_string(),
        });
        capture.push(ToolOutputChunk {
            text: "c".to_string(),
        });
        let ToolOutputCapture::Captured {
            chunks,
            omitted_bytes,
        } = capture
        else {
            panic!("a report captures");
        };
        assert_eq!(chunks.len(), 1);
        assert_eq!(omitted_bytes, 3);
    }

    #[test]
    fn stop_reasons_follow_the_terminal() {
        assert_eq!(
            StopReason::of_stop(&crate::TurnStop::MaxTurns),
            StopReason::Other {
                cause: OtherStopCause::ProtocolStop
            }
        );
        assert_eq!(
            StopReason::of_stop(&crate::TurnStop::ProviderError),
            StopReason::Other {
                cause: OtherStopCause::ProviderFailure
            }
        );
    }
}

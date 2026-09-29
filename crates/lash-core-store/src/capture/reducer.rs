//! The capture reducer: one pure fold from a turn's capture frames to its
//! stopped partial (ADR 0114 §3.1).

use std::collections::{BTreeSet, HashMap};

use lash_sansio::llm::types::StreamBlockIdentity;
use lash_sansio::{
    CaptureCoverage, CompleteToolCall, CutState, FragmentState, InterruptedToolOutcome,
    PartialItem, PartialItemId, PartialItemKey, RunningTool, StopReason, StoppedPartial,
    StoppedPartialId, ToolExecutionState, ToolInputIdentity, ToolOutputCapture,
};

use crate::store::{CaptureFrame, CaptureFrameKey, CaptureInvocationKey};

/// Frames that cannot be folded. A violation is never a smaller partial: the
/// store answers it as `StoreError::CaptureCorrupt`.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "violation", rename_all = "snake_case")]
pub enum CaptureReduceViolation {
    /// A delta for a block or call that is not open: never started, or
    /// already ended.
    DeltaWithoutStart {
        sequence: u64,
    },
    EndWithoutStart {
        sequence: u64,
    },
    /// A second end of one block or call, a second parse verdict of one
    /// call, or a second settlement of one tool call.
    DuplicateEnd {
        sequence: u64,
    },
    /// A second start of one block or call, or an execution start after the
    /// call settled.
    DuplicateStart {
        sequence: u64,
    },
    /// A parse verdict for a call whose arguments the provider never closed.
    ParseWithoutEnd {
        sequence: u64,
    },
    /// Progress or a settlement for a call whose execution never started.
    ToolFrameWithoutStart {
        sequence: u64,
        call_id: lash_sansio::ToolCallId,
    },
    /// Progress for a call that already settled.
    ToolFrameAfterSettle {
        sequence: u64,
        call_id: lash_sansio::ToolCallId,
    },
    /// The selected frames are not dense in sequence: `found` follows
    /// `after`.
    SequenceGap {
        after: u64,
        found: u64,
    },
    /// The folded value could not be encoded for its digest.
    Unencodable {
        message: String,
    },
}

/// Folds `frames` into the partial `id` names.
///
/// It keeps frames of `id`'s turn whose base equals `id.base` and whose
/// sequence is at most `id.sealed_through`. Those must be dense in sequence.
/// It then drops frames whose `(invocation, attempt_epoch)` is `retracted`,
/// and folds the rest in sequence order. Tool frames attach to their call by
/// `call_id`; a call id with no provider call item in the tail belongs to a
/// call committed at an earlier checkpoint and is dropped.
pub fn reduce_capture(
    id: StoppedPartialId,
    reason: StopReason,
    recovered_after_process_loss: bool,
    coverage: CaptureCoverage,
    frames: &[(CaptureFrameKey, CaptureFrame)],
    retracted: &BTreeSet<(CaptureInvocationKey, u32)>,
) -> Result<StoppedPartial, CaptureReduceViolation> {
    let mut selected = frames
        .iter()
        .filter(|(key, _)| {
            key.turn.session_id == id.session_id
                && key.turn.turn_id == id.turn_id
                && key.base == id.base
                && key.sequence <= id.sealed_through
        })
        .collect::<Vec<_>>();
    selected.sort_by_key(|(key, _)| key.sequence);
    for pair in selected.windows(2) {
        let (after, found) = (pair[0].0.sequence, pair[1].0.sequence);
        if after.checked_add(1) != Some(found) {
            return Err(CaptureReduceViolation::SequenceGap { after, found });
        }
    }

    let mut fold = Fold::default();
    for (key, frame) in selected {
        if retracted.contains(&(key.invocation.clone(), key.attempt_epoch)) {
            continue;
        }
        fold.apply(key, frame)?;
    }
    let items = fold.into_items();
    StoppedPartial::seal(id, reason, recovered_after_process_loss, coverage, items).map_err(
        |error| CaptureReduceViolation::Unencodable {
            message: error.to_string(),
        },
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum BlockKind {
    Text,
    Reasoning,
}

/// A block's identity within one attempt of one invocation.
type BlockKey = (CaptureInvocationKey, u32, BlockKind, String, u64);
/// A call's identity within one attempt of one invocation.
type CallKey = (CaptureInvocationKey, u32, u64);

struct BlockEntry {
    kind: BlockKind,
    invocation: CaptureInvocationKey,
    attempt_epoch: u32,
    key: String,
    text: String,
    ended: bool,
}

enum Verdict {
    Parsed {
        call_id: String,
        tool_name: String,
        arguments: serde_json::Value,
    },
    Unparseable {
        parse_error: String,
    },
}

struct CallEntry {
    invocation: CaptureInvocationKey,
    attempt_epoch: u32,
    ordinal: u64,
    call_id: Option<String>,
    tool_name: Option<String>,
    raw_arguments: String,
    ended: bool,
    verdict: Option<Verdict>,
    execution: ToolExecutionState,
}

enum Entry {
    Block(BlockEntry),
    Call(Box<CallEntry>),
}

#[derive(Default)]
struct Fold {
    /// In order of each entry's first frame.
    entries: Vec<Entry>,
    blocks: HashMap<BlockKey, usize>,
    calls: HashMap<CallKey, usize>,
    /// Parsed calls by their provider call id: where a started attempt
    /// attaches.
    parsed: HashMap<String, usize>,
    /// Started calls by lash's `ToolCallId`: where progress and settlement
    /// frames attach.
    executions: HashMap<lash_sansio::ToolCallId, usize>,
}

fn block_key_text(block: &StreamBlockIdentity) -> String {
    if block.id.is_empty() {
        format!("#{}", block.ordinal)
    } else {
        block.id.clone()
    }
}

impl Fold {
    fn apply(
        &mut self,
        key: &CaptureFrameKey,
        frame: &CaptureFrame,
    ) -> Result<(), CaptureReduceViolation> {
        let sequence = key.sequence;
        match frame {
            CaptureFrame::TextStart { block } => self.start_block(key, BlockKind::Text, block),
            CaptureFrame::ReasoningStart { block } => {
                self.start_block(key, BlockKind::Reasoning, block)
            }
            CaptureFrame::TextDelta { block, text } => {
                self.open_block(key, BlockKind::Text, block, sequence, false)?
                    .text
                    .push_str(text);
                Ok(())
            }
            CaptureFrame::ReasoningDelta { block, text } => {
                self.open_block(key, BlockKind::Reasoning, block, sequence, false)?
                    .text
                    .push_str(text);
                Ok(())
            }
            CaptureFrame::TextEnd { block, text } => {
                self.end_block(key, BlockKind::Text, block, text)
            }
            CaptureFrame::ReasoningEnd { block, text } => {
                self.end_block(key, BlockKind::Reasoning, block, text)
            }
            CaptureFrame::ToolInputStart { call } => self.start_call(key, call),
            CaptureFrame::ToolInputDelta { call, text } => {
                let entry = self
                    .call(key, call)
                    .ok_or(CaptureReduceViolation::DeltaWithoutStart { sequence })?;
                if entry.ended {
                    return Err(CaptureReduceViolation::DeltaWithoutStart { sequence });
                }
                entry.raw_arguments.push_str(text);
                Ok(())
            }
            CaptureFrame::ToolInputEnd {
                call,
                raw_arguments,
            } => {
                let entry = self
                    .call(key, call)
                    .ok_or(CaptureReduceViolation::EndWithoutStart { sequence })?;
                if entry.ended {
                    return Err(CaptureReduceViolation::DuplicateEnd { sequence });
                }
                entry.raw_arguments.clone_from(raw_arguments);
                entry.ended = true;
                Ok(())
            }
            CaptureFrame::ToolCallParsed {
                call,
                call_id,
                tool_name,
                arguments,
            } => {
                let index = self.judge(
                    key,
                    call,
                    Verdict::Parsed {
                        call_id: call_id.clone(),
                        tool_name: tool_name.clone(),
                        arguments: arguments.clone(),
                    },
                )?;
                self.parsed.insert(call_id.clone(), index);
                Ok(())
            }
            CaptureFrame::ToolCallUnparseable { call, parse_error } => self
                .judge(
                    key,
                    call,
                    Verdict::Unparseable {
                        parse_error: parse_error.clone(),
                    },
                )
                .map(|_| ()),
            CaptureFrame::ToolExecutionStarted {
                call_id,
                provider_call_id,
            } => {
                // A call with no streamed parse — a language runtime's call,
                // or one whose provider correlation the tail never saw — is
                // not a partial item.
                let Some(index) = provider_call_id
                    .as_deref()
                    .and_then(|provider_call_id| self.parsed.get(provider_call_id))
                    .copied()
                else {
                    return Ok(());
                };
                self.executions.insert(call_id.clone(), index);
                let Some(execution) = self.execution(call_id, None) else {
                    return Ok(());
                };
                match execution {
                    ToolExecutionState::NotStarted => {
                        *execution = ToolExecutionState::Running(RunningTool {
                            output: ToolOutputCapture::Unavailable,
                            outcome: InterruptedToolOutcome::OutcomeUnknown,
                        });
                        Ok(())
                    }
                    // A retried attempt of a running call keeps what the
                    // earlier attempt reported.
                    ToolExecutionState::Running(_) => Ok(()),
                    ToolExecutionState::Settled { .. } => {
                        Err(CaptureReduceViolation::DuplicateStart { sequence })
                    }
                }
            }
            CaptureFrame::ToolOutputProgress {
                call_id,
                provider_call_id,
                chunk,
            } => {
                let Some(execution) = self.execution(call_id, provider_call_id.as_deref()) else {
                    return Ok(());
                };
                match execution {
                    ToolExecutionState::NotStarted => {
                        Err(CaptureReduceViolation::ToolFrameWithoutStart {
                            sequence,
                            call_id: call_id.clone(),
                        })
                    }
                    ToolExecutionState::Running(running) => {
                        running.output.push(chunk.clone());
                        Ok(())
                    }
                    ToolExecutionState::Settled { .. } => {
                        Err(CaptureReduceViolation::ToolFrameAfterSettle {
                            sequence,
                            call_id: call_id.clone(),
                        })
                    }
                }
            }
            CaptureFrame::ToolSettled {
                call_id,
                provider_call_id,
                output,
            } => {
                let Some(execution) = self.execution(call_id, provider_call_id.as_deref()) else {
                    return Ok(());
                };
                match execution {
                    ToolExecutionState::NotStarted => {
                        Err(CaptureReduceViolation::ToolFrameWithoutStart {
                            sequence,
                            call_id: call_id.clone(),
                        })
                    }
                    ToolExecutionState::Running(_) => {
                        *execution = ToolExecutionState::Settled {
                            output: output.clone(),
                        };
                        Ok(())
                    }
                    ToolExecutionState::Settled { .. } => {
                        Err(CaptureReduceViolation::DuplicateEnd { sequence })
                    }
                }
            }
        }
    }

    fn block_key(key: &CaptureFrameKey, kind: BlockKind, block: &StreamBlockIdentity) -> BlockKey {
        (
            key.invocation.clone(),
            key.attempt_epoch,
            kind,
            block.id.clone(),
            block.ordinal,
        )
    }

    fn start_block(
        &mut self,
        key: &CaptureFrameKey,
        kind: BlockKind,
        block: &StreamBlockIdentity,
    ) -> Result<(), CaptureReduceViolation> {
        let block_key = Self::block_key(key, kind, block);
        if self.blocks.contains_key(&block_key) {
            return Err(CaptureReduceViolation::DuplicateStart {
                sequence: key.sequence,
            });
        }
        self.blocks.insert(block_key, self.entries.len());
        self.entries.push(Entry::Block(BlockEntry {
            kind,
            invocation: key.invocation.clone(),
            attempt_epoch: key.attempt_epoch,
            key: block_key_text(block),
            text: String::new(),
            ended: false,
        }));
        Ok(())
    }

    /// The open block a delta or end names. `ending` picks the violation a
    /// missing block answers.
    fn open_block(
        &mut self,
        key: &CaptureFrameKey,
        kind: BlockKind,
        block: &StreamBlockIdentity,
        sequence: u64,
        ending: bool,
    ) -> Result<&mut BlockEntry, CaptureReduceViolation> {
        let missing = if ending {
            CaptureReduceViolation::EndWithoutStart { sequence }
        } else {
            CaptureReduceViolation::DeltaWithoutStart { sequence }
        };
        let Some(&index) = self.blocks.get(&Self::block_key(key, kind, block)) else {
            return Err(missing);
        };
        let Some(Entry::Block(entry)) = self.entries.get_mut(index) else {
            return Err(missing);
        };
        if entry.ended {
            return Err(if ending {
                CaptureReduceViolation::DuplicateEnd { sequence }
            } else {
                CaptureReduceViolation::DeltaWithoutStart { sequence }
            });
        }
        Ok(entry)
    }

    fn end_block(
        &mut self,
        key: &CaptureFrameKey,
        kind: BlockKind,
        block: &StreamBlockIdentity,
        text: &str,
    ) -> Result<(), CaptureReduceViolation> {
        let entry = self.open_block(key, kind, block, key.sequence, true)?;
        // The end's text is authoritative, like `TextBlockEnd::text`.
        entry.text = text.to_string();
        entry.ended = true;
        Ok(())
    }

    fn start_call(
        &mut self,
        key: &CaptureFrameKey,
        call: &ToolInputIdentity,
    ) -> Result<(), CaptureReduceViolation> {
        let call_key = (key.invocation.clone(), key.attempt_epoch, call.ordinal);
        if self.calls.contains_key(&call_key) {
            return Err(CaptureReduceViolation::DuplicateStart {
                sequence: key.sequence,
            });
        }
        self.calls.insert(call_key, self.entries.len());
        self.entries.push(Entry::Call(Box::new(CallEntry {
            invocation: key.invocation.clone(),
            attempt_epoch: key.attempt_epoch,
            ordinal: call.ordinal,
            call_id: call.call_id.clone(),
            tool_name: call.tool_name.clone(),
            raw_arguments: String::new(),
            ended: false,
            verdict: None,
            execution: ToolExecutionState::NotStarted,
        })));
        Ok(())
    }

    /// The call a frame names, with the identity facts the frame knows
    /// filled in where the call had none yet.
    fn call(&mut self, key: &CaptureFrameKey, call: &ToolInputIdentity) -> Option<&mut CallEntry> {
        let index = *self
            .calls
            .get(&(key.invocation.clone(), key.attempt_epoch, call.ordinal))?;
        let Some(Entry::Call(entry)) = self.entries.get_mut(index) else {
            return None;
        };
        if entry.call_id.is_none() {
            entry.call_id.clone_from(&call.call_id);
        }
        if entry.tool_name.is_none() {
            entry.tool_name.clone_from(&call.tool_name);
        }
        Some(entry)
    }

    fn judge(
        &mut self,
        key: &CaptureFrameKey,
        call: &ToolInputIdentity,
        verdict: Verdict,
    ) -> Result<usize, CaptureReduceViolation> {
        let sequence = key.sequence;
        let index = self
            .calls
            .get(&(key.invocation.clone(), key.attempt_epoch, call.ordinal))
            .copied()
            .ok_or(CaptureReduceViolation::ParseWithoutEnd { sequence })?;
        let entry = self
            .call(key, call)
            .ok_or(CaptureReduceViolation::ParseWithoutEnd { sequence })?;
        if !entry.ended {
            return Err(CaptureReduceViolation::ParseWithoutEnd { sequence });
        }
        if entry.verdict.is_some() {
            return Err(CaptureReduceViolation::DuplicateEnd { sequence });
        }
        entry.verdict = Some(verdict);
        Ok(index)
    }

    /// The execution state of the call `call_id` names: a started call, or
    /// else the streamed call `provider_call_id` correlates, which has not
    /// started. `None` when the tail holds no such call.
    fn execution(
        &mut self,
        call_id: &lash_sansio::ToolCallId,
        provider_call_id: Option<&str>,
    ) -> Option<&mut ToolExecutionState> {
        let index = match self.executions.get(call_id) {
            Some(index) => *index,
            None => *self.parsed.get(provider_call_id?)?,
        };
        match self.entries.get_mut(index) {
            Some(Entry::Call(entry)) => Some(&mut entry.execution),
            _ => None,
        }
    }

    fn into_items(self) -> Vec<PartialItem> {
        self.entries
            .into_iter()
            .filter_map(|entry| match entry {
                Entry::Block(block) => block_item(block),
                Entry::Call(call) => Some(call_item(*call)),
            })
            .collect()
    }
}

/// A block with no text carries nothing a host could quote, so it is left
/// out rather than returned as an empty item.
fn block_item(block: BlockEntry) -> Option<PartialItem> {
    if block.text.is_empty() {
        return None;
    }
    let state = if block.ended {
        CutState::Complete
    } else {
        CutState::Interrupted
    };
    let invocation = block.invocation.as_str();
    Some(match block.kind {
        BlockKind::Text => PartialItem::Text {
            id: PartialItemId::new(
                invocation,
                block.attempt_epoch,
                PartialItemKey::Text(&block.key),
            ),
            state,
            text: block.text,
        },
        BlockKind::Reasoning => PartialItem::Reasoning {
            id: PartialItemId::new(
                invocation,
                block.attempt_epoch,
                PartialItemKey::Reasoning(&block.key),
            ),
            state,
            summary: block.text,
        },
    })
}

fn call_item(call: CallEntry) -> PartialItem {
    let invocation = call.invocation.as_str();
    match call.verdict {
        Some(Verdict::Parsed {
            call_id,
            tool_name,
            arguments,
        }) => PartialItem::ToolCall {
            id: PartialItemId::new(
                invocation,
                call.attempt_epoch,
                PartialItemKey::Tool(&call_id),
            ),
            call: CompleteToolCall {
                call_id,
                tool_name,
                arguments,
            },
            execution: call.execution,
        },
        verdict => {
            let key = call
                .call_id
                .clone()
                .unwrap_or_else(|| format!("#{}", call.ordinal));
            // Closed arguments the stop left unjudged never completed from
            // the turn's view, so they are interrupted, like unclosed ones.
            let state = match verdict {
                Some(Verdict::Unparseable { parse_error }) => {
                    FragmentState::Invalid { parse_error }
                }
                _ => FragmentState::Interrupted,
            };
            PartialItem::ArgumentFragment {
                id: PartialItemId::new(invocation, call.attempt_epoch, PartialItemKey::Tool(&key)),
                call_id: call.call_id,
                tool_name: call.tool_name,
                raw_arguments: call.raw_arguments,
                state,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{SessionId, TurnAddress, TurnId};
    use lash_sansio::{
        CaptureBase, ResubmissionEligibility, SelectionReason, ToolCallOutput, ToolOutputChunk,
    };

    const LLM: &str = "turn-1/llm/0";
    const TOOL: &str = "turn-1/tool/call-1";

    fn turn() -> TurnAddress {
        TurnAddress::new("session-1", "turn-1")
    }

    fn partial_id(base: u32, sealed_through: u64) -> StoppedPartialId {
        StoppedPartialId {
            session_id: SessionId::from("session-1"),
            root: TurnId::from("root-1"),
            turn_id: TurnId::from("turn-1"),
            base: CaptureBase(base),
            sealed_through,
        }
    }

    /// Frames under one base with dense sequences from `first`, each tagged
    /// with its invocation and attempt epoch.
    struct Tape {
        base: u32,
        next: u64,
        frames: Vec<(CaptureFrameKey, CaptureFrame)>,
    }

    impl Tape {
        fn new(base: u32, first: u64) -> Self {
            Self {
                base,
                next: first,
                frames: Vec::new(),
            }
        }

        fn push(&mut self, invocation: &str, epoch: u32, frame: CaptureFrame) -> &mut Self {
            self.frames.push((
                CaptureFrameKey {
                    turn: turn(),
                    base: CaptureBase(self.base),
                    invocation: CaptureInvocationKey(invocation.to_string()),
                    attempt_epoch: epoch,
                    sequence: self.next,
                },
                frame,
            ));
            self.next += 1;
            self
        }

        fn last(&self) -> u64 {
            self.next - 1
        }
    }

    fn block(id: &str, ordinal: u64) -> StreamBlockIdentity {
        StreamBlockIdentity::new(id, ordinal)
    }

    fn call(ordinal: u64, call_id: Option<&str>, tool: Option<&str>) -> ToolInputIdentity {
        ToolInputIdentity {
            ordinal,
            call_id: call_id.map(str::to_string),
            tool_name: tool.map(str::to_string),
            item_id: None,
        }
    }

    fn reduce(
        tape: &Tape,
        base: u32,
        sealed_through: u64,
        retracted: &[(&str, u32)],
    ) -> Result<StoppedPartial, CaptureReduceViolation> {
        let retracted = retracted
            .iter()
            .map(|(invocation, epoch)| (CaptureInvocationKey(invocation.to_string()), *epoch))
            .collect();
        reduce_capture(
            partial_id(base, sealed_through),
            StopReason::UserCancel,
            false,
            CaptureCoverage::Complete,
            &tape.frames,
            &retracted,
        )
    }

    #[test]
    fn no_frames_reduce_to_an_empty_partial() {
        let tape = Tape::new(0, 0);
        let partial = reduce(&tape, 0, 0, &[]).expect("reduce");
        assert!(partial.items.is_empty());
        assert_eq!(partial.eligibility(), ResubmissionEligibility::Empty);
        assert_eq!(partial.verify_digest(), Ok(()));
    }

    #[test]
    fn a_text_cut_keeps_the_exact_emitted_bytes_as_interrupted() {
        let mut tape = Tape::new(0, 0);
        tape.push(
            LLM,
            1,
            CaptureFrame::TextStart {
                block: block("msg", 0),
            },
        )
        .push(
            LLM,
            1,
            CaptureFrame::TextDelta {
                block: block("msg", 0),
                text: "Hello, wor".to_string(),
            },
        )
        .push(
            LLM,
            1,
            CaptureFrame::TextDelta {
                block: block("msg", 0),
                text: "ld — ünï".to_string(),
            },
        );
        let partial = reduce(&tape, 0, tape.last(), &[]).expect("reduce");
        assert_eq!(
            partial.items,
            vec![PartialItem::Text {
                id: PartialItemId(format!("{LLM}/1/text/msg")),
                state: CutState::Interrupted,
                text: "Hello, world — ünï".to_string(),
            }]
        );
        assert_eq!(partial.eligibility(), ResubmissionEligibility::Ready);
        assert!(!partial.cut_mid_tool_call());
    }

    #[test]
    fn a_completed_block_takes_its_authoritative_end_text() {
        let mut tape = Tape::new(0, 0);
        tape.push(
            LLM,
            1,
            CaptureFrame::TextStart {
                block: block("", 4),
            },
        )
        .push(
            LLM,
            1,
            CaptureFrame::TextDelta {
                block: block("", 4),
                text: "drift".to_string(),
            },
        )
        .push(
            LLM,
            1,
            CaptureFrame::TextEnd {
                block: block("", 4),
                text: "sealed text".to_string(),
            },
        );
        let partial = reduce(&tape, 0, tape.last(), &[]).expect("reduce");
        assert_eq!(
            partial.items,
            vec![PartialItem::Text {
                id: PartialItemId(format!("{LLM}/1/text/#4")),
                state: CutState::Complete,
                text: "sealed text".to_string(),
            }]
        );
    }

    #[test]
    fn a_reasoning_cut_needs_selection() {
        let mut tape = Tape::new(0, 0);
        tape.push(
            LLM,
            1,
            CaptureFrame::ReasoningStart {
                block: block("rs", 0),
            },
        )
        .push(
            LLM,
            1,
            CaptureFrame::ReasoningDelta {
                block: block("rs", 0),
                text: "Considering the".to_string(),
            },
        );
        let partial = reduce(&tape, 0, tape.last(), &[]).expect("reduce");
        let id = PartialItemId(format!("{LLM}/1/reasoning/rs"));
        assert_eq!(
            partial.items,
            vec![PartialItem::Reasoning {
                id: id.clone(),
                state: CutState::Interrupted,
                summary: "Considering the".to_string(),
            }]
        );
        assert_eq!(
            partial.eligibility(),
            ResubmissionEligibility::NeedsSelection {
                reasons: vec![SelectionReason::Reasoning { item: id }],
            }
        );
    }

    #[test]
    fn a_mid_arguments_cut_is_a_raw_interrupted_fragment() {
        let mut tape = Tape::new(0, 0);
        tape.push(
            LLM,
            1,
            CaptureFrame::ToolInputStart {
                call: call(0, Some("call-1"), Some("read")),
            },
        )
        .push(
            LLM,
            1,
            CaptureFrame::ToolInputDelta {
                call: call(0, None, None),
                text: r#"{"path":"#.to_string(),
            },
        )
        .push(
            LLM,
            1,
            CaptureFrame::ToolInputDelta {
                call: call(0, None, None),
                text: r#""READ"#.to_string(),
            },
        );
        let partial = reduce(&tape, 0, tape.last(), &[]).expect("reduce");
        assert_eq!(
            partial.items,
            vec![PartialItem::ArgumentFragment {
                id: PartialItemId(format!("{LLM}/1/tool/call-1")),
                call_id: Some("call-1".to_string()),
                tool_name: Some("read".to_string()),
                raw_arguments: r#"{"path":"READ"#.to_string(),
                state: FragmentState::Interrupted,
            }]
        );
        assert!(partial.cut_mid_tool_call());
        assert!(!partial.safe_to_resubmit());
    }

    #[test]
    fn a_fragment_without_a_call_id_is_named_by_its_ordinal() {
        let mut tape = Tape::new(0, 0);
        tape.push(
            LLM,
            1,
            CaptureFrame::ToolInputStart {
                call: call(2, None, None),
            },
        );
        let partial = reduce(&tape, 0, tape.last(), &[]).expect("reduce");
        assert_eq!(partial.items[0].id().as_str(), format!("{LLM}/1/tool/#2"));
    }

    #[test]
    fn a_closed_unparseable_call_is_an_invalid_fragment() {
        let mut tape = Tape::new(0, 0);
        tape.push(
            LLM,
            1,
            CaptureFrame::ToolInputStart {
                call: call(0, Some("call-1"), Some("read")),
            },
        )
        .push(
            LLM,
            1,
            CaptureFrame::ToolInputEnd {
                call: call(0, None, None),
                raw_arguments: "{not json".to_string(),
            },
        )
        .push(
            LLM,
            1,
            CaptureFrame::ToolCallUnparseable {
                call: call(0, None, None),
                parse_error: "expected value".to_string(),
            },
        );
        let partial = reduce(&tape, 0, tape.last(), &[]).expect("reduce");
        assert!(matches!(
            &partial.items[..],
            [PartialItem::ArgumentFragment {
                raw_arguments,
                state: FragmentState::Invalid { parse_error },
                ..
            }] if raw_arguments == "{not json" && parse_error == "expected value"
        ));
    }

    fn parsed_call(tape: &mut Tape) {
        tape.push(
            LLM,
            1,
            CaptureFrame::ToolInputStart {
                call: call(0, Some("call-1"), Some("read")),
            },
        )
        .push(
            LLM,
            1,
            CaptureFrame::ToolInputEnd {
                call: call(0, None, None),
                raw_arguments: r#"{"path":"README"}"#.to_string(),
            },
        )
        .push(
            LLM,
            1,
            CaptureFrame::ToolCallParsed {
                call: call(0, None, None),
                call_id: "call-1".to_string(),
                tool_name: "read".to_string(),
                arguments: serde_json::json!({"path": "README"}),
            },
        );
    }

    fn complete_call() -> CompleteToolCall {
        CompleteToolCall {
            call_id: "call-1".to_string(),
            tool_name: "read".to_string(),
            arguments: serde_json::json!({"path": "README"}),
        }
    }

    #[test]
    fn a_parsed_call_nobody_ran_is_not_started() {
        let mut tape = Tape::new(0, 0);
        tape.push(
            LLM,
            1,
            CaptureFrame::TextStart {
                block: block("m", 0),
            },
        )
        .push(
            LLM,
            1,
            CaptureFrame::TextEnd {
                block: block("m", 0),
                text: "Reading it.".to_string(),
            },
        );
        parsed_call(&mut tape);
        let partial = reduce(&tape, 0, tape.last(), &[]).expect("reduce");
        assert_eq!(partial.items.len(), 2);
        assert!(matches!(partial.items[0], PartialItem::Text { .. }));
        assert_eq!(
            partial.items[1],
            PartialItem::ToolCall {
                id: PartialItemId(format!("{LLM}/1/tool/call-1")),
                call: complete_call(),
                execution: ToolExecutionState::NotStarted,
            }
        );
        assert_eq!(partial.eligibility(), ResubmissionEligibility::Ready);
    }

    #[test]
    fn a_running_tool_keeps_its_progress_and_an_unknown_outcome() {
        let mut tape = Tape::new(0, 0);
        parsed_call(&mut tape);
        tape.push(
            TOOL,
            1,
            CaptureFrame::ToolExecutionStarted {
                call_id: lash_sansio::ToolCallId::fixture("call-1"),
                provider_call_id: Some("call-1".to_string()),
            },
        )
        .push(
            TOOL,
            1,
            CaptureFrame::ToolOutputProgress {
                call_id: lash_sansio::ToolCallId::fixture("call-1"),
                provider_call_id: Some("call-1".to_string()),
                chunk: ToolOutputChunk {
                    text: "one".to_string(),
                },
            },
        )
        .push(
            TOOL,
            1,
            CaptureFrame::ToolOutputProgress {
                call_id: lash_sansio::ToolCallId::fixture("call-1"),
                provider_call_id: Some("call-1".to_string()),
                chunk: ToolOutputChunk {
                    text: "two".to_string(),
                },
            },
        );
        let partial = reduce(&tape, 0, tape.last(), &[]).expect("reduce");
        assert_eq!(
            partial.items,
            vec![PartialItem::ToolCall {
                id: PartialItemId(format!("{LLM}/1/tool/call-1")),
                call: complete_call(),
                execution: ToolExecutionState::Running(RunningTool {
                    output: ToolOutputCapture::Captured {
                        chunks: vec![
                            ToolOutputChunk {
                                text: "one".to_string()
                            },
                            ToolOutputChunk {
                                text: "two".to_string()
                            },
                        ],
                        omitted_bytes: 0,
                    },
                    outcome: InterruptedToolOutcome::OutcomeUnknown,
                }),
            }]
        );
        assert!(partial.tool_outcome_unknown());
        assert!(partial.cut_mid_tool_call());
    }

    #[test]
    fn a_running_tool_without_progress_is_unavailable_and_a_settled_one_keeps_its_result() {
        let mut tape = Tape::new(0, 0);
        parsed_call(&mut tape);
        tape.push(
            TOOL,
            1,
            CaptureFrame::ToolExecutionStarted {
                call_id: lash_sansio::ToolCallId::fixture("call-1"),
                provider_call_id: Some("call-1".to_string()),
            },
        );
        let running = reduce(&tape, 0, tape.last(), &[]).expect("reduce");
        assert!(matches!(
            &running.items[0],
            PartialItem::ToolCall {
                execution: ToolExecutionState::Running(RunningTool {
                    output: ToolOutputCapture::Unavailable,
                    ..
                }),
                ..
            }
        ));

        let output = ToolCallOutput::success(serde_json::json!("contents"));
        tape.push(
            TOOL,
            1,
            CaptureFrame::ToolSettled {
                call_id: lash_sansio::ToolCallId::fixture("call-1"),
                provider_call_id: Some("call-1".to_string()),
                output: output.clone(),
            },
        );
        let settled = reduce(&tape, 0, tape.last(), &[]).expect("reduce");
        assert_eq!(
            settled.items[0],
            PartialItem::ToolCall {
                id: PartialItemId(format!("{LLM}/1/tool/call-1")),
                call: complete_call(),
                execution: ToolExecutionState::Settled { output },
            }
        );
        assert!(!settled.tool_outcome_unknown());
    }

    #[test]
    fn tool_frames_of_a_call_committed_at_an_earlier_checkpoint_are_dropped() {
        let mut tape = Tape::new(1, 10);
        tape.push(
            TOOL,
            1,
            CaptureFrame::ToolSettled {
                call_id: lash_sansio::ToolCallId::fixture("earlier"),
                provider_call_id: None,
                output: ToolCallOutput::success(serde_json::json!(1)),
            },
        );
        let partial = reduce(&tape, 1, tape.last(), &[]).expect("reduce");
        assert!(partial.items.is_empty());
    }

    #[test]
    fn a_retracted_attempt_and_frames_past_the_seal_or_base_are_excluded() {
        let mut stale = Tape::new(0, 0);
        stale.push(
            LLM,
            1,
            CaptureFrame::TextStart {
                block: block("m", 0),
            },
        );
        let mut tape = Tape::new(1, 5);
        tape.push(
            LLM,
            1,
            CaptureFrame::TextStart {
                block: block("m", 0),
            },
        )
        .push(
            LLM,
            1,
            CaptureFrame::TextDelta {
                block: block("m", 0),
                text: "abandoned".to_string(),
            },
        )
        .push(
            LLM,
            2,
            CaptureFrame::TextStart {
                block: block("m", 0),
            },
        )
        .push(
            LLM,
            2,
            CaptureFrame::TextDelta {
                block: block("m", 0),
                text: "kept".to_string(),
            },
        );
        let sealed_through = tape.last();
        tape.push(
            LLM,
            2,
            CaptureFrame::TextDelta {
                block: block("m", 0),
                text: " after the seal".to_string(),
            },
        );
        tape.frames.extend(stale.frames);
        let partial = reduce(&tape, 1, sealed_through, &[(LLM, 1)]).expect("reduce");
        assert_eq!(
            partial.items,
            vec![PartialItem::Text {
                id: PartialItemId(format!("{LLM}/2/text/m")),
                state: CutState::Interrupted,
                text: "kept".to_string(),
            }]
        );
    }

    #[test]
    fn items_follow_their_first_frame() {
        let mut tape = Tape::new(0, 0);
        tape.push(
            LLM,
            1,
            CaptureFrame::TextStart {
                block: block("a", 0),
            },
        )
        .push(
            LLM,
            1,
            CaptureFrame::ReasoningStart {
                block: block("b", 1),
            },
        )
        .push(
            LLM,
            1,
            CaptureFrame::ReasoningDelta {
                block: block("b", 1),
                text: "r".to_string(),
            },
        )
        .push(
            LLM,
            1,
            CaptureFrame::TextDelta {
                block: block("a", 0),
                text: "t".to_string(),
            },
        );
        let partial = reduce(&tape, 0, tape.last(), &[]).expect("reduce");
        assert!(matches!(
            &partial.items[..],
            [PartialItem::Text { .. }, PartialItem::Reasoning { .. }]
        ));
    }

    #[test]
    fn a_block_that_never_carried_text_is_left_out() {
        let mut tape = Tape::new(0, 0);
        tape.push(
            LLM,
            1,
            CaptureFrame::TextStart {
                block: block("a", 0),
            },
        )
        .push(
            LLM,
            1,
            CaptureFrame::ReasoningStart {
                block: block("r", 1),
            },
        )
        .push(
            LLM,
            1,
            CaptureFrame::ReasoningEnd {
                block: block("r", 1),
                text: String::new(),
            },
        );
        let partial = reduce(&tape, 0, tape.last(), &[]).expect("reduce");
        assert!(partial.items.is_empty());
    }

    #[test]
    fn broken_tapes_are_violations_never_smaller_partials() {
        let delta = |tape: &mut Tape| {
            tape.push(
                LLM,
                1,
                CaptureFrame::TextDelta {
                    block: block("m", 0),
                    text: "x".to_string(),
                },
            );
        };
        let mut tape = Tape::new(0, 0);
        delta(&mut tape);
        assert_eq!(
            reduce(&tape, 0, tape.last(), &[]),
            Err(CaptureReduceViolation::DeltaWithoutStart { sequence: 0 })
        );

        let mut tape = Tape::new(0, 0);
        tape.push(
            LLM,
            1,
            CaptureFrame::TextEnd {
                block: block("m", 0),
                text: String::new(),
            },
        );
        assert_eq!(
            reduce(&tape, 0, tape.last(), &[]),
            Err(CaptureReduceViolation::EndWithoutStart { sequence: 0 })
        );

        let mut tape = Tape::new(0, 0);
        let end = CaptureFrame::ToolInputEnd {
            call: call(0, None, None),
            raw_arguments: "{}".to_string(),
        };
        tape.push(
            LLM,
            1,
            CaptureFrame::ToolInputStart {
                call: call(0, None, None),
            },
        )
        .push(LLM, 1, end.clone())
        .push(LLM, 1, end);
        assert_eq!(
            reduce(&tape, 0, tape.last(), &[]),
            Err(CaptureReduceViolation::DuplicateEnd { sequence: 2 })
        );

        let mut tape = Tape::new(0, 0);
        parsed_call(&mut tape);
        tape.push(
            TOOL,
            1,
            CaptureFrame::ToolSettled {
                call_id: lash_sansio::ToolCallId::fixture("call-1"),
                provider_call_id: Some("call-1".to_string()),
                output: ToolCallOutput::success(serde_json::json!(1)),
            },
        );
        assert_eq!(
            reduce(&tape, 0, tape.last(), &[]),
            Err(CaptureReduceViolation::ToolFrameWithoutStart {
                sequence: 3,
                call_id: lash_sansio::ToolCallId::fixture("call-1"),
            })
        );

        let mut tape = Tape::new(0, 0);
        tape.push(
            LLM,
            1,
            CaptureFrame::ToolInputStart {
                call: call(0, None, None),
            },
        )
        .push(
            LLM,
            1,
            CaptureFrame::ToolCallUnparseable {
                call: call(0, None, None),
                parse_error: "eof".to_string(),
            },
        );
        assert_eq!(
            reduce(&tape, 0, tape.last(), &[]),
            Err(CaptureReduceViolation::ParseWithoutEnd { sequence: 1 })
        );

        let mut tape = Tape::new(0, 0);
        tape.push(
            LLM,
            1,
            CaptureFrame::TextStart {
                block: block("m", 0),
            },
        );
        tape.next += 1;
        delta(&mut tape);
        assert_eq!(
            reduce(&tape, 0, tape.last(), &[]),
            Err(CaptureReduceViolation::SequenceGap { after: 0, found: 2 })
        );
    }
}

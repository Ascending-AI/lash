//! Typed limit refusals survive every crash cut independently of model prose.

use super::*;

impl Crash {
    pub(super) async fn limit_laws(&self) -> Vec<String> {
        let (counted, requested) = match self.turn {
            Turn::LimitStep => (0, LIMIT + 1),
            Turn::LimitCell => (LIMIT, 1),
            _ => return Vec::new(),
        };
        let expected = lash::ToolCallLimitExceeded {
            scope: lash::ToolCallLimitScope::Cell,
            limit: lash::MaxToolCalls::new(LIMIT),
            counted,
            requested,
        };
        let Some((session, handle)) = self.host.lock_recover().take() else {
            return vec!["the host sent no limit scenario".into()];
        };
        let mut violations = Vec::new();
        if self.turn == Turn::LimitCell {
            let view = session
                .read()
                .await
                .expect("committed limit scenario")
                .expect("head");
            let transcript = view.transcript().expect("typed cell transcript");
            let limits = transcript
                .entries()
                .iter()
                .filter_map(|entry| {
                    let lash::transcript::TranscriptItem::Cell(cell) = &entry.item else {
                        return None;
                    };
                    cell.result
                        .failure()
                        .and_then(|failure| failure.tool_call_limit)
                })
                .collect::<Vec<_>>();
            if limits != [expected] {
                violations.push(format!(
                    "the cell records {limits:?}, expected {expected:?}"
                ));
            }
        } else {
            let output = match tokio::time::timeout(served::WATCHDOG, handle.output()).await {
                Ok(Ok(output)) => output,
                other => return vec![format!("the limit scenario did not settle: {other:?}")],
            };
            let failures = output
                .result
                .tool_calls
                .iter()
                .filter_map(|call| match &call.output.outcome {
                    lash_core::ToolCallOutcome::Failure(failure) => Some(failure),
                    _ => None,
                })
                .collect::<Vec<_>>();
            if failures.len() != requested
                || failures.iter().any(|failure| {
                    failure.code != lash::ToolCallLimitExceeded::CODE
                        || failure.class != lash_core::ToolFailureClass::ResourceLimit
                        || failure.source != lash_protocol_types::ToolFailureSource::Runtime
                })
            {
                violations.push(format!(
                    "expected {requested} typed limit refusals, got {failures:?}"
                ));
            }
            let requests = self.scripts.requests(INPUT);
            let messages: Vec<lash_core::llm::types::LlmMessage> =
                serde_json::from_str(requests.last().expect("model continuation"))
                    .expect("typed provider messages");
            let statuses = messages
                .iter()
                .flat_map(|message| message.blocks.iter())
                .filter_map(|block| match block {
                    lash_core::llm::types::LlmContentBlock::ToolResult { status, .. } => {
                        Some(*status)
                    }
                    _ => None,
                })
                .collect::<Vec<_>>();
            let mut expected_statuses = vec![lash_protocol_types::ToolCallStatus::Success; LIMIT];
            expected_statuses.extend(vec![
                lash_protocol_types::ToolCallStatus::Failure;
                requested
            ]);
            if statuses != expected_statuses {
                violations.push(format!(
                    "model result statuses {statuses:?}, expected {expected_statuses:?}"
                ));
            }
        }
        violations
    }
}

/// A turn that is cut anywhere refuses the same call the same way: the
/// first group's calls are counted once, however many executions formed
/// the group, and the refused calls run in no execution. A step of native
/// calls counts its own group.
pub(super) async fn tool_call_limit_refuses_the_same_call_across_a_crash(tier: Tier) {
    prove(Turn::LimitStep, tier).await;
}

/// The same law in a cell, which counts its total: after a cut anywhere the
/// cell's next call past the limit is refused with the first aggregate's
/// calls counted once.
pub(super) async fn tool_call_limit_refuses_the_same_call_across_a_crash_in_a_cell(tier: Tier) {
    prove(Turn::LimitCell, tier).await;
}

impl Turn {
    pub(super) fn max_tool_calls(self) -> usize {
        match self {
            Self::LimitStep | Self::LimitCell => LIMIT,
            Self::Plain
            | Self::CellIdentity
            | Self::Activity
            | Self::Put
            | Self::State
            | Self::StatePair
            | Self::StateCell
            | Self::StatePairCell
            | Self::Trace
            | Self::CellTrace
            | Self::Finish => 64,
        }
    }
}

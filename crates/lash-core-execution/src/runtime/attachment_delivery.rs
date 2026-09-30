//! Delivering a process's output to its receiver: acquire, record, release.
//!
//! A process terminal can carry stored attachments. Before the value is
//! recorded anywhere a receiver can read it, the receiver acquires its own
//! referrer edge on every stored attachment the value names. The source's
//! edges (the process record, or the producer's execution, session or upload)
//! may end only after that record exists (ADR 0124). The receiver acquires
//! under the claim its scope names: a process receiver holds through its
//! record, every other receiver through its journal.

use crate::AttachmentReferrers;
use crate::{AttachmentId, ExecutionScope, PluginError, ProcessAwaitOutput};
use lash_core_store::artifact_referrer::{ArtifactCleanupPlan, ArtifactReferrer, ReferrerClaim};

/// Stored attachment ids of a process terminal output, sorted and
/// deduplicated. Only a settled output carries a value; an abandoned or
/// no-longer-retained terminal delivers no attachment.
pub fn delivered_attachment_ids(output: &ProcessAwaitOutput) -> Vec<AttachmentId> {
    let ProcessAwaitOutput::Settled { output } = output else {
        return Vec::new();
    };
    let mut ids = output
        .attachments()
        .iter()
        .filter_map(|attachment| attachment.stored_ref())
        .map(|attachment_ref| attachment_ref.id.clone())
        .collect::<Vec<_>>();
    ids.sort();
    ids.dedup();
    ids
}

/// The claim a delivery into `scope` acquires under: a process holds what it
/// receives through its record, and every other receiver through the journal
/// that records the value.
pub fn receiving_claim(scope: &ExecutionScope) -> Result<ReferrerClaim, PluginError> {
    let claim = match scope {
        ExecutionScope::Process { process_id } => {
            ReferrerClaim::unguarded(ArtifactReferrer::ProcessRecord(process_id.clone()))
        }
        _ => {
            let journal = scope.journal_identity().map_err(|error| {
                PluginError::Session(format!(
                    "a delivery into `{scope:?}` names no journal to hold its attachments: {error}"
                ))
            })?;
            ReferrerClaim::guarded(
                ArtifactReferrer::Execution(journal),
                ArtifactCleanupPlan::AwaitJournal,
            )
        }
    };
    claim.map_err(|error| PluginError::Session(error.to_string()))
}

/// What acquiring a delivered value's attachments found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeliveryAcquisition {
    /// The receiver holds every stored attachment the value names.
    Held,
    /// A digest had no upload evidence: its source was ended and swept.
    SourceGone { digest: AttachmentId },
    /// The receiver is fenced and acquires no attachment edge.
    ReceiverEnded { referrer: ArtifactReferrer },
}

/// Acquire `receiving_claim(receiver)` on every stored attachment `output`
/// delivers. An attachment the store holds no evidence for answers
/// `SourceGone`; a fenced receiver answers `ReceiverEnded`. Other store
/// failures retain their typed controller classification.
pub async fn acquire_delivered_attachments(
    attachments: &dyn AttachmentReferrers,
    receiver: &ExecutionScope,
    output: &ProcessAwaitOutput,
) -> Result<DeliveryAcquisition, PluginError> {
    let ids = delivered_attachment_ids(output);
    if ids.is_empty() {
        return Ok(DeliveryAcquisition::Held);
    }
    let claim = receiving_claim(receiver)?;
    acquire_under(attachments, &claim, &ids).await
}

/// Acquire `claim` on `ids`, distinguishing a missing source from an ended
/// receiver. Other store failures retain their classification and cause.
pub async fn acquire_under(
    attachments: &dyn AttachmentReferrers,
    claim: &ReferrerClaim,
    ids: &[AttachmentId],
) -> Result<DeliveryAcquisition, PluginError> {
    if ids.is_empty() {
        return Ok(DeliveryAcquisition::Held);
    }
    match attachments.acquire_attachment_refs(claim, ids).await {
        Ok(()) => Ok(DeliveryAcquisition::Held),
        Err(crate::StoreError::UnknownAttachment { digest }) => {
            Ok(DeliveryAcquisition::SourceGone { digest })
        }
        Err(crate::StoreError::ArtifactReferrerEnded { referrer }) => {
            Ok(DeliveryAcquisition::ReceiverEnded { referrer })
        }
        Err(error) => {
            let mut error = crate::RuntimeEffectControllerError::from(error);
            error.message = format!(
                "failed to acquire the delivered attachments under `{}`: {}",
                claim.referrer().canonical_id(),
                error.message
            );
            Err(PluginError::RuntimeEffectController(error))
        }
    }
}

/// The typed failure a delivery records in place of the value when its
/// acquisition answered `SourceGone`.
pub fn source_gone_output(digest: &AttachmentId) -> ProcessAwaitOutput {
    ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::failure(
        crate::ToolFailure::runtime(
            crate::ToolFailureClass::Internal,
            "process_result_attachment_unavailable",
            digest.to_string(),
        ),
    ))
}

/// A completed delivery that acquires nothing because its receiver ended.
pub fn receiver_ended_output(referrer: &ArtifactReferrer) -> ProcessAwaitOutput {
    ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::failure(
        crate::ToolFailure::runtime(
            crate::ToolFailureClass::Internal,
            "process_result_receiver_ended",
            referrer.canonical_id(),
        ),
    ))
}

fn receiver_ended_error(referrer: ArtifactReferrer) -> PluginError {
    PluginError::RuntimeEffectController(
        crate::StoreError::ArtifactReferrerEnded { referrer }.into(),
    )
}

/// Acquire what `output` delivers into `receiver`, and answer the value the
/// receiver records: `output` itself, or `source_gone_output` when a
/// delivered attachment's source was already swept, or
/// `receiver_ended_output` when the receiver is fenced.
pub async fn deliver_output(
    attachments: &dyn AttachmentReferrers,
    receiver: &ExecutionScope,
    output: ProcessAwaitOutput,
) -> Result<ProcessAwaitOutput, PluginError> {
    match acquire_delivered_attachments(attachments, receiver, &output).await? {
        DeliveryAcquisition::Held => Ok(output),
        DeliveryAcquisition::SourceGone { digest } => Ok(source_gone_output(&digest)),
        DeliveryAcquisition::ReceiverEnded { referrer } => Ok(receiver_ended_output(&referrer)),
    }
}

/// The claim a process's record holds what it publishes or receives under.
fn process_record_claim(process_id: &crate::ProcessId) -> Result<ReferrerClaim, PluginError> {
    ReferrerClaim::unguarded(ArtifactReferrer::ProcessRecord(process_id.clone()))
        .map_err(|error| PluginError::Session(error.to_string()))
}

/// The terminal a process's own run publishes: its record acquires every
/// stored attachment the output delivers before the registry records it
/// (ADR 0124 §4). An output whose source was already swept is published as
/// the typed source-gone failure instead.
pub async fn publish_process_terminal(
    attachments: &dyn AttachmentReferrers,
    process_id: &crate::ProcessId,
    output: ProcessAwaitOutput,
) -> Result<ProcessAwaitOutput, PluginError> {
    let ids = delivered_attachment_ids(&output);
    match acquire_under(attachments, &process_record_claim(process_id)?, &ids).await? {
        DeliveryAcquisition::Held => Ok(output),
        DeliveryAcquisition::SourceGone { digest } => Ok(source_gone_output(&digest)),
        DeliveryAcquisition::ReceiverEnded { referrer } => Err(receiver_ended_error(referrer)),
    }
}

/// An external or host completion's output: the record acquires what the
/// output delivers before the registry records it, and an output whose
/// source was already swept is refused with nothing recorded.
pub async fn acquire_completion_output(
    attachments: &dyn AttachmentReferrers,
    process_id: &crate::ProcessId,
    output: &ProcessAwaitOutput,
) -> Result<(), PluginError> {
    let ids = delivered_attachment_ids(output);
    match acquire_under(attachments, &process_record_claim(process_id)?, &ids).await? {
        DeliveryAcquisition::Held => Ok(()),
        DeliveryAcquisition::SourceGone { digest } => {
            Err(PluginError::ProcessOutputAttachmentUnavailable { digest })
        }
        DeliveryAcquisition::ReceiverEnded { referrer } => Err(receiver_ended_error(referrer)),
    }
}

/// The start input a registered process holds through its record
/// (ADR 0124 §4): acquired after the registration committed, since the
/// id is minted there. A missing upload refuses the step.
pub async fn acquire_start_input(
    attachments: &dyn AttachmentReferrers,
    record: &crate::ProcessRecord,
) -> Result<(), PluginError> {
    let ids = record.input.stored_attachment_ids();
    match acquire_under(attachments, &process_record_claim(&record.id)?, &ids).await? {
        DeliveryAcquisition::Held => Ok(()),
        DeliveryAcquisition::SourceGone { digest } => Err(PluginError::Session(format!(
            "process `{}` start input names attachment `{digest}`, which has no upload evidence",
            record.id
        ))),
        DeliveryAcquisition::ReceiverEnded { referrer } => Err(receiver_ended_error(referrer)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_process_receives_through_its_record_and_a_turn_through_its_journal() {
        let process = crate::ProcessId::fixture("receiver");
        let claim = receiving_claim(&ExecutionScope::Process {
            process_id: process.clone(),
        })
        .expect("a process scope claims through its record");
        assert_eq!(claim.referrer(), &ArtifactReferrer::ProcessRecord(process));

        let turn = ExecutionScope::turn("session", "turn");
        let claim = receiving_claim(&turn).expect("a turn scope claims through its journal");
        assert_eq!(
            claim.referrer(),
            &ArtifactReferrer::Execution(turn.journal_identity().expect("a valid turn scope"))
        );
    }

    #[test]
    fn only_a_settled_output_delivers_attachments() {
        let abandoned = ProcessAwaitOutput::NoLongerRetained {
            terminal_label: "completed".to_string(),
            pruned_at_ms: 1,
        };
        assert!(delivered_attachment_ids(&abandoned).is_empty());
    }
}

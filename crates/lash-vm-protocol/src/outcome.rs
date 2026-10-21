//! Worker testimony for neutral infrastructure outcomes.
use lash_protocol_types::engine_fault::{InfrastructureOutcome, WorkerRefusal};
pub fn testimony(outcome: InfrastructureOutcome) -> Option<crate::WorkerMessage> {
    match outcome {
        InfrastructureOutcome::WorkerLimitExceeded { limit } => {
            Some(crate::WorkerMessage::LimitExceeded { limit })
        }
        InfrastructureOutcome::ProtocolViolation { breach } => {
            Some(crate::WorkerMessage::Refused {
                refusal: WorkerRefusal::Breach(breach),
            })
        }
        InfrastructureOutcome::RunRefused { refusal } => Some(crate::WorkerMessage::Refused {
            refusal: WorkerRefusal::Run(refusal),
        }),
        InfrastructureOutcome::HeapFault { fault } => Some(crate::WorkerMessage::Refused {
            refusal: WorkerRefusal::Heap(fault),
        }),
        // The page service's refusal is the parent's own evidence.
        InfrastructureOutcome::WorkerDeployment { .. }
        | InfrastructureOutcome::WorkerCrashed { .. }
        | InfrastructureOutcome::RunCrashedWorker { .. }
        | InfrastructureOutcome::WorkerUnresponsive { .. }
        | InfrastructureOutcome::PageRefused { .. } => None,
    }
}

#[cfg(test)]
mod laws {
    use super::*;
    use crate::{DecodeLimits, FrameCodec, MessageFence, WorkerFrame, WorkerMessage};
    use lash_protocol_types::engine_fault::{
        Detail, DetailLimit, ExecutionLease, FrameEpoch, OwnerEpoch, ProtocolBreach, RunRefusal,
        SequenceFault, SupervisorEvidence,
    };

    /// FIG-4645: the worker may testify to its own typed cause; bounded
    /// diagnostics preserve retry classification and cannot forge supervisor facts.
    #[test]
    fn worker_testimony_preserves_typed_causes_under_the_diagnostic_bound() {
        for outcome in [
            InfrastructureOutcome::WorkerCrashed {
                evidence: SupervisorEvidence::Exited { code: 0 },
            },
            InfrastructureOutcome::WorkerUnresponsive { silent_ms: 1 },
        ] {
            assert!(testimony(outcome).is_none());
        }
        let codec = FrameCodec::new(DecodeLimits::standard())
            .with_detail_limit(DetailLimit::new(3).unwrap());
        for (outcome, retryable) in [
            (
                RunRefusal::Start {
                    detail: Detail::new("ééé"),
                }
                .into(),
                false,
            ),
            (
                ProtocolBreach::Machine {
                    detail: Detail::new("ééé"),
                }
                .into(),
                true,
            ),
            (
                ProtocolBreach::from(SequenceFault::WrongReadId).into(),
                true,
            ),
        ] {
            let message = testimony(outcome).expect("worker-owned cause");
            let frame = WorkerFrame {
                header: MessageFence::new(ExecutionLease(1), OwnerEpoch(1), FrameEpoch(1))
                    .next_header(),
                message,
            };
            let bytes = codec.encode_worker(&frame).unwrap();
            let WorkerMessage::Refused { refusal } = codec.decode_worker(&bytes).unwrap().message
            else {
                panic!("typed refusal");
            };
            let outcome = refusal.into_outcome();
            assert_eq!(outcome.is_retryable(), retryable);
            match outcome {
                InfrastructureOutcome::RunRefused {
                    refusal: RunRefusal::Start { detail },
                }
                | InfrastructureOutcome::ProtocolViolation {
                    breach: ProtocolBreach::Machine { detail },
                } => assert_eq!(detail.as_str(), "é"),
                InfrastructureOutcome::ProtocolViolation {
                    breach:
                        ProtocolBreach::Sequence {
                            fault: SequenceFault::WrongReadId,
                        },
                } => {}
                other => panic!("cause changed: {other:?}"),
            }
        }
        let forged = serde_json::json!({"refused": {"refusal": {"worker_crashed": {"evidence": "end_of_stream"}}}});
        assert!(serde_json::from_value::<WorkerMessage>(forged).is_err());
    }
}

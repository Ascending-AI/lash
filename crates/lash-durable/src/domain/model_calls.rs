//! Model call records owned by the same execution as their prompt snapshots.
use super::PromptCallKey;
use lash_core_store::checkpoint_graph::NodeId;
use lash_protocol_types::llm::types::LlmCallId;

/// Publish the one immutable sealed fact in the call's existing settling commit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelCallWrite {
    pub key: PromptCallKey,
    pub call_id: LlmCallId,
    pub record: NodeId,
}

/// The existing opaque outcome that must commit a nested call's sealed fact.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelCallOwner {
    pub owner: super::OwnerKey,
    pub run: super::RunSeq,
    pub call: lash_protocol_types::ToolCallId,
}

/// Dispatch reservations, distinct from sealed records. An accepted tail
/// retains its identity here after its answer receipt has been consumed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelCallPinRow {
    pub key: PromptCallKey,
    pub call_id: LlmCallId,
    pub attempt: u32,
    pub answered: bool,
    pub enclosing: Option<ModelCallOwner>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ModelCallPinWrite {
    Reserve {
        key: PromptCallKey,
        call_id: LlmCallId,
        attempt: u32,
        enclosing: Option<ModelCallOwner>,
    },
    Answered {
        key: PromptCallKey,
    },
}

impl ModelCallPinRow {
    /// All dispatches whose observations died with the owner stay visible.
    pub fn unobserved(&self) -> lash_core_store::model_calls::ModelCallFact {
        use lash_core_store::model_calls::{ModelCallEnd, ModelCallEndReason, ModelCallFact};
        use lash_protocol_types::{
            llm::types::AttemptOutcome, llm::types::AttemptRecord, llm::types::LlmCallRecord,
            llm::types::ProtocolPosition,
        };

        let call = match self.key.call {
            super::ModelCallId::Turn { ordinal, .. } => ordinal,
            super::ModelCallId::Owned { .. } => 0,
        };
        ModelCallFact {
            call,
            call_id: self.call_id.clone(),
            end: if self.answered {
                ModelCallEnd::Answered
            } else {
                ModelCallEnd::Unanswered {
                    reason: ModelCallEndReason::LostWithProcess,
                }
            },
            record: LlmCallRecord {
                call_id: self.call_id.clone(),
                label: None,
                replay_drops: Vec::new(),
                attempts: (1..=self.attempt)
                    .map(|ordinal| AttemptRecord {
                        ordinal,
                        outcome: AttemptOutcome::Interrupted,
                        protocol_position: ProtocolPosition::Unobserved,
                        retry_budget_consumed: false,
                        retry_decision: None,
                        error: None,
                        evidence: None,
                        generation_disposition: None,
                        usage: None,
                    })
                    .collect(),
            },
        }
    }
}

/// Decode a reservation's keys without interpreting opaque call ids.
pub fn pin_row(
    session: &lash_vocabulary::identity::SessionId,
    owner: &str,
    call: String,
    call_id: String,
    attempt: i64,
    answered: bool,
    enclosing: (Option<String>, Option<i64>, Option<String>),
) -> Result<ModelCallPinRow, crate::DurableError> {
    let invalid = || {
        crate::DurableError::Store(crate::StoreFailure {
            kind: crate::StoreFailureKind::Corrupt,
            message: "invalid model call reservation".into(),
        })
    };
    let address = if let Some(run) = owner.strip_prefix("turn:") {
        super::ModelCallId::Turn {
            run: lash_vocabulary::identity::TurnId::try_from(run.to_owned())
                .map_err(|_| invalid())?,
            ordinal: call.parse().map_err(|_| invalid())?,
        }
    } else {
        super::ModelCallId::Owned {
            owner: owner.strip_prefix("owned:").ok_or_else(invalid)?.to_owned(),
            key: call,
        }
    };
    Ok(ModelCallPinRow {
        key: PromptCallKey {
            session: session.clone(),
            call: address,
        },
        call_id: LlmCallId(call_id),
        attempt: u32::try_from(attempt).map_err(|_| invalid())?,
        answered,
        enclosing: match enclosing {
            (None, None, None) => None,
            (Some(owner), Some(run), Some(call)) => Some(ModelCallOwner {
                owner: super::OwnerKey::parse(&owner).map_err(|_| invalid())?,
                run: super::RunSeq(u64::try_from(run).map_err(|_| invalid())?),
                call: lash_protocol_types::ToolCallId::parse(&call).map_err(|_| invalid())?,
            }),
            _ => return Err(invalid()),
        },
    })
}

/// A sealed nested call waiting for its enclosing outcome or command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OwnedModelCall {
    pub key: PromptCallKey,
    pub fact: lash_core_store::model_calls::ModelCallFact,
}
impl OwnedModelCall {
    pub fn stage(&self, tx: &mut crate::ActorTx) -> Result<(), crate::DurableError> {
        use lash_core_store::checkpoint_graph::{EncodeBudget, ExplicitLimit, GraphStaging};
        let mut staging = GraphStaging::new(EncodeBudget::new(ExplicitLimit::new(u64::MAX)));
        let record = self.fact.stage(&mut staging)?;
        tx.stage_all(staging);
        tx.write(super::DomainWrite::ModelCall(ModelCallWrite {
            key: self.key.clone(),
            call_id: self.fact.call_id.clone(),
            record,
        }));
        Ok(())
    }
}

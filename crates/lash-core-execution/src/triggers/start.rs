//! The rows one trigger start commits (ADR 0132 §12).
//!
//! The router prepares every delivery's process before the transaction and
//! hands the store these rows, encoded, inside one `trigger.start` mailbox
//! commit. The store records the occurrence, checks that the subscriptions
//! it matches are still the planned ones, registers each prepared process
//! with its actor ready and records each started binding or terminal refusal.
//! A crash before the commit leaves nothing; after it, each delivery has its
//! first disposition.

use serde::{Deserialize, Serialize};

use super::{TriggerOccurrenceRecord, TriggerSubscriptionRecord};
use crate::plugin::PluginError;
use crate::{ProcessId, ProcessRegistration, SessionId};

/// One occurrence's start, as its `trigger.start` commit carries it.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TriggerStartRows {
    /// The occurrence, stamped by its plan.
    pub occurrence: TriggerOccurrenceRecord,
    /// Every enabled subscription the plan matched, started or refused: the
    /// commit refuses when the store matches any other set.
    pub planned: Vec<TriggerSubscriptionFence>,
    /// Every started or refused delivery, in delivery order.
    pub deliveries: Vec<TriggerDeliveryStartRows>,
}

/// One subscription revision an occurrence's plan matched.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TriggerSubscriptionFence {
    pub subscription_id: String,
    pub incarnation: String,
    pub revision: u64,
}

impl TriggerSubscriptionFence {
    /// The revision `subscription` stands at.
    #[must_use]
    pub fn of(subscription: &TriggerSubscriptionRecord) -> Self {
        Self {
            subscription_id: subscription.subscription_id.clone(),
            incarnation: subscription.incarnation.clone(),
            revision: subscription.revision,
        }
    }
}

/// One delivery's disposition: the subscription snapshot and either the
/// prepared process registration or the terminal refusal that prevents it.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum TriggerDeliveryStartRows {
    Started {
        subscription: TriggerSubscriptionRecord,
        registration: Box<ProcessRegistration>,
        observers: Vec<SessionId>,
        process_id: ProcessId,
    },
    Refused {
        subscription: TriggerSubscriptionRecord,
        code: crate::RuntimeErrorCode,
        reason: String,
        value_mismatch: Option<Box<lash_sansio::ValueMismatch>>,
    },
}

impl TriggerDeliveryStartRows {
    /// The subscription snapshot this disposition belongs to.
    #[must_use]
    pub fn subscription(&self) -> &TriggerSubscriptionRecord {
        match self {
            Self::Started { subscription, .. } | Self::Refused { subscription, .. } => subscription,
        }
    }

    /// The durable receipt of this delivery.
    #[must_use]
    pub fn outcome(&self) -> super::TriggerDeliveryEmitOutcome {
        match self {
            Self::Started { process_id, .. } => super::TriggerDeliveryEmitOutcome::Started {
                process_id: process_id.clone(),
            },
            Self::Refused {
                code,
                reason,
                value_mismatch,
                ..
            } => super::TriggerDeliveryEmitOutcome::Failed {
                code: code.clone(),
                reason: reason.clone(),
                value_mismatch: value_mismatch.clone(),
            },
        }
    }
}

impl TriggerStartRows {
    /// The rows as the `trigger.start` commit carries them.
    ///
    /// # Errors
    ///
    /// A row that does not encode.
    pub fn encode(&self) -> Result<String, PluginError> {
        serde_json::to_string(self).map_err(|error| {
            PluginError::Session(format!("failed to encode a trigger start: {error}"))
        })
    }

    /// The rows a `trigger.start` commit carried.
    ///
    /// # Errors
    ///
    /// [`PluginError::StoredDataCorrupt`] for rows that do not decode.
    pub fn decode(json: &str) -> Result<Self, PluginError> {
        serde_json::from_str(json).map_err(|error| PluginError::StoredDataCorrupt {
            record_kind: "TriggerStart".to_string(),
            message: error.to_string(),
        })
    }

    /// The mailbox transaction that commits these rows.
    ///
    /// # Errors
    ///
    /// A row that does not encode.
    pub fn mail_tx(&self) -> Result<lash_durable::MailTx, PluginError> {
        let mut tx = lash_durable::MailTx::new();
        tx.write(lash_durable::MailDomainWrite::StartTrigger(
            lash_durable::domain::TriggerStart {
                occurrence_id: self.occurrence.occurrence_id.clone(),
                start_json: self.encode()?,
            },
        ));
        Ok(tx)
    }

    /// What the `trigger.start` commit of these rows answered: the process
    /// each delivery started, in delivery order; or `None` when the plan no
    /// longer held (the occurrence was recorded, or the subscriptions it
    /// matches moved, since the plan read them), so nothing committed and
    /// the occurrence is planned again.
    ///
    /// # Errors
    ///
    /// The refusal of a reclaimed occurrence; any other refusal or store
    /// failure, after which nothing committed or the commit is unknown and the
    /// emission runs again.
    pub fn answer(
        &self,
        committed: Result<lash_durable::MailCommit, lash_durable::DurableError>,
    ) -> Result<Option<Vec<ProcessId>>, PluginError> {
        use lash_durable::{DomainRefusal, DurableError};
        match committed {
            Ok(commit) => match commit.answers.as_slice() {
                [lash_durable::MailAnswer::StartTrigger(answer)]
                    if answer.processes
                        == self
                            .deliveries
                            .iter()
                            .filter_map(|delivery| match delivery {
                                TriggerDeliveryStartRows::Started { process_id, .. } => {
                                    Some(process_id.clone())
                                }
                                TriggerDeliveryStartRows::Refused { .. } => None,
                            })
                            .collect::<Vec<_>>() =>
                {
                    Ok(Some(answer.processes.clone()))
                }
                answers => Err(PluginError::Session(format!(
                    "trigger occurrence `{}` started with an unexpected answer: {answers:?}",
                    self.occurrence.occurrence_id
                ))),
            },
            Err(DurableError::Domain(
                DomainRefusal::TriggerOccurrenceHeld { .. }
                | DomainRefusal::TriggerSubscriptionsMoved { .. },
            )) => Ok(None),
            Err(DurableError::Domain(DomainRefusal::TriggerOccurrenceReclaimed { occurrence })) => {
                Err(crate::trigger_occurrence_reclaimed(&occurrence))
            }
            Err(error) => Err(start_failure(error)),
        }
    }

    /// The same row set used to build fresh and held emission receipts.
    #[must_use]
    pub fn reservations(&self) -> Vec<super::TriggerDeliveryReservation> {
        self.deliveries
            .iter()
            .map(|delivery| super::TriggerDeliveryReservation {
                occurrence: self.occurrence.clone(),
                subscription: delivery.subscription().clone(),
                outcome: delivery.outcome(),
                created_at_ms: self.occurrence.occurred_at_ms,
            })
            .collect()
    }

    /// Whether `matched`, the enabled subscriptions the store matches the
    /// occurrence with now, are exactly the ones the plan matched.
    #[must_use]
    pub fn plan_holds(&self, matched: &[TriggerSubscriptionRecord]) -> bool {
        let mut planned = self.planned.clone();
        planned.sort();
        let mut now = matched
            .iter()
            .map(TriggerSubscriptionFence::of)
            .collect::<Vec<_>>();
        now.sort();
        planned == now
    }
}

/// The error a start that did not commit, or whose commit is unknown, raises.
fn start_failure(error: lash_durable::DurableError) -> PluginError {
    use lash_durable::{DomainRefusal, DurableError, StoreFailure, StoreFailureKind};
    match error {
        DurableError::Store(StoreFailure {
            kind: StoreFailureKind::Contended,
            ..
        }) => PluginError::StoreUnavailable {
            fault: crate::store::StoreFault::Contended,
        },
        DurableError::Store(StoreFailure {
            kind: StoreFailureKind::Corrupt,
            message,
        }) => PluginError::StoredDataCorrupt {
            record_kind: "TriggerStart".to_string(),
            message,
        },
        DurableError::Domain(DomainRefusal::TriggerStartRefused { reason, .. }) => {
            PluginError::Session(reason)
        }
        // An unavailable store, a retired writer, or an acknowledgement lost
        // after the commit: the emission runs again, and finds the occurrence
        // held when it did commit.
        error => PluginError::StoreUnavailable {
            fault: crate::store::StoreFault::Backend {
                message: error.to_string(),
            },
        },
    }
}

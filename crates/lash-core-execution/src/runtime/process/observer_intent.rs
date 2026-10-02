use super::{ProcessObserverBy, ProcessRegistry};
use crate::SessionId;
use crate::store::{RuntimeStore, StoreError};
use crate::{SessionObservedProcessOutcome, SessionObservedProcessReceipt, SessionObserverIntent};

/// Source of the relation whose process-observer intents must be settled.
pub enum SessionObserverIntentSource<'a> {
    /// Load a required durable relation, then persist it once at the end.
    Persisted(&'a dyn RuntimeStore),
    /// Settle a durable relation when metadata already exists.
    ///
    /// Opening a brand-new store has no relation to reconcile yet, so missing
    /// metadata is a no-op on that path.
    PersistedIfPresent(&'a dyn RuntimeStore),
    /// Settle in-memory intents that have no persistence recovery path.
    Unstored(Vec<SessionObserverIntent>),
}

/// Publish pending process-observer intents and clear them after settlement.
///
/// A retryable publication retains the whole unresolved selector, including
/// observers already published, until every publication succeeds. Missing or
/// pruned processes settle separately with their typed outcomes. A failed
/// metadata clear leaves the durable selector available for wholesale replay.
pub async fn reconcile_session_process_observer_intents(
    process_registry: Option<&dyn ProcessRegistry>,
    session_id: &SessionId,
    source: SessionObserverIntentSource<'_>,
) -> Result<Vec<SessionObservedProcessReceipt>, StoreError> {
    let (pending_observer_intents, persisted) = match source {
        SessionObserverIntentSource::Persisted(store) => {
            let mut meta = store.load_session_meta(session_id).await?.ok_or_else(|| {
                StoreError::SessionNotFound {
                    session_id: session_id.clone(),
                }
            })?;
            let pending = std::mem::take(&mut meta.pending_observer_intents);
            (pending, Some(store))
        }
        SessionObserverIntentSource::PersistedIfPresent(store) => {
            let Some(mut meta) = store.load_session_meta(session_id).await? else {
                return Ok(Vec::new());
            };
            let pending = std::mem::take(&mut meta.pending_observer_intents);
            (pending, Some(store))
        }
        SessionObserverIntentSource::Unstored(intents) => (intents, None),
    };
    if pending_observer_intents.is_empty() {
        return Ok(Vec::new());
    }

    let results =
        apply_process_observers(process_registry, session_id, &pending_observer_intents).await;

    if let Some(store) = persisted {
        let mut remaining = Vec::new();
        if results.iter().any(|receipt| {
            matches!(
                receipt.outcome,
                SessionObservedProcessOutcome::Unavailable { .. }
            )
        }) {
            remaining = pending_observer_intents
                .into_iter()
                .zip(&results)
                .filter_map(|(intent, receipt)| match receipt.outcome {
                    SessionObservedProcessOutcome::NotFound
                    | SessionObservedProcessOutcome::NoLongerRetained { .. } => None,
                    SessionObservedProcessOutcome::Observed
                    | SessionObservedProcessOutcome::Unavailable { .. } => Some(intent),
                })
                .collect();
        }
        store.settle_observer_intents(session_id, remaining).await?;
    }

    Ok(results)
}

async fn apply_process_observers(
    process_registry: Option<&dyn ProcessRegistry>,
    session_id: &SessionId,
    intents: &[SessionObserverIntent],
) -> Vec<SessionObservedProcessReceipt> {
    let mut results = Vec::with_capacity(intents.len());
    for intent in intents {
        let observer_by = ProcessObserverBy::host(format!("session-create:{session_id}"));
        let outcome =
            apply_process_observer(process_registry, session_id, intent, observer_by).await;
        results.push(SessionObservedProcessReceipt {
            process_id: intent.process_id.clone(),
            outcome,
        });
    }
    results
}

async fn apply_process_observer(
    process_registry: Option<&dyn ProcessRegistry>,
    session_id: &SessionId,
    intent: &SessionObserverIntent,
    observer_by: ProcessObserverBy,
) -> SessionObservedProcessOutcome {
    let Some(process_registry) = process_registry else {
        return SessionObservedProcessOutcome::Unavailable {
            message: "process registry is unavailable in this runtime".to_string(),
        };
    };

    match process_registry.get_process(&intent.process_id).await {
        Ok(Some(_)) => {}
        Ok(None) => return SessionObservedProcessOutcome::NotFound,
        Err(crate::PluginError::ProcessNoLongerRetained {
            terminal_label,
            pruned_at_ms,
        }) => {
            return SessionObservedProcessOutcome::NoLongerRetained {
                terminal_label,
                pruned_at_ms,
            };
        }
        Err(error) => {
            return SessionObservedProcessOutcome::Unavailable {
                message: error.to_string(),
            };
        }
    }

    match process_registry
        .add_observer(session_id, &intent.process_id, observer_by)
        .await
    {
        Ok(()) => SessionObservedProcessOutcome::Observed,
        Err(crate::PluginError::ProcessNoLongerRetained {
            terminal_label,
            pruned_at_ms,
        }) => SessionObservedProcessOutcome::NoLongerRetained {
            terminal_label,
            pruned_at_ms,
        },
        Err(apply_error) => {
            // A process may disappear between the point read and the
            // replay-keyed observer append. Re-read to preserve the most
            // specific typed outcome without hiding an unrelated apply error.
            match process_registry.get_process(&intent.process_id).await {
                Ok(None) => SessionObservedProcessOutcome::NotFound,
                Err(crate::PluginError::ProcessNoLongerRetained {
                    terminal_label,
                    pruned_at_ms,
                }) => SessionObservedProcessOutcome::NoLongerRetained {
                    terminal_label,
                    pruned_at_ms,
                },
                Ok(Some(_)) | Err(_) => SessionObservedProcessOutcome::Unavailable {
                    message: apply_error.to_string(),
                },
            }
        }
    }
}

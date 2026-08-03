use super::{ProcessObserverBy, ProcessRegistry};
use crate::store::{RuntimePersistence, StoreError};
use crate::{SessionObservedProcessOutcome, SessionObservedProcessResult, SessionRelation};

/// Source of the relation whose process-observer intents must be settled.
pub enum SessionObserverIntentSource<'a> {
    /// Load a required durable relation, then persist it once at the end.
    Persisted(&'a dyn RuntimePersistence),
    /// Settle a durable relation when metadata already exists.
    ///
    /// Opening a brand-new store has no relation to reconcile yet, so missing
    /// metadata is a no-op on that path.
    PersistedIfPresent(&'a dyn RuntimePersistence),
    /// Settle an in-memory relation that has no persistence recovery path.
    Unstored(SessionRelation),
}

/// Publish and consume every pending process-observer layer for a session.
///
/// Observer publication is best effort per process. The returned results cover
/// `ObserverIntent` layers (the ids explicitly requested at session creation);
/// fork-inheritance outcomes are consumed without changing session creation's
/// public result shape. Unknown, pruned, or temporarily unavailable processes
/// never prevent the relation from reaching its fully settled base form. This
/// is deliberate: hosts can add an observer again after a transient failure,
/// while retaining an intent would make settlement behavior depend on which
/// session-creation path happened to publish it.
pub async fn reconcile_session_process_observer_intents(
    process_registry: Option<&dyn ProcessRegistry>,
    session_id: &str,
    source: SessionObserverIntentSource<'_>,
) -> Result<Vec<SessionObservedProcessResult>, StoreError> {
    let (mut relation, persisted) = match source {
        SessionObserverIntentSource::Persisted(store) => {
            let meta = store.load_session_meta().await?.ok_or_else(|| {
                StoreError::Backend(format!(
                    "session `{session_id}` has no metadata for observer intent settlement"
                ))
            })?;
            (meta.relation.clone(), Some((store, meta)))
        }
        SessionObserverIntentSource::PersistedIfPresent(store) => {
            let Some(meta) = store.load_session_meta().await? else {
                return Ok(Vec::new());
            };
            (meta.relation.clone(), Some((store, meta)))
        }
        SessionObserverIntentSource::Unstored(relation) => (relation, None),
    };
    let mut create_observer_results = Vec::new();
    let mut settled_layer = false;

    loop {
        if matches!(relation, SessionRelation::ObserverIntent { .. }) {
            let SessionRelation::ObserverIntent {
                relation: inner,
                pending_observer_process_ids,
            } = relation
            else {
                unreachable!("relation variant was checked above");
            };
            create_observer_results.extend(
                apply_process_observers(
                    process_registry,
                    session_id,
                    &pending_observer_process_ids,
                    ProcessObserverBy::host(format!("session-create:{session_id}")),
                )
                .await,
            );
            relation = *inner;
            settled_layer = true;
            continue;
        }

        let SessionRelation::Fork {
            pending_observer_process_ids,
            ..
        } = &mut relation
        else {
            break;
        };
        if pending_observer_process_ids.is_empty() {
            break;
        }
        let pending_observer_process_ids = std::mem::take(pending_observer_process_ids);
        let _fork_observer_results = apply_process_observers(
            process_registry,
            session_id,
            &pending_observer_process_ids,
            ProcessObserverBy::ForkInheritance,
        )
        .await;
        settled_layer = true;
    }

    if settled_layer && let Some((store, mut meta)) = persisted {
        meta.relation = relation;
        store.save_session_meta(meta).await?;
    }

    Ok(create_observer_results)
}

async fn apply_process_observers(
    process_registry: Option<&dyn ProcessRegistry>,
    session_id: &str,
    process_ids: &[crate::ProcessId],
    observer_by: ProcessObserverBy,
) -> Vec<SessionObservedProcessResult> {
    let mut results = Vec::with_capacity(process_ids.len());
    for process_id in process_ids {
        let outcome = apply_process_observer(
            process_registry,
            session_id,
            process_id,
            observer_by.clone(),
        )
        .await;
        results.push(SessionObservedProcessResult {
            process_id: process_id.clone(),
            outcome,
        });
    }
    results
}

async fn apply_process_observer(
    process_registry: Option<&dyn ProcessRegistry>,
    session_id: &str,
    process_id: &str,
    observer_by: ProcessObserverBy,
) -> SessionObservedProcessOutcome {
    let Some(process_registry) = process_registry else {
        return SessionObservedProcessOutcome::Unavailable {
            message: "process registry is unavailable in this runtime".to_string(),
        };
    };

    match process_registry.get_process(process_id).await {
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
        .add_observer(session_id, process_id, observer_by)
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
            match process_registry.get_process(process_id).await {
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

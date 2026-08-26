use super::{ProcessObserverBy, ProcessRegistry};
use crate::store::{RuntimePersistence, StoreError};
use crate::{
    SessionObservedProcessOutcome, SessionObservedProcessReceipt, SessionObserverIntent,
    SessionObserverIntentAttribution,
};

/// Source of the relation whose process-observer intents must be settled.
pub enum SessionObserverIntentSource<'a> {
    /// Load a required durable relation, then persist it once at the end.
    Persisted(&'a dyn RuntimePersistence),
    /// Settle a durable relation when metadata already exists.
    ///
    /// Opening a brand-new store has no relation to reconcile yet, so missing
    /// metadata is a no-op on that path.
    PersistedIfPresent(&'a dyn RuntimePersistence),
    /// Settle in-memory intents that have no persistence recovery path.
    Unstored(Vec<SessionObserverIntent>),
}

/// Publish and consume every pending process-observer intent for a session.
///
/// Observer publication is best effort per process. The returned results cover
/// both host-requested and fork-inherited intents and preserve that attribution.
/// Unknown, pruned, or temporarily unavailable processes never prevent the
/// durable intent set from reaching its fully settled empty form. This is
/// deliberate: hosts can add an observer again after a transient failure,
/// while retaining an intent would make settlement behavior depend on which
/// session-creation path happened to publish it.
pub async fn reconcile_session_process_observer_intents(
    process_registry: Option<&dyn ProcessRegistry>,
    session_id: &str,
    source: SessionObserverIntentSource<'_>,
) -> Result<Vec<SessionObservedProcessReceipt>, StoreError> {
    let (pending_observer_intents, persisted) = match source {
        SessionObserverIntentSource::Persisted(store) => {
            let mut meta = store.load_session_meta().await?.ok_or_else(|| {
                StoreError::Backend(format!(
                    "session `{session_id}` has no metadata for observer intent settlement"
                ))
            })?;
            let pending = std::mem::take(&mut meta.pending_observer_intents);
            (pending, Some((store, meta)))
        }
        SessionObserverIntentSource::PersistedIfPresent(store) => {
            let Some(mut meta) = store.load_session_meta().await? else {
                return Ok(Vec::new());
            };
            let pending = std::mem::take(&mut meta.pending_observer_intents);
            (pending, Some((store, meta)))
        }
        SessionObserverIntentSource::Unstored(intents) => (intents, None),
    };
    if pending_observer_intents.is_empty() {
        return Ok(Vec::new());
    }

    let results =
        apply_process_observers(process_registry, session_id, &pending_observer_intents).await;

    if let Some((store, meta)) = persisted {
        store.save_session_meta(meta).await?;
    }

    Ok(results)
}

async fn apply_process_observers(
    process_registry: Option<&dyn ProcessRegistry>,
    session_id: &str,
    intents: &[SessionObserverIntent],
) -> Vec<SessionObservedProcessReceipt> {
    let mut results = Vec::with_capacity(intents.len());
    for intent in intents {
        let observer_by = match intent.attribution {
            SessionObserverIntentAttribution::HostRequested => {
                ProcessObserverBy::host(format!("session-create:{session_id}"))
            }
            SessionObserverIntentAttribution::ForkInherited => ProcessObserverBy::ForkInheritance,
        };
        let outcome = apply_process_observer(
            process_registry,
            session_id,
            &intent.process_id,
            observer_by,
        )
        .await;
        results.push(SessionObservedProcessReceipt {
            process_id: intent.process_id.clone(),
            attribution: intent.attribution,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn noproc_receipts_preserve_both_attributions() {
        let registry = crate::TestLocalProcessRegistry::default();
        registry
            .register_process(crate::ProcessRegistration::new(
                "pruned-process",
                crate::ProcessInput::External {
                    metadata: serde_json::Value::Null,
                },
                crate::RecoveryContract::ExternallyOwned,
                crate::ProcessProvenance::host(),
            ))
            .await
            .expect("register process before pruning");
        let pruned = registry
            .complete_process(
                "pruned-process",
                crate::ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::success(
                    serde_json::Value::Null,
                )),
                crate::ProcessCompletionAuthority::external_owner(),
            )
            .await
            .expect("complete process before pruning");
        registry
            .prune_terminal_processes(
                pruned.updated_at_ms.saturating_add(1),
                None,
                crate::ProjectionWatermark::NoProjector,
            )
            .await
            .expect("prune terminal process");

        let receipts = reconcile_session_process_observer_intents(
            Some(&registry),
            "noproc-session",
            SessionObserverIntentSource::Unstored(vec![
                SessionObserverIntent::host_requested("unknown-host"),
                SessionObserverIntent::fork_inherited("unknown-fork"),
                SessionObserverIntent::host_requested("pruned-process"),
                SessionObserverIntent::fork_inherited("pruned-process"),
            ]),
        )
        .await
        .expect("noproc settlement remains best effort");

        assert_eq!(receipts.len(), 4);
        assert_eq!(
            receipts
                .iter()
                .map(|receipt| receipt.attribution)
                .collect::<Vec<_>>(),
            vec![
                SessionObserverIntentAttribution::HostRequested,
                SessionObserverIntentAttribution::ForkInherited,
                SessionObserverIntentAttribution::HostRequested,
                SessionObserverIntentAttribution::ForkInherited,
            ]
        );
        assert!(
            receipts[..2]
                .iter()
                .all(|receipt| receipt.outcome == SessionObservedProcessOutcome::NotFound)
        );
        assert!(receipts[2..].iter().all(|receipt| matches!(
            receipt.outcome,
            SessionObservedProcessOutcome::NoLongerRetained { .. }
        )));
    }
}

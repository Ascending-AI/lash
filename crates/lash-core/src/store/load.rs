use super::*;

fn persisted_session_state_from_read(
    read: PersistedSessionRead,
) -> Result<crate::RuntimeSessionState, StoreError> {
    persisted_session_state_from_head(
        SessionHead {
            session_id: read.session_id,
            head_revision: read.head_revision,
            current_frame_node_id: read.current_frame_node_id,
            graph: read.graph,
            config: read.config,
            checkpoint_ref: read.checkpoint_ref,
            token_ledger: read.token_ledger,
        },
        read.checkpoint,
    )
}

/// Presence-aware durable session load used by the facade's reopen authority
/// reconciliation.
#[doc(hidden)]
pub struct LoadedPersistedSession {
    pub state: crate::RuntimeSessionState,
    pub config: crate::PersistedSessionConfig,
}

#[doc(hidden)]
pub async fn load_persisted_session(
    store: &(dyn RuntimePersistence + '_),
) -> Result<Option<LoadedPersistedSession>, StoreError> {
    let read = store.load_session().await?;
    let Some(read) = read else {
        return Ok(None);
    };
    // Defend against third-party stores that do not use SessionGraph::from_nodes after reading.
    read.graph.validate_resident_integrity()?;
    store.load_session_meta().await?.ok_or_else(|| {
        StoreError::Backend(format!(
            "session `{}` has durable head state but no session metadata",
            read.session_id
        ))
    })?;
    let config = read.config.clone();
    Ok(Some(LoadedPersistedSession {
        state: persisted_session_state_from_read(read)?,
        config,
    }))
}

/// Recover a session only after completing lease-fenced state admission.
#[doc(hidden)]
pub async fn load_persisted_session_admitted(
    store: &(dyn RuntimePersistence + '_),
    session_id: &str,
    owner: &crate::LeaseOwnerIdentity,
    executor_id: &str,
    lease_ttl_ms: u64,
) -> Result<Option<LoadedPersistedSession>, StoreError> {
    let acquisition = store
        .try_claim_session_execution_lease(session_id, owner, executor_id, lease_ttl_ms)
        .await?
        .acquisition()
        .ok_or(StoreError::Contended)?;
    crate::runtime::session_execution_lease::trace_acquisition(&acquisition);
    let lease = acquisition.lease;
    let fence = lease.fence();
    let result = async {
        store.admit_session_state(&fence).await?;
        load_persisted_session(store).await
    }
    .await;
    let release = store.release_session_execution_lease(&fence).await;
    match result {
        Err(error) => Err(error),
        Ok(loaded) => {
            release?;
            Ok(loaded)
        }
    }
}

pub async fn load_persisted_session_state(
    store: &(dyn RuntimePersistence + '_),
) -> Result<Option<crate::RuntimeSessionState>, StoreError> {
    Ok(load_persisted_session(store)
        .await?
        .map(|loaded| loaded.state))
}

pub async fn refresh_persisted_session_state(
    store: &(dyn RuntimePersistence + '_),
    state: &mut crate::RuntimeSessionState,
) -> Result<(), StoreError> {
    if let Some(mut fresh) = load_persisted_session_state(store).await? {
        fresh.policy.session_id = state.policy.session_id.clone();
        fresh.policy.turn_budget = state.policy.turn_budget;
        *state = fresh;
    }
    Ok(())
}

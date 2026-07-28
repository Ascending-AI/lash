use super::*;

fn persisted_session_state_from_read(
    read: PersistedSessionRead,
    incarnation_id: IncarnationId,
) -> crate::RuntimeSessionState {
    let mut state = persisted_session_state_from_head(
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
    );
    state.bind_durable_incarnation(incarnation_id);
    state
}

pub async fn load_persisted_session_state(
    store: &(dyn RuntimePersistence + '_),
) -> Result<Option<crate::RuntimeSessionState>, StoreError> {
    let read = store.load_session().await?;
    let Some(read) = read else {
        return Ok(None);
    };
    let meta = store.load_session_meta().await?.ok_or_else(|| {
        StoreError::Backend(format!(
            "session `{}` has durable head state but no session metadata",
            read.session_id
        ))
    })?;
    Ok(Some(persisted_session_state_from_read(
        read,
        meta.incarnation_id,
    )))
}

pub async fn refresh_persisted_session_state(
    store: &(dyn RuntimePersistence + '_),
    state: &mut crate::RuntimeSessionState,
) -> Result<(), StoreError> {
    if let Some(mut fresh) = load_persisted_session_state(store).await? {
        fresh.policy.session_id = state.policy.session_id.clone();
        fresh.policy.max_turns = state.policy.max_turns;
        *state = fresh;
    }
    Ok(())
}

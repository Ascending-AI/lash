use super::*;

pub(super) fn closed_root_cancel_prefix(root: &lash_core::TurnId) -> String {
    format!("turn/{}/{}:", root.as_str().len(), root.as_str())
}

fn belongs_to_closed_root(
    key: &AwaitEventKey,
    session_id: &SessionId,
    root: &lash_core::TurnId,
) -> bool {
    match &key.scope {
        ExecutionScope::Turn {
            session_id: key_session,
            turn_id,
        } => {
            key_session == session_id
                && lash_core::store::PhysicalTurn::split_turn_id(turn_id).0 == *root
        }
        _ => false,
    }
}

/// Whether `key` waits on the terminal of a physical turn the root's ending
/// commit covers: that turn committed, the ending one or a frame switch before
/// it, and its terminal is published one-way after its commit (FIG-4025).
fn owes_published_terminal(
    key: &AwaitEventKey,
    committed_turn: Option<&lash_core::TurnId>,
) -> bool {
    let (Some(committed_turn), AwaitEventWaitIdentity::TurnTerminal, Some(turn_id)) =
        (committed_turn, &key.wait, key.scope.turn_id())
    else {
        return false;
    };
    lash_core::store::PhysicalTurn::split_turn_id(turn_id).1
        <= lash_core::store::PhysicalTurn::split_turn_id(committed_turn).1
}

pub(super) async fn retire_root(
    ctx: ObjectContext<'_>,
    namespace: &crate::RestateNamespace,
    request: RestateDurableWaitRootRequest,
) -> HandlerResult<Json<()>> {
    if request.session_id.as_str() != ctx.key()
        || request.root.as_str().is_empty()
        || request.committed_turn.as_ref().is_some_and(|turn| {
            lash_core::store::PhysicalTurn::split_turn_id(turn).0 != request.root
        })
    {
        return Err(TerminalError::new(format!(
            "root retirement for session `{}`, root `{}` and committed turn {:?} addressed index {}",
            request.session_id,
            request.root,
            request.committed_turn,
            ctx.key()
        ))
        .into());
    }
    let mut metadata = load_durable_wait_index_metadata(&ctx).await?;
    for key in load_indexed_waits(&ctx)
        .await?
        .into_iter()
        .filter(|key| belongs_to_closed_root(key, &request.session_id, &request.root))
    {
        let address = RestateDurableWaitAddress::for_key(&key);
        if owes_published_terminal(&key, request.committed_turn.as_ref()) {
            // Its commit's terminal wins the promise, not a `Cancelled` from
            // here. A wait whose terminal has not landed stays registered: the
            // landing publish settles it, and session revocation still can.
            let Json(landed) = namespace
                .durable_wait_workflow(&ctx, address.workflow_key.clone())
                .peek()
                .header(LASH_REPLAY_KEY_HEADER.to_string(), key.key_id.clone())
                .call()
                .await?;
            if landed.is_some() {
                ctx.clear(&durable_wait_index_state_key(&address));
                ctx.clear(&durable_wait_index_resolution_key(&address));
            }
            continue;
        }
        if object_state::get_stamped::<Resolution>(
            &ctx,
            &durable_wait_index_resolution_key(&address),
            &DURABLE_WAIT_REGISTRY_FORMATS,
        )
        .await?
        .is_none()
        {
            resolve_indexed_waits(&ctx, namespace, vec![key.clone()], false).await?;
        }
        ctx.clear(&durable_wait_index_state_key(&address));
        ctx.clear(&durable_wait_index_resolution_key(&address));
    }

    let before = metadata.cancel_decided.len() + metadata.awakeables.len();
    let prefix = closed_root_cancel_prefix(&request.root);
    metadata
        .cancel_decided
        .retain(|id| !id.starts_with(&prefix));
    let mut retained = Vec::with_capacity(metadata.awakeables.len());
    for entry in std::mem::take(&mut metadata.awakeables) {
        if belongs_to_closed_root(&entry.key, &request.session_id, &request.root) {
            resolve_durable_wait_awakeable(&ctx, &entry, &Resolution::Cancelled);
        } else {
            retained.push(entry);
        }
    }
    metadata.awakeables = retained;
    if metadata.cancel_decided.len() + metadata.awakeables.len() != before {
        object_state::set_stamped(
            &ctx,
            DURABLE_WAIT_INDEX_METADATA_KEY,
            &DURABLE_WAIT_REGISTRY_FORMATS,
            metadata,
        );
    }
    Ok(Json(()))
}

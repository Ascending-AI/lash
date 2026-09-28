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

pub(super) async fn retire_root(
    ctx: ObjectContext<'_>,
    namespace: &crate::RestateNamespace,
    request: RestateDurableWaitRootRequest,
) -> HandlerResult<Json<()>> {
    if request.session_id.as_str() != ctx.key() || request.root.as_str().is_empty() {
        return Err(TerminalError::new(format!(
            "root retirement for session `{}` and root `{}` addressed index {}",
            request.session_id,
            request.root,
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

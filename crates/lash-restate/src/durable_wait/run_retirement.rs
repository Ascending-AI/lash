use super::*;

pub(super) fn closed_run_cancel_prefix(run: &lash_core::TurnId) -> String {
    format!("turn/{}/{}:", run.as_str().len(), run.as_str())
}

fn belongs_to_closed_run(
    key: &AwaitEventKey,
    session_id: &SessionId,
    run: &lash_core::TurnId,
) -> bool {
    match &key.scope {
        ExecutionScope::Turn {
            session_id: key_session,
            turn_id,
        } => {
            key_session == session_id
                && lash_core::store::PhysicalTurn::split_turn_id(turn_id).0 == *run
        }
        _ => false,
    }
}

/// Whether `key` waits on the terminal of a physical turn the run's ending
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

pub(super) async fn retire_run(
    ctx: ObjectContext<'_>,
    object: object_state::AdmittedObject,
    namespace: &crate::RestateNamespace,
    request: RestateDurableWaitRunRequest,
) -> HandlerResult<()> {
    if request.session_id.as_str() != ctx.key()
        || request.run.as_str().is_empty()
        || request.committed_turn.as_ref().is_some_and(|turn| {
            lash_core::store::PhysicalTurn::split_turn_id(turn).0 != request.run
        })
    {
        return Err(TerminalError::new(format!(
            "run retirement for session `{}`, run `{}` and committed turn {:?} addressed index {}",
            request.session_id,
            request.run,
            request.committed_turn,
            ctx.key()
        ))
        .into());
    }
    let mut metadata = load_durable_wait_index_metadata(&ctx, object.writer).await?;
    let keys = ctx.get_keys().await?;
    for wait in load_indexed_waits_in(&ctx, &keys)
        .await?
        .into_iter()
        .filter(|wait| belongs_to_closed_run(&wait.key, &request.session_id, &request.run))
    {
        let key = wait.key;
        let address = RestateDurableWaitAddress::for_key(&key);
        if owes_published_terminal(&key, request.committed_turn.as_ref()) {
            // Its commit's terminal wins the promise, not a `Cancelled` from
            // here. A wait whose terminal has not landed stays registered: the
            // landing publish settles it, and session revocation still can.
            let landed = namespace
                .durable_wait_workflow(&ctx, address.workflow_key.clone())
                .peek()
                .header(LASH_REPLAY_KEY_HEADER.to_string(), key.key_id.clone())
                .call()
                .await?
                .into_body();
            if landed.is_some() {
                ctx.clear(&durable_wait_index_state_key(&address));
            }
            continue;
        }
        if wait.terminal.is_none() {
            resolve_indexed_waits(&ctx, object.writer, namespace, vec![key.clone()], false).await?;
        }
        ctx.clear(&durable_wait_index_state_key(&address));
    }

    // The closed run's Deferred sources end with it: an unsealed one is
    // sealed `Cancelled`, its subscribers wake, and every row goes. A late
    // write then finds no armed source and revives nothing.
    let sources = source_seal::load_sources(&ctx, &keys, |armed| {
        belongs_to_closed_run(&armed.descriptor.source, &request.session_id, &request.run)
    })
    .await?;
    let cleared: Vec<_> = sources
        .iter()
        .map(|(address, _)| source_seal::source_state_key(address))
        .collect();
    source_seal::retire_sources(namespace, &ctx, object.writer, sources).await?;
    for state_key in cleared {
        ctx.clear(&state_key);
    }

    let detached = super::process_terminal::detach(namespace, &ctx, &mut metadata, |key| {
        belongs_to_closed_run(key, &request.session_id, &request.run)
    });
    let before = metadata.cancel_decided.len() + metadata.awakeables.len();
    let prefix = closed_run_cancel_prefix(&request.run);
    metadata
        .cancel_decided
        .retain(|id| !id.starts_with(&prefix));
    let mut retained = Vec::with_capacity(metadata.awakeables.len());
    for entry in std::mem::take(&mut metadata.awakeables) {
        if belongs_to_closed_run(&entry.key, &request.session_id, &request.run) {
            resolve_durable_wait_awakeable(&ctx, &entry, &Resolution::Cancelled);
        } else {
            retained.push(entry);
        }
    }
    metadata.awakeables = retained;
    if detached || metadata.cancel_decided.len() + metadata.awakeables.len() != before {
        object_state::set_stamped(
            &ctx,
            DURABLE_WAIT_INDEX_METADATA_KEY,
            object.writer,
            metadata,
        );
    }
    Ok(())
}

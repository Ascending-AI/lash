//! Short observers and the logical wait terminals they watch.
use super::*;

pub(super) async fn register(
    registry: &LashDurableWaitRegistryImpl,
    ctx: ObjectContext<'_>,
    call: Call<RestateDurableWaitIndexRequest>,
) -> HandlerResult<Reply<RestateDurableWaitRegistration>> {
    let (wire, request) = call.open()?;
    let object = registry.admit(&ctx).await?;
    let address = derive_durable_wait_index_address(ctx.key(), &request.key)?;
    let metadata = load_durable_wait_index_metadata(&ctx, object.writer).await?;
    let registration = if metadata.revoked {
        RestateDurableWaitRegistration::Revoked
    } else if let Some(terminal) = source_seal::event_terminal(registry, &ctx, &address).await? {
        RestateDurableWaitRegistration::Resolved(terminal)
    } else if let Some(resolution) = object_state::get_stamped::<IndexedWait>(
        &ctx,
        &durable_wait_index_state_key(&address),
        &DURABLE_WAIT_REGISTRY_FORMATS,
    )
    .await?
    .and_then(|wait| wait.terminal)
    {
        RestateDurableWaitRegistration::Resolved(resolution)
    } else {
        store_indexed_wait(&ctx, object.writer, &request.key, &address, None);
        RestateDurableWaitRegistration::Registered
    };
    #[cfg(test)]
    wait_registration_witness::observe_wait_registration(&request.key, &registration);
    Ok(Reply::at(wire, registration))
}

pub(super) async fn settle(
    registry: &LashDurableWaitRegistryImpl,
    ctx: ObjectContext<'_>,
    call: Call<RestateDurableWaitSettleRequest>,
) -> HandlerResult<Reply<()>> {
    let (wire, request) = call.open()?;
    let object = registry.admit(&ctx).await?;
    let address = derive_durable_wait_index_address(ctx.key(), &request.key)?;
    let mut metadata = load_durable_wait_index_metadata(&ctx, object.writer).await?;
    if metadata.revoked {
        return Ok(Reply::at(wire, ()));
    }
    if matches!(request.key.wait, AwaitEventWaitIdentity::TurnTerminal) {
        // A late attach may register after CloseRunScope retired the run.
        // Its workflow promise already owns the terminal; settling the
        // attach must not restore a session-lifetime index row.
        ctx.clear(&durable_wait_index_state_key(&address));
        return Ok(Reply::at(wire, ()));
    }
    // A wait that ended inside its own workflow, on its
    // invocation's cancel, reaches the index only here. A turn-control
    // gate settles its entries through `resolve`.
    if !request.key.wait.is_turn_control()
        && wake_ended_waits(
            &registry.namespace,
            &ctx,
            &mut metadata,
            &request.resolution,
            |key| *key == request.key,
        )
    {
        object_state::set_stamped(
            &ctx,
            DURABLE_WAIT_INDEX_METADATA_KEY,
            object.writer,
            metadata,
        );
    }
    store_indexed_wait(
        &ctx,
        object.writer,
        &request.key,
        &address,
        Some(request.resolution),
    );
    Ok(Reply::at(wire, ()))
}

pub(super) async fn register_awakeable(
    registry: &LashDurableWaitRegistryImpl,
    ctx: ObjectContext<'_>,
    call: Call<RestateDurableWaitAwakeableRequest>,
) -> HandlerResult<Reply<RestateDurableWaitRegistration>> {
    let (wire, request) = call.open()?;
    let object = registry.admit(&ctx).await?;
    let address = derive_durable_wait_index_address(ctx.key(), &request.key)?;
    let mut metadata = load_durable_wait_index_metadata(&ctx, object.writer).await?;
    if metadata.revoked {
        return Ok(Reply::at(wire, RestateDurableWaitRegistration::Revoked));
    }
    // A key its owning group child's cancel decision closed has ended,
    // whether or not its wait ever registered: no resolve of it lands.
    if metadata.is_cancel_decided(&request.key.scope, &request.key.wait)? {
        resolve_durable_wait_awakeable(&ctx, &request, &Resolution::Cancelled);
        return Ok(Reply::at(wire, RestateDurableWaitRegistration::Registered));
    }
    if let Some(terminal) = source_seal::event_terminal(registry, &ctx, &address).await? {
        resolve_durable_wait_awakeable(&ctx, &request, &terminal);
        return Ok(Reply::at(wire, RestateDurableWaitRegistration::Registered));
    }
    if let Some(resolution) = object_state::get_stamped::<IndexedWait>(
        &ctx,
        &durable_wait_index_state_key(&address),
        &DURABLE_WAIT_REGISTRY_FORMATS,
    )
    .await?
    .and_then(|wait| wait.terminal)
    {
        resolve_durable_wait_awakeable(&ctx, &request, &resolution);
        return Ok(Reply::at(wire, RestateDurableWaitRegistration::Registered));
    }
    let peek = registry
        .namespace
        .durable_wait_workflow(&ctx, address.workflow_key)
        .peek()
        .header(
            LASH_REPLAY_KEY_HEADER.to_string(),
            request.key.key_id.clone(),
        );
    let resolution = peek.call().await?.into_body();
    if let Some(resolution) = resolution {
        resolve_durable_wait_awakeable(&ctx, &request, &resolution);
    } else if !metadata
        .awakeables
        .iter()
        .any(|entry| entry.key == request.key && entry.awakeable_id == request.awakeable_id)
    {
        metadata.awakeables.push(request);
        object_state::set_stamped(
            &ctx,
            DURABLE_WAIT_INDEX_METADATA_KEY,
            object.writer,
            metadata,
        );
    }
    Ok(Reply::at(wire, RestateDurableWaitRegistration::Registered))
}

pub(super) async fn unregister_awakeable(
    registry: &LashDurableWaitRegistryImpl,
    ctx: ObjectContext<'_>,
    call: Call<RestateDurableWaitAwakeableRequest>,
) -> HandlerResult<Reply<()>> {
    let (wire, request) = call.open()?;
    let object = registry.admit(&ctx).await?;
    let _address = derive_durable_wait_index_address(ctx.key(), &request.key)?;
    let mut metadata = load_durable_wait_index_metadata(&ctx, object.writer).await?;
    metadata
        .awakeables
        .retain(|entry| entry.key != request.key || entry.awakeable_id != request.awakeable_id);
    object_state::set_stamped(
        &ctx,
        DURABLE_WAIT_INDEX_METADATA_KEY,
        object.writer,
        metadata,
    );
    Ok(Reply::at(wire, ()))
}

pub(super) async fn hand_over_turns(
    registry: &LashDurableWaitRegistryImpl,
    ctx: ObjectContext<'_>,
    call: Call<RestateDurableWaitHandOverRequest>,
) -> HandlerResult<Reply<u64>> {
    let (wire, request) = call.open()?;
    let object = registry.admit(&ctx).await?;
    let mut metadata = load_durable_wait_index_metadata(&ctx, object.writer).await?;
    let mut woken = 0_u64;
    metadata.awakeables.retain(|entry| {
        let hands_over = entry.hand_over.as_ref() == Some(&request.generation);
        if hands_over {
            ctx.resolve_awakeable(&entry.awakeable_id, Json(RestateTurnCancelWake::HandedOver));
            woken += 1;
        }
        !hands_over
    });
    if woken > 0 {
        object_state::set_stamped(
            &ctx,
            DURABLE_WAIT_INDEX_METADATA_KEY,
            object.writer,
            metadata,
        );
    }
    Ok(Reply::at(wire, woken))
}

pub(super) async fn resolve(
    registry: &LashDurableWaitRegistryImpl,
    ctx: ObjectContext<'_>,
    call: Call<RestateDurableWaitResolveRequest>,
) -> HandlerResult<Reply<RestateDurableWaitResolveResponse>> {
    let (wire, request) = call.open()?;
    let object = registry.admit(&ctx).await?;
    let address = derive_durable_wait_index_address(ctx.key(), &request.key)?;
    let mut metadata = load_durable_wait_index_metadata(&ctx, object.writer).await?;
    if metadata.revoked {
        return Ok(Reply::at(
            wire,
            RestateDurableWaitResolveResponse::Outcome(ResolveOutcome::UnknownOrRevoked),
        ));
    }
    // §4, W17: the owning group child's cancel decision closed this key.
    // Checked before any retained terminal, because the close's own
    // release of the child's wait may have retained one since.
    if metadata.is_cancel_decided(&request.key.scope, &request.key.wait)? {
        return Ok(Reply::at(
            wire,
            RestateDurableWaitResolveResponse::Refused(
                RestateDurableWaitResolveRefusal::CancelDecided,
            ),
        ));
    }
    if let Some(response) = source_seal::resolve_completion(
        registry,
        &ctx,
        object.writer,
        &request.key,
        request.resolution.clone(),
    )
    .await?
    {
        return Ok(Reply::at(wire, response));
    }
    if matches!(
        request.key.wait,
        AwaitEventWaitIdentity::ToolCompletion { .. }
    ) {
        return Ok(Reply::at(
            wire,
            RestateDurableWaitResolveResponse::Refused(RestateDurableWaitResolveRefusal::Source {
                refusal: lash_core::tool_run::SourceRefusal::NotArmed,
            }),
        ));
    }
    let state_key = durable_wait_index_state_key(&address);
    if let Some(terminal) =
        object_state::get_stamped::<IndexedWait>(&ctx, &state_key, &DURABLE_WAIT_REGISTRY_FORMATS)
            .await?
            .and_then(|wait| wait.terminal)
    {
        process_terminal::publish(
            &registry.namespace,
            &ctx,
            &mut metadata,
            &request.key,
            &terminal,
        )
        .await?;
        object_state::set_stamped(
            &ctx,
            DURABLE_WAIT_INDEX_METADATA_KEY,
            object.writer,
            metadata,
        );
        return Ok(Reply::at(
            wire,
            RestateDurableWaitResolveResponse::Outcome(ResolveOutcome::AlreadyResolved {
                terminal,
            }),
        ));
    }
    let resolution = request.resolution.clone();
    let replay_key = request.key.key_id.clone();
    let resolve = registry
        .namespace
        .durable_wait_workflow(&ctx, address.workflow_key.clone())
        .resolve(request.clone())
        .header(LASH_REPLAY_KEY_HEADER.to_string(), replay_key);
    let outcome = resolve.call().await?.into_body();
    // Wake with the terminal the gate actually holds: a request that lost
    // to an earlier writer must not report its own mode to the waiter.
    let settled = match &outcome {
        ResolveOutcome::AlreadyResolved { terminal } => terminal.clone(),
        ResolveOutcome::Accepted | ResolveOutcome::UnknownOrRevoked => resolution.clone(),
    };
    // A turn's terminal is published one-way after its commit, so it can
    // land after CloseRunScope retired the run. Its workflow promise
    // owns the terminal, as in `settle`; mirroring it would restore a
    // session-lifetime row (FIG-3978).
    if !matches!(request.key.wait, AwaitEventWaitIdentity::TurnTerminal) {
        mirror_resolve_outcome(
            &ctx,
            object.writer,
            &request.key,
            &address,
            resolution,
            &outcome,
        );
    }
    if outcome == ResolveOutcome::UnknownOrRevoked {
        return Ok(Reply::at(
            wire,
            RestateDurableWaitResolveResponse::Outcome(outcome),
        ));
    }
    process_terminal::publish(
        &registry.namespace,
        &ctx,
        &mut metadata,
        &request.key,
        &settled,
    )
    .await?;
    wake_ended_waits(&registry.namespace, &ctx, &mut metadata, &settled, |key| {
        *key == request.key
    });
    object_state::set_stamped(
        &ctx,
        DURABLE_WAIT_INDEX_METADATA_KEY,
        object.writer,
        metadata,
    );
    Ok(Reply::at(
        wire,
        RestateDurableWaitResolveResponse::Outcome(outcome),
    ))
}

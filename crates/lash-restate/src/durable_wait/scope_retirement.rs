//! Scope retirement proofs and their durable fence.

use lash_core::ExecutionScope;
use restate_sdk::context::{
    ContextReadState, ContextSideEffects, ContextWriteState, ObjectContext, RunFuture,
};
use restate_sdk::errors::{HandlerResult, TerminalError};
use restate_sdk::serde::Json;

use crate::compat::{Call, Reply};

use super::source_seal::{load_sources, retire_sources};
use super::{
    DURABLE_WAIT_INDEX_CLOSURE_PARTICIPANT_PREFIX, DURABLE_WAIT_INDEX_EFFECT_PREFIX,
    DURABLE_WAIT_INDEX_GROUP_PREFIX, DURABLE_WAIT_INDEX_METADATA_KEY,
    DURABLE_WAIT_INDEX_PROCESS_JOURNAL_PREFIX, DURABLE_WAIT_REGISTRY_FORMATS,
    RestateDurableWaitProcessJournalRequest, durable_wait_index_key_for_scope,
    load_durable_wait_index_metadata, load_indexed_waits, object_state, resolve_indexed_waits,
    revoke_durable_wait_awakeable,
};

/// Revoke the index: fence it, revoke its awakeables, and cancel its waits.
/// With `only_if_quiescent`, an unresolved wait or a live awakeable leaves
/// the index untouched and answers `false`.
pub(super) async fn revoke_index(
    ctx: &ObjectContext<'_>,
    object: object_state::AdmittedObject,
    namespace: &crate::RestateNamespace,
    admin: Option<&crate::RestateAdminClient>,
    only_if_quiescent: bool,
) -> HandlerResult<bool> {
    let mut metadata = load_durable_wait_index_metadata(ctx, object.writer).await?;
    let waits: Vec<_> = load_indexed_waits(ctx)
        .await?
        .into_iter()
        .filter(|wait| wait.terminal.is_none())
        .map(|wait| wait.key)
        .collect();
    let keys = ctx.get_keys().await?;
    // Process quiescence is conservative per segment. Even owner-terminal
    // retirement must wait until an admitted journal can issue no more effects.
    // A cancel uses the existing process path; a kill is proved by the engine's
    // completed invocation, so it cannot strand the scope's pin.
    let process_scope = ctx
        .key()
        .strip_prefix("scope:")
        .and_then(ExecutionScope::from_journal_key)
        .is_some_and(|scope| matches!(scope, ExecutionScope::Process { .. }));
    if process_scope && !process_journals_are_quiescent(ctx, &keys, admin).await? {
        return Ok(false);
    }
    if keys
        .iter()
        .any(|state_key| state_key.starts_with(DURABLE_WAIT_INDEX_CLOSURE_PARTICIPANT_PREFIX))
    {
        return Ok(false);
    }
    // An unsealed Deferred source keeps a quiescence-proved or process
    // retirement open; an unconditional revocation seals it `Cancelled`.
    let open_sources = load_sources(ctx, &keys, |armed| armed.seal.is_none()).await?;
    if (only_if_quiescent
        && (!waits.is_empty()
            || !metadata.awakeables.is_empty()
            || !metadata.process_sources.is_empty()
            || !metadata.process_receivers.is_empty()
            || !open_sources.is_empty()))
        || (process_scope && !open_sources.is_empty())
        || ((only_if_quiescent || process_scope)
            && !scope_effects_and_groups_are_quiescent(ctx, namespace).await?)
    {
        return Ok(false);
    }
    retire_sources(namespace, ctx, object.writer, open_sources).await?;
    super::process_terminal::detach(namespace, ctx, &mut metadata, |_| true);
    let awakeables = std::mem::take(&mut metadata.awakeables);
    metadata.revoked = true;
    // A revoked index keeps its `_compat` record: it fences a stale handler
    // from recreating the state the revocation cleared.
    object.clear_all(ctx);
    object_state::set_stamped(
        ctx,
        DURABLE_WAIT_INDEX_METADATA_KEY,
        object.writer,
        metadata,
    );
    for entry in awakeables {
        revoke_durable_wait_awakeable(ctx, &entry);
    }
    resolve_indexed_waits(ctx, object.writer, namespace, waits, false).await?;
    Ok(true)
}

pub(super) fn process_journal_key(invocation: &crate::RestateInvocationId) -> String {
    format!(
        "{DURABLE_WAIT_INDEX_PROCESS_JOURNAL_PREFIX}{}",
        invocation.as_str()
    )
}

async fn process_journals_are_quiescent(
    ctx: &ObjectContext<'_>,
    keys: &[String],
    admin: Option<&crate::RestateAdminClient>,
) -> HandlerResult<bool> {
    for key in keys {
        if !key.starts_with(DURABLE_WAIT_INDEX_PROCESS_JOURNAL_PREFIX) {
            continue;
        }
        let Some(admin) = admin else {
            return Ok(false);
        };
        let invocation = object_state::get_stamped::<crate::RestateInvocationId>(
            ctx,
            key,
            &DURABLE_WAIT_REGISTRY_FORMATS,
        )
        .await?
        .ok_or_else(|| TerminalError::new("process journal pin disappeared"))?;
        let Json(ended) = ctx
            .run(|| async move {
                admin
                    .invocation_status(&invocation)
                    .await
                    .map(|status| Json(status.is_some_and(|status| !status.is_still_active())))
                    .map_err(|error| std::io::Error::other(error.to_string()).into())
            })
            .name(format!("lash.process-journal-ended:{key}"))
            .await?;
        if !ended {
            return Ok(false);
        }
        ctx.clear(key);
    }
    Ok(true)
}

/// Whether nothing recorded by `begin_effect` or `record_group` is still
/// live: no executing effect, and every recorded group's index reports no
/// unsettled child. A group found settled is forgotten here, so a caller
/// that never closed it does not fence its scope forever.
async fn scope_effects_and_groups_are_quiescent(
    ctx: &ObjectContext<'_>,
    namespace: &crate::RestateNamespace,
) -> Result<bool, TerminalError> {
    let keys = ctx.get_keys().await?;
    if keys
        .iter()
        .any(|state_key| state_key.starts_with(DURABLE_WAIT_INDEX_EFFECT_PREFIX))
    {
        return Ok(false);
    }
    let mut live = false;
    for (state_key, group_key) in keys.iter().filter_map(|state_key| {
        state_key
            .strip_prefix(DURABLE_WAIT_INDEX_GROUP_PREFIX)
            .map(|group_key| (state_key, group_key))
    }) {
        let unsettled = namespace
            .effect_group_state(ctx, group_key.to_string())
            .unsettled_children()
            .call()
            .await?
            .into_body();
        if unsettled > 0 {
            live = true;
        } else {
            ctx.clear(state_key);
        }
    }
    Ok(!live)
}

pub(super) async fn register_process_journal(
    registry: &super::LashDurableWaitRegistryImpl,
    ctx: ObjectContext<'_>,
    call: Call<RestateDurableWaitProcessJournalRequest>,
) -> HandlerResult<Reply<bool>> {
    let (wire, request) = call.open()?;
    let scope = ExecutionScope::process(request.process_id);
    if durable_wait_index_key_for_scope(&scope) != ctx.key() {
        return Err(TerminalError::new("process journal addressed another scope").into());
    }
    let object = registry.admit(&ctx).await?;
    if load_durable_wait_index_metadata(&ctx, object.writer)
        .await?
        .revoked
    {
        return Ok(Reply::at(wire, false));
    }
    object_state::set_stamped(
        &ctx,
        &process_journal_key(&request.invocation_id),
        object.writer,
        request.invocation_id,
    );
    Ok(Reply::at(wire, true))
}

pub(super) async fn release_process_journal(
    registry: &super::LashDurableWaitRegistryImpl,
    ctx: ObjectContext<'_>,
    call: Call<RestateDurableWaitProcessJournalRequest>,
) -> HandlerResult<Reply<()>> {
    let (wire, request) = call.open()?;
    if durable_wait_index_key_for_scope(&ExecutionScope::process(request.process_id)) != ctx.key() {
        return Err(TerminalError::new("process journal addressed another scope").into());
    }
    registry.admit(&ctx).await?;
    ctx.clear(&process_journal_key(&request.invocation_id));
    Ok(Reply::at(wire, ()))
}

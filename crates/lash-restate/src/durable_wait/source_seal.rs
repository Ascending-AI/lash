//! K4: a Deferred source's one immutable terminal on the durable-wait family
//! (FIG-4883).
//!
//! The Run arms a source before its completion key leaves the call: the
//! scope's `LashDurableWaitIndex` pins the [`SourceDescriptor`] under the
//! key's workflow address. Every seal write goes through that exclusive
//! object, which authenticates it against the pinned descriptor and applies
//! first-writer-wins; the source's `LashDurableWaitWorkflow` promise then
//! holds the seal immutably, and the index row mirrors it for reads. A
//! segment waits through a short subscription naming its awakeable: the
//! subscribe call answers an existing seal at once, and a seal wakes every
//! subscribed segment with it. Nothing waits inside a long invocation.
//!
//! A cancel is the owning Run's seal write, and its reply is the source's
//! answer: when a resolution won first the cancel reads that seal, so the
//! Run accepts the resolved result rather than a cancellation. A late write
//! reads the existing seal and revives nothing. The seal decides one call's
//! result versus its cancellation; the Run's rank and drain are its own
//! records, with no transaction across the two objects. There is no deadline
//! and no timeout terminal (binding Q3).

use lash_core::AwaitEventKey;
use lash_core::tool_run::{
    SealOutcome, SealWriter, SegmentOrdinal, SourceDescriptor, SourceRefusal, SourceSeal,
    SourceSubscription,
};
use restate_sdk::context::{
    ContextAwakeables, ContextPromises, ObjectContext, SharedWorkflowContext,
};
use restate_sdk::errors::{HandlerResult, TerminalError};
use restate_sdk::serde::Json;
use serde::{Deserialize, Serialize};

use super::{
    DURABLE_WAIT_REGISTRY_FORMATS, LASH_REPLAY_KEY_HEADER, LashDurableWaitRegistryImpl,
    RestateDurableWaitAddress, derive_durable_wait_index_address, load_durable_wait_index_metadata,
    object_state, verify_durable_wait_workflow_key,
};
use crate::compat::{Call, Reply};

/// The workflow promise that holds a source's seal. It is distinct from the
/// wait promise, so a key's older wait consumers read what they always did.
pub(crate) const SOURCE_SEAL_PROMISE_KEY: &str = "source-seal";

/// One armed source, keyed by its workflow address: the descriptor its Run
/// pinned, the seal mirrored from the workflow promise, and the segments
/// subscribed to it.
/// version_surface = "coexist"
/// version_guard(items(DURABLE_WAIT_INDEX_SOURCE_PREFIX, source_state_key))
pub(super) const DURABLE_WAIT_INDEX_SOURCE_PREFIX: &str = "wait-index/v2/source/";

/// An armed source's index row.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct IndexedSource {
    pub(crate) descriptor: SourceDescriptor,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) seal: Option<SourceSeal>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) subscribers: Vec<SourceSubscriber>,
}

/// One subscribed segment: the awakeable a seal resolves with it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SourceSubscriber {
    pub(crate) segment: SegmentOrdinal,
    pub(crate) awakeable_id: String,
}

/// Pin `descriptor` before the source's key leaves its call.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestateSourceArmRequest {
    pub descriptor: SourceDescriptor,
}

/// A segment's short subscription: the seal resolves `awakeable_id` with
/// the [`SourceSeal`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestateSourceSubscribeRequest {
    pub subscription: SourceSubscription,
    pub awakeable_id: String,
}

/// One authenticated seal write.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestateSourceSealRequest {
    pub source: AwaitEventKey,
    pub writer: SealWriter,
    pub seal: SourceSeal,
}

/// The seal the index hands its source's workflow to hold.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestateSourceSealWrite {
    pub source: AwaitEventKey,
    pub seal: SourceSeal,
}

/// What `arm_source` answers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reply", rename_all = "snake_case", deny_unknown_fields)]
pub enum RestateSourceArmReply {
    /// The source is armed; a re-arm reads any seal it already holds.
    Armed {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        seal: Option<SourceSeal>,
    },
    Refused {
        refusal: SourceRefusal,
    },
}

/// What `subscribe_source` answers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reply", rename_all = "snake_case", deny_unknown_fields)]
pub enum RestateSourceSubscribeReply {
    /// The seal will resolve the subscription's awakeable.
    Subscribed,
    /// The source was already sealed: this is its seal, and nothing was
    /// subscribed.
    Sealed {
        seal: SourceSeal,
    },
    Refused {
        refusal: SourceRefusal,
    },
}

/// What `seal_source` answers: the source's one seal, or a refusal.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reply", rename_all = "snake_case", deny_unknown_fields)]
pub enum RestateSourceSealReply {
    Outcome { outcome: SealOutcome },
    Refused { refusal: SourceRefusal },
}

pub(super) fn source_state_key(address: &RestateDurableWaitAddress) -> String {
    format!("{DURABLE_WAIT_INDEX_SOURCE_PREFIX}{}", address.workflow_key)
}

/// L13 retains identity and terminal kind after releasing the source's body.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RetiredSource {
    source: AwaitEventKey,
    terminal: RetiredSourceTerminal,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RetiredSourceTerminal {
    Resolved,
    Cancelled,
}

/// The retained source identity and terminal's index key.
/// version_surface = "coexist"
/// version_guard(items(retired_source_key, RetiredSource, RetiredSourceTerminal))
const RETIRED_SOURCE_PREFIX: &str = "wait-index/v2/source-retired/";

fn retired_source_key(address: &RestateDurableWaitAddress) -> String {
    format!("{RETIRED_SOURCE_PREFIX}{}", address.workflow_key)
}

async fn source_is_retired(
    ctx: &ObjectContext<'_>,
    address: &RestateDurableWaitAddress,
    source: &AwaitEventKey,
) -> Result<bool, TerminalError> {
    let fence = object_state::get_stamped::<RetiredSource>(
        ctx,
        &retired_source_key(address),
        &DURABLE_WAIT_REGISTRY_FORMATS,
    )
    .await?;
    match fence {
        Some(fence) if fence.source != *source => Err(TerminalError::new(
            "retired source identity does not match its address",
        )),
        fence => Ok(fence.is_some()),
    }
}

async fn load_source(
    ctx: &ObjectContext<'_>,
    address: &RestateDurableWaitAddress,
) -> Result<Option<IndexedSource>, TerminalError> {
    object_state::get_stamped(
        ctx,
        &source_state_key(address),
        &DURABLE_WAIT_REGISTRY_FORMATS,
    )
    .await
}

pub(super) async fn arm_source(
    registry: &LashDurableWaitRegistryImpl,
    ctx: ObjectContext<'_>,
    call: Call<RestateSourceArmRequest>,
) -> HandlerResult<Reply<RestateSourceArmReply>> {
    let (wire, request) = call.open()?;
    let object = registry.admit(&ctx).await?;
    arm_descriptor(registry, &ctx, object.writer, request.descriptor)
        .await
        .map(|reply| Reply::at(wire, reply))
        .map_err(Into::into)
}

pub(super) async fn arm_descriptor(
    _registry: &LashDurableWaitRegistryImpl,
    ctx: &ObjectContext<'_>,
    writer: object_state::StoredValueWriter,
    descriptor: SourceDescriptor,
) -> Result<RestateSourceArmReply, TerminalError> {
    let address = derive_durable_wait_index_address(ctx.key(), &descriptor.source)?;
    let refused = |refusal| Ok(RestateSourceArmReply::Refused { refusal });
    if load_durable_wait_index_metadata(ctx, writer).await?.revoked {
        return refused(SourceRefusal::Retired);
    }
    if source_is_retired(ctx, &address, &descriptor.source).await? {
        return refused(SourceRefusal::Retired);
    }
    if let Some(armed) = load_source(ctx, &address).await? {
        if armed.descriptor != descriptor {
            return refused(SourceRefusal::DescriptorMismatch);
        }
        return Ok(RestateSourceArmReply::Armed { seal: armed.seal });
    }
    object_state::set_stamped(
        ctx,
        &source_state_key(&address),
        writer,
        IndexedSource {
            descriptor,
            seal: None,
            subscribers: Vec::new(),
        },
    );
    Ok(RestateSourceArmReply::Armed { seal: None })
}

/// The existing signed completion entry resolves an armed K4 source directly.
/// The source seal, rather than a second wait promise, chooses its outcome.
pub(super) async fn resolve_completion(
    registry: &LashDurableWaitRegistryImpl,
    ctx: &ObjectContext<'_>,
    writer: object_state::StoredValueWriter,
    key: &AwaitEventKey,
    resolution: lash_core::Resolution,
) -> Result<Option<super::RestateDurableWaitResolveResponse>, TerminalError> {
    use super::{
        RestateDurableWaitResolveRefusal as Refused, RestateDurableWaitResolveResponse as Response,
    };
    use crate::controller::RestateControllerContext as _;
    use lash_core::tool_run::{
        MaterialBundle, MaterialHolder, MaterialOwner, MaterialPayload, MaterialRole,
        SourceAuthority,
    };
    use lash_core::{Resolution, ResolveOutcome};
    let address = derive_durable_wait_index_address(ctx.key(), key)?;
    let Some(armed) = load_source(ctx, &address).await? else {
        if source_is_retired(ctx, &address, key).await? {
            return Ok(Some(Response::Refused(Refused::Source {
                refusal: SourceRefusal::Retired,
            })));
        }
        return Ok(None);
    };
    let refused = |refusal| Some(Response::Refused(Refused::Source { refusal }));
    let materials = registry
        .materials
        .clone()
        .ok_or_else(|| TerminalError::new("completion material store is unavailable"))?;
    let holder = MaterialHolder::Source {
        source: key.clone(),
    };
    let existing = armed.seal.clone();
    let outcome = if let Some(seal) = existing {
        SealOutcome::AlreadySealed { seal }
    } else {
        if armed.descriptor.authority != SourceAuthority::ExternalCompletion {
            return Ok(refused(SourceRefusal::Seal {
                seal: lash_core::tool_run::SealRefusal::WrongAuthority,
            }));
        }
        let capture = match &resolution {
            Resolution::Ok(value) => lash_core::tool_dispatch::SingletonCapture::Done {
                output: serde_json::to_string(value).map_err(TerminalError::from_error)?,
                commands: Vec::new(),
                intents: Vec::new(),
                stream: Default::default(),
                start: None,
            },
            _ => lash_core::tool_dispatch::SingletonCapture::Failed {
                output: serde_json::to_string(&resolution).map_err(TerminalError::from_error)?,
                stream: Default::default(),
            },
        };
        let payload = MaterialPayload::new(
            MaterialOwner::Source {
                source: key.clone(),
            },
            MaterialRole::AttemptOutput,
            Some(armed.descriptor.resolver.clone()),
            serde_json::to_string(&capture).map_err(TerminalError::from_error)?,
        );
        let bundle = MaterialBundle::of([payload])
            .map_err(|error| TerminalError::new(error.to_record()))?
            .ok_or_else(|| TerminalError::new("completion bundle is empty"))?;
        let store = materials.clone();
        let retained_holder = holder.clone();
        let Json(retained) = ctx.run_json_or_retry_send::<Result<lash_core::tool_run::RetainedBundle, lash_core::RuntimeEffectControllerError>, _>("source:completion:retain".into(), async move {
            match store.retain_material(&retained_holder, &bundle).await {
                Err(lash_core::tool_run::MaterialRetentionError::Store(error)) => Err(error.to_string()),
                outcome => Ok(outcome.map_err(super::process_terminal::material_error)),
            }
        }).await?;
        let retained = retained.map_err(|error| TerminalError::new(error.to_record()))?;
        let result = retained
            .references
            .first()
            .cloned()
            .ok_or_else(|| TerminalError::new("completion bundle has no result"))?;
        match seal_descriptor(
            registry,
            ctx,
            writer,
            RestateSourceSealRequest {
                source: key.clone(),
                writer: SealWriter::External,
                seal: SourceSeal::Resolved {
                    result: Box::new(result),
                },
            },
        )
        .await?
        {
            RestateSourceSealReply::Outcome { outcome } => outcome,
            RestateSourceSealReply::Refused { refusal } => return Ok(refused(refusal)),
        }
    };
    let (SealOutcome::Sealed { seal } | SealOutcome::AlreadySealed { seal }) = &outcome;
    let terminal = resolution_of_seal(registry, &armed.descriptor, seal).await?;
    Ok(Some(Response::Outcome(match outcome {
        SealOutcome::Sealed { .. } => ResolveOutcome::Accepted,
        SealOutcome::AlreadySealed { .. } => ResolveOutcome::AlreadyResolved { terminal },
    })))
}

/// Event observers read the source's one authoritative seal. They do not
/// create a second terminal in the event workflow or its index row.
pub(super) async fn event_terminal(
    registry: &LashDurableWaitRegistryImpl,
    ctx: &ObjectContext<'_>,
    address: &RestateDurableWaitAddress,
) -> Result<Option<lash_core::Resolution>, TerminalError> {
    let Some(armed) = load_source(ctx, address).await? else {
        return Ok(None);
    };
    match armed.seal {
        Some(seal) => resolution_of_seal(registry, &armed.descriptor, &seal)
            .await
            .map(Some),
        None => Ok(None),
    }
}

async fn resolution_of_seal(
    registry: &LashDurableWaitRegistryImpl,
    descriptor: &SourceDescriptor,
    seal: &SourceSeal,
) -> Result<lash_core::Resolution, TerminalError> {
    match seal {
        SourceSeal::Cancelled => Ok(lash_core::Resolution::Cancelled),
        SourceSeal::Resolved { result } => {
            let materials = registry
                .materials
                .as_ref()
                .ok_or_else(|| TerminalError::new("completion material store is unavailable"))?;
            let payload = materials
                .read_material(
                    &lash_core::tool_run::MaterialHolder::Source {
                        source: descriptor.source.clone(),
                    },
                    result,
                    &lash_core::tool_run::MaterialOwner::Source {
                        source: descriptor.source.clone(),
                    },
                    std::slice::from_ref(&descriptor.resolver),
                )
                .await
                .map_err(|error| {
                    TerminalError::new(super::process_terminal::material_error(error).to_record())
                })?;
            super::process_terminal::terminal_resolution(
                serde_json::from_str(&payload.text).map_err(TerminalError::from_error)?,
            )
        }
    }
}

async fn wake_event_observers(
    registry: &LashDurableWaitRegistryImpl,
    ctx: &ObjectContext<'_>,
    writer: object_state::StoredValueWriter,
    address: &RestateDurableWaitAddress,
    source: &AwaitEventKey,
) -> Result<(), TerminalError> {
    let mut metadata = load_durable_wait_index_metadata(ctx, writer).await?;
    if !metadata
        .awakeables
        .iter()
        .any(|observer| observer.key == *source)
    {
        return Ok(());
    }
    if let Some(terminal) = event_terminal(registry, ctx, address).await? {
        super::wake_ended_waits(&registry.namespace, ctx, &mut metadata, &terminal, |key| {
            key == source
        });
        object_state::set_stamped(
            ctx,
            super::DURABLE_WAIT_INDEX_METADATA_KEY,
            writer,
            metadata,
        );
    }
    Ok(())
}

pub(super) async fn subscribe_source(
    registry: &LashDurableWaitRegistryImpl,
    ctx: ObjectContext<'_>,
    call: Call<RestateSourceSubscribeRequest>,
) -> HandlerResult<Reply<RestateSourceSubscribeReply>> {
    let (wire, request) = call.open()?;
    let object = registry.admit(&ctx).await?;
    let address = derive_durable_wait_index_address(ctx.key(), &request.subscription.source)?;
    let refused = |refusal| {
        Ok(Reply::at(
            wire,
            RestateSourceSubscribeReply::Refused { refusal },
        ))
    };
    if load_durable_wait_index_metadata(&ctx, object.writer)
        .await?
        .revoked
    {
        return refused(SourceRefusal::Retired);
    }
    let Some(mut armed) = load_source(&ctx, &address).await? else {
        if source_is_retired(&ctx, &address, &request.subscription.source).await? {
            return refused(SourceRefusal::Retired);
        }
        return refused(SourceRefusal::NotArmed);
    };
    if armed.descriptor.owner != request.subscription.owner {
        return refused(SourceRefusal::WrongOwner);
    }
    if let Some(seal) = armed.seal {
        return Ok(Reply::at(
            wire,
            RestateSourceSubscribeReply::Sealed { seal },
        ));
    }
    let subscriber = SourceSubscriber {
        segment: request.subscription.segment,
        awakeable_id: request.awakeable_id,
    };
    if !armed.subscribers.contains(&subscriber) {
        armed.subscribers.push(subscriber);
        object_state::set_stamped(&ctx, &source_state_key(&address), object.writer, armed);
    }
    Ok(Reply::at(wire, RestateSourceSubscribeReply::Subscribed))
}

/// Drop one segment's subscription: a segment that stops waiting, or a
/// predecessor whose successor subscribed. A source sealed since keeps its
/// seal; an unknown subscription is already gone.
pub(super) async fn unsubscribe_source(
    registry: &LashDurableWaitRegistryImpl,
    ctx: ObjectContext<'_>,
    call: Call<RestateSourceSubscribeRequest>,
) -> HandlerResult<Reply<()>> {
    let (wire, request) = call.open()?;
    let object = registry.admit(&ctx).await?;
    let address = derive_durable_wait_index_address(ctx.key(), &request.subscription.source)?;
    let Some(mut armed) = load_source(&ctx, &address).await? else {
        return Ok(Reply::at(wire, ()));
    };
    let before = armed.subscribers.len();
    armed.subscribers.retain(|subscriber| {
        subscriber.segment != request.subscription.segment
            || subscriber.awakeable_id != request.awakeable_id
    });
    if armed.subscribers.len() != before {
        object_state::set_stamped(&ctx, &source_state_key(&address), object.writer, armed);
    }
    Ok(Reply::at(wire, ()))
}

pub(super) async fn seal_source(
    registry: &LashDurableWaitRegistryImpl,
    ctx: ObjectContext<'_>,
    call: Call<RestateSourceSealRequest>,
) -> HandlerResult<Reply<RestateSourceSealReply>> {
    let (wire, request) = call.open()?;
    let object = registry.admit(&ctx).await?;
    seal_descriptor(registry, &ctx, object.writer, request)
        .await
        .map(|reply| Reply::at(wire, reply))
        .map_err(Into::into)
}

pub(super) async fn seal_descriptor(
    registry: &LashDurableWaitRegistryImpl,
    ctx: &ObjectContext<'_>,
    writer: object_state::StoredValueWriter,
    request: RestateSourceSealRequest,
) -> Result<RestateSourceSealReply, TerminalError> {
    let address = derive_durable_wait_index_address(ctx.key(), &request.source)?;
    let refused = |refusal| Ok(RestateSourceSealReply::Refused { refusal });
    if load_durable_wait_index_metadata(ctx, writer).await?.revoked {
        return refused(SourceRefusal::Retired);
    }
    let Some(armed) = load_source(ctx, &address).await? else {
        if source_is_retired(ctx, &address, &request.source).await? {
            return refused(SourceRefusal::Retired);
        }
        return refused(SourceRefusal::NotArmed);
    };
    if armed.descriptor.source != request.source {
        return refused(SourceRefusal::DescriptorMismatch);
    }
    let outcome = match armed
        .descriptor
        .seal(armed.seal.as_ref(), &request.writer, request.seal)
    {
        Err(seal) => return refused(SourceRefusal::Seal { seal }),
        Ok(outcome @ SealOutcome::AlreadySealed { .. }) => outcome,
        Ok(SealOutcome::Sealed { seal }) => {
            seal_and_wake(&registry.namespace, ctx, writer, &address, armed, seal).await?
        }
    };
    // The immutable seal now owns the result. End its terminal subscription
    // only after sealing and waking, retaining source material until Run retirement.
    let mut metadata = load_durable_wait_index_metadata(ctx, writer).await?;
    if super::process_terminal::detach(&registry.namespace, ctx, &mut metadata, |key| {
        key == &request.source
    }) {
        object_state::set_stamped(
            ctx,
            super::DURABLE_WAIT_INDEX_METADATA_KEY,
            writer,
            metadata,
        );
    }
    wake_event_observers(registry, ctx, writer, &address, &request.source).await?;
    Ok(RestateSourceSealReply::Outcome { outcome })
}

/// Hand `seal` to the source's workflow, then wake every subscribed segment
/// with the seal the workflow holds and mirror it into the row. The
/// workflow call precedes every wake, so a crash between them replays the
/// recorded seal and still wakes each subscriber once.
async fn seal_and_wake(
    namespace: &crate::RestateNamespace,
    ctx: &ObjectContext<'_>,
    writer: object_state::StoredValueWriter,
    address: &RestateDurableWaitAddress,
    mut armed: IndexedSource,
    seal: SourceSeal,
) -> Result<SealOutcome, TerminalError> {
    let outcome = namespace
        .durable_wait_workflow(ctx, address.workflow_key.clone())
        .seal_source(RestateSourceSealWrite {
            source: armed.descriptor.source.clone(),
            seal,
        })
        .header(
            LASH_REPLAY_KEY_HEADER.to_string(),
            armed.descriptor.source.key_id.clone(),
        )
        .call()
        .await?
        .into_body();
    let (SealOutcome::Sealed { seal } | SealOutcome::AlreadySealed { seal }) = &outcome;
    for subscriber in std::mem::take(&mut armed.subscribers) {
        ctx.resolve_awakeable(&subscriber.awakeable_id, Json(seal.clone()));
    }
    armed.seal = Some(seal.clone());
    object_state::set_stamped(ctx, &source_state_key(address), writer, armed);
    Ok(outcome)
}

/// The armed sources among `keys` that `retiring` selects, with their
/// addresses.
pub(super) async fn load_sources(
    ctx: &ObjectContext<'_>,
    keys: &[String],
    mut retiring: impl FnMut(&IndexedSource) -> bool,
) -> Result<Vec<(RestateDurableWaitAddress, IndexedSource)>, TerminalError> {
    let mut sources = Vec::new();
    for state_key in keys
        .iter()
        .filter(|state_key| state_key.starts_with(DURABLE_WAIT_INDEX_SOURCE_PREFIX))
    {
        let armed: IndexedSource =
            object_state::get_stamped(ctx, state_key, &DURABLE_WAIT_REGISTRY_FORMATS)
                .await?
                .ok_or_else(|| {
                    TerminalError::new(format!("source index entry {state_key} disappeared"))
                })?;
        let address = RestateDurableWaitAddress::for_key(&armed.descriptor.source);
        if source_state_key(&address) != *state_key || address.index_key() != ctx.key() {
            return Err(TerminalError::new(format!(
                "source index entry {state_key} does not match its descriptor"
            )));
        }
        if retiring(&armed) {
            sources.push((address, armed));
        }
    }
    Ok(sources)
}

/// Retire `sources`: an unsealed one is sealed `Cancelled` for its owning
/// Run, and its subscribers wake with the seal the workflow holds. The row
/// and its material and attachment leases end behind the holder fence. The
/// row itself is the caller's to clear.
pub(super) async fn retire_sources(
    registry: &LashDurableWaitRegistryImpl,
    ctx: &ObjectContext<'_>,
    writer: object_state::StoredValueWriter,
    sources: Vec<(RestateDurableWaitAddress, IndexedSource)>,
) -> Result<(), TerminalError> {
    use crate::controller::RestateControllerContext as _;
    for (address, armed) in sources {
        let source = armed.descriptor.source.clone();
        let holder = lash_core::tool_run::MaterialHolder::Source {
            source: source.clone(),
        };
        let seal = match armed.seal.clone() {
            Some(seal) => seal,
            None => {
                let (SealOutcome::Sealed { seal } | SealOutcome::AlreadySealed { seal }) =
                    seal_and_wake(
                        &registry.namespace,
                        ctx,
                        writer,
                        &address,
                        armed,
                        SourceSeal::Cancelled,
                    )
                    .await?;
                seal
            }
        };
        let fence = RetiredSource {
            source,
            terminal: match seal {
                SourceSeal::Resolved { .. } => RetiredSourceTerminal::Resolved,
                SourceSeal::Cancelled => RetiredSourceTerminal::Cancelled,
            },
        };
        let materials = registry.materials.clone();
        let attachments = registry.attachments.clone();
        ctx.run_json_or_retry_send::<(), _>(
            format!("source-retire:{}", address.workflow_key),
            async move {
                if let Some(materials) = materials {
                    materials
                        .release_material(&holder)
                        .await
                        .map_err(|error| error.to_string())?;
                }
                attachments
                    .end_attachment_referrer(&holder.referrer())
                    .await
                    .map_err(|error| error.to_string())?;
                Ok(())
            },
        )
        .await?;
        object_state::set_stamped(ctx, &retired_source_key(&address), writer, fence);
    }
    Ok(())
}

/// The workflow half of a seal: the first seal its promise receives stays.
/// Only the source's index calls it, one write at a time.
pub(super) async fn hold_seal(
    ctx: SharedWorkflowContext<'_>,
    call: Call<RestateSourceSealWrite>,
) -> HandlerResult<Reply<SealOutcome>> {
    let (wire, request) = call.open()?;
    verify_durable_wait_workflow_key(ctx.key(), &request.source)?;
    if let Some(payload) = ctx.peek_promise::<String>(SOURCE_SEAL_PROMISE_KEY).await? {
        let seal = serde_json::from_str(&payload).map_err(TerminalError::from_error)?;
        return Ok(Reply::at(wire, SealOutcome::AlreadySealed { seal }));
    }
    let payload = serde_json::to_string(&request.seal).map_err(TerminalError::from_error)?;
    ctx.resolve_promise(SOURCE_SEAL_PROMISE_KEY, payload);
    Ok(Reply::at(wire, SealOutcome::Sealed { seal: request.seal }))
}

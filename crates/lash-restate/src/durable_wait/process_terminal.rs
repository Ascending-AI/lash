//! Short process-terminal subscriptions. The source index owns the terminal;
//! the receiver index acquires delivered material before resolving its wait.

use super::*;
use crate::controller::RestateControllerContext as _;
use lash_core::ProcessAwaitOutput;
use lash_core::runtime::attachment_delivery::{
    DeliveryAcquisition, acquire_under, delivered_attachment_ids, source_gone_output,
};
use lash_core::tool_run::{
    MaterialBundle, MaterialHolder, MaterialOwner, MaterialPayload, MaterialRole, RetainedBundle,
    SealOutcome, SealWriter, SourceAuthority, SourceDescriptor, SourceRefusal, SourceSeal,
};
use std::sync::Arc;

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessTerminalSubscription {
    pub terminal: AwaitEventKey,
    pub receiver: AwaitEventKey,
    pub descriptor: SourceDescriptor,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessTerminalDelivery {
    pub subscription: ProcessTerminalSubscription,
    pub output: ProcessAwaitOutput,
}

impl ProcessTerminalSubscription {
    /// The process terminal that supplies this Run-owned source.
    pub fn for_source(
        descriptor: SourceDescriptor,
    ) -> Result<Self, lash_core::RuntimeEffectControllerError> {
        let SourceAuthority::ProcessTerminal { process_id } = &descriptor.authority else {
            return Err(SourceRefusal::DescriptorMismatch.into());
        };
        let authority =
            restate_authority_id_for_key(&descriptor.source).ok_or(SourceRefusal::WrongOwner)?;
        let terminal = restate_await_event_key_for_authority(
            &authority,
            &ExecutionScope::process(process_id.clone()),
            AwaitEventWaitIdentity::Custom {
                key: "process_terminal".into(),
            },
        )
        .map_err(lash_core::RuntimeEffectControllerError::from)?;
        Ok(Self {
            terminal,
            receiver: descriptor.source.clone(),
            descriptor,
        })
    }
}

#[derive(Clone, Debug)]
pub struct RestateProcessTerminalRequest {
    pub process_id: lash_core::ProcessId,
    pub key: AwaitEventKey,
}

fn refusal(cause: SourceRefusal) -> TerminalError {
    TerminalError::new(lash_core::RuntimeEffectControllerError::from(cause).to_record())
}

pub(super) fn material_error(
    error: lash_core::tool_run::MaterialRetentionError,
) -> lash_core::RuntimeEffectControllerError {
    use lash_core::tool_run::MaterialRetentionError;
    match error {
        MaterialRetentionError::Refused(refusal) => refusal.into(),
        MaterialRetentionError::HolderEnded { holder } => {
            lash_core::RuntimeError::artifact_referrer_ended(holder.referrer()).into()
        }
        MaterialRetentionError::Controller(error) => *error,
        MaterialRetentionError::Store(error) => error.into(),
    }
}

fn receiver_address(
    object_key: &str,
    key: &AwaitEventKey,
) -> Result<RestateDurableWaitAddress, TerminalError> {
    let address = RestateDurableWaitAddress::for_key(key);
    if address.index_key() != object_key {
        return Err(refusal(SourceRefusal::WrongOwner));
    }
    Ok(address)
}

fn validate(subscription: &ProcessTerminalSubscription) -> Result<(), TerminalError> {
    if subscription.descriptor.source != subscription.receiver
        || !matches!((&subscription.terminal.scope, &subscription.descriptor.authority),
            (ExecutionScope::Process { process_id }, SourceAuthority::ProcessTerminal { process_id: by }) if process_id == by)
        || subscription.descriptor.owner.admitted_scope().scope() != &subscription.receiver.scope
        || !matches!(subscription.terminal.scope, ExecutionScope::Process { .. })
        || subscription.terminal.wait
            != (AwaitEventWaitIdentity::Custom {
                key: "process_terminal".into(),
            })
        || subscription.terminal.signature != subscription.receiver.signature
        || !restate_await_event_key_is_valid(&subscription.terminal)
        || !restate_await_event_key_is_valid(&subscription.receiver)
    {
        return Err(refusal(SourceRefusal::DescriptorMismatch));
    }
    if RestateDurableWaitAddress::for_key(&subscription.terminal).index_key()
        == RestateDurableWaitAddress::for_key(&subscription.receiver).index_key()
    {
        return Err(refusal(SourceRefusal::WrongOwner));
    }
    Ok(())
}

/// Register the receiver before the caller registers with the source. These
/// are separate calls: a publishing source may await this receiver, so the
/// receiver must never hold its object lock while calling the source.
pub(super) async fn attach(
    registry: &LashDurableWaitRegistryImpl,
    ctx: ObjectContext<'_>,
    call: Call<ProcessTerminalSubscription>,
) -> HandlerResult<Reply<bool>> {
    let (wire, subscription) = call.open()?;
    validate(&subscription)?;
    let object = registry.admit(&ctx).await?;
    let address = receiver_address(ctx.key(), &subscription.receiver)?;
    let mut metadata = load_durable_wait_index_metadata(&ctx, object.writer).await?;
    if metadata.revoked {
        return Ok(Reply::at(wire, false));
    }
    if object_state::get_stamped::<IndexedWait>(
        &ctx,
        &durable_wait_index_state_key(&address),
        &DURABLE_WAIT_REGISTRY_FORMATS,
    )
    .await?
    .is_some_and(|wait| wait.terminal.is_some())
    {
        return Ok(Reply::at(wire, false));
    }
    match source_seal::arm_descriptor(
        registry,
        &ctx,
        object.writer,
        subscription.descriptor.clone(),
    )
    .await?
    {
        RestateSourceArmReply::Armed { seal: None } => {}
        RestateSourceArmReply::Armed { seal: Some(_) } => return Ok(Reply::at(wire, false)),
        RestateSourceArmReply::Refused { refusal: cause } => return Err(refusal(cause).into()),
    }
    store_indexed_wait(&ctx, object.writer, &subscription.receiver, &address, None);
    if let Some(existing) = metadata
        .process_sources
        .iter()
        .find(|entry| entry.receiver == subscription.receiver)
    {
        if existing != &subscription {
            return Err(refusal(SourceRefusal::DescriptorMismatch).into());
        }
    } else {
        metadata.process_sources.push(subscription.clone());
        object_state::set_stamped(
            &ctx,
            DURABLE_WAIT_INDEX_METADATA_KEY,
            object.writer,
            metadata,
        );
    }
    Ok(Reply::at(wire, true))
}

/// Source-side registration is one exclusive call and retains only a record.
pub(super) async fn subscribe(
    registry: &LashDurableWaitRegistryImpl,
    ctx: ObjectContext<'_>,
    call: Call<ProcessTerminalSubscription>,
) -> HandlerResult<Reply<Option<ProcessAwaitOutput>>> {
    let (wire, subscription) = call.open()?;
    validate(&subscription)?;
    let object = registry.admit(&ctx).await?;
    let address = receiver_address(ctx.key(), &subscription.terminal)?;
    let mut metadata = load_durable_wait_index_metadata(&ctx, object.writer).await?;
    if metadata.revoked {
        return Err(refusal(SourceRefusal::Retired).into());
    }
    if metadata
        .process_unsubscribed
        .contains(&subscription.receiver)
    {
        return Ok(Reply::at(wire, None));
    }
    let terminal = object_state::get_stamped::<IndexedWait>(
        &ctx,
        &durable_wait_index_state_key(&address),
        &DURABLE_WAIT_REGISTRY_FORMATS,
    )
    .await?
    .and_then(|wait| wait.terminal);
    if let Some(terminal) = terminal {
        let Resolution::Ok(value) = terminal else {
            return Err(refusal(SourceRefusal::DescriptorMismatch).into());
        };
        return Ok(Reply::at(
            wire,
            Some(serde_json::from_value(value).map_err(TerminalError::from_error)?),
        ));
    }
    if !metadata.process_receivers.contains(&subscription) {
        metadata.process_receivers.push(subscription);
        object_state::set_stamped(
            &ctx,
            DURABLE_WAIT_INDEX_METADATA_KEY,
            object.writer,
            metadata,
        );
    }
    Ok(Reply::at(wire, None))
}

pub(super) async fn unsubscribe(
    registry: &LashDurableWaitRegistryImpl,
    ctx: ObjectContext<'_>,
    call: Call<ProcessTerminalSubscription>,
) -> HandlerResult<Reply<()>> {
    let (wire, subscription) = call.open()?;
    validate(&subscription)?;
    let object = registry.admit(&ctx).await?;
    receiver_address(ctx.key(), &subscription.terminal)?;
    let mut metadata = load_durable_wait_index_metadata(&ctx, object.writer).await?;
    metadata
        .process_receivers
        .retain(|entry| entry != &subscription);
    if !metadata
        .process_unsubscribed
        .contains(&subscription.receiver)
    {
        metadata.process_unsubscribed.push(subscription.receiver);
    }
    object_state::set_stamped(
        &ctx,
        DURABLE_WAIT_INDEX_METADATA_KEY,
        object.writer,
        metadata,
    );
    Ok(Reply::at(wire, ()))
}

/// Cleanup sends instead of calling back into a publishing source. The source
/// may currently be awaiting this receiver's delivery acknowledgement.
pub(super) fn detach(
    namespace: &crate::RestateNamespace,
    ctx: &ObjectContext<'_>,
    metadata: &mut RestateDurableWaitIndexMetadata,
    mut ended: impl FnMut(&AwaitEventKey) -> bool,
) -> bool {
    let before = metadata.process_sources.len();
    metadata.process_sources.retain(|entry| {
        if !ended(&entry.receiver) {
            return true;
        }
        let address = RestateDurableWaitAddress::for_key(&entry.terminal);
        namespace
            .durable_wait_registry(ctx, address.index_key())
            .unsubscribe_process_terminal(entry.clone())
            .send();
        false
    });
    before != metadata.process_sources.len()
}

/// The receiver authenticates its recorded subscription, then acquires material
/// in its own journal. Cancellation that already won never acquires a late value.
pub(super) async fn deliver(
    registry: &LashDurableWaitRegistryImpl,
    ctx: ObjectContext<'_>,
    call: Call<ProcessTerminalDelivery>,
) -> HandlerResult<Reply<()>> {
    let (wire, delivery) = call.open()?;
    let subscription = &delivery.subscription;
    validate(subscription)?;
    let object = registry.admit(&ctx).await?;
    let address = receiver_address(ctx.key(), &subscription.receiver)?;
    let metadata = load_durable_wait_index_metadata(&ctx, object.writer).await?;
    if metadata.revoked
        || !metadata.process_sources.contains(subscription)
        || object_state::get_stamped::<IndexedWait>(
            &ctx,
            &durable_wait_index_state_key(&address),
            &DURABLE_WAIT_REGISTRY_FORMATS,
        )
        .await?
        .is_some_and(|wait| wait.terminal.is_some())
    {
        return Ok(Reply::at(wire, ()));
    }
    let attachments = Arc::clone(&registry.attachments);
    let holder = MaterialHolder::Source {
        source: subscription.receiver.clone(),
    };
    let claim = lash_core::ReferrerClaim::unguarded(holder.referrer())
        .map_err(|_| refusal(SourceRefusal::WrongOwner))?;
    let output = delivery.output.clone();
    let Json(acquisition) = ctx
        .run_json_or_retry_send(AcquireTerminalStep, async move {
            acquire_under(
                attachments.as_ref(),
                &claim,
                &delivered_attachment_ids(&output),
            )
            .await
            .map_err(|error| error.to_string())
        })
        .await?;
    let mut resolution = match acquisition {
        DeliveryAcquisition::Held => Resolution::Ok(
            serde_json::to_value(&delivery.output).map_err(TerminalError::from_error)?,
        ),
        DeliveryAcquisition::SourceGone { digest } => Resolution::Ok(
            serde_json::to_value(source_gone_output(&digest)).map_err(TerminalError::from_error)?,
        ),
        DeliveryAcquisition::ReceiverEnded { .. } => return Ok(Reply::at(wire, ())),
        DeliveryAcquisition::Refused { refusal } => {
            Resolution::Err(lash_core::runtime::ExternalCompletionError {
                code: (&refusal.code).into(),
                message: refusal.message,
                raw: None,
            })
        }
    };
    let materials = registry
        .materials
        .clone()
        .ok_or_else(|| TerminalError::new("process-terminal material store is unavailable"))?;
    let capture = match &resolution {
        Resolution::Ok(output) => lash_core::tool_dispatch::SingletonCapture::Done {
            output: serde_json::to_string(output).map_err(TerminalError::from_error)?,
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
            source: subscription.receiver.clone(),
        },
        MaterialRole::AttemptOutput,
        Some(subscription.descriptor.resolver.clone()),
        serde_json::to_string(&capture).map_err(TerminalError::from_error)?,
    );
    let bundle = MaterialBundle::of([payload])
        .map_err(|error| TerminalError::new(error.to_record()))?
        .ok_or_else(|| TerminalError::new("the process result bundle is empty"))?;
    let holder = MaterialHolder::Source {
        source: subscription.receiver.clone(),
    };
    let store = Arc::clone(&materials);
    let Json(retained) = ctx
        .run_json_or_retry_send(RetainTerminalStep, async move {
            match store.retain_material(&holder, &bundle).await {
                Err(lash_core::tool_run::MaterialRetentionError::Store(error)) => {
                    Err(error.to_string())
                }
                result => Ok(result.map_err(material_error)),
            }
        })
        .await?;
    let retained = retained.map_err(|error| TerminalError::new(error.to_record()))?;
    let result = retained
        .references
        .first()
        .ok_or_else(|| TerminalError::new("the retained result is missing"))?
        .clone();
    let SourceAuthority::ProcessTerminal { process_id } = &subscription.descriptor.authority else {
        return Err(refusal(SourceRefusal::DescriptorMismatch).into());
    };
    let sealed = source_seal::seal_descriptor(
        registry,
        &ctx,
        object.writer,
        RestateSourceSealRequest {
            source: subscription.receiver.clone(),
            writer: SealWriter::Process {
                process_id: process_id.clone(),
            },
            seal: SourceSeal::Resolved {
                result: Box::new(result),
            },
        },
    )
    .await?;
    match sealed {
        RestateSourceSealReply::Refused { refusal: cause } => return Err(refusal(cause).into()),
        RestateSourceSealReply::Outcome {
            outcome: SealOutcome::Sealed { seal } | SealOutcome::AlreadySealed { seal },
        } => match seal {
            SourceSeal::Cancelled => resolution = Resolution::Cancelled,
            SourceSeal::Resolved { result } => {
                let payload = materials
                    .read_material(
                        &MaterialHolder::Source {
                            source: subscription.receiver.clone(),
                        },
                        &result,
                        &MaterialOwner::Source {
                            source: subscription.receiver.clone(),
                        },
                        std::slice::from_ref(&subscription.descriptor.resolver),
                    )
                    .await
                    .map_err(|error| TerminalError::new(material_error(error).to_record()))?;
                let capture: lash_core::tool_dispatch::SingletonCapture =
                    serde_json::from_str(&payload.text).map_err(TerminalError::from_error)?;
                resolution = terminal_resolution(capture)?;
            }
        },
    }
    registry
        .resolve(
            ctx,
            Call::new(RestateDurableWaitResolveRequest {
                key: subscription.receiver.clone(),
                resolution,
            }),
        )
        .await?;
    Ok(Reply::at(wire, ()))
}

pub(super) fn terminal_resolution(
    capture: lash_core::tool_dispatch::SingletonCapture,
) -> Result<Resolution, TerminalError> {
    use lash_core::tool_dispatch::SingletonCapture;

    match capture {
        SingletonCapture::Done { output, .. } => Ok(Resolution::Ok(
            serde_json::from_str(&output).map_err(TerminalError::from_error)?,
        )),
        SingletonCapture::Failed { output, .. } => {
            serde_json::from_str(&output).map_err(TerminalError::from_error)
        }
        _ => Err(refusal(SourceRefusal::DescriptorMismatch)),
    }
}

/// Deliver the immutable terminal to every recorded receiver before publication
/// is acknowledged, so process retention cannot release attachments too soon.
pub(super) async fn publish(
    namespace: &crate::RestateNamespace,
    ctx: &ObjectContext<'_>,
    metadata: &mut RestateDurableWaitIndexMetadata,
    key: &AwaitEventKey,
    resolution: &Resolution,
) -> HandlerResult<()> {
    let Resolution::Ok(value) = resolution else {
        return Ok(());
    };
    let receivers: Vec<_> = metadata
        .process_receivers
        .iter()
        .filter(|entry| &entry.terminal == key)
        .cloned()
        .collect();
    if receivers.is_empty() {
        return Ok(());
    }
    let output: ProcessAwaitOutput =
        serde_json::from_value(value.clone()).map_err(TerminalError::from_error)?;
    for subscription in receivers {
        let address = RestateDurableWaitAddress::for_key(&subscription.receiver);
        namespace
            .durable_wait_registry(ctx, address.index_key())
            .deliver_process_terminal(ProcessTerminalDelivery {
                subscription,
                output: output.clone(),
            })
            .call()
            .await?;
    }
    metadata
        .process_receivers
        .retain(|entry| &entry.terminal != key);
    Ok(())
}

struct AcquireTerminalStep;
impl crate::JournalStep for AcquireTerminalStep {
    type Output = DeliveryAcquisition;
    const SURFACE: lash_core::store::SurfaceFormat =
        lash_core::surface_format!(crate::JOURNAL_LOGIC_EPOCH);
    const KIND: &'static str = "process-terminal-acquire";
    fn instance(&self) -> String {
        String::new()
    }
}

struct RetainTerminalStep;
impl crate::JournalStep for RetainTerminalStep {
    type Output = Result<RetainedBundle, lash_core::RuntimeEffectControllerError>;
    const SURFACE: lash_core::store::SurfaceFormat =
        lash_core::surface_format!(crate::JOURNAL_LOGIC_EPOCH);
    const KIND: &'static str = "process-terminal-retain";
    fn instance(&self) -> String {
        String::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// L12: the retained bridge preserves a delivery refusal's error kind and details.
    #[test]
    fn a_retained_delivery_refusal_stays_an_error() -> Result<(), TerminalError> {
        let expected = Resolution::Err(lash_core::runtime::ExternalCompletionError {
            code: lash_sansio::FailureCode::from_foreign_wire(
                "lash:await_event_unknown_or_revoked",
            ),
            message: "the delivered material could not be acquired".into(),
            raw: Some(serde_json::json!({ "cause": "wrong_owner" })),
        });
        let capture = lash_core::tool_dispatch::SingletonCapture::Failed {
            output: serde_json::to_string(&expected).map_err(TerminalError::from_error)?,
            stream: Default::default(),
        };
        assert_eq!(terminal_resolution(capture)?, expected);
        Ok(())
    }
}

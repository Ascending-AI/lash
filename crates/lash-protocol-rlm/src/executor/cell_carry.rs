//! A session's explicit succession uses the process engine's linked page carry.
use super::envelope::CellEnvelope;
use lash_core_store::checkpoint_graph::{
    CommitId, EncodeBudget, ExplicitLimit, GraphDecoder, GraphStaging, NodeKind,
};
use lash_execution_contracts::process::{
    UseCorrespondence,
    linking::{ContentRef, ReachSelector},
};
use lash_vm_broker::kernel::graph::{Admission, EffectMembership, RootParts, stage_exec_root};
use std::collections::BTreeMap;

pub(super) async fn carry(
    snapshots: &lash_vm_broker::DurableSnapshotStore,
    kernel: &super::KernelCarry,
    services: &super::CellServices,
    documents: Option<&lash_vm_runtime::KernelDocuments>,
    ctx: &lash_core::RuntimeExecutionContext<'_>,
) -> Result<(), Error> {
    let text = |error: &dyn std::fmt::Display| Error::Host(error.to_string());
    let Some(read) = snapshots.latest_root().await.map_err(|e| text(&e))? else {
        return Ok(());
    };
    if read.body.status != lash_core_store::checkpoint_graph::bodies::ExecStatus::Parked {
        return Ok(());
    }
    let mut envelope = {
        let mut decoder = GraphDecoder::new(&read.graph, read.read_limits);
        CellEnvelope::load(
            &mut decoder,
            read.body.envelope.ok_or("a parked cell has no envelope")?,
        )
        .map_err(|e| text(&e))?
    };
    if envelope.document.kernel() == kernel.writes().number() {
        return Ok(());
    }
    let refusal = |reason: String| {
        Error::Migration(lash_vm_runtime::KernelMigrationRefusal::NotAdmitted {
            document: envelope.document.id(),
            reason,
        })
    };
    // Authorize and authenticate the correspondence before publishing code or pages.
    if !ctx
        .actor_context()
        .carries_sessions()
        .await
        .map_err(|e| text(&e))?
    {
        return Err(refusal(
            "the fleet cannot write the session successor format".into(),
        ));
    }
    let mut plan = kernel
        .successor(&envelope.document)
        .map_err(Error::Migration)?;
    if plan.to != kernel.writes() {
        return Err(refusal("session successor has the wrong kernel".into()));
    }
    let documents = documents.ok_or("session succession has no document owner")?;
    let mut content = documents
        .load_linked(envelope.link)
        .await
        .map_err(|e| text(&e))?;
    for link in &envelope.environment.authored {
        if content.links.contains_key(link) {
            continue;
        }
        let held = documents.load_linked(*link).await.map_err(|e| text(&e))?;
        content.documents.extend(held.documents);
        content.links.extend(held.links);
        content.packages.extend(held.packages);
    }
    let predecessor = envelope.link;
    let next_document = lash_kernel_doc::IdentifiedDocument::new(plan.rewritten.document.clone());
    let next = lash_vocabulary::package::Link {
        document: next_document.id(),
        fills: content.links[&predecessor].fills.clone(),
    };
    let successor = next.identity();
    content.documents.insert(
        next_document.id(),
        std::sync::Arc::clone(next_document.shared()),
    );
    content.links.insert(successor, next);
    content
        .inspect(
            successor,
            &lash_kernel_package::ReachSelection::Start(lash_vocabulary::StartSelector::Main),
            documents.helpers().as_ref(),
        )
        .map_err(|e| refusal(e.to_string()))?;
    let execution = super::link::execution_id(snapshots.exec());
    let stores = ctx.actor_context().backend().stores();
    let resolutions = stores.execution_resolutions();
    let mut outcomes = BTreeMap::new();
    for use_ in envelope.environment.uses.keys() {
        if let Some(record) = resolutions
            .load(&lash_core_store::resolutions::ResolutionKey {
                execution: execution.clone(),
                use_: *use_,
            })
            .await
            .map_err(|e| text(&e))?
        {
            outcomes.insert(*use_, record.entry);
        }
    }
    let adopted = lash_vm_runtime::plan_adoption(
        &content,
        predecessor,
        successor,
        &envelope.environment.uses.keys().copied().collect(),
        &outcomes,
        &BTreeMap::new(),
    )
    .map_err(|e| refusal(e.to_string()))?;
    let uses = UseCorrespondence { uses: adopted.uses };
    let mut environment = envelope.environment.clone();
    // Keep old uses for exact creation-code callables; successor uses inherit
    // the same validated routes and the same immutable resolution winners.
    for (before, after) in &uses.uses {
        let binding = environment
            .uses
            .get(before)
            .cloned()
            .ok_or_else(|| refusal("session correspondence names an absent use".into()))?;
        if environment
            .uses
            .insert(*after, binding.clone())
            .is_some_and(|held| held != binding)
        {
            return Err(refusal(
                "session correspondence merges unequal routes".into(),
            ));
        }
    }
    environment.roots.remove(&predecessor);
    environment.roots.insert(successor);
    environment.authored.insert(successor);
    // Written rows for earlier saved values use the same authenticated
    // identity correspondence as their frames and frozen callables.
    let retained_documents = content
        .links
        .iter()
        .filter_map(|(link, instance)| {
            content
                .documents
                .get(&instance.document)
                .map(|document| (*link, document.as_ref().clone()))
        })
        .collect();
    let mut retained_code = BTreeMap::<_, std::collections::BTreeSet<_>>::new();
    for callable in &envelope.environment.retained_code {
        let reach = content
            .inspect_ordinary(
                callable.link,
                &lash_kernel_package::ReachSelection::Callable(callable.function),
            )
            .map_err(|error| refusal(error.to_string()))?;
        for (link, functions) in reach.functions {
            retained_code.entry(link).or_default().extend(functions);
        }
    }
    plan.rewritten = lash_vm_runtime::process::qualify_correspondence(
        plan.rewritten,
        predecessor,
        &content.links,
        &retained_documents,
        &retained_code,
    )
    .map_err(|error| refusal(error.to_string()))?;
    let (mut staging, writes, membership, mut host) = {
        let mut decoder = GraphDecoder::new(&read.graph, read.read_limits);
        let mut host = super::host_graph::HostState::load(
            &mut decoder,
            read.body.host.ok_or("a parked cell has no host")?,
        )
        .map_err(|e| text(&e))?;
        let membership_id = read
            .body
            .effect_membership
            .ok_or("a parked cell has no membership")?;
        let (mut membership, _) = EffectMembership::load(
            &mut decoder,
            membership_id,
            snapshots.exec(),
            &super::host::DataProjections,
        )
        .map_err(|e| text(&e))?;
        let helpers = documents.helpers();
        let correspondence = &plan.rewritten.correspondence;
        let mut staging = GraphStaging::new(EncodeBudget::new(ExplicitLimit::new(u64::MAX)));
        let mut admissions = BTreeMap::new();
        let mut writes = Vec::new();
        for entry in &mut membership.entries {
            let old = entry.admission;
            let original_effect = entry.effect.clone();
            let mut admission: Admission = decoder
                .decode(old, NodeKind::ADMISSION)
                .map_err(|e| text(&e))?;
            admission.effect =
                lash_kernel_migrate::carry_effect(correspondence, &admission.effect, helpers)
                    .map_err(|e| refusal(e.to_string()))?;
            if let Some(use_) = admission.effect_name.as_mut() {
                *use_ = *uses
                    .uses
                    .get(use_)
                    .ok_or_else(|| refusal("an admitted operation has no successor use".into()))?;
            }
            entry.effect = admission.effect.clone();
            entry.admission = admission.stage(&mut staging).map_err(|e| text(&e))?;
            admissions.insert(old, entry.admission);
            writes.push(lash_durable::DomainWrite::EffectAdmission(
                lash_durable::domain::graph::EffectAdmissionWrite::Remove {
                    exec: snapshots.exec().clone(),
                    effect: lash_vm_broker::kernel::graph::effect_key(&original_effect),
                    expected: old,
                },
            ));
            writes.push(admission.write(entry.admission));
        }
        membership =
            EffectMembership::new(membership.next_park, membership.growth, membership.entries)
                .map_err(|e| text(&e))?;
        for ordinal in 0..host.calls.len() {
            let mut call = host.calls.items()[ordinal as usize].clone();
            if !matches!(call.status, super::host_graph::CallStatus::Pending) {
                continue;
            }
            call.effect = lash_kernel_migrate::carry_effect(correspondence, &call.effect, helpers)
                .map_err(|e| refusal(e.to_string()))?;
            call.operation = *uses
                .uses
                .get(&call.operation)
                .ok_or_else(|| refusal("a recorded operation has no successor use".into()))?;
            // Completed archival admissions stay immutable. Pending admissions are
            // shared with the carried membership, selected by their identity.
            if let Some(next) = admissions.get(&call.admission) {
                call.admission = *next;
            }
            host.calls.replace(call.ordinal, call);
        }
        (staging, writes, membership, host)
    };
    let correspondence = &plan.rewritten.correspondence;
    envelope.source_map = envelope
        .source_map
        .as_ref()
        .map(|map| {
            map.rekey(correspondence)
                .ok_or_else(|| refusal("the source map has no successor correspondence".into()))
        })
        .transpose()?;
    let mut written = BTreeMap::new();
    for (site, value) in &envelope.written {
        let next = correspondence
            .successor(site)
            .ok_or_else(|| refusal("a Written site has no successor correspondence".into()))?;
        written.insert(next.clone(), value.clone());
    }
    // Existing function values keep their predecessor sites and Written source.
    written.extend(envelope.written.clone());
    envelope.written = written;
    // The owner retains the exact defining DAG and the explicit successor before
    // any root can name it. A refusal so far has changed no stored object.
    let claim = ctx.execution_claim().map_err(|e| text(&e))?;
    documents
        .hold_format_successor(&claim, predecessor, &next_document)
        .await
        .map_err(|e| text(&e))?;
    documents
        .hold(&claim, &ContentRef::Link(predecessor))
        .await
        .map_err(|e| text(&e))?;
    let engine = lash_vm_runtime::KernelProcessEngine::new(
        documents.clone(),
        &envelope.dialect,
        services.workers.clone(),
        super::cell_bounds(services),
    )
    .map_err(|error| match error {
        lash_vm_runtime::KernelProcessSetupError::Library(error) => text(&error),
        lash_vm_runtime::KernelProcessSetupError::Worker(error) => Error::Worker(error),
    })?;
    let start = |link| lash_vm_runtime::process::InlineRequest::Linked {
        start: lash_vocabulary::LinkedStart {
            link,
            selector: lash_vocabulary::StartSelector::Main,
        },
    };
    let prepared = engine
        .prepare_checkpoint_carry(
            stores.as_ref(),
            snapshots.exec(),
            read.body.state,
            &start(predecessor),
            &start(successor),
            &plan.rewritten,
            &envelope.environment.retained_code,
            &uses,
        )
        .await
        .map_err(|error| match error {
            lash_kernel_migrate::CarryFault::Refused(cause) => {
                Error::Migration(lash_vm_runtime::KernelMigrationRefusal::Parked {
                    document: envelope.document.id(),
                    refusal: cause,
                })
            }
            other => text(&other),
        })?;
    environment.retained_code.extend(prepared.retained);
    environment.requirements.machine = prepared.machine;
    envelope.document = next_document;
    envelope.link = successor;
    let mut inspection = documents
        .inspect_link(
            successor,
            &ReachSelector::Start(lash_vocabulary::StartSelector::Main),
        )
        .await
        .map_err(|e| text(&e))?;
    inspection
        .operations
        .extend(envelope.inspection.operations.clone());
    inspection
        .artifacts
        .extend(envelope.inspection.artifacts.clone());
    inspection.machine = environment.requirements.machine.clone();
    envelope.inspection = inspection;
    envelope.environment = environment;
    let envelope_id = envelope.stage(&mut staging).map_err(|e| text(&e))?;
    let host_id = host
        .stage(&mut staging, &mut Default::default())
        .map_err(|e| text(&e))?;
    let (_root, write) = stage_exec_root(
        &mut staging,
        snapshots.exec(),
        Some(&membership),
        RootParts {
            state: prepared.root,
            status: read.body.status,
            executable_identity: envelope.document.id().to_string(),
            format: plan.to.number(),
            host: Some(host_id),
            envelope: Some(envelope_id),
            owed: read.body.owed.clone(),
            requirements: envelope.environment.requirements.clone(),
        },
        (Some(read.held.rev), Some(read.held.root)),
        CommitId::mint(),
    )
    .map_err(|e| text(&e))?;
    let mut tx = ctx.actor_context().begin().await.map_err(|e| text(&e))?;
    tx.stage(prepared.nodes);
    tx.stage_all(staging);
    for write in writes {
        tx.write(write);
    }
    tx.write(write);
    tx.adopt_execution(lash_core::facade_support::registration(
        execution.clone(),
        &envelope.inspection,
        claim.referrer(),
        envelope.environment.requirements.clone(),
        envelope.environment.registry,
    ));
    for (use_, entry) in adopted.resolutions {
        let requested = content.documents[&content.links[&use_.link].document].imports[&use_.slot]
            .name
            .clone();
        tx.seed_resolution(lash_core_store::resolutions::ResolutionRecord {
            key: lash_core_store::resolutions::ResolutionKey {
                execution: execution.clone(),
                use_,
            },
            requested,
            entry,
        });
    }
    let owing = lash_core::ExecutionDebts {
        exec: snapshots.exec().clone(),
        owed: read.body.owed.clone(),
        requirements: envelope.environment.requirements.clone(),
    };
    tx.require(read.held.root);
    tx.stamp_formats(ctx.actor_context().backend().formats().session().clone());
    ctx.actor_context()
        .commit_owing(tx, lash_durable::CommitLabel::new("cell.successor"), &owing)
        .await
        .map_err(|e| text(&e))?;
    snapshots.invalidate_root();
    Ok(())
}
/// Refusals keep their serialized cause across the generic plugin boundary;
/// infrastructure aborts the attempt without recording a cell outcome.
#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    #[error(transparent)]
    Worker(lash_vm_client::PoolError),
    #[error(transparent)]
    Migration(lash_vm_runtime::KernelMigrationRefusal),
    #[error("{0}")]
    Host(String),
}
impl From<String> for Error {
    fn from(message: String) -> Self {
        Self::Host(message)
    }
}
impl From<&str> for Error {
    fn from(message: &str) -> Self {
        Self::Host(message.into())
    }
}
impl Error {
    pub(crate) fn into_runtime(self) -> lash_core::RuntimeEffectControllerError {
        use lash_protocol_types::{PluginFailureClass, PluginOperationFailure};
        if let Self::Worker(error) = self {
            return lash_core::RuntimeEffectControllerError::from(error.into_runtime_error());
        }

        let code = lash_core::RuntimeErrorCode::ExecutionStateCaptureFailed;
        let message = self.to_string();
        let typed = match &self {
            Self::Migration(refusal) => Some((
                "lash.kernel.migration",
                serde_json::to_value(refusal),
                PluginFailureClass::Parked,
            )),
            _ => None,
        };
        let mut error = lash_core::RuntimeError::new(code.clone(), message.clone());
        if let Some((error_type, payload, class)) = typed {
            match payload {
                Ok(payload) => {
                    error.cause = Some(lash_core::RuntimeErrorCause::PluginOperation {
                        failure: Box::new(PluginOperationFailure {
                            error_type: error_type.into(),
                            error_version: std::num::NonZeroU32::MIN,
                            payload,
                            class,
                            code: (&code).into(),
                            message,
                            origin: None,
                        }),
                    })
                }
                Err(fault) => {
                    return lash_core::RuntimeEffectControllerError::new(
                        lash_core::RuntimeErrorCode::RecordEncodingFailed,
                        fault.to_string(),
                    );
                }
            }
        }
        lash_core::RuntimeEffectControllerError::from(error).retryable_uncommitted_derivation()
    }
}

#[cfg(test)]
mod tests {
    /// A semantic refusal keeps its discriminant and detail through the
    /// plugin payload; it parks the turn instead of becoming a cell result.
    #[test]
    fn a_linked_migration_refusal_keeps_its_typed_cause_at_the_plugin_boundary() {
        let refusal = lash_vm_runtime::KernelMigrationRefusal::NotAdmitted {
            document: lash_vocabulary::DocumentId::from_bytes([1; 32]),
            reason: "linked carry has no successor Link correspondence".into(),
        };
        let error = super::Error::Migration(refusal.clone()).into_runtime();
        let Some(lash_core::RuntimeErrorCause::PluginOperation { failure }) = error.cause else {
            panic!("{error:?}");
        };
        assert_eq!(
            failure.class,
            lash_protocol_types::PluginFailureClass::Parked
        );
        assert_eq!(
            serde_json::from_value::<lash_vm_runtime::KernelMigrationRefusal>(failure.payload)
                .unwrap(),
            refusal
        );
    }
}

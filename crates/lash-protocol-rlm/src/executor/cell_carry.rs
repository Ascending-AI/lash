//! A parked cell's state, typed Written table and admissions move in one
//! fenced owner transaction. Requests and committed results stay recorded.
use super::envelope::CellEnvelope;
use super::host_graph::HostState;
use lash_core_store::checkpoint_graph::bodies::ExecStatus;
use lash_core_store::checkpoint_graph::{
    CommitId, EncodeBudget, ExplicitLimit, GraphDecoder, GraphStaging, NodeKind,
};
use lash_durable::domain::graph::{EffectAdmissionWrite, ExecRootWrite};
use lash_durable::{CommitLabel, DomainWrite};
use lash_kernel_doc::{IdentifiedDocument, KernelVersion};
use lash_vm_broker::kernel::graph::{Admission, EffectMembership, effect_key};
use std::collections::BTreeMap;
use std::sync::Arc;

pub(super) async fn carry(
    snapshots: &lash_vm_broker::DurableSnapshotStore,
    kernel: &super::KernelCarry,
    bounds: lash_kernel_vm::Bounds,
) -> Result<(), Error> {
    let Some(read) = snapshots
        .latest_root()
        .await
        .map_err(|error| error.to_string())?
    else {
        return Ok(());
    };
    if read.body.status != ExecStatus::Parked {
        return Ok(());
    }
    let mut envelope = {
        let mut decoder = GraphDecoder::new(&read.graph, read.read_limits);
        CellEnvelope::load(
            &mut decoder,
            read.body.envelope.ok_or("a parked cell has no envelope")?,
        )
        .map_err(|error| error.to_string())?
    };
    if envelope.document.manifest.kernel >= kernel.writes().number() {
        return Ok(());
    }
    let cx = snapshots.context();
    if !cx
        .carries_sessions()
        .await
        .map_err(|error| error.to_string())?
    {
        return Ok(());
    }
    let documents = lash_vm_runtime::KernelDocuments::new(cx.backend().module_artifacts());
    let held = kernel.library().map_err(Error::Migration)?;
    let stored = documents
        .stored_definitions(&envelope.document, &held)
        .await
        .map_err(|error| error.to_string())?
        .map_err(|error| {
            Error::Carry(lash_kernel_migrate::CarryFault::Refused(
                lash_kernel_migrate::ParkedRefusal::StateNotCarried {
                    detail: format!("pinned snapshot {}: {error:?}", envelope.packages),
                },
            ))
        })?;
    let old_closure = lash_vm_broker::kernel::closure::derive(
        envelope.document.manifest.functions.keys().copied(),
        &held,
        &stored,
    )
    .map_err(|error| error.to_string())?;
    let packages = kernel.packages()?;
    lash_vm_runtime::carry_snapshot(
        &lash_vm_runtime::StoredAuthority::new(cx.backend().stores()),
        &packages,
        envelope.packages,
        envelope.document.manifest.functions.keys().copied(),
    )
    .await
    .map_err(Error::Carry)?;
    let pinned_kernel = envelope.document.manifest.kernel;
    let mut functions = lash_vm_broker::kernel::loader::declare(
        envelope.document.manifest.functions.keys().copied(),
        envelope.document.manifest.kernel,
        &held,
        &stored,
    )
    .map_err(|error| error.to_string())?;
    let migration = KernelVersion::of(envelope.document.manifest.kernel)
        .and_then(lash_kernel_migrate::migration_from)
        .ok_or("no kernel migration carries the cell")?;
    lash_kernel_migrate::migrate_registry(&mut functions, migration)
        .map_err(|error| error.to_string())?;
    let plan = lash_vm_runtime::plan_migration(&envelope.document, &functions)
        .map_err(Error::Migration)?
        .ok_or("no cell rewrite")?;
    let new_stored = plan
        .rewritten
        .document
        .manifest
        .functions
        .keys()
        .map(|function| {
            functions
                .get(function)
                .map(|held| (*function, Arc::clone(held.definition())))
                .ok_or_else(|| {
                    format!(
                        "pinned snapshot {} lacks rewritten function {function}",
                        envelope.packages
                    )
                })
        })
        .collect::<Result<BTreeMap<_, _>, _>>()?;
    lash_vm_runtime::admit_internal(
        &lash_vm_runtime::StoredAuthority::new(cx.backend().stores()),
        &new_stored,
    )
    .await
    .map_err(|error| error.to_string())?
    .map_err(|error| error.to_string())?;
    let declared = lash_vm_broker::kernel::loader::declare(
        plan.rewritten.document.manifest.functions.keys().copied(),
        plan.rewritten.document.manifest.kernel,
        &held,
        &new_stored,
    )
    .map_err(|error| error.to_string())?;
    let closure = lash_vm_broker::kernel::closure::derive(
        plan.rewritten.document.manifest.functions.keys().copied(),
        &held,
        &new_stored,
    )
    .map_err(|error| error.to_string())?;
    let program = lash_kernel_vm::Program {
        closure: lash_kernel_doc::heap::PageId(closure.reference().0),
        document: Arc::new(plan.rewritten.document.clone()),
        library: lash_kernel_vm::PreparedLibrary::new(Arc::new(declared)),
    };
    let pages = cx.pages().map_err(|error| error.to_string())?;
    let (page, nodes) = lash_vm_runtime::carry_parked_checkpoint(
        pages,
        lash_durable::domain::pages::RootLeaseRequest {
            owner: lash_durable::domain::RootOwner::Exec(snapshots.exec().clone()),
            expected: read.held.root,
            fence: lash_durable::domain::pages::LeaseFence::Actor {
                actor: cx.actor().clone(),
                epoch: cx.epoch(),
            },
            ttl_millis: 600_000,
        },
        read.body.state,
        plan.rewritten.clone(),
        program,
        bounds,
        (
            pinned_kernel,
            lash_kernel_doc::heap::PageId(old_closure.reference().0),
        ),
    )
    .await
    .map_err(Error::Carry)?;
    let owing;
    let mut tx = cx.begin().await.map_err(|error| error.to_string())?;
    tx.stamp_formats(cx.backend().formats().session().clone());
    {
        let mut decoder = GraphDecoder::new(&read.graph, read.read_limits);
        let c = &plan.rewritten.correspondence;
        let identify =
            |effect: &lash_vocabulary::sites::EffectId| -> Result<lash_vocabulary::sites::EffectId, String> {
                lash_kernel_migrate::carry_effect(c, effect).map_err(|error| error.to_string())
            };
        let mut staging = GraphStaging::new(EncodeBudget::new(ExplicitLimit::new(u64::MAX)));
        for node in nodes {
            staging.adopt(node).map_err(|error| error.to_string())?;
        }
        for node in closure.nodes().values() {
            staging
                .adopt(node.clone())
                .map_err(|error| error.to_string())?;
        }
        envelope.written = std::mem::take(&mut envelope.written)
            .into_iter()
            .map(|(site, written)| {
                c.successor(&site)
                    .cloned()
                    .map(|site| (site, written))
                    .ok_or_else(|| format!("Written site {site} has no successor"))
            })
            .collect::<Result<_, _>>()?;
        envelope.source_map = envelope
            .source_map
            .map(|map| map.rekey(c).ok_or("source map does not rekey"))
            .transpose()?;
        envelope.document = IdentifiedDocument::new(plan.rewritten.document);
        let new_envelope = envelope
            .stage(&mut staging)
            .map_err(|error| error.to_string())?;
        let mut membership: EffectMembership = decoder
            .decode(
                read.body.effect_membership.ok_or("no membership")?,
                NodeKind::EFFECT_MEMBERSHIP,
            )
            .map_err(|error| error.to_string())?;
        let mut admissions = BTreeMap::new();
        // Delete every old lookup before inserting successors, including site swaps.
        for entry in &mut membership.entries {
            let mut admission: Admission = decoder
                .decode(entry.admission, NodeKind::ADMISSION)
                .map_err(|error| error.to_string())?;
            tx.write(DomainWrite::EffectAdmission(EffectAdmissionWrite::Remove {
                exec: snapshots.exec().clone(),
                effect: effect_key(&entry.effect),
                expected: entry.admission,
            }));
            admission.effect = identify(&entry.effect)?;
            let node = admission
                .stage(&mut staging)
                .map_err(|error| error.to_string())?;
            admissions.insert(entry.admission, (admission.effect.clone(), node, admission));
            entry.effect = identify(&entry.effect)?;
            entry.admission = node;
        }
        for (_, node, admission) in admissions.values() {
            tx.write(admission.write(*node));
        }
        membership.entries.sort_by(|a, b| a.effect.cmp(&b.effect));
        let new_membership = membership
            .stage(&mut staging)
            .map_err(|error| error.to_string())?;
        let mut host = HostState::load(&mut decoder, read.body.host.ok_or("no host")?)
            .map_err(|error| error.to_string())?;
        for ordinal in 0..host.calls.len() {
            let mut summary = host
                .calls
                .get(ordinal)
                .ok_or("missing call summary")?
                .clone();
            if let Some((effect, admission, _)) = admissions.get(&summary.admission) {
                summary.effect = effect.clone();
                summary.admission = *admission;
                host.calls.replace(ordinal, summary);
            }
        }
        let mut required = Default::default();
        let host = host
            .stage(&mut staging, &mut required)
            .map_err(|error| error.to_string())?;
        let mut body = read.body.clone();
        body.state = page;
        body.format = envelope.document.manifest.kernel;
        body.executable_identity = envelope.document.id().to_string();
        body.envelope = Some(new_envelope);
        body.host = Some(host);
        body.effect_membership = Some(new_membership);
        body.requirements =
            lash_vm_broker::kernel::loader::requirements(&closure, body.format, std::iter::empty());
        // The linked snapshot and links stay pinned across a version carry.
        body.requirements
            .providers
            .clone_from(&read.body.requirements.providers);
        body.requirements
            .codecs
            .clone_from(&read.body.requirements.codecs);
        owing = lash_core::ExecutionDebts {
            exec: snapshots.exec().clone(),
            owed: body.owed.clone(),
            requirements: body.requirements.clone(),
        };
        let root = staging
            .encode_value(NodeKind::EXEC_ROOT, &body)
            .map_err(|error| error.to_string())?;
        tx.stage_all(staging);
        for node in required {
            tx.require(node);
        }
        tx.write(DomainWrite::ExecRoot(ExecRootWrite::Put {
            exec: snapshots.exec().clone(),
            expected: Some(read.held.rev),
            expected_root: Some(read.held.root),
            root,
            commit_id: CommitId::mint(),
        }));
    }
    cx.commit_owing(tx, CommitLabel::CELL_SNAPSHOT_ADMIT, &owing)
        .await
        .map_err(|error| error.to_string())?;
    snapshots.invalidate_root();
    Ok(())
}

/// Refusals keep their serialized cause across the generic plugin boundary;
/// infrastructure aborts the attempt without recording a cell outcome.
#[derive(Debug, thiserror::Error)]
pub(super) enum Error {
    #[error(transparent)]
    Carry(lash_kernel_migrate::CarryFault),
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
    pub(super) fn into_runtime(self) -> lash_core::RuntimeEffectControllerError {
        use lash_protocol_types::{PluginFailureClass, PluginOperationFailure};
        let code = lash_core::RuntimeErrorCode::ExecutionStateCaptureFailed;
        let message = self.to_string();
        let typed = match &self {
            Self::Carry(lash_kernel_migrate::CarryFault::Refused(refusal)) => Some((
                "lash.kernel.parked_carry",
                serde_json::to_value(refusal),
                PluginFailureClass::Parked,
            )),
            Self::Migration(refusal) => Some((
                "lash.kernel.migration",
                serde_json::to_value(refusal),
                PluginFailureClass::Parked,
            )),
            Self::Carry(lash_kernel_migrate::CarryFault::Heap(fault)) => {
                use lash_kernel_doc::heap::HeapFault;
                let reason = match fault {
                    HeapFault::Missing { .. } => "missing",
                    HeapFault::Corrupt { .. } => "corrupt",
                    HeapFault::Unauthorized { .. } => "unauthorized",
                    HeapFault::LeaseLost => "lease_lost",
                    HeapFault::Exhausted { .. } => "exhausted",
                    HeapFault::Transport(_) => "transport",
                    HeapFault::Malformed(_) => "malformed",
                    _ => "unknown",
                };
                Some((
                    "lash.kernel.heap",
                    Ok(serde_json::json!({"reason": reason, "detail": fault.to_string()})),
                    PluginFailureClass::Redrivable,
                ))
            }
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
    fn a_cell_carry_refusal_keeps_its_typed_cause_at_the_plugin_boundary() {
        let refusal = lash_kernel_migrate::ParkedRefusal::StateNotCarried {
            detail: "pinned snapshot R1 lacks function f".into(),
        };
        let error = super::Error::Carry(refusal.clone().into()).into_runtime();
        let Some(lash_core::RuntimeErrorCause::PluginOperation { failure }) = error.cause else {
            panic!("{error:?}");
        };
        assert_eq!(
            failure.class,
            lash_protocol_types::PluginFailureClass::Parked
        );
        assert_eq!(
            serde_json::from_value::<lash_kernel_migrate::ParkedRefusal>(failure.payload).unwrap(),
            refusal
        );
    }
}

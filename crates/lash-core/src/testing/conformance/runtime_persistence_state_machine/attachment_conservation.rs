//! Conservation checks for committed attachment references and physical blobs.
//!
//! The model records only references confirmed by successful production
//! `RuntimeCommit`s. The oracle independently asks the factory-wide root-set
//! APIs what survived, so a backend that drops a commit ref or inherits the
//! blanket empty root-set default cannot agree with the model by construction.

use super::*;

const RECONCILE_SQL_SAFE_MAX: u64 = i64::MAX as u64;

/// Production handles required by the runtime-persistence state machine.
///
/// Attachment laws deliberately compose the runtime commit boundary with the
/// factory-wide root oracle and a real blob backend instead of substituting a
/// test-only manifest.
pub struct RuntimePersistenceStateMachineHandles {
    pub(super) runtime: Arc<dyn RuntimePersistence>,
    pub(super) session_factory: Arc<dyn crate::SessionStoreFactory>,
    pub(super) attachment_backend: Arc<dyn crate::AttachmentStore>,
    pub(super) process_owner_liveness_wired: bool,
}

impl RuntimePersistenceStateMachineHandles {
    /// Create the state machine's primary session through the same factory that
    /// supplies its attachment root-set oracle.
    pub async fn create(
        session_factory: Arc<dyn crate::SessionStoreFactory>,
        process_owner_liveness_wired: bool,
    ) -> Result<Self, crate::StoreError> {
        let attachment_backend = Arc::new(crate::InMemoryAttachmentStore::new());
        let runtime = session_factory
            .create_store(&super::session_store_request(
                SESSION_ID,
                "runtime-persistence-model",
                crate::SessionRelation::Root,
            ))
            .await?;
        Ok(Self {
            runtime,
            session_factory,
            attachment_backend,
            process_owner_liveness_wired,
        })
    }
}

pub(super) struct ModeledAttachmentSession {
    session_id: String,
    head_revision: u64,
    committed_refs: BTreeSet<crate::AttachmentId>,
    last_commit: RuntimeCommit,
}

struct AttachmentCommitInput {
    new_session: bool,
    session_selection: u8,
    attachment_slot: u8,
    value: u8,
    turn_owned: bool,
}

pub(super) async fn apply_attachment_operation(
    handles: &RuntimePersistenceStateMachineHandles,
    model: &mut ReferenceModel,
    shape: &mut RunShape,
    seed: u64,
    operation: &RuntimePersistenceOp,
) -> Result<(), String> {
    match operation {
        RuntimePersistenceOp::CommitWithAttachmentRefs {
            new_session,
            session_selection,
            attachment_slot,
            value,
            turn_owned,
        } => {
            commit_with_attachment_refs(
                handles,
                model,
                shape,
                seed,
                AttachmentCommitInput {
                    new_session: *new_session,
                    session_selection: *session_selection,
                    attachment_slot: *attachment_slot,
                    value: *value,
                    turn_owned: *turn_owned,
                },
            )
            .await
        }
        RuntimePersistenceOp::PutAttachmentIntent {
            owner_kind,
            attachment_slot,
            value,
        } => {
            put_attachment_intent(
                handles,
                model,
                shape,
                seed,
                *owner_kind,
                *attachment_slot,
                *value,
            )
            .await
        }
        RuntimePersistenceOp::ReplayAttachmentCommit { selection } => {
            replay_attachment_commit(handles, model, shape, *selection).await
        }
        RuntimePersistenceOp::ReclaimAttachmentSession { selection } => {
            reclaim_attachment_session(handles, model, shape, *selection).await
        }
        RuntimePersistenceOp::ProbeAttachmentGc => probe_attachment_gc(handles, model, shape).await,
        _ => Err("non-attachment operation reached attachment dispatcher".to_string()),
    }
}

async fn commit_with_attachment_refs(
    handles: &RuntimePersistenceStateMachineHandles,
    model: &mut ReferenceModel,
    shape: &mut RunShape,
    seed: u64,
    input: AttachmentCommitInput,
) -> Result<(), String> {
    let AttachmentCommitInput {
        new_session,
        session_selection,
        attachment_slot,
        value,
        turn_owned,
    } = input;
    let create = new_session || model.attachment_sessions.is_empty();
    let (session_index, session_id, head_revision, store) = if create {
        model.attachment_session_sequence += 1;
        let session_id = format!(
            "runtime-persistence-attachment-{seed}-{}",
            model.attachment_session_sequence
        );
        let store = handles
            .session_factory
            .create_store(&session_request(&session_id))
            .await
            .map_err(|error| error.to_string())?;
        (None, session_id, 0, store)
    } else {
        let index = usize::from(session_selection) % model.attachment_sessions.len();
        let session = &model.attachment_sessions[index];
        let store = open_session(handles, &session.session_id).await?;
        (
            Some(index),
            session.session_id.clone(),
            session.head_revision,
            store,
        )
    };

    let facade = Arc::new(crate::SessionAttachmentStore::new(
        Arc::clone(&handles.attachment_backend),
        Arc::new(crate::attachments::PersistenceManifestAdapter(Arc::clone(
            &store,
        ))),
        session_id.clone(),
    ));
    let turn_id = format!("attachment-turn:{seed}:{session_id}:{head_revision}");
    let _owner_binding = turn_owned.then(|| facade.bind_turn_scoped(turn_id.clone()));
    let attachment = facade
        .put(
            attachment_bytes(attachment_slot, value),
            attachment_meta(attachment_slot, value)?,
        )
        .await
        .map_err(|error| error.to_string())?;

    let mut state = RuntimeSessionState {
        session_id: session_id.clone(),
        head_revision,
        ..RuntimeSessionState::default()
    };
    let operation = if turn_owned {
        crate::OperationId::turn(&session_id, turn_id, "commit-with-attachment-refs")
    } else {
        crate::OperationId::new(
            crate::ExecutionScope::runtime_operation(format!(
                "attachment-conservation:{seed}:{session_id}:{head_revision}"
            )),
            "commit-with-attachment-refs",
        )
    };
    let (commit, _) =
        RuntimeCommit::persisted_state_with_operation_and_staged_usage(&mut state, &[], operation)
            .map_err(|error| error.to_string())?;
    let commit = if turn_owned {
        commit
    } else {
        commit.with_committed_attachments([attachment.id.clone()])
    };
    let result = store
        .commit_runtime_state(commit.clone())
        .await
        .map_err(|error| error.to_string())?;
    if result.receipt_replayed {
        return Err("fresh attachment commit unexpectedly replayed a receipt".to_string());
    }
    if result.head_revision != head_revision + 1 {
        return Err(format!(
            "attachment commit advanced head {head_revision} -> {}",
            result.head_revision
        ));
    }
    if turn_owned
        && store
            .list_uncommitted(RECONCILE_SQL_SAFE_MAX)
            .map_err(|error| error.to_string())?
            .iter()
            .any(|entry| entry.attachment_id == attachment.id)
    {
        return Err("turn commit did not stamp its owner-bound attachment intent".to_string());
    }

    model.known_attachment_ids.insert(attachment.id.clone());
    if let Some(index) = session_index {
        let session = &mut model.attachment_sessions[index];
        session.head_revision = result.head_revision;
        session.committed_refs.insert(attachment.id);
        session.last_commit = commit;
    } else {
        model.attachment_sessions.push(ModeledAttachmentSession {
            session_id,
            head_revision: result.head_revision,
            committed_refs: BTreeSet::from([attachment.id]),
            last_commit: commit,
        });
    }
    shape.attachment_commits += 1;
    Ok(())
}

async fn put_attachment_intent(
    handles: &RuntimePersistenceStateMachineHandles,
    model: &mut ReferenceModel,
    _shape: &mut RunShape,
    seed: u64,
    owner_kind: u8,
    attachment_slot: u8,
    value: u8,
) -> Result<(), String> {
    model.attachment_session_sequence += 1;
    let session_id = format!(
        "runtime-persistence-intent-{seed}-{}",
        model.attachment_session_sequence
    );
    let store = handles
        .session_factory
        .create_store(&session_request(&session_id))
        .await
        .map_err(|error| error.to_string())?;
    let facade = Arc::new(crate::SessionAttachmentStore::new(
        Arc::clone(&handles.attachment_backend),
        Arc::new(crate::attachments::PersistenceManifestAdapter(store)),
        session_id,
    ));
    let owner_id = format!(
        "attachment-intent-owner:{seed}:{}",
        model.attachment_session_sequence
    );
    let _owner_binding = match owner_kind % 3 {
        1 => Some(facade.bind_turn_scoped(owner_id)),
        2 => Some(facade.bind_process_scoped(owner_id)),
        _ => None,
    };
    let attachment = facade
        .put(
            attachment_bytes(attachment_slot, value),
            attachment_meta(attachment_slot, value)?,
        )
        .await
        .map_err(|error| error.to_string())?;
    model.known_attachment_ids.insert(attachment.id.clone());
    let remains_live = match owner_kind % 3 {
        1 => true,
        2 => !handles.process_owner_liveness_wired,
        _ => false,
    };
    if remains_live {
        model.live_uncommitted_attachment_refs.insert(attachment.id);
    }
    Ok(())
}

async fn replay_attachment_commit(
    handles: &RuntimePersistenceStateMachineHandles,
    model: &ReferenceModel,
    shape: &mut RunShape,
    selection: u8,
) -> Result<(), String> {
    if model.attachment_sessions.is_empty() {
        return Ok(());
    }
    let index = usize::from(selection) % model.attachment_sessions.len();
    let session = &model.attachment_sessions[index];
    let before = handles
        .session_factory
        .live_attachment_refs(RECONCILE_SQL_SAFE_MAX)
        .await
        .map_err(|error| error.to_string())?;
    let result = open_session(handles, &session.session_id)
        .await?
        .commit_runtime_state(session.last_commit.clone())
        .await
        .map_err(|error| error.to_string())?;
    if !result.receipt_replayed {
        return Err("attachment commit replay did not return its durable receipt".to_string());
    }
    if result.head_revision != session.head_revision {
        return Err("attachment commit replay changed its committed head".to_string());
    }
    let after = handles
        .session_factory
        .live_attachment_refs(RECONCILE_SQL_SAFE_MAX)
        .await
        .map_err(|error| error.to_string())?;
    if after != before {
        return Err("attachment commit replay changed the live root set".to_string());
    }
    shape.attachment_receipt_replays += 1;
    Ok(())
}

async fn reclaim_attachment_session(
    handles: &RuntimePersistenceStateMachineHandles,
    model: &mut ReferenceModel,
    shape: &mut RunShape,
    selection: u8,
) -> Result<(), String> {
    if model.attachment_sessions.is_empty() {
        return Ok(());
    }
    let index = usize::from(selection) % model.attachment_sessions.len();
    let session = model.attachment_sessions.remove(index);
    handles
        .session_factory
        .delete_session(&session.session_id)
        .await?;
    shape.attachment_session_reclaims += 1;
    Ok(())
}

async fn probe_attachment_gc(
    handles: &RuntimePersistenceStateMachineHandles,
    model: &ReferenceModel,
    shape: &mut RunShape,
) -> Result<(), String> {
    let expected = expected_live_refs(model);
    let roots = handles
        .session_factory
        .live_attachment_refs(RECONCILE_SQL_SAFE_MAX)
        .await
        .map_err(|error| error.to_string())?;
    if !expected.is_empty() && roots.is_empty() {
        return Err(
            "attachment GC observed an empty root set while committed references survive"
                .to_string(),
        );
    }
    if roots != expected {
        return Err(format!(
            "attachment GC root snapshot differs from surviving committed references: expected {expected:?}, got {roots:?}"
        ));
    }
    let before = backend_ids(handles).await?;
    let expected_reclaimed = before.difference(&expected).count();
    let report = crate::reclaim_unreferenced_attachments(
        handles.session_factory.as_ref(),
        handles.attachment_backend.as_ref(),
        0,
    )
    .await
    .map_err(|error| error.to_string())?;
    if report.reclaimed_count != expected_reclaimed {
        return Err(format!(
            "attachment GC reclaimed {} blobs, expected {expected_reclaimed}: {report:?}",
            report.reclaimed_count
        ));
    }
    if !report.failed_ids.is_empty() || !report.deleted_while_referenced.is_empty() {
        return Err(format!(
            "attachment GC reported an unsafe or failed deletion: {report:?}"
        ));
    }
    let after = backend_ids(handles).await?;
    if after != expected {
        return Err(format!(
            "attachment GC physical survivors differ from the live root set: expected {expected:?}, got {after:?}"
        ));
    }
    shape.attachment_gc_probes += 1;
    Ok(())
}

pub(super) async fn assert_attachment_conservation(
    handles: &RuntimePersistenceStateMachineHandles,
    model: &ReferenceModel,
) -> Result<(), String> {
    let expected = expected_live_refs(model);
    let actual = handles
        .session_factory
        .live_attachment_refs(RECONCILE_SQL_SAFE_MAX)
        .await
        .map_err(|error| error.to_string())?;
    if !expected.is_empty() && actual.is_empty() {
        return Err(
            "live attachment root set was empty while committed references survive".to_string(),
        );
    }
    if actual != expected {
        return Err(format!(
            "live attachment roots differ from the union of surviving committed references: expected {expected:?}, got {actual:?}"
        ));
    }

    for attachment_id in &model.known_attachment_ids {
        let expected_live = expected.contains(attachment_id);
        let targeted = handles
            .session_factory
            .has_live_attachment_ref(attachment_id, RECONCILE_SQL_SAFE_MAX)
            .await
            .map_err(|error| error.to_string())?;
        if targeted != expected_live {
            return Err(format!(
                "targeted attachment root probe disagreed for {attachment_id}: expected {expected_live}, got {targeted}"
            ));
        }
        if expected_live {
            handles
                .attachment_backend
                .get(attachment_id)
                .await
                .map_err(|error| {
                    format!(
                        "referenced attachment {attachment_id} disappeared while its committing session survives: {error}"
                    )
                })?;
        }
    }
    Ok(())
}

fn expected_live_refs(model: &ReferenceModel) -> BTreeSet<crate::AttachmentId> {
    let mut refs: BTreeSet<_> = model
        .attachment_sessions
        .iter()
        .flat_map(|session| session.committed_refs.iter().cloned())
        .collect();
    refs.extend(model.live_uncommitted_attachment_refs.iter().cloned());
    refs
}

async fn open_session(
    handles: &RuntimePersistenceStateMachineHandles,
    session_id: &str,
) -> Result<Arc<dyn RuntimePersistence>, String> {
    handles
        .session_factory
        .open_existing_store(&session_request(session_id))
        .await
        .map_err(|error| error.to_string())?
        .ok_or_else(|| format!("surviving attachment session `{session_id}` could not be reopened"))
}

async fn backend_ids(
    handles: &RuntimePersistenceStateMachineHandles,
) -> Result<BTreeSet<crate::AttachmentId>, String> {
    Ok(handles
        .attachment_backend
        .list()
        .await
        .map_err(|error| error.to_string())?
        .into_iter()
        .map(|blob| blob.id)
        .collect())
}

fn session_request(session_id: &str) -> crate::SessionStoreCreateRequest {
    super::session_store_request(
        session_id,
        "attachment-conservation-model",
        crate::SessionRelation::Root,
    )
}

fn attachment_bytes(slot: u8, value: u8) -> Vec<u8> {
    vec![b'F', b'I', b'G', 9, 2, 1, slot, value]
}

fn attachment_meta(slot: u8, value: u8) -> Result<crate::AttachmentCreateMeta, String> {
    Ok(crate::AttachmentCreateMeta::new(
        crate::MediaType::parse("application/octet-stream").map_err(|error| error.to_string())?,
        None,
        Some(format!("attachment-{slot}-{value}.bin")),
    ))
}

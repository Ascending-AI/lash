//! Durable actor-row parks for quiescent host tests.
use crate::durable::{
    ActorKey, ActorState, CommitLabel, DomainWrite, DurableError, Fenced, MailRefusal, NodeId,
    NodeSpec, Release, StoreFailure, StoreFailureKind,
};

/// Park a session's durable actor with its typed reason, after stopping its
/// serving nodes or otherwise quiescing execution. This is distinct from
/// [`LashSession::park`](crate::LashSession::park), which releases a live runtime.
///
/// Acquires a fresh test writer without stealing a claim, then commits the
/// reason and `Release::Parked` through the production epoch fence. It leaves
/// mail, turns and checkpoints intact. A racing control wake retains its
/// production semantics, so a successful commit need not stay parked.
///
/// # Errors
///
/// Missing/terminal actors and ownership conflicts retain their existing
/// typed durable refusals; store, lease and commit errors remain typed too.
pub async fn park_session(
    backend: &crate::Backend,
    session: &lash_kernel_value::identity::SessionId,
    reason: crate::SessionParkReason,
) -> Result<(), DurableError> {
    let actor = ActorKey::session(session.as_str()).map_err(invalid_actor)?;
    park(
        backend,
        actor,
        reason.encode(),
        CommitLabel::SESSION_RELEASE,
    )
    .await
}

/// Park a process's durable actor with its typed reason, once execution is
/// quiescent. Uses the same fenced test writer and control-wake semantics as
/// [`park_session`], without changing the process's execution or terminal state.
///
/// # Errors
///
/// Missing/terminal actors and live or racing owners are refused through the
/// existing typed durable errors. A store or commit failure remains typed.
pub async fn park_process(
    backend: &crate::Backend,
    process: &lash_vocabulary::identity::ProcessId,
    reason: crate::process::ProcessParkReason,
) -> Result<(), DurableError> {
    let actor = ActorKey::process(process.as_str()).map_err(invalid_actor)?;
    park(
        backend,
        actor,
        reason.encode(),
        CommitLabel::PROCESS_ADVANCE,
    )
    .await
}

fn invalid_actor(error: impl std::fmt::Display) -> DurableError {
    DurableError::Store(StoreFailure {
        kind: StoreFailureKind::Corrupt,
        message: error.to_string(),
    })
}

async fn park(
    backend: &crate::Backend,
    actor: ActorKey,
    reason_json: String,
    label: CommitLabel,
) -> Result<(), DurableError> {
    let store = backend.durable();
    let row = store
        .actor(&actor)
        .await?
        .ok_or_else(|| DurableError::MailRefused(MailRefusal::UnknownActor(actor.clone())))?;
    if row.state == ActorState::Terminal {
        return Err(DurableError::MailRefused(MailRefusal::ActorTerminal(actor)));
    }
    if row.state == ActorState::Owned {
        return Err(DurableError::OwnershipLost(Fenced {
            actor,
            held: row.epoch,
            current: Some(row.epoch),
        }));
    }
    let node = store
        .register_node(&NodeSpec {
            node: NodeId::new(format!("test-park-{}", uuid::Uuid::new_v4())),
            decodes: Vec::new(),
            serves: Vec::new(),
            capabilities: Default::default(),
            ttl_millis: backend.config().lease().ttl_millis(),
        })
        .await?;
    let result: Result<(), DurableError> = async {
        let epoch = store
            .acquire_idle_for_testing(&node, &actor, row.epoch)
            .await?;
        let mut tx = store.begin(&actor, epoch).await?;
        tx.write(DomainWrite::ActorPark {
            reason_json: Some(reason_json),
        });
        tx.give_up(Release::Parked);
        store.commit(tx, label).await?;
        Ok(())
    }
    .await;
    let release = store.release_node(&node).await;
    result?;
    release?;
    Ok(())
}

//! A deletion observer cannot mistake an accepted close for a live session
//! when the actor acknowledges the mail between the observer's reads.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use super::*;
use lash_core::durable_port::domain::{
    ExecKey, OwnerKey, ParkEventRow, ParkEventSeq, ProcessActorRow, RunRecordRow, ScopeKey,
    SessionCloseRow, SnapshotRow, TurnEnd, TurnRow, WaitId, WaitRow,
};
use lash_core::durable_port::{
    ActorCommit, ActorKey, ActorSnapshot, ActorTx, Claimed, CommitLabel, DurableError,
    DurableInstant, DurableReads, DurableStore, Epoch, HeartbeatOutcome, MailCommit, MailTx,
    NodeId, NodeLease, NodeSpec, Reaped,
};
use lash_core::testing::runtime_helpers::LayeredStores;
use lash_core::{ProcessId, TurnId};
use std::result::Result;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

struct CloseDuringRead {
    inner: Arc<dyn DurableStore>,
    actor: ActorKey,
    epoch: Epoch,
    armed: AtomicBool,
}

#[async_trait::async_trait]
impl DurableReads for CloseDuringRead {
    async fn turn(&self, session: &SessionId) -> Result<Option<TurnRow>, DurableError> {
        self.inner.turn(session).await
    }

    async fn turn_namespaces(
        &self,
        session: &SessionId,
        run: &TurnId,
    ) -> Result<Vec<lash_core::durable_port::domain::TurnNamespace>, DurableError> {
        self.inner.turn_namespaces(session, run).await
    }

    async fn turn_end(
        &self,
        session: &SessionId,
        run: &TurnId,
    ) -> Result<Option<TurnEnd>, DurableError> {
        self.inner.turn_end(session, run).await
    }

    async fn run_records(&self, owner: &OwnerKey) -> Result<Vec<RunRecordRow>, DurableError> {
        self.inner.run_records(owner).await
    }

    async fn snapshot(&self, exec: &ExecKey) -> Result<Option<SnapshotRow>, DurableError> {
        self.inner.snapshot(exec).await
    }

    async fn pending_waits(&self, owner: &ActorKey) -> Result<Vec<WaitRow>, DurableError> {
        self.inner.pending_waits(owner).await
    }

    async fn wait(&self, id: &WaitId) -> Result<Option<WaitRow>, DurableError> {
        self.inner.wait(id).await
    }

    async fn process(&self, process: &ProcessId) -> Result<Option<ProcessActorRow>, DurableError> {
        self.inner.process(process).await
    }

    async fn live_until_descendants(
        &self,
        scope: &ScopeKey,
        limit: usize,
    ) -> Result<Vec<ProcessId>, DurableError> {
        self.inner.live_until_descendants(scope, limit).await
    }

    async fn until_children(
        &self,
        scope: &ScopeKey,
        after: Option<&ProcessId>,
        limit: usize,
    ) -> Result<Vec<ProcessId>, DurableError> {
        self.inner.until_children(scope, after, limit).await
    }

    async fn session_close(
        &self,
        session: &SessionId,
    ) -> Result<Option<SessionCloseRow>, DurableError> {
        let observed = self.inner.session_close(session).await?;
        if self.armed.swap(false, Ordering::SeqCst) {
            assert!(observed.is_none(), "the close had not begun at this read");
            let mut tx = self.inner.begin(&self.actor, self.epoch).await?;
            lash_core::runtime::durable::session_close::begin_session_close(&mut tx, session);
            tx.ack_seen();
            self.inner
                .commit(tx, CommitLabel::SESSION_CLOSE_BEGIN)
                .await?;
        }
        Ok(observed)
    }

    async fn ending_scopes(&self, session: &SessionId) -> Result<Vec<ScopeKey>, DurableError> {
        self.inner.ending_scopes(session).await
    }

    async fn session_mailbox(
        &self,
        session: &SessionId,
    ) -> Result<lash_core::durable_port::domain::SessionMailbox, DurableError> {
        self.inner.session_mailbox(session).await
    }

    async fn park_events(
        &self,
        after: Option<ParkEventSeq>,
        limit: usize,
    ) -> Result<Vec<ParkEventRow>, DurableError> {
        self.inner.park_events(after, limit).await
    }

    async fn prompt_snapshot(
        &self,
        call: &lash_core::durable_port::domain::PromptCallKey,
    ) -> Result<Option<lash_core::durable_port::domain::PromptSnapshotRow>, DurableError> {
        self.inner.prompt_snapshot(call).await
    }

    async fn prompt_texts(
        &self,
        hashes: &[String],
    ) -> Result<Vec<lash_core::durable_port::domain::PromptText>, DurableError> {
        self.inner.prompt_texts(hashes).await
    }
}

#[async_trait::async_trait]
impl DurableStore for CloseDuringRead {
    async fn now(&self) -> Result<DurableInstant, DurableError> {
        self.inner.now().await
    }

    async fn register_node(&self, spec: &NodeSpec) -> Result<NodeLease, DurableError> {
        self.inner.register_node(spec).await
    }

    async fn heartbeat(&self, node: &NodeLease) -> Result<HeartbeatOutcome, DurableError> {
        self.inner.heartbeat(node).await
    }

    async fn reap(&self, reaper: &NodeLease) -> Result<Vec<Reaped>, DurableError> {
        self.inner.reap(reaper).await
    }

    async fn release_node(&self, node: &NodeLease) -> Result<Vec<ActorKey>, DurableError> {
        self.inner.release_node(node).await
    }

    async fn claim(&self, node: &NodeLease, limit: usize) -> Result<Vec<Claimed>, DurableError> {
        self.inner.claim(node, limit).await
    }

    async fn mark_draining(&self, node: &NodeLease) -> Result<(), DurableError> {
        self.inner.mark_draining(node).await
    }

    async fn live_decodes(
        &self,
    ) -> Result<Vec<Vec<lash_core::durable_port::FormatSet>>, DurableError> {
        self.inner.live_decodes().await
    }

    async fn owned(&self, node: &NodeLease) -> Result<Vec<Claimed>, DurableError> {
        self.inner.owned(node).await
    }

    async fn begin(
        &self,
        actor: &ActorKey,
        epoch: lash_core::durable_port::Epoch,
    ) -> Result<ActorTx, DurableError> {
        self.inner.begin(actor, epoch).await
    }

    async fn commit(&self, tx: ActorTx, label: CommitLabel) -> Result<ActorCommit, DurableError> {
        self.inner.commit(tx, label).await
    }

    async fn commit_mail(
        &self,
        tx: MailTx,
        label: CommitLabel,
    ) -> Result<MailCommit, DurableError> {
        self.inner.commit_mail(tx, label).await
    }

    async fn actor(&self, actor: &ActorKey) -> Result<Option<ActorSnapshot>, DurableError> {
        self.inner.actor(actor).await
    }
}

/// The close-begin transaction acknowledges its mail and records the close
/// atomically. An observer that sampled the old close before that transaction
/// must not combine it with the new empty mailbox to answer `NotClosing`.
#[tokio::test]
async fn an_accepted_close_stays_pending_when_its_mail_is_acknowledged_between_reads() {
    let stores: Arc<dyn lash_core::StoreSet> = crate::tests::sqlite_memory_store_set().await;
    let inner = stores.durable_store();
    let session = SessionId::from("close-observer-race");
    let core = crate::tests::explicit_ephemeral_facets(LashCore::standard_builder(
        lash_conformance::backend_over(Arc::clone(&stores)),
    ))
    .serve_test_llm_profile(
        crate::testing::TestProvider::builder()
            .build()
            .into_handle(),
        crate::tests::mock_llm_profile_spec(),
    )
    .serve_sessions(false)
    .build(crate::testing::runtime_lease_owner())
    .expect("the non-serving core builds");
    core.session(session.clone())
        .create(crate::SessionCreation::root(
            crate::plugins::SessionToolAccess::ambient(),
            crate::tests::mock_session_spec(),
        ))
        .await
        .expect("create the session");
    let administration = core.session_administration().await;
    let deletion = LashCore::delete_session(
        administration
            .delete_context(session.as_str())
            .expect("delete context"),
    )
    .await
    .expect("request its close");
    assert!(matches!(deletion, crate::SessionDeletion::Requested { .. }));
    let actor = ActorKey::session(session.as_str()).expect("session actor");
    let formats = inner.actor(&actor).await.unwrap().unwrap().formats;
    let node = inner
        .register_node(&NodeSpec {
            node: NodeId::new("close-observer-law"),
            decodes: vec![formats],
            ttl_millis: 60_000,
        })
        .await
        .expect("register the actor's node");
    let claimed = inner
        .claim(&node, 1)
        .await
        .unwrap()
        .pop()
        .expect("claim the close");
    let race = Arc::new(CloseDuringRead {
        inner: Arc::clone(&inner),
        actor,
        epoch: claimed.epoch,
        armed: AtomicBool::new(true),
    });
    let layered = LayeredStores::over(stores)
        .map_durable_store({
            let race = Arc::clone(&race);
            move |_| race
        })
        .into_store_set();
    let observer = crate::tests::explicit_ephemeral_facets(LashCore::standard_builder(
        lash_conformance::backend_over(layered),
    ))
    .serve_test_llm_profile(
        crate::testing::TestProvider::builder()
            .build()
            .into_handle(),
        crate::tests::mock_llm_profile_spec(),
    )
    .serve_sessions(false)
    .build(crate::testing::runtime_lease_owner())
    .expect("the observing core builds");
    assert_eq!(
        observer.deletion_completion(&session).await.unwrap(),
        None,
        "the accepted close is still pending after its actor acknowledges the mail"
    );
    assert!(
        !race.armed.load(Ordering::SeqCst),
        "the interleaving executed"
    );
    assert!(inner.session_close(&session).await.unwrap().is_some());
    assert_eq!(
        inner
            .actor(&race.actor)
            .await
            .unwrap()
            .unwrap()
            .pending_mail,
        0
    );
    inner
        .release_node(&node)
        .await
        .expect("release the law's node");
    observer.shutdown().await.unwrap();
    core.shutdown().await.unwrap();
}

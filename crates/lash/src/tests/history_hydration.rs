//! A persistent session's sends decode what they add, not what came before
//! (FIG-5930).
//!
//! A send reads its session's window several times: the engine's turn, the
//! runtime its pass opens, the commit's publication, the open session's
//! refresh and the handle's terminal report. The store reuses the window it
//! decoded and reads only the rows past its leaf, so the node bodies a send
//! decodes are bounded by the nodes it appends, whatever the session's
//! history depth.

use std::time::Duration;

use super::*;
use crate::{SessionId, TurnInput};
use lash_core::store::{SessionHistoryStore, StoreTestSupport, WindowSelector};

/// The window reads one send makes: the engine's turn, the runtime its pass
/// opens, the commit's publication, the open session's refresh and the
/// handle's terminal report.
const WINDOW_READS_PER_SEND: u64 = 5;
/// Sends before the measured ones: the session's first turns open its
/// runtime and decode the window its creation wrote.
const WARM_SENDS: usize = 3;
const MEASURED_SENDS: usize = 24;

async fn window_len(store: &lash_sqlite_store::SqliteStore, session: &SessionId) -> u64 {
    store
        .load_session_window(session, WindowSelector::Current)
        .await
        .expect("read the window")
        .expect("the session has a window")
        .window
        .nodes
        .len() as u64
}

/// Each measured send decodes at most one body per window read for each
/// node it appends, so the total stays proportional to the nodes appended
/// while the window it reads grows by them on every send.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_send_decodes_only_the_nodes_it_appends() -> Result<()> {
    let files = tempfile::tempdir().expect("SQLite store directory");
    let stores = lash_sqlite_store::SqliteStoreSet::open(
        files.path().join("lash.db"),
        lash_sqlite_store::SqliteSynchronous::Normal,
    )
    .await
    .expect("SQLite file stores");
    let store = stores.session_store_factory();
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        lash_conformance::backend_over(Arc::new(stores)),
    ))
    .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
    .build(crate::testing::runtime_lease_owner())?;
    let id = SessionId::fixture("history-hydration");
    core.session(id.clone())
        .create(crate::SessionCreation::root(mock_session_spec()))
        .await?;
    let session = core.session(id.clone()).open().await?;
    for turn in 0..WARM_SENDS {
        session
            .send(TurnInput::text(format!("warm {turn}")))
            .output()
            .await?;
    }
    let depth_before = window_len(&store, &id).await;
    let mut decoded = 0;
    let mut read = 0;
    for turn in 0..MEASURED_SENDS {
        let before = store.decoded_row_counts_for_testing().graph_node_bodies;
        session
            .send(TurnInput::text(format!("turn {turn}")))
            .output()
            .await?;
        decoded += store.decoded_row_counts_for_testing().graph_node_bodies - before;
        read += window_len(&store, &id).await;
    }
    let appended = window_len(&store, &id).await - depth_before;
    assert!(
        appended >= MEASURED_SENDS as u64,
        "{appended} nodes appended"
    );
    assert!(
        decoded <= WINDOW_READS_PER_SEND * appended,
        "{MEASURED_SENDS} sends appended {appended} nodes and decoded {decoded} bodies \
         (windows of {read} nodes in all)"
    );
    session.close().await?;
    core.shutdown().await?;
    Ok(())
}

/// FIG-6208: the reader opened before the first turn still keeps its new
/// anchored window warm after the engine releases its idle head.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_open_empty_session_keeps_its_first_window_warm_after_idle_eviction() -> Result<()> {
    let stores = lash_sqlite_store::SqliteStoreSet::memory()
        .await
        .expect("SQLite memory stores");
    let store = stores.session_store_factory();
    let settings = crate::durable::DurableSettings {
        idle_evict: Duration::from_millis(1),
        ..Default::default()
    };
    let backend = crate::durable::DurableBackendBuilder::new(Arc::new(stores))
        .config(settings)
        .build()
        .expect("fixture durable backend");
    let core = explicit_ephemeral_facets(LashCore::standard_builder(backend))
        .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
        .build(crate::testing::runtime_lease_owner())?;
    let id = SessionId::fixture("empty-reader-idle-eviction");
    core.session(id.clone())
        .create(crate::SessionCreation::root(mock_session_spec()))
        .await?;
    let session = core.session(id.clone()).open().await?;
    session.send(TurnInput::text("first turn")).output().await?;
    let actor = lash_core::durable_port::ActorKey::session(id.as_str()).expect("session actor");
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let snapshot = core
                .backend()
                .durable()
                .actor(&actor)
                .await
                .expect("read the session actor");
            if snapshot
                .is_some_and(|snapshot| snapshot.state == lash_core::durable_port::ActorState::Idle)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the engine releases its idle head");
    let before = store.decoded_row_counts_for_testing().graph_node_bodies;
    assert!(
        window_len(&store, &id).await > 1,
        "the first turn anchored the window"
    );
    assert_eq!(
        store.decoded_row_counts_for_testing().graph_node_bodies - before,
        0,
        "the open empty reader keeps the first anchored window warm"
    );
    session.close().await?;
    core.shutdown().await?;
    Ok(())
}

/// FIG-6231: a store probe counts the turn ledgers a facade read hydrates.
#[cfg(feature = "rlm")]
mod tool_call_reads {
    use super::super::*;
    use lash_core::durable_port as lash_durable;
    use lash_durable::domain::{
        ExecKey, ExecRootRow, HydratedGraph, OwnerKey, ProcessActorRow, RootOwner, RunFacts,
        ScopeKey, SessionCloseRow, TurnRow, WaitId, WaitRow,
    };
    use lash_durable::{
        ActorCommit, ActorKey, ActorSnapshot, ActorTx, Claimed, CommitLabel, DurableError,
        DurableInstant, DurableReads, DurableStore, Epoch, HeartbeatOutcome, MailCommit, MailTx,
        NodeLease, NodeSpec, Reaped,
    };
    use lash_sansio::{ProcessId, SessionId};
    use std::result::Result;
    use std::sync::{Arc, Mutex};

    struct ReadProbe {
        inner: Arc<dyn DurableStore>,
        owners: Mutex<Vec<OwnerKey>>,
        /// Reads of the session's unfinished turn.
        turns: Mutex<usize>,
    }

    async fn fixture() -> crate::Result<(
        LashCore,
        crate::DurableSession,
        Arc<ReadProbe>,
        Vec<(crate::TurnId, Vec<crate::tools::ToolCallRecord>)>,
    )> {
        let stores = sqlite_memory_store_set().await;
        let probe = Arc::new(ReadProbe {
            inner: Arc::new(stores.durable_store()),
            owners: Mutex::new(Vec::new()),
            turns: Mutex::new(0),
        });
        let counted = probe.clone();
        let stores = lash_core::testing::runtime_helpers::LayeredStores::over(stores)
            .map_durable_store(move |_| counted)
            .into_store_set();
        let core = explicit_ephemeral_facets(rlm_core_builder_over(lash_conformance::backend_over(stores)))
        .serve_test_llm_profile(text_provider("tool-read-cost", typescript_block("await tools.app_lookup({}); await tools.app_lookup({}); await control.finish(\"done\");")), mock_llm_profile_spec())
        .tools(crate::tool_provider_key!("tools"), Arc::new(AppTools))
        .build(crate::testing::runtime_lease_owner())?;
        let session = core
            .session(crate::SessionId::fixture("tool-read-cost"))
            .created()
            .await
            .open()
            .await?;
        let mut turns = Vec::new();
        for index in 0..5 {
            let turn = crate::TurnId::fixture(format!("tool-read-{index}"));
            let output = session
                .send(crate::TurnInput::text("lookup"))
                .id(turn.clone())
                .output()
                .await?;
            turns.push((turn, output.result.tool_calls.clone()));
        }
        let durable = session.durable();
        drop(session);
        core.node.stop().await;
        probe.owners.lock().expect("read probe").clear();
        *probe.turns.lock().expect("read probe") = 0;
        Ok((core, durable, probe, turns))
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_tool_call_batch_decodes_exactly_the_named_turns() -> crate::Result<()> {
        let (_core, durable, probe, turns) = fixture().await?;
        let reference = |index: usize, call_id| crate::TurnToolCallRef {
            turn_id: turns[index].0.clone(),
            call_id,
        };
        let calls = vec![
            reference(3, turns[3].1[0].call_id.clone()),
            reference(1, turns[1].1[1].call_id.clone()),
            reference(3, turns[3].1[0].call_id.clone()),
            reference(3, crate::ToolCallId::fixture("missing")),
            reference(1, turns[3].1[0].call_id.clone()),
        ];
        let reads = durable.read_tool_calls(&calls).await?;
        assert_eq!(
            reads,
            vec![
                crate::ToolCallRead::Settled {
                    record: Box::new(turns[3].1[0].clone())
                },
                crate::ToolCallRead::Settled {
                    record: Box::new(turns[1].1[1].clone())
                },
                crate::ToolCallRead::Settled {
                    record: Box::new(turns[3].1[0].clone())
                },
                crate::ToolCallRead::NotFound {
                    call_id: calls[3].call_id.clone()
                },
                crate::ToolCallRead::NotFound {
                    call_id: calls[4].call_id.clone()
                },
            ]
        );
        let owners = probe.owners.lock().expect("read probe").clone();
        assert_eq!(
            owners,
            vec![
                OwnerKey::Turn(
                    crate::SessionId::fixture("tool-read-cost"),
                    turns[1].0.clone()
                ),
                OwnerKey::Turn(
                    crate::SessionId::fixture("tool-read-cost"),
                    turns[3].0.clone()
                )
            ],
            "one decode per named turn, even for duplicate and missing ids"
        );
        assert_eq!(
            *probe.turns.lock().expect("read probe"),
            1,
            "the batch reads the session's unfinished turn once"
        );
        Ok(())
    }
    /// FIG-6231: opting in reads exactly one ledger for each returned page turn;
    /// plain pages and empty pages do not decode any call ledger. A committed
    /// turn is never the unfinished one, which the page never reads.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_committed_page_decodes_only_its_turns_tool_calls() -> crate::Result<()> {
        let (_core, durable, probe, turns) = fixture().await?;
        let limit = std::num::NonZeroU32::MIN.saturating_add(1);
        let first = durable.committed_turns(None, limit).await?;
        assert!(probe.owners.lock().expect("read probe").is_empty());
        let page = durable
            .committed_turns_with_tool_calls(Some(&first.next), limit)
            .await?;
        assert_eq!(page.turns.len(), 2);
        for (read, expected) in page.turns.iter().zip(&turns[2..4]) {
            assert_eq!(read.turn.turn_id, expected.0);
            assert_eq!(read.tool_calls, expected.1);
            assert!(
                read.turn
                    .entries
                    .iter()
                    .any(|entry| matches!(entry.item, crate::transcript::TranscriptItem::Cell(_)))
            );
        }
        assert_eq!(
            *probe.owners.lock().expect("read probe"),
            vec![
                OwnerKey::Turn(
                    crate::SessionId::fixture("tool-read-cost"),
                    turns[2].0.clone()
                ),
                OwnerKey::Turn(
                    crate::SessionId::fixture("tool-read-cost"),
                    turns[3].0.clone()
                )
            ]
        );
        assert_eq!(*probe.turns.lock().expect("read probe"), 0);
        let tail = durable.committed_turns(Some(&page.next), limit).await?;
        probe.owners.lock().expect("read probe").clear();
        let empty = durable
            .committed_turns_with_tool_calls(Some(&tail.next), limit)
            .await?;
        assert!(empty.turns.is_empty());
        assert!(probe.owners.lock().expect("read probe").is_empty());
        Ok(())
    }

    #[async_trait::async_trait]
    impl DurableReads for ReadProbe {
        async fn turn(&self, session: &SessionId) -> Result<Option<TurnRow>, DurableError> {
            *self.turns.lock().expect("read probe") += 1;
            self.inner.turn(session).await
        }

        async fn turn_namespaces(
            &self,
            session: &SessionId,
            run: &lash_sansio::TurnId,
        ) -> Result<Vec<lash_durable::domain::TurnNamespace>, DurableError> {
            self.inner.turn_namespaces(session, run).await
        }

        async fn turn_end(
            &self,
            session: &SessionId,
            run: &lash_sansio::TurnId,
        ) -> Result<Option<lash_durable::domain::TurnEnd>, DurableError> {
            self.inner.turn_end(session, run).await
        }

        async fn run_facts(&self, owner: &OwnerKey) -> Result<RunFacts, DurableError> {
            self.owners.lock().expect("read probe").push(owner.clone());
            self.inner.run_facts(owner).await
        }

        async fn run_facts_of(
            &self,
            owner: &OwnerKey,
            run: lash_durable::domain::RunSeq,
        ) -> Result<RunFacts, DurableError> {
            self.inner.run_facts_of(owner, run).await
        }
        async fn run_facts_within(
            &self,
            owner: &lash_durable::domain::OwnerKey,
            run: Option<lash_durable::domain::RunSeq>,
            max_rows: usize,
            limits: lash_durable::domain::checkpoint_graph::DecodeLimits,
        ) -> Result<lash_durable::domain::RunFacts, DurableError> {
            self.inner
                .run_facts_within(owner, run, max_rows, limits)
                .await
        }

        async fn exec_root(&self, exec: &ExecKey) -> Result<Option<ExecRootRow>, DurableError> {
            self.inner.exec_root(exec).await
        }

        async fn cell_roots(
            &self,
            session: &lash_sansio::SessionId,
            run: &lash_sansio::TurnId,
        ) -> Result<Vec<ExecRootRow>, DurableError> {
            self.inner.cell_roots(session, run).await
        }
        async fn cell_roots_within(
            &self,
            session: &lash_sansio::SessionId,
            run: &lash_sansio::TurnId,
            max_rows: usize,
        ) -> Result<Vec<lash_durable::domain::ExecRootRow>, DurableError> {
            self.inner.cell_roots_within(session, run, max_rows).await
        }

        async fn hydrate(
            &self,
            owner: RootOwner,
            expected: Option<lash_durable::domain::checkpoint_graph::NodeId>,
            limits: lash_durable::domain::checkpoint_graph::DecodeLimits,
        ) -> Result<HydratedGraph, DurableError> {
            self.inner.hydrate(owner, expected, limits).await
        }

        async fn pending_waits(&self, owner: &ActorKey) -> Result<Vec<WaitRow>, DurableError> {
            self.inner.pending_waits(owner).await
        }

        async fn wait(&self, id: &WaitId) -> Result<Option<WaitRow>, DurableError> {
            self.inner.wait(id).await
        }

        async fn process(
            &self,
            process: &ProcessId,
        ) -> Result<Option<ProcessActorRow>, DurableError> {
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
            self.inner.session_close(session).await
        }

        async fn ending_scopes(&self, session: &SessionId) -> Result<Vec<ScopeKey>, DurableError> {
            self.inner.ending_scopes(session).await
        }

        async fn session_mailbox(
            &self,
            session: &lash_sansio::SessionId,
        ) -> Result<lash_durable::domain::SessionMailbox, DurableError> {
            self.inner.session_mailbox(session).await
        }

        async fn prompt_snapshot(
            &self,
            call: &lash_durable::domain::PromptCallKey,
        ) -> Result<Option<lash_durable::domain::PromptSnapshotRow>, DurableError> {
            self.inner.prompt_snapshot(call).await
        }

        async fn prompt_texts(
            &self,
            hashes: &[String],
        ) -> Result<Vec<lash_durable::domain::PromptText>, DurableError> {
            self.inner.prompt_texts(hashes).await
        }
    }

    #[async_trait::async_trait]
    impl DurableStore for ReadProbe {
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

        async fn acquire_idle_for_testing(
            &self,
            node: &lash_durable::NodeLease,
            actor: &lash_durable::ActorKey,
            epoch: lash_durable::Epoch,
        ) -> Result<lash_durable::Epoch, lash_durable::DurableError> {
            self.inner
                .acquire_idle_for_testing(node, actor, epoch)
                .await
        }

        async fn claim(
            &self,
            node: &NodeLease,
            limit: usize,
        ) -> Result<Vec<Claimed>, DurableError> {
            self.inner.claim(node, limit).await
        }

        async fn mark_draining(&self, node: &NodeLease) -> Result<(), DurableError> {
            self.inner.mark_draining(node).await
        }

        async fn live_decodes(&self) -> Result<Vec<Vec<lash_durable::FormatSet>>, DurableError> {
            self.inner.live_decodes().await
        }

        async fn owned(&self, node: &NodeLease) -> Result<Vec<Claimed>, DurableError> {
            self.inner.owned(node).await
        }

        async fn begin(&self, actor: &ActorKey, epoch: Epoch) -> Result<ActorTx, DurableError> {
            self.inner.begin(actor, epoch).await
        }

        async fn commit(
            &self,
            tx: ActorTx,
            label: CommitLabel,
        ) -> Result<ActorCommit, DurableError> {
            self.inner.commit(tx, label).await
        }

        async fn commit_mail(
            &self,
            tx: MailTx,
            label: CommitLabel,
        ) -> Result<MailCommit, DurableError> {
            self.inner.commit_mail(tx, label).await
        }

        async fn claim_facts(
            &self,
            node: Option<&lash_durable::Owner>,
            actor: &ActorKey,
        ) -> Result<lash_durable::ClaimFacts, DurableError> {
            self.inner.claim_facts(node, actor).await
        }

        async fn actor(&self, actor: &ActorKey) -> Result<Option<ActorSnapshot>, DurableError> {
            self.inner.actor(actor).await
        }

        async fn await_owner_commit(&self, actor: &ActorKey) -> Result<(), DurableError> {
            self.inner.await_owner_commit(actor).await
        }

        async fn actors_in(
            &self,
            formats: &lash_durable::FormatSet,
            after: Option<&ActorKey>,
            limit: usize,
        ) -> Result<Vec<ActorKey>, DurableError> {
            self.inner.actors_in(formats, after, limit).await
        }
    }
}

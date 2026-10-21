// A persistent session's sends decode what they add, not what came before
// (FIG-5930).
//
// A send reads its session's window several times: the engine's turn, the
// runtime its pass opens, the commit's publication, the open session's
// refresh and the handle's terminal report. The store reuses the window it
// decoded and reads only the rows past its leaf, so the node bodies a send
// decodes are bounded by the nodes it appends, whatever the session's
// history depth.

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
    use lash_protocol_types::{ProcessId, SessionId};
    use std::result::Result;
    use std::sync::{Arc, Mutex};

    struct ReadProbe {
        inner: Arc<dyn DurableStore>,
        /// Ledger reads, by owner.
        owners: Mutex<Vec<OwnerKey>>,
        /// Transcript hydrates, by run (FIG-6601).
        transcripts: Mutex<Vec<crate::TurnId>>,
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
            transcripts: Mutex::new(Vec::new()),
            turns: Mutex::new(0),
        });
        let counted = probe.clone();
        let stores = lash_core::testing::runtime_helpers::LayeredStores::over(stores)
            .map_durable_store(move |_| counted)
            .into_store_set();
        let core = explicit_ephemeral_facets(rlm_core_builder_over(lash_conformance::backend_over(stores)))
        .serve_test_llm_profile(text_provider("tool-read-cost", typescript_block("await tools.appLookup({}); await tools.appLookup({}); await control.finish(\"done\");")), mock_llm_profile_spec())
        .tools(crate::tool_provider_key!("tools"), Arc::new(AppTools))
        .build(crate::tests::host::runtime_lease_owner())?;
        let session = created_session(&core, crate::SessionId::fixture("tool-read-cost"))
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
            let output_tool_calls =
                crate::testing::tool_calls_for_activity(&session.durable(), &output.activities)
                    .await;
            turns.push((turn, output_tool_calls.clone()));
        }
        let durable = session.durable();
        drop(session);
        core.node.stop().await;
        probe.owners.lock().expect("read probe").clear();
        probe.transcripts.lock().expect("read probe").clear();
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
        assert!(
            probe.owners.lock().expect("read probe").is_empty(),
            "a settled turn's calls read no ledger"
        );
        assert_eq!(
            *probe.transcripts.lock().expect("read probe"),
            vec![turns[1].0.clone(), turns[3].0.clone()],
            "one transcript decode per named turn, even for duplicate and missing ids"
        );
        assert_eq!(
            *probe.turns.lock().expect("read probe"),
            1,
            "the batch reads the session's unfinished turn once"
        );
        Ok(())
    }
    /// FIG-6231: opting in decodes exactly one transcript for each returned
    /// page turn and no ledger (FIG-6601); plain pages and empty pages decode
    /// no calls. A committed turn is never the unfinished one, which the page
    /// never reads.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_committed_page_decodes_only_its_turns_tool_calls() -> crate::Result<()> {
        let (_core, durable, probe, turns) = fixture().await?;
        let limit = std::num::NonZeroU32::MIN.saturating_add(1);
        let first = durable.committed_turns(None, limit).await?;
        assert!(probe.owners.lock().expect("read probe").is_empty());
        assert!(probe.transcripts.lock().expect("read probe").is_empty());
        let page = durable
            .committed_turns_with_calls(Some(&first.next), limit)
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
        assert!(probe.owners.lock().expect("read probe").is_empty());
        assert_eq!(
            *probe.transcripts.lock().expect("read probe"),
            vec![turns[2].0.clone(), turns[3].0.clone()]
        );
        assert_eq!(*probe.turns.lock().expect("read probe"), 0);
        let tail = durable.committed_turns(Some(&page.next), limit).await?;
        probe.transcripts.lock().expect("read probe").clear();
        let empty = durable
            .committed_turns_with_calls(Some(&tail.next), limit)
            .await?;
        assert!(empty.turns.is_empty());
        assert!(probe.owners.lock().expect("read probe").is_empty());
        assert!(probe.transcripts.lock().expect("read probe").is_empty());
        Ok(())
    }

    #[async_trait::async_trait]
    impl DurableReads for ReadProbe {
        async fn model_call_pins(
            &self,
            session: &lash_protocol_types::SessionId,
            owner: &str,
        ) -> Result<Vec<lash_durable::domain::ModelCallPinRow>, DurableError> {
            self.inner.model_call_pins(session, owner).await
        }

        async fn model_calls(
            &self,
            session: &lash_protocol_types::SessionId,
            owner: &str,
        ) -> Result<Vec<lash_core_store::model_calls::ModelCallRecord>, DurableError> {
            self.inner.model_calls(session, owner).await
        }

        async fn turn(&self, session: &SessionId) -> Result<Option<TurnRow>, DurableError> {
            *self.turns.lock().expect("read probe") += 1;
            self.inner.turn(session).await
        }

        async fn turn_namespaces(
            &self,
            session: &SessionId,
            run: &lash_protocol_types::TurnId,
        ) -> Result<Vec<lash_durable::domain::TurnNamespace>, DurableError> {
            self.inner.turn_namespaces(session, run).await
        }

        async fn turn_end(
            &self,
            session: &SessionId,
            run: &lash_protocol_types::TurnId,
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
            session: &lash_protocol_types::SessionId,
            run: &lash_protocol_types::TurnId,
        ) -> Result<Vec<ExecRootRow>, DurableError> {
            self.inner.cell_roots(session, run).await
        }
        async fn cell_roots_within(
            &self,
            session: &lash_protocol_types::SessionId,
            run: &lash_protocol_types::TurnId,
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
            if let RootOwner::RunTranscript { run, .. } = &owner {
                self.transcripts
                    .lock()
                    .expect("read probe")
                    .push(run.clone());
            }
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
            session: &lash_protocol_types::SessionId,
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

        fn pages(&self) -> Option<Arc<dyn lash_durable::domain::pages::PageStore>> {
            self.inner.pages()
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

        async fn readiness(&self) -> Result<lash_durable::Readiness, DurableError> {
            self.inner.readiness().await
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

        async fn live_nodes(&self) -> Result<Vec<lash_durable::NodeCapability>, DurableError> {
            self.inner.live_nodes().await
        }

        async fn actor_requirements(
            &self,
            after: Option<&lash_durable::ActorKey>,
            limit: usize,
        ) -> Result<Vec<(lash_durable::ActorKey, Vec<String>)>, DurableError> {
            self.inner.actor_requirements(after, limit).await
        }

        async fn owned(&self, node: &NodeLease) -> Result<Vec<Claimed>, DurableError> {
            self.inner.owned(node).await
        }

        async fn begin(&self, actor: &ActorKey, epoch: Epoch) -> Result<ActorTx, DurableError> {
            self.inner.begin(actor, epoch).await
        }

        async fn open_process(
            &self,
            actor: &ActorKey,
            epoch: Epoch,
        ) -> Result<(ActorTx, lash_durable::domain::ProcessView), DurableError> {
            self.inner.open_process(actor, epoch).await
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

//! What a turn's end and its session's delete leave behind (FIG-5939; ADR
//! 0067 §3, ADR 0132 §4, §6).
//!
//! A turn's run facts are its recovery ledger until its terminal, which
//! publishes its transcript, the calls it answered, and retires the ledger
//! in the same commit (FIG-6601): the transcript lives as long as its
//! run row, and its report reads it alone. Everything else a turn wrote per
//! call is working state that dies with its last reader: a consumed cell's
//! own facts and admissions with its root, the root, admissions and facts of
//! a cell the turn stopped in, and the turn's cancel request at its end; its
//! waits compact to the tombstones a late resolution is answered from. A
//! session's delete removes every engine row it owns but its terminal actor.
//!
//! Every law runs turns on the core's own node over a SQLite memory store
//! set, whose engine rows it reads back raw.

use super::*;
use crate::{TurnCancelMode, TurnInput};

/// The cell every turn runs: a lookup that answers at once, then an
/// approval that parks on its completion key, then, when `finishes`, the
/// turn's finish.
fn cell(finishes: bool) -> LlmOutputPart {
    let finish = if finishes {
        "\nawait control.finish(\"approved\");"
    } else {
        ""
    };
    typescript_block(&format!(
        "const found = await tools.appLookup({{}});\n\
         const approved = await tools.appApprove({{}});{finish}"
    ))
}

/// `tools.<name>`; a deferring one may park on its completion key.
fn tool(name: &str, deferring: bool) -> lash_vocabulary::tool_contract::ToolDefinition {
    let definition = lash_vocabulary::tool_contract::ToolDefinition::raw(
        format!("tool:{name}"),
        format!("tools.{name}"),
        "A law's tool.",
        serde_json::json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        }),
        serde_json::json!({ "type": "object" }),
    )
    .expect("valid declared tool schemas")
    .with_execution(std::time::Duration::from_secs(120));
    if !deferring {
        return definition;
    }
    definition
        .with_declaration(
            lash_vocabulary::tool_declaration::ToolDeclaration::deferring(),
            Some(lash_vocabulary::tool_contract::ParkBound::Within(
                std::time::Duration::from_secs(120),
            )),
        )
        .expect("a deferring tool declares its park bound")
}

/// `app_lookup` answers at once; `app_approve` parks, handing the law its
/// completion key.
struct ApprovalTools {
    keys: tokio::sync::mpsc::UnboundedSender<String>,
    return_gate: Option<Arc<tokio::sync::Notify>>,
}

impl ApprovalTools {
    /// Offer the tools to the model and their operations to the code cell.
    fn plugin(self) -> Arc<StaticPluginFactory> {
        let key = crate::tool_provider_key!("tools");
        let provider: Arc<dyn ToolProvider> = Arc::new(self);
        let packages = crate::package::ToolPackageProvider::new(
            key.as_str(),
            key,
            [crate::package::Segment::new(key.as_str()).expect("the tools package root")].into(),
            Arc::clone(&provider),
        );
        Arc::new(StaticPluginFactory::new(
            crate::plugins::PluginDeclaration::initial(key.as_str()),
            crate::plugins::PluginSpec::new()
                .with_tool_provider(key, provider)
                .with_package_provider(key, Arc::new(packages)),
        ))
    }
}

#[async_trait]
impl ToolProvider for ApprovalTools {
    fn tool_manifests(
        &self,
        _config: &lash_core::OwnerConfigView,
    ) -> std::result::Result<
        Vec<lash_vocabulary::tool_contract::ToolManifest>,
        crate::plugins::PluginError,
    > {
        Ok({
            vec![
                tool("app_lookup", false).manifest(),
                tool("app_approve", true).manifest(),
            ]
        })
    }

    fn resolve_contract(
        &self,
        name: &str,
        _config: &lash_core::OwnerConfigView,
    ) -> Option<Arc<lash_vocabulary::tool_contract::ToolContract>> {
        let Some(("tools", name)) = name.split_once('.') else {
            return None;
        };
        ["app_lookup", "app_approve"]
            .contains(&name)
            .then(|| Arc::new(tool(name, name == "tools.app_approve").contract()))
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        if call.name() == "tools.app_lookup" {
            return lash_core::ToolOutcome::ok(serde_json::json!({ "found": true })).into();
        }
        let key = match call.context.completion_key() {
            Ok(key) => key,
            Err(error) => return lash_core::ToolOutcome::err_fmt(error).into(),
        };
        let _ = self.keys.send(key.as_str().to_owned());
        if let Some(gate) = &self.return_gate {
            gate.notified().await;
        }
        lash_core::ToolAttemptOutcome::Pending(lash_core::PendingCompletion::new())
    }
}

/// An RLM core over a SQLite memory store set the law reads back, whose
/// model answers every call with [`cell`].
struct World {
    stores: Arc<lash_sqlite_store::SqliteStoreSet>,
    core: LashCore,
    keys: tokio::sync::mpsc::UnboundedReceiver<String>,
}

impl World {
    async fn new(finishes: bool) -> Result<Self> {
        Self::with_return_gate(finishes, None).await
    }

    async fn with_return_gate(
        finishes: bool,
        return_gate: Option<Arc<tokio::sync::Notify>>,
    ) -> Result<Self> {
        let stores = sqlite_memory_store_set().await;
        let backend = lash_conformance::backend_over(Arc::clone(&stores) as Arc<_>);
        let (keys, received) = tokio::sync::mpsc::unbounded_channel();
        let core = explicit_ephemeral_facets(rlm_core_builder_over(backend))
            .serve_test_llm_profile(
                part_provider("turn-retirement", cell(finishes)),
                mock_llm_profile_spec(),
            )
            .plugin(ApprovalTools { keys, return_gate }.plugin())
            .build(crate::tests::host::runtime_lease_owner())?;
        Ok(Self {
            stores,
            core,
            keys: received,
        })
    }

    async fn session(&self, name: &str) -> Result<crate::DurableSession> {
        created_session(
            &self.core,
            lash_kernel_value::identity::SessionId::fixture(name),
        )
        .await
        .durable()
        .await
    }

    /// The approval's completion key, once the cell parked on it.
    async fn parked(&mut self) -> String {
        tokio::time::timeout(std::time::Duration::from_secs(60), self.keys.recv())
            .await
            .expect("the cell parks on its approval")
            .expect("the tools outlive the law")
    }

    /// Every engine row whose rendered line holds `needle`.
    fn rows(&self, needle: &str) -> Vec<String> {
        lash_sqlite_store::testing::engine_rows_for_testing(&self.stores)
            .expect("the engine rows read")
            .into_iter()
            .filter(|row| row.contains(needle))
            .collect()
    }

    /// The hash of every stored blob.
    fn blobs(&self) -> std::collections::BTreeSet<String> {
        lash_sqlite_store::testing::read_rows_for_testing(&self.stores, "SELECT hash FROM blobs")
            .expect("the blobs read")
            .into_iter()
            .filter_map(|row| {
                row.first()
                    .and_then(|(_, hash)| hash.as_str().map(str::to_owned))
            })
            .collect()
    }

    async fn resolve(&self, key: &str, value: serde_json::Value) -> lash_core::ResolveAnswer {
        self.core
            .completions()
            .resolve(key, lash_core::Resolution::Ok(value))
            .await
            .expect("the resolution answers")
    }

    /// A collection: the page retirement drained, then the host's.
    async fn collect(&self) {
        use lash_core::durable_port::domain::pages::{PageStore as _, RetirementBudget};
        let store = self.stores.durable_store();
        loop {
            let step = store
                .retire_step(RetirementBudget { nodes: 1024 })
                .await
                .expect("a retirement step completes");
            if step.holds_advanced == 0 && step.nodes_deleted == 0 {
                break;
            }
        }
        self.core
            .store_factory
            .gc_unreachable()
            .await
            .expect("the host collection completes");
    }
}

/// The engine rows `run` of `session` left at its end: none of its cells'
/// and no cancel request, only wait tombstones, and no ledger; its
/// transcript instead. Answers the transcript's nodes.
async fn assert_retired(
    world: &World,
    session: &str,
    run: &str,
) -> std::collections::BTreeSet<String> {
    let journal = lash_core::ExecutionScope::turn(
        lash_kernel_value::identity::SessionId::fixture(session),
        lash_vocabulary::identity::TurnId::fixture(run),
    )
    .journal_identity()
    .expect("the turn journal identity");
    let edges = lash_sqlite_store::testing::read_rows_for_testing(
        &world.stores,
        "SELECT referrer_id FROM artifact_referrer_edges WHERE referrer_kind = 'execution'",
    )
    .expect("the execution edges read");
    assert!(
        edges
            .iter()
            .all(|row| row.iter().all(|(_, id)| id.as_str() != Some(journal.key()))),
        "the turn's terminal left its artifact edges: {edges:#?}"
    );
    let cells = world.rows(&format!(" c/{session}/{run}/"));
    assert!(
        cells.is_empty(),
        "the turn's end left its cells' rows: {cells:#?}"
    );
    let cancel = world.rows(&format!("turn_cancel_requests {session} {run}"));
    assert!(
        cancel.is_empty(),
        "the turn's end left its cancel request: {cancel:#?}"
    );
    let waits = world.rows(&format!("waits t/{session}/{run} "));
    assert!(!waits.is_empty(), "the turn pinned its approval's wait");
    assert!(
        waits.iter().all(|wait| wait.ends_with(" tombstone")),
        "the turn's end left a wait whole: {waits:#?}"
    );
    let ledger = ledger(&world.stores, session, run);
    assert!(
        ledger.is_empty(),
        "the turn's end left its ledger: {ledger:#?}"
    );
    let transcript = lash_core::testing::turn_transcript(
        world
            .core
            .actor_context()
            .durable_reads()
            .expect("the durable reads"),
        &lash_kernel_value::identity::SessionId::fixture(session),
        &lash_vocabulary::identity::TurnId::fixture(run),
    )
    .await
    .expect("the turn's end published its transcript");
    transcript.nodes.keys().map(|id| id.to_hex()).collect()
}

/// The tool names in the settled call read, in admission order.
fn tools(calls: &[crate::tools::ToolCallRecord]) -> Vec<String> {
    calls.iter().map(|call| call.tool.clone()).collect()
}

/// A turn cancelled after its pending cell's owner stops retires, in its
/// cancel, the cell it stopped in (root, admissions, the cell's own facts),
/// its cancel request and its waits' payloads, and its ledger, keeping its
/// transcript. The report a follower reads after that cleanup, and after a collection, is
/// the live one, the stopped cell's lookup included; a late resolution of
/// the approval answers `Revoked`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cancelled_turn_retires_the_cell_it_stopped_in_and_keeps_its_transcript() -> Result<()> {
    use lash_core::durable_port::domain::{DurableReads as _, RunFactKind};
    use lash_core::durable_port::runner::Stopped;
    use lash_core::durable_port::{ActorKey, DurableStore as _};

    let (session_name, run) = ("retire-cancelled", "retire-cancelled-run");
    let mut world = World::new(false).await?;
    let session = world.session(session_name).await?;
    let handle = session
        .send(TurnInput::text("look up, then ask"))
        .id(lash_vocabulary::identity::TurnId::fixture(run))
        .await?;
    let input = handle.input_id().clone();
    let key = tokio::time::timeout(std::time::Duration::from_secs(60), async {
        let key = world.keys.recv().await.expect("the tools outlive the law");
        let store = world.stores.durable_store();
        let turn = lash_vocabulary::identity::TurnId::fixture(run);
        // The key is sent inside the body, before Pending commits. Cancel
        // only after that result is durable, while the approval is pending.
        loop {
            let roots = store.cell_roots(session.session_id(), &turn).await?;
            let [root] = roots.as_slice() else {
                panic!("the admitted approval retains its one cell: {roots:?}");
            };
            {
                let facts = store.run_facts(&root.exec.owner()).await?;
                if facts.rows.iter().any(|row| row.kind == RunFactKind::XWait) {
                    // Pending alone leaves the live cell able to observe a
                    // terminal cancellation. Join that observer before the
                    // cancel, so this law reads the still-pending cell's rows.
                    world.core.node.stop().await;
                    let stopped = world.core.node.stopped(&world.core).await;
                    assert!(
                        matches!(&stopped, Some(Ok(Stopped::Requested))),
                        "the pending cell's owner stopped and joined: {stopped:?}"
                    );
                    let actor = ActorKey::session(session.session_id().as_str())
                        .expect("the session actor");
                    let actor = store.actor(&actor).await?.expect("the retained actor");
                    assert!(
                        actor.owner.is_none(),
                        "the cell has no live owner: {actor:?}"
                    );
                    return Ok::<_, lash_core::durable_port::DurableError>(key);
                }
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the cell parks on its approval")?;
    let (settled, pending) =
        lash_core::runtime::durable::services::RuntimeTurnServices::tool_call_bodies(
            &world.core.actor_context(),
            session.session_id(),
            &lash_vocabulary::identity::TurnId::fixture(run),
            Some(&lash_vocabulary::identity::TurnId::fixture(run)),
            &world.core.transcript_decoders(),
        )
        .await?;
    assert!(!pending.is_empty(), "the parked approval is a known call");
    assert_eq!(
        settled.len(),
        1,
        "the saved lookup already has a readable body"
    );
    assert_eq!(
        session
            .read_tool_call(&crate::TurnToolCallRef {
                turn_id: lash_vocabulary::identity::TurnId::fixture(run),
                call_id: settled[0].call_id.clone()
            })
            .await?,
        crate::ToolCallRead::Settled {
            record: Box::new(settled[0].clone())
        }
    );
    for id in &pending {
        assert!(matches!(
            session
                .read_tool_call(&crate::TurnToolCallRef {
                    turn_id: lash_vocabulary::identity::TurnId::fixture(run),
                    call_id: id.clone()
                })
                .await?,
            crate::ToolCallRead::NotYetSettled { .. }
        ));
    }
    assert!(
        !world
            .rows(&format!("exec_snapshots c/{session_name}/{run}/"))
            .is_empty(),
        "the parked cell holds its root"
    );
    let cancel = handle
        .cancel()
        .request_id("retire-cancelled-stop")
        .mode(TurnCancelMode::Immediate)
        .await?;
    let crate::CancelReceipt::Cancelled { receipt, .. } = &cancel else {
        panic!("the pending turn accepts its cancellation: {cancel:?}");
    };
    assert!(
        matches!(
            &receipt.outcome,
            lash_core::facade_support::TurnCancelOutcome::Requested(_)
        ),
        "the cancel committed before a successor can resume the cell: {receipt:?}"
    );
    let backend = lash_conformance::backend_over(Arc::clone(&world.stores) as Arc<_>);
    let (keys, unexpected_approval) = tokio::sync::mpsc::unbounded_channel();
    let successor = explicit_ephemeral_facets(rlm_core_builder_over(backend))
        .serve_test_llm_profile(
            part_provider("turn-retirement", cell(false)),
            mock_llm_profile_spec(),
        )
        .plugin(
            ApprovalTools {
                keys,
                return_gate: None,
            }
            .plugin(),
        )
        .build(crate::tests::host::runtime_lease_owner())?;
    let previous = std::mem::replace(&mut world.core, successor);
    let live = handle.output().await?;
    assert!(
        unexpected_approval.is_empty(),
        "the successor honours Immediate cancellation before running the approval"
    );
    let live_calls = crate::testing::tool_calls_for_activity(&session, &live.activities).await;
    let live = live.outcome;
    assert_eq!(
        crate::TurnStatus::of_outcome(&live),
        crate::TurnStatus::Cancelled
    );
    assert_eq!(
        tools(&live_calls),
        vec!["tools.app_lookup".to_owned()],
        "the stopped cell's settled lookup is reported: {live_calls:#?}"
    );

    assert_retired(&world, session_name, run).await;
    assert_eq!(
        world.resolve(&key, serde_json::json!("late")).await,
        lash_core::ResolveAnswer::Revoked,
        "a resolution after the turn's end answers Revoked"
    );
    world.collect().await;
    let follower = session.attach(input);
    let followed_run = follower.run().await?.expect("settled run");
    follower.output().await?;
    let durable_calls = session
        .read_turn_tool_calls(&lash_vocabulary::identity::TurnId::parse(
            followed_run.as_str(),
        )?)
        .await?;
    assert_eq!(
        durable_calls, live_calls,
        "the transcript reads the same calls as the live read"
    );
    drop(session);
    previous.shutdown().await?;
    world.core.shutdown().await?;
    Ok(())
}

/// A turn whose cell parks on an approval the host grants, and then
/// finishes, consumes the cell at its end: its root, admissions and own
/// facts go, its waits compact, and its transcript replaces its ledger. The compacted
/// approval still answers its host as ADR 0132 §6 says: the granted
/// answer's retry is `AlreadyResolved`, another answer a `Conflict`. A
/// follower's report after a collection is the live one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_committed_turn_retires_its_consumed_cell_and_compacts_its_waits() -> Result<()> {
    let (session_name, run) = ("retire-committed", "retire-committed-run");
    let mut world = World::new(true).await?;
    let session = world.session(session_name).await?;
    let handle = session
        .send(TurnInput::text("look up, then ask"))
        .id(lash_vocabulary::identity::TurnId::fixture(run))
        .await?;
    let input = handle.input_id().clone();
    let key = world.parked().await;
    let granted = serde_json::json!({ "approved": true });
    assert_eq!(
        world.resolve(&key, granted.clone()).await,
        lash_core::ResolveAnswer::Resolved
    );
    let live = handle.output().await?;
    let live_calls = crate::testing::tool_calls_for_activity(&session, &live.activities).await;
    let live = live.outcome;
    assert_eq!(
        crate::TurnStatus::of_outcome(&live),
        crate::TurnStatus::Answered
    );
    assert_eq!(
        tools(&live_calls).len(),
        3,
        "the cell's lookup, approval and finish are reported: {:?}",
        tools(&live_calls)
    );

    assert_retired(&world, session_name, run).await;
    let transcript = lash_core::testing::turn_transcript(
        world.core.actor_context().durable_reads()?,
        session.session_id(),
        &lash_vocabulary::identity::TurnId::fixture(run),
    )
    .await?;
    let sizes = lash_core::testing::cell_ledger_bytes(&transcript, &live_calls)
        .expect("the stored ledger sizes read");
    eprintln!(
        "FIG-6171 3 calls: report={} held={} previous_copy={}",
        sizes.0, sizes.1, sizes.2
    );
    assert!(
        sizes.1 < sizes.2,
        "the full retained call ledger shrinks: {sizes:?}"
    );
    assert_eq!(
        world.resolve(&key, granted).await,
        lash_core::ResolveAnswer::AlreadyResolved,
        "a lost acknowledgement's retry"
    );
    assert_eq!(
        world.resolve(&key, serde_json::json!("other")).await,
        lash_core::ResolveAnswer::Conflict,
        "another answer after the first won"
    );
    world.collect().await;
    let follower = session.attach(input);
    let followed_run = follower.run().await?.expect("settled run");
    follower.output().await?;
    let durable_calls = session
        .read_turn_tool_calls(&lash_vocabulary::identity::TurnId::parse(
            followed_run.as_str(),
        )?)
        .await?;
    assert_eq!(durable_calls, live_calls);
    for call in &live_calls {
        assert_eq!(
            session
                .read_tool_call(&crate::TurnToolCallRef {
                    turn_id: lash_vocabulary::identity::TurnId::fixture(run),
                    call_id: call.call_id.clone()
                })
                .await?,
            crate::ToolCallRead::Settled {
                record: Box::new(call.clone())
            }
        );
    }
    drop(session);
    world.core.shutdown().await?;
    Ok(())
}

/// A session's delete removes every engine row that names it but its
/// terminal actor: its runs, with their transcripts, and its wait
/// tombstones go in the same transaction, and its delete and a collection
/// reclaim the nodes only its transcripts held. Another session's
/// transcript, and its report, stay whole.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_deleted_session_leaves_no_engine_row_and_only_its_own_nodes_go() -> Result<()> {
    let mut world = World::new(true).await?;
    let mut transcripts = Vec::new();
    let mut kept = None;
    for name in ["retire-deleted", "retire-kept"] {
        let run = format!("{name}-run");
        let session = world.session(name).await?;
        let handle = session
            .send(TurnInput::text("look up, then ask"))
            .id(lash_vocabulary::identity::TurnId::fixture(run.clone()))
            .await?;
        let input = handle.input_id().clone();
        let key = world.parked().await;
        world
            .resolve(&key, serde_json::json!({ "approved": true }))
            .await;
        let live = handle.output().await?;
        let live_calls = crate::testing::tool_calls_for_activity(&session, &live.activities).await;
        let live = live.outcome;
        assert_eq!(
            crate::TurnStatus::of_outcome(&live),
            crate::TurnStatus::Answered
        );
        transcripts.push(assert_retired(&world, name, &run).await);
        if name == "retire-kept" {
            kept = Some((session, input, live_calls));
        }
    }
    let (deleted, others) = (&transcripts[0], &transcripts[1]);

    delete_session_and_await(&world.core, "retire-deleted").await?;
    world.collect().await;
    let left = world.rows("retire-deleted");
    assert!(
        left.iter().all(|row| row.starts_with("actors ")),
        "the deleted session left engine rows: {left:#?}"
    );
    let blobs = world.blobs();
    for node in deleted.difference(others) {
        assert!(
            !blobs.contains(node),
            "the deleted session's transcript retained node {node}"
        );
    }
    assert!(
        others.iter().all(|node| blobs.contains(node)),
        "another session's transcript stays reachable"
    );
    let (session, input, live_calls) = kept.expect("the kept session ran");
    let follower = session.attach(input);
    let followed_run = follower.run().await?.expect("settled run");
    follower.output().await?;
    let durable_calls = session
        .read_turn_tool_calls(&lash_vocabulary::identity::TurnId::parse(
            followed_run.as_str(),
        )?)
        .await?;
    assert_eq!(durable_calls, live_calls);
    drop(session);
    world.core.shutdown().await?;
    Ok(())
}

/// RETIRED-SESSION-USED (FIG-6335): a run using a removed session
/// provider refuses before it resumes any work.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_recorded_run_using_a_retired_session_plugins_effect_parks_typed() -> Result<()> {
    let mut world = World::new(true).await?;
    let session = world.session("retired-session-used").await?;
    let running = session.send(TurnInput::text("approval")).await?;
    let key = world.parked().await;
    world.core.node.stop().await;
    let backend = lash_conformance::backend_over(Arc::clone(&world.stores) as Arc<_>);
    let second = explicit_ephemeral_facets(rlm_core_builder_over(backend))
        .retire_session_plugin(crate::plugins::PluginId::new("tools"))
        .serve_test_llm_profile(
            part_provider("turn-retirement", cell(true)),
            mock_llm_profile_spec(),
        )
        .build(crate::tests::host::runtime_lease_owner())?;
    second
        .completions()
        .resolve(&key, lash_core::Resolution::Ok(serde_json::json!({})))
        .await?;
    let durable = second
        .session(lash_kernel_value::identity::SessionId::fixture(
            "retired-session-used",
        ))
        .durable()
        .await?;
    let park = tokio::time::timeout(std::time::Duration::from_secs(60), async {
        loop {
            if let Some(park) = durable.park_reason().await? {
                return Ok::<_, EmbedError>(park);
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await;
    assert!(
        matches!(park, Ok(Ok(crate::SessionParkReason::PluginRetired {
        refusal: crate::plugins::RetirementRefusal::Serves { registry: lash_vocabulary::claims::RegistryKind::Session, ref id, .. }
    })) if id == "tools"),
        "the removed provider must produce a typed park: {park:?}"
    );
    drop(running);
    second.shutdown().await?;
    recorded_session_with_retired_plugin(true).await
}

/// RETIRED-SESSION-SEALED (FIG-6335): recorded before-turn decisions
/// survive removal of their plugin; a resumed run invokes none again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_recorded_session_naming_a_retired_plugin_rebuilds_without_it_and_resumes() -> Result<()>
{
    recorded_session_with_retired_plugin(false).await
}

async fn recorded_session_with_retired_plugin(retain_state: bool) -> Result<()> {
    let stores = sqlite_memory_store_set().await;
    let backend = lash_conformance::backend_over(Arc::clone(&stores) as Arc<_>);
    let entered = Arc::new(tokio::sync::Notify::new());
    let calls = Arc::new(AtomicUsize::new(0));
    let plugin = StaticPluginFactory::new(
        crate::plugins::PluginDeclaration::initial("retired-spare"),
        crate::plugins::PluginSpec::new().with_before_turn(crate::hook_key!("prepare"), {
            let calls = Arc::clone(&calls);
            Arc::new(move |_| {
                calls.fetch_add(1, Ordering::SeqCst);
                Box::pin(async move {
                    Ok(crate::plugins::TurnContributions {
                        state: if retain_state {
                            crate::plugins::StateCommands::new()
                                .set("kept", serde_json::json!(true))
                        } else {
                            Default::default()
                        },
                        ..Default::default()
                    })
                })
            })
        }),
    );
    let provider = crate::testing::TestProvider::builder()
        .kind("retired-session-unused")
        .complete({
            let entered = Arc::clone(&entered);
            move |_| {
                let entered = Arc::clone(&entered);
                async move {
                    entered.notify_one();
                    std::future::pending().await
                }
            }
        })
        .build()
        .into_handle();
    let first = explicit_ephemeral_facets(rlm_core_builder_over(backend.clone()))
        .plugin(Arc::new(plugin))
        .serve_test_llm_profile(provider, mock_llm_profile_spec())
        .build(crate::tests::host::runtime_lease_owner())?;
    let session = created_session(
        &first,
        lash_kernel_value::identity::SessionId::fixture("retired-session-unused"),
    )
    .await
    .durable()
    .await?;
    let running = session.send(TurnInput::text("resume me")).await?;
    tokio::time::timeout(std::time::Duration::from_secs(60), entered.notified())
        .await
        .expect("the first model call starts");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    first.node.stop().await;
    let second = explicit_ephemeral_facets(rlm_core_builder_over(backend))
        .retire_session_plugin(crate::plugins::PluginId::new("retired-spare"))
        .serve_test_llm_profile(
            part_provider(
                "retired-session-unused",
                typescript_block("await control.finish(\"resumed\");"),
            ),
            mock_llm_profile_spec(),
        )
        .build(crate::tests::host::runtime_lease_owner())?;
    if retain_state {
        let durable = second
            .session(lash_kernel_value::identity::SessionId::fixture(
                "retired-session-unused",
            ))
            .durable()
            .await?;
        let park = tokio::time::timeout(std::time::Duration::from_secs(60), async {
            loop {
                if let Some(park) = durable.park_reason().await? {
                    return Ok::<_, EmbedError>(park);
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("retained plugin state refuses before replay")?;
        assert_eq!(
            park,
            crate::SessionParkReason::PluginRetired {
                refusal: crate::plugins::RetirementRefusal::HasState {
                    registry: lash_vocabulary::claims::RegistryKind::Session,
                    id: "retired-spare".into(),
                }
            }
        );
        drop(running);
    } else {
        let output = tokio::time::timeout(std::time::Duration::from_secs(60), running.output())
            .await
            .expect("the recorded turn resumes")?;
        assert_eq!(
            crate::TurnStatus::of_outcome(&output.outcome),
            crate::TurnStatus::Answered
        );
    }
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "recorded decisions are read without the retired callback"
    );
    second.shutdown().await?;
    Ok(())
}

/// RETIRED-SESSION-NEW (FIG-6335): new sessions cannot request a retired
/// plugin. The refusal retains its typed cause across the facade boundary.
#[tokio::test]
async fn a_new_session_naming_a_retired_plugin_is_refused_typed() -> Result<()> {
    let backend = sqlite_memory_store_backend().await;
    let builder = || {
        explicit_ephemeral_facets(rlm_core_builder_over(backend.clone()))
            .retire_session_plugin(crate::plugins::PluginId::new("retired-spare"))
            .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
    };
    assert!(
        builder()
            .retire_session_plugin(crate::plugins::PluginId::new("retired-spare"))
            .build(crate::tests::host::runtime_lease_owner())
            .is_err(),
        "duplicate retirement refuses the build"
    );
    assert!(
        builder()
            .plugin(Arc::new(StaticPluginFactory::new(
                crate::plugins::PluginDeclaration::initial("retired-spare"),
                crate::plugins::PluginSpec::new(),
            )))
            .build(crate::tests::host::runtime_lease_owner())
            .is_err(),
        "an installed retirement refuses the build"
    );
    let core = builder().build(crate::tests::host::runtime_lease_owner())?;
    let mut spec = mock_session_spec();
    spec.plugin_options
        .insert_typed("retired-spare", serde_json::json!({}))
        .expect("plain config");
    let error = match core
        .session(lash_kernel_value::identity::SessionId::fixture(
            "retired-session-new",
        ))
        .create(crate::SessionCreation::root(spec))
        .await
    {
        Ok(_) => panic!("a new session refuses its retired plugin"),
        Err(error) => error,
    };
    let EmbedError::Session(lash_core::SessionError::Plugin(crate::plugins::PluginError::Runtime(
        error,
    ))) = error
    else {
        panic!("a typed plugin refusal: {error:?}");
    };
    assert!(error.is_terminal(), "admission refuses permanently");
    assert!(
        matches!(error.cause, Some(lash_core::RuntimeErrorCause::PluginRetired { refusal })
        if *refusal == crate::plugins::RetirementRefusal::Admission { registry: lash_vocabulary::claims::RegistryKind::Session, id: "retired-spare".into() })
    );
    core.shutdown().await?;
    Ok(())
}

/// A settled turn's tool calls as each read path answers them on `core`:
/// the per-turn bulk read, the committed-turn read, and each call's
/// turn-addressed read, after a follower has observed settlement.
async fn settled_reads(
    core: &LashCore,
    session: &str,
    run: &str,
    input: &lash_vocabulary::identity::InputId,
) -> Result<Vec<lash_core::ToolCallRecord>> {
    let session = core
        .session(lash_kernel_value::identity::SessionId::fixture(session))
        .durable()
        .await?;
    session.attach(input.clone()).output().await?;
    let calls = session
        .read_turn_tool_calls(&lash_vocabulary::identity::TurnId::fixture(run))
        .await?;
    let committed = session
        .committed_turn(&lash_vocabulary::identity::TurnId::fixture(run))
        .await?
        .expect("the turn committed");
    assert_eq!(committed.tool_calls, calls);
    for call in &calls {
        assert_eq!(
            session
                .read_tool_call(&crate::TurnToolCallRef {
                    turn_id: lash_vocabulary::identity::TurnId::fixture(run),
                    call_id: call.call_id.clone()
                })
                .await?,
            crate::ToolCallRead::Settled {
                record: Box::new(call.clone())
            }
        );
    }
    Ok(calls)
}

/// The turn's operational ledger rows.
fn ledger(stores: &lash_sqlite_store::SqliteStoreSet, session: &str, run: &str) -> Vec<String> {
    lash_sqlite_store::testing::engine_rows_for_testing(stores)
        .expect("the engine rows read")
        .into_iter()
        .filter(|row| row.starts_with(&format!("run_records t/{session}/{run} ")))
        .collect()
}

/// The transcript root the turn's terminal published.
fn transcript(
    stores: &lash_sqlite_store::SqliteStoreSet,
    session: &str,
    run: &str,
) -> Option<String> {
    lash_sqlite_store::testing::read_rows_for_testing(
        stores,
        &format!(
            "SELECT terminal_transcript_ref FROM session_runs \
             WHERE session_id = '{session}' AND run = '{run}'"
        ),
    )
    .expect("the run row reads")
    .into_iter()
    .next()
    .and_then(|row| {
        row.first()
            .and_then(|(_, hash)| hash.as_str().map(str::to_owned))
    })
}

/// TERMINAL-TRANSCRIPT-WITHOUT-RECOVERY-LOG (FIG-6601), its cell leg: a
/// turn whose cell parks on an approval loses its node before its terminal.
/// A successor recovers the turn from its ledger, the cell answers, and the
/// terminal publishes the turn's transcript and retires the ledger in the
/// same commit. Every read of the settled calls answers the cell's lookup
/// and approval from the transcript alone, and a reopened core reads the
/// same records after a collection.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_recovered_cell_turn_reads_its_calls_from_its_transcript_without_its_ledger() -> Result<()>
{
    use lash_core::durable_port::domain::{DurableReads as _, OwnerKey, RunFactKind};

    let (session_name, run) = ("transcript-cell", "transcript-cell-run");
    let return_gate = Arc::new(tokio::sync::Notify::new());
    let mut world = World::with_return_gate(true, Some(return_gate.clone())).await?;
    let session = world.session(session_name).await?;
    let handle = session
        .send(TurnInput::text("look up, then ask"))
        .id(lash_vocabulary::identity::TurnId::fixture(run))
        .await?;
    let input = handle.input_id().clone();
    let key = world.parked().await;
    assert!(
        !world
            .rows(&format!("exec_snapshots c/{session_name}/{run}/"))
            .is_empty(),
        "the parked cell's recovery state is saved before the terminal"
    );
    // Receiving a key proves admission, not that the body's Pending result
    // committed. Hold that gap open, then wait for the recorded park before
    // losing the node: an unrecorded Once result recovers as Interrupted.
    let snapshots = world.rows(&format!("exec_snapshots c/{session_name}/{run}/"));
    let owner = OwnerKey::parse(
        snapshots[0]
            .strip_prefix("exec_snapshots ")
            .expect("a cell snapshot key"),
    )
    .expect("the cell's ledger owner");
    let store = world.stores.durable_store();
    assert!(
        store
            .run_facts(&owner)
            .await?
            .rows
            .iter()
            .all(|row| row.kind != RunFactKind::XWait),
        "the tool still holds its Pending result after handing out the key"
    );
    return_gate.notify_one();
    tokio::time::timeout(std::time::Duration::from_secs(60), async {
        loop {
            if store
                .run_facts(&owner)
                .await
                .expect("the cell's ledger reads")
                .rows
                .iter()
                .any(|row| row.kind == RunFactKind::XWait)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the approval's park commits before node loss");
    world.core.node.stop().await;
    drop((handle, session));

    let backend = lash_conformance::backend_over(Arc::clone(&world.stores) as Arc<_>);
    let successor = || {
        let (keys, received) = tokio::sync::mpsc::unbounded_channel();
        explicit_ephemeral_facets(rlm_core_builder_over(backend.clone()))
            .serve_test_llm_profile(
                part_provider("turn-retirement", cell(true)),
                mock_llm_profile_spec(),
            )
            .plugin(
                ApprovalTools {
                    keys,
                    return_gate: None,
                }
                .plugin(),
            )
            .build(crate::tests::host::runtime_lease_owner())
            .map(|core| (core, received))
    };
    let (second, mut unexpected) = successor()?;
    assert_eq!(
        second
            .completions()
            .resolve(
                &key,
                lash_core::Resolution::Ok(serde_json::json!({ "approved": true }))
            )
            .await?,
        lash_core::ResolveAnswer::Resolved
    );
    let recovered = tokio::select! {
        calls = settled_reads(&second, session_name, run, &input) => calls?,
        key = unexpected.recv() => panic!(
            "recovery must resume the recorded approval, never ask for a second one: {key:?}"
        ),
    };
    assert_eq!(
        recovered
            .iter()
            .map(|call| call.output.outcome.clone())
            .collect::<Vec<_>>(),
        [
            serde_json::json!({ "found": true }),
            serde_json::json!({ "approved": true }),
            serde_json::Value::Null,
        ]
        .map(|value| lash_core::ToolCallOutcome::Success(value.into())),
        "the recovered turn reports the cell's lookup, approval and finish in order"
    );
    let left = ledger(&world.stores, session_name, run);
    assert!(
        left.is_empty(),
        "the settled turn retired its recovery ledger: {left:#?}"
    );
    assert!(
        transcript(&world.stores, session_name, run).is_some(),
        "the terminal published its transcript"
    );
    second.shutdown().await?;

    world.collect().await;
    let (reopened, _unexpected) = successor()?;
    assert_eq!(
        settled_reads(&reopened, session_name, run, &input).await?,
        recovered,
        "a reopened core reads the same calls after a collection"
    );
    reopened.shutdown().await?;
    Ok(())
}

/// The native tool of the retry leg: its first attempt fails transiently,
/// its second answers.
struct FlakyLookup {
    attempts: Arc<AtomicUsize>,
}

const FLAKY: &str = "flaky_lookup";

const FLAKY_OPERATION: &str = "tools.flaky_lookup";

fn flaky_definition() -> lash_vocabulary::tool_contract::ToolDefinition {
    tool(FLAKY, false).with_execution_policy(
        lash_kernel_value::tool_contract::ExecutionPolicy::repeatable(
            std::num::NonZeroU32::new(2).expect("nonzero attempt bound"),
            0,
            0,
        ),
    )
}

#[async_trait]
impl ToolProvider for FlakyLookup {
    fn tool_manifests(
        &self,
        _config: &lash_core::OwnerConfigView,
    ) -> std::result::Result<
        Vec<lash_vocabulary::tool_contract::ToolManifest>,
        crate::plugins::PluginError,
    > {
        Ok(vec![flaky_definition().manifest()])
    }

    fn resolve_contract(
        &self,
        name: &str,
        _config: &lash_core::OwnerConfigView,
    ) -> Option<Arc<lash_vocabulary::tool_contract::ToolContract>> {
        (name == FLAKY_OPERATION).then(|| Arc::new(flaky_definition().contract()))
    }

    async fn execute(&self, _call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        if self.attempts.fetch_add(1, Ordering::SeqCst) == 0 {
            return lash_core::ToolOutcome::failure_with_delay(
                lash_core::ToolFailureClass::External,
                "transient",
                "transient failure",
                Some(0),
            )
            .into();
        }
        lash_core::ToolOutcome::ok(serde_json::json!({ "found": "after a retry" })).into()
    }
}

/// TERMINAL-TRANSCRIPT-WITHOUT-RECOVERY-LOG (FIG-6601), its native leg: a
/// standard turn's native call fails once and answers on its retry. Its
/// terminal retires the admission, starts, retry, outcome and presentation
/// facts, and the live report's call is what every read answers from the
/// transcript, on the core that ran it and on a reopened one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_retried_native_call_reads_from_its_transcript_without_its_ledger() -> Result<()> {
    let (session_name, run) = ("transcript-native", "transcript-native-run");
    let stores = sqlite_memory_store_set().await;
    let backend = lash_conformance::backend_over(Arc::clone(&stores) as Arc<_>);
    let attempts = Arc::new(AtomicUsize::new(0));
    let core = || {
        let model = crate::testing::TestProvider::builder()
            .kind("transcript-native")
            .complete(move |request| async move {
                let answered = request.messages.iter().any(|message| {
                    message
                        .blocks
                        .iter()
                        .any(|block| matches!(block, LlmContentBlock::ToolResult { .. }))
                });
                if answered {
                    return Ok(text_response("looked up"));
                }
                Ok(LlmResponse {
                    parts: vec![LlmOutputPart::ToolCall {
                        call_id: "flaky-call".to_string(),
                        tool_name: FLAKY_OPERATION.replace('.', "__"),
                        input_json: "{}".to_string(),
                        replay: None,
                    }],
                    ..LlmResponse::default()
                })
            })
            .build()
            .into_handle();
        explicit_ephemeral_facets(LashCore::standard_builder(backend.clone()))
            .serve_test_llm_profile(model, mock_llm_profile_spec())
            .tools(
                crate::tool_provider_key!("tools"),
                Arc::new(FlakyLookup {
                    attempts: Arc::clone(&attempts),
                }),
            )
            .build(crate::tests::host::runtime_lease_owner())
    };
    let first = core()?;
    let session = first
        .session(lash_kernel_value::identity::SessionId::fixture(
            session_name,
        ))
        .create(crate::SessionCreation::root(mock_session_spec()))
        .await?;
    let handle = session
        .send(TurnInput::text("look it up"))
        .id(lash_vocabulary::identity::TurnId::fixture(run))
        .await?;
    let input = handle.input_id().clone();
    let live = handle.output().await?;
    let live_calls = session
        .read_turn_tool_calls(&lash_vocabulary::identity::TurnId::fixture(run))
        .await?;
    assert_eq!(live.status(), crate::TurnStatus::Answered);
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        2,
        "the call was retried once"
    );
    assert_eq!(tools(&live_calls), vec![FLAKY_OPERATION.to_owned()]);
    let left = ledger(&stores, session_name, run);
    assert!(
        left.is_empty(),
        "the settled turn retired its recovery ledger: {left:#?}"
    );
    assert!(
        transcript(&stores, session_name, run).is_some(),
        "the terminal published its transcript"
    );
    assert_eq!(
        settled_reads(&first, session_name, run, &input).await?,
        live_calls
    );
    drop(session);
    first.shutdown().await?;

    let reopened = core()?;
    assert_eq!(
        settled_reads(&reopened, session_name, run, &input).await?,
        live_calls,
        "a reopened core reads the live report's call"
    );
    reopened.shutdown().await?;
    Ok(())
}

/// FIG-7358: a capture can reuse committed pages while still owing reader cleanup.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_deleted_session_reclaims_a_reused_capture_on_sqlite() {
    // This publication law needs no process environment; keep its graph free
    // of the unrelated environment artifact the general facade fixture installs.
    let stores = Arc::new(lash_sqlite_store::SqliteStoreSet::memory().await.unwrap());
    let backend = lash_conformance::backend_over(stores);
    lash_core::testing::deleted_session_reclaims_reused_capture(backend).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires PostgreSQL; run with --include-ignored inside a with-service.sh pg gate"]
async fn a_deleted_session_reclaims_a_reused_capture_on_postgres() {
    let (stores, _database, _attachments) = postgres_store_parts().await;
    let backend = lash_conformance::backend_over(stores);
    lash_core::testing::deleted_session_reclaims_reused_capture(backend).await;
}

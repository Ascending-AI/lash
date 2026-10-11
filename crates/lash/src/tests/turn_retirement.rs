//! What a turn's end and its session's delete leave behind (FIG-5939; ADR
//! 0067 §3, ADR 0132 §4, §6).
//!
//! A turn's run facts are its one tool-call ledger: they live as long as its
//! run row, and its report reads them alone. Everything else a turn wrote per
//! call is working state that dies with its last reader: a consumed cell's
//! own facts and admissions with its root, the root, admissions and facts of
//! a cell the turn stopped in, and the turn's cancel request at its end; its
//! waits compact to the tombstones a late resolution is answered from. A
//! session's delete removes every engine row it owns but its terminal actor.
//!
//! Every law runs RLM turns on the core's own node over a SQLite memory
//! store set, whose engine rows it reads back raw.

use super::*;
use crate::{TurnCancelMode, TurnInput};

/// The cell every turn runs: a lookup that answers at once, then an
/// approval that parks on its completion key, then, when `finishes`, the
/// turn's finish.
fn cell(finishes: bool) -> String {
    let finish = if finishes {
        "\nawait control.finish(\"approved\");"
    } else {
        ""
    };
    typescript_block(&format!(
        "const found = await tools.app_lookup({{}});\n\
         const approved = await tools.app_approve({{}});{finish}"
    ))
}

/// `tools.<name>`; a deferring one may park on its completion key.
fn tool(name: &str, deferring: bool) -> lash_core::ToolDefinition {
    let definition = test_tool_definition_with_tool_binding(
        lash_core::ToolDefinition::raw(
            format!("tool:{name}"),
            name,
            "A law's tool.",
            serde_json::json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
            serde_json::json!({ "type": "object" }),
        )
        .expect("valid declared tool schemas")
        .with_execution(std::time::Duration::from_secs(120)),
        name,
    );
    if !deferring {
        return definition;
    }
    definition
        .with_declaration(
            lash_core::ToolDeclaration::deferring(),
            Some(lash_core::ParkBound::Within(
                std::time::Duration::from_secs(120),
            )),
        )
        .expect("a deferring tool declares its park bound")
}

/// `app_lookup` answers at once; `app_approve` parks, handing the law its
/// completion key.
struct ApprovalTools {
    keys: tokio::sync::mpsc::UnboundedSender<String>,
}

#[async_trait]
impl ToolProvider for ApprovalTools {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![
            tool("app_lookup", false).manifest(),
            tool("app_approve", true).manifest(),
        ]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        ["app_lookup", "app_approve"]
            .contains(&name)
            .then(|| Arc::new(tool(name, name == "app_approve").contract()))
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        if call.name() == "app_lookup" {
            return lash_core::ToolOutcome::ok(serde_json::json!({ "found": true })).into();
        }
        let key = match call.context.completion_key() {
            Ok(key) => key,
            Err(error) => return lash_core::ToolOutcome::err_fmt(error).into(),
        };
        let _ = self.keys.send(key.as_str().to_owned());
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
        let stores = sqlite_memory_store_set().await;
        let backend = lash_conformance::backend_over(Arc::clone(&stores) as Arc<_>);
        let (keys, received) = tokio::sync::mpsc::unbounded_channel();
        let core = explicit_ephemeral_facets(rlm_core_builder_over(backend))
            .serve_test_llm_profile(
                text_provider("turn-retirement", cell(finishes)),
                mock_llm_profile_spec(),
            )
            .tools(Arc::new(ApprovalTools { keys }))
            .build(crate::testing::runtime_lease_owner())?;
        Ok(Self {
            stores,
            core,
            keys: received,
        })
    }

    async fn session(&self, name: &str) -> Result<crate::DurableSession> {
        self.core
            .session(crate::SessionId::fixture(name))
            .created()
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

    async fn collect(&self) {
        self.core
            .store_factory
            .gc_unreachable()
            .await
            .expect("the host collection completes");
    }
}

/// The engine rows `run` of `session` left at its end: none of its cells'
/// and no cancel request, only wait tombstones, and its ledger. Answers the
/// ledger's RoundFact nodes.
fn assert_retired(world: &World, session: &str, run: &str) -> Vec<String> {
    let journal = lash_core::ExecutionScope::turn(
        crate::SessionId::fixture(session),
        crate::TurnId::fixture(run),
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
    let ledger = world.rows(&format!("run_records t/{session}/{run} "));
    assert!(!ledger.is_empty(), "the turn's end kept its ledger");
    ledger
        .iter()
        .filter_map(|row| row.rsplit(' ').next().map(str::to_owned))
        .collect()
}

/// The tool names a report lists, in its order.
fn tools(report: &crate::TurnReport) -> Vec<String> {
    report
        .tool_calls
        .iter()
        .map(|call| call.tool.clone())
        .collect()
}

/// A turn cancelled while its cell parks on an approval retires, in its
/// cancel, the cell it stopped in (root, admissions, the cell's own facts),
/// its cancel request and its waits' payloads, and keeps its ledger. The
/// report a follower reads after that cleanup, and after a collection, is
/// the live one, the stopped cell's lookup included; a late resolution of
/// the approval answers `Revoked`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cancelled_turn_retires_the_cell_it_stopped_in_and_keeps_its_ledger() -> Result<()> {
    let (session_name, run) = ("retire-cancelled", "retire-cancelled-run");
    let mut world = World::new(false).await?;
    let session = world.session(session_name).await?;
    let handle = session
        .send(TurnInput::text("look up, then ask"))
        .id(crate::TurnId::fixture(run))
        .await?;
    let input = handle.input_id().clone();
    let key = world.parked().await;
    assert!(
        !world
            .rows(&format!("exec_snapshots c/{session_name}/{run}/"))
            .is_empty(),
        "the parked cell holds its root"
    );
    handle
        .cancel()
        .request_id("retire-cancelled-stop")
        .mode(TurnCancelMode::Immediate)
        .await?;
    let live = handle.output().await?.result;
    assert_eq!(live.status(), crate::TurnStatus::Cancelled);
    assert_eq!(
        tools(&live),
        vec!["app_lookup".to_owned()],
        "the stopped cell's settled lookup is reported"
    );

    assert_retired(&world, session_name, run);
    assert_eq!(
        world.resolve(&key, serde_json::json!("late")).await,
        lash_core::ResolveAnswer::Revoked,
        "a resolution after the turn's end answers Revoked"
    );
    world.collect().await;
    let durable = session.attach(input).output().await?.result;
    assert_eq!(durable.source, crate::ReportSource::Durable);
    assert_eq!(
        durable.tool_calls, live.tool_calls,
        "the ledger reports what the live report did"
    );
    assert_eq!(durable.omitted, live.omitted);
    drop(session);
    world.core.shutdown().await?;
    Ok(())
}

/// A turn whose cell parks on an approval the host grants, and then
/// finishes, consumes the cell at its end: its root, admissions and own
/// facts go, its waits compact, and its ledger stays. The compacted
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
        .id(crate::TurnId::fixture(run))
        .await?;
    let input = handle.input_id().clone();
    let key = world.parked().await;
    let granted = serde_json::json!({ "approved": true });
    assert_eq!(
        world.resolve(&key, granted.clone()).await,
        lash_core::ResolveAnswer::Resolved
    );
    let live = handle.output().await?.result;
    assert_eq!(live.status(), crate::TurnStatus::Answered);
    assert_eq!(
        tools(&live).len(),
        3,
        "the cell's lookup, approval and finish are reported: {:?}",
        tools(&live)
    );

    assert_retired(&world, session_name, run);
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
    let durable = session.attach(input).output().await?.result;
    assert_eq!(durable.tool_calls, live.tool_calls);
    assert_eq!(durable.omitted, live.omitted);
    drop(session);
    world.core.shutdown().await?;
    Ok(())
}

/// A session's delete removes every engine row that names it but its
/// terminal actor: its turns' ledgers and wait tombstones go in the same
/// transaction, and its delete and a collection reclaim the nodes only its
/// ledgers held. Another session's ledger, and its report, stay whole.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_deleted_session_leaves_no_engine_row_and_only_its_own_nodes_go() -> Result<()> {
    let mut world = World::new(true).await?;
    let mut ledgers = Vec::new();
    let mut kept = None;
    for name in ["retire-deleted", "retire-kept"] {
        let run = format!("{name}-run");
        let session = world.session(name).await?;
        let handle = session
            .send(TurnInput::text("look up, then ask"))
            .id(crate::TurnId::fixture(run.clone()))
            .await?;
        let input = handle.input_id().clone();
        let key = world.parked().await;
        world
            .resolve(&key, serde_json::json!({ "approved": true }))
            .await;
        let live = handle.output().await?.result;
        assert_eq!(live.status(), crate::TurnStatus::Answered);
        ledgers.push(assert_retired(&world, name, &run));
        if name == "retire-kept" {
            kept = Some((session, input, live));
        }
    }
    let (deleted, others) = (&ledgers[0], &ledgers[1]);

    delete_session_and_await(&world.core, "retire-deleted").await?;
    world.collect().await;
    let left = world.rows("retire-deleted");
    assert!(
        left.iter().all(|row| row.starts_with("actors ")),
        "the deleted session left engine rows: {left:#?}"
    );
    let blobs = world.blobs();
    let reclaimed = deleted.iter().filter(|node| !blobs.contains(*node)).count();
    assert_eq!(
        reclaimed,
        deleted.len(),
        "the deleted session's ledger nodes are reclaimed"
    );
    assert!(
        others.iter().all(|node| blobs.contains(node)),
        "another session's ledger nodes survive"
    );
    let (session, input, live) = kept.expect("the kept session ran");
    let durable = session.attach(input).output().await?.result;
    assert_eq!(durable.tool_calls, live.tool_calls);
    drop(session);
    world.core.shutdown().await?;
    Ok(())
}

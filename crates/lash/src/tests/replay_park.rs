//! FIG-3586 end to end: a real code cell whose redrive cannot replay its
//! journal parks its turn.
//!
//! Each law runs the RLM facade over a file-backed SQLite backend. The first
//! run of a turn calls a tool from a code cell and is cut down by a journal
//! fault after the tool dispatched, so the turn aborts with its journal
//! holding the call. The redrive then runs under a changed build — the tool
//! the cell's alias resolves to moved (FIG-3587's drift), or the journaled
//! execution-environment sync predates the replay-key grammar stamp — and must
//! park: the turn aborts with the typed refusal, its input stays held, a
//! `TurnPark` is recorded, nothing terminal is written, and nothing is
//! dispatched, on every redrive.

use super::*;
use lash_core::runtime::effect::effect_replay_driver::EffectJournalFaultPoint;

const CELL: &str =
    "<typescript>\nconst reply = await tools.probe({});\nfinish(reply);\n</typescript>";
const TURN: &str = "parked-cell-turn";

/// `tools.probe`, resolving to `tool:{id}` so a redrive can move the alias.
struct ProbeTool {
    id: &'static str,
    executions: Arc<AtomicUsize>,
}

impl ProbeTool {
    fn definition(&self) -> lash_core::ToolDefinition {
        lash_core::ToolDefinition::raw(
            format!("tool:{}", self.id),
            self.id.to_string(),
            "Replay-park probe tool.",
            serde_json::json!({ "type": "object", "properties": {}, "additionalProperties": false }),
            serde_json::json!({ "type": "object" }),
        )
        .with_tool_binding(lash_core::ToolBinding::new(["tools"], "probe"))
    }
}

#[async_trait]
impl lash_core::ToolProvider for ProbeTool {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![self.definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        let definition = self.definition();
        (definition.manifest.name == name).then(|| Arc::new(definition.contract()))
    }

    async fn execute(&self, _call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        self.executions.fetch_add(1, Ordering::SeqCst);
        (async { lash_core::ToolOutcome::ok(serde_json::json!({ "probed": true })) })
            .await
            .into()
    }
}

struct Backend {
    directory: tempfile::TempDir,
    effect_host: Arc<lash_sqlite_store::SqliteEffectHost>,
    store_factory: Arc<lash_sqlite_store::SqliteSessionStoreFactory>,
    provider_calls: Arc<AtomicUsize>,
    executions: Arc<AtomicUsize>,
}

impl Backend {
    async fn open() -> Self {
        let directory = tempfile::tempdir().expect("temporary durable backend");
        let effect_host = Arc::new(
            lash_sqlite_store::SqliteEffectHost::open(&directory.path().join("effects.sqlite"))
                .await
                .expect("file-backed SQLite effect journal"),
        );
        let store_factory = Arc::new(lash_sqlite_store::SqliteSessionStoreFactory::new(
            directory.path().join("sessions"),
        ));
        Self {
            directory,
            effect_host,
            store_factory,
            provider_calls: Arc::default(),
            executions: Arc::default(),
        }
    }

    /// A core of the build whose `tools.probe` resolves to `tool:{tool_id}`.
    fn core(&self, tool_id: &'static str) -> LashCore {
        let calls = Arc::clone(&self.provider_calls);
        let provider = crate::testing::TestProvider::builder()
            .kind("replay-park")
            .complete(move |_| {
                let calls = Arc::clone(&calls);
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok(text_response(CELL))
                }
            })
            .build()
            .into_handle();
        explicit_ephemeral_facets(rlm_core_builder())
            .provider(provider)
            .model(mock_model_spec())
            .tools(Arc::new(ProbeTool {
                id: tool_id,
                executions: Arc::clone(&self.executions),
            }))
            .effect_host(Arc::clone(&self.effect_host) as Arc<dyn EffectHost>)
            .store_factory(Arc::clone(&self.store_factory) as Arc<dyn SessionStoreFactory>)
            .build(crate::testing::runtime_lease_owner())
            .expect("file-backed SQLite RLM backend")
    }

    fn journal(&self) -> rusqlite::Connection {
        rusqlite::Connection::open(self.directory.path().join("effects.sqlite"))
            .expect("open the effect journal")
    }

    /// Every replay key the journal holds under `session_id`'s turn.
    fn keys_of(&self, session_id: &str) -> Vec<String> {
        let journal = self.journal();
        let mut statement = journal
            .prepare("SELECT replay_key FROM runtime_effect_replay ORDER BY replay_key")
            .expect("prepare the key listing");
        statement
            .query_map([], |row| row.get::<_, String>(0))
            .expect("list the journal's keys")
            .map(|key| key.expect("read a key"))
            .filter(|key| key.contains(session_id))
            .collect()
    }

    /// The key of the cell's first tool attempt in `session_id`'s turn, found
    /// by running the same turn to completion in a same-length probe session
    /// first: keys spell the session id at a fixed width, so the probe's key
    /// names the real one once its session id is substituted.
    async fn first_attempt_key(&self, probe: &str, session_id: &str) -> String {
        assert_eq!(probe.len(), session_id.len(), "same-length session ids");
        let core = self.core("probe");
        let session = core.session(probe).open().await.expect("open the probe");
        session
            .turn(TurnInput::text("probe"))
            .turn_id(TURN)
            .run()
            .await
            .expect("the probe turn completes");
        self.keys_of(probe)
            .into_iter()
            .find(|key| key.ends_with(":lk2:0000000000:attempt:1"))
            .expect("the probe's cell journaled its tool attempt")
            .replace(probe, session_id)
    }

    /// Runs `session_id`'s turn with a journal fault on the tool attempt's
    /// finalize: the tool dispatches, its settlement cannot be journaled, and
    /// the turn aborts with the journal holding the attempt.
    async fn abort_after_dispatch(&self, session_id: &str, attempt_key: &str) {
        let faults = self.effect_host.effect_journal_faults();
        faults.fail_next(EffectJournalFaultPoint::Finalize, attempt_key);
        let core = self.core("probe");
        let session = core
            .session(session_id)
            .open()
            .await
            .expect("open the session");
        let error = session
            .turn(TurnInput::text("call the probe"))
            .turn_id(TURN)
            .run()
            .await
            .expect_err("the journal fault aborts the turn");
        assert!(faults.fired(), "the armed finalize fault fired: {error:?}");
    }

    async fn park_of(&self, session_id: &str) -> Option<lash_core::store::TurnPark> {
        let store = self
            .store_factory
            .open_existing_store_by_id(&lash_core::SessionId::from(session_id))
            .await
            .expect("open the session store")
            .expect("the session exists");
        store
            .load_turn_park(&lash_core::SessionId::from(session_id))
            .await
            .expect("read the park")
    }
}

/// Redrives `TURN` of `session_id` on `core` and returns the refusal code.
async fn redrive(core: &LashCore, session_id: &str) -> lash_core::RuntimeErrorCode {
    let session = core
        .session(session_id)
        .open()
        .await
        .expect("open the session");
    let error = session
        .turn(TurnInput::text("call the probe"))
        .turn_id(TURN)
        .run()
        .await
        .expect_err("a redrive that cannot replay its journal aborts");
    let EmbedError::Runtime(runtime_error) = &error else {
        panic!("the abort is the typed runtime error: {error:?}");
    };
    runtime_error.code.clone()
}

async fn assert_parked(
    backend: &Backend,
    core: &LashCore,
    session_id: &str,
    code: lash_core::RuntimeErrorCode,
) {
    let park = backend
        .park_of(session_id)
        .await
        .expect("the refused turn is parked");
    assert_eq!(park.turn_id.as_str(), TURN);
    assert_eq!(
        lash_core::RuntimeErrorCode::LashlangCellReplayDivergence == code,
        matches!(
            park.reason,
            lash_core::store::TurnParkReason::ReplayDivergence { .. }
        ),
        "the park names the refusal: {park:?}"
    );
    let session = core
        .session(session_id)
        .open()
        .await
        .expect("open the session");
    let pending = session
        .durable()
        .pending_turn_inputs()
        .await
        .expect("read the pending inputs");
    assert_eq!(pending.len(), 1, "the parked turn's input stays accepted");
    assert!(
        matches!(
            &pending[0].status,
            lash_core::PendingTurnInputReadStatus::TurnBound { turn_id, .. } if turn_id.as_str() == TURN
        ),
        "the parked turn holds its input: {:?}",
        pending[0].status
    );
    assert!(
        session
            .durable()
            .turn_input_applications()
            .await
            .expect("read the committed applications")
            .is_empty(),
        "nothing terminal is written: the parked turn committed nothing"
    );
    let status = core.drain_status(false).await.expect("read drain status");
    assert_eq!(status.parked_turns, 1);
    assert!(!status.drained());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cell_whose_tool_moved_parks_its_turn_on_every_redrive() -> Result<()> {
    const SESSION: &str = "drift-real";
    let backend = Backend::open().await;
    let attempt_key = backend.first_attempt_key("drift-prob", SESSION).await;
    backend.abort_after_dispatch(SESSION, &attempt_key).await;
    let dispatched = backend.executions.load(Ordering::SeqCst);
    let asked = backend.provider_calls.load(Ordering::SeqCst);

    // The redrive build resolves `tools.probe` to another tool.
    let moved = backend.core("probe_v2");
    for _ in 0..2 {
        let code = redrive(&moved, SESSION).await;
        assert_eq!(
            code,
            lash_core::RuntimeErrorCode::LashlangCellReplayDivergence
        );
        assert_parked(&backend, &moved, SESSION, code).await;
        assert_eq!(
            backend.executions.load(Ordering::SeqCst),
            dispatched,
            "a parked redrive dispatches nothing"
        );
        assert_eq!(
            backend.provider_calls.load(Ordering::SeqCst),
            asked,
            "the model call replays from the journal"
        );
    }
    Ok(())
}

/// T11 end to end: the iteration's journaled sync carries no replay-key
/// grammar stamp, as one written before the stamp existed does. The redrive
/// reaches the cutover refusal through the replayed sync and parks.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cell_whose_sync_predates_the_grammar_stamp_parks_its_turn() -> Result<()> {
    const SESSION: &str = "stamp-real";
    let backend = Backend::open().await;
    let attempt_key = backend.first_attempt_key("stamp-prob", SESSION).await;
    backend.abort_after_dispatch(SESSION, &attempt_key).await;
    let dispatched = backend.executions.load(Ordering::SeqCst);

    let stripped = backend
        .journal()
        .execute(
            "UPDATE runtime_effect_replay
                SET outcome_json = replace(outcome_json, ',\"cell_replay_grammar\":2', '')
              WHERE replay_key LIKE ?1 AND outcome_json LIKE '%cell_replay_grammar%'",
            [format!("%{SESSION}%")],
        )
        .expect("strip the sync's grammar stamp");
    assert!(stripped >= 1, "the turn journaled a stamped sync");

    let core = backend.core("probe");
    for _ in 0..2 {
        let code = redrive(&core, SESSION).await;
        assert_eq!(
            code,
            lash_core::RuntimeErrorCode::LashlangCellReplayKeyFormatCutover
        );
        assert_parked(&backend, &core, SESSION, code).await;
        assert_eq!(backend.executions.load(Ordering::SeqCst), dispatched);
    }
    Ok(())
}

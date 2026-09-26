//! FIG-3586 and FIG-3587 end to end: a real code cell whose redrive cannot
//! replay its journal parks its turn.
//!
//! Each law runs the RLM facade over a file-backed SQLite backend. The first
//! run of a turn calls a tool from a code cell and is cut down by a journal
//! fault, so the turn aborts with its journal holding the call. The redrive
//! then runs under a changed build — the tool the cell's alias resolved to
//! moved, was removed or was redescribed (FIG-3587's binding drift), or the
//! journaled execution-environment sync names an older cell journal grammar —
//! and must replay what the journal recorded or park: the turn aborts with
//! the typed refusal, its input stays held, a `TurnPark` is recorded, nothing
//! terminal is written, and nothing is dispatched, on every redrive.

use super::*;
use lash_core::runtime::effect::effect_replay_driver::EffectJournalFaultPoint;

const CELL: &str =
    "<typescript>\nconst reply = await tools.probe({});\nfinish(reply);\n</typescript>";
const TURN: &str = "parked-cell-turn";

/// What the build registers for `tools.probe`.
#[derive(Clone, Copy, Debug)]
enum Probe {
    /// `tool:{id}`, so a redrive can move the alias.
    Id(&'static str),
    /// `tool:probe` under another description.
    Described(&'static str),
    /// `tool:probe` under another retry policy, a descriptor change the
    /// model's prompt does not render.
    Retried,
    /// Nothing.
    Removed,
}

struct ProbeTool {
    probe: Probe,
    executions: Arc<AtomicUsize>,
}

impl ProbeTool {
    fn definition(&self) -> Option<lash_core::ToolDefinition> {
        let (id, description) = match self.probe {
            Probe::Id(id) => (id, "Replay-park probe tool."),
            Probe::Described(description) => ("probe", description),
            Probe::Retried => ("probe", "Replay-park probe tool."),
            Probe::Removed => return None,
        };
        let mut definition = lash_core::ToolDefinition::raw(
            format!("tool:{id}"),
            id.to_string(),
            description,
            serde_json::json!({ "type": "object", "properties": {}, "additionalProperties": false }),
            serde_json::json!({ "type": "object" }),
        )
        .with_tool_binding(lash_core::ToolBinding::new(["tools"], "probe"));
        if matches!(self.probe, Probe::Retried) {
            definition.manifest.retry_policy = lash_core::ToolRetryPolicy::Safe {
                max_attempts: 3,
                base_delay_ms: 10,
                max_delay_ms: 100,
            };
        }
        Some(definition)
    }
}

#[async_trait]
impl lash_core::ToolProvider for ProbeTool {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        self.definition()
            .map(|definition| definition.manifest())
            .into_iter()
            .collect()
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        let definition = self.definition()?;
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
    backend: Arc<lash_sqlite_store::SqliteBackend>,
    provider_calls: Arc<AtomicUsize>,
    executions: Arc<AtomicUsize>,
}

impl Backend {
    async fn open() -> Self {
        let directory = tempfile::tempdir().expect("temporary durable backend");
        let backend = Arc::new(
            lash_sqlite_store::SqliteBackend::open(directory.path())
                .await
                .expect("file-backed SQLite backend"),
        );
        Self {
            directory,
            backend,
            provider_calls: Arc::default(),
            executions: Arc::default(),
        }
    }

    /// A core of the build whose `tools.probe` resolves to `tool:{tool_id}`.
    fn core(&self, tool_id: &'static str) -> LashCore {
        self.core_for(Probe::Id(tool_id))
    }

    /// A core of the build that registers `probe` for `tools.probe`.
    fn core_for(&self, probe: Probe) -> LashCore {
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
        explicit_ephemeral_facets(rlm_core_builder_over(self.backend.clone().into()))
            .provider(provider)
            .model(mock_model_spec())
            .tools(Arc::new(ProbeTool {
                probe,
                executions: Arc::clone(&self.executions),
            }))
            .build(crate::testing::runtime_lease_owner())
            .expect("file-backed SQLite RLM backend")
    }

    fn journal(&self) -> rusqlite::Connection {
        rusqlite::Connection::open(
            self.directory
                .path()
                .join(lash_sqlite_store::SqliteDatabase::EffectReplay.file_name()),
        )
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
            .send(TurnInput::text("probe"))
            .id(TURN)
            .output()
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
        self.abort_at(session_id, EffectJournalFaultPoint::Finalize, attempt_key)
            .await;
    }

    /// Runs `session_id`'s turn with a journal fault at `point` on `key`.
    async fn abort_at(&self, session_id: &str, point: EffectJournalFaultPoint, key: &str) {
        let faults = self.backend.effect_host().effect_journal_faults();
        faults.fail_next(point, key);
        let core = self.core("probe");
        let session = core
            .session(session_id)
            .open()
            .await
            .expect("open the session");
        let error = session
            .send(TurnInput::text("call the probe"))
            .id(TURN)
            .output()
            .await
            .expect_err("the journal fault aborts the turn");
        assert!(faults.fired(), "the armed finalize fault fired: {error:?}");
    }

    async fn park_of(&self, session_id: &str) -> Option<lash_core::store::TurnPark> {
        let store = self
            .backend
            .session_store_factory()
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

/// Reobserves `TURN` on `core` and returns its durable park.
async fn redrive(core: &LashCore, session_id: &str) -> crate::ParkedTurn {
    let session = core
        .session(session_id)
        .open()
        .await
        .expect("open the session");
    let outcome = session
        .root(TURN)
        .outcome()
        .await
        .expect("the root answers its durable park");
    assert!(outcome.output.is_none(), "a park has no terminal report");
    let crate::TurnStatus::Parked(parked) = outcome.status else {
        panic!("the root must park: {outcome:?}");
    };
    parked
}

async fn assert_parked(
    backend: &Backend,
    core: &LashCore,
    session_id: &str,
    parked: &crate::ParkedTurn,
) {
    let park = backend
        .park_of(session_id)
        .await
        .expect("the refused turn is parked");
    assert_eq!(park.turn_id.as_str(), TURN);
    assert_eq!(
        park.reason, parked.reason,
        "the handle names the stored park"
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
    // The park blocks the session's admission, so its input stays pending
    // and no other root drives it (FIG-3600).
    assert!(
        matches!(
            &pending[0].status,
            lash_core::PendingTurnInputReadStatus::Pending
        ),
        "the parked turn's input stays pending: {:?}",
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
    assert_eq!(status.oldest_parked_since_ms, Some(park.since_ms));
    assert!(!status.drained());
}

/// FIG-3659 parked-work metrics end to end: a durable park write counts once
/// on `lash.parked_work.parks`, and `drain_status` reports the live count and
/// oldest age. The test-metrics recorder is thread-local, so this law runs on
/// a single-threaded runtime where every spawned task shares its slot.
#[tokio::test]
async fn a_parked_turn_records_the_parked_work_metrics() -> Result<()> {
    #[cfg(feature = "otel-trace")]
    let metrics = lash_core::operational_metrics::TestMetrics::install();
    const SESSION: &str = "metric-park";
    let backend = Backend::open().await;
    let attempt_key = backend.first_attempt_key("metric-prob", SESSION).await;
    backend.abort_after_dispatch(SESSION, &attempt_key).await;

    let drifted = backend.core_for(Probe::Removed);
    let parked = redrive(&drifted, SESSION).await;
    assert!(matches!(
        parked.reason,
        lash_core::store::ParkReason::BindingDrift { .. }
    ));
    #[cfg(feature = "otel-trace")]
    assert_eq!(
        metrics.counter_value("lash.parked_work.parks"),
        1,
        "the durable park write counts once"
    );

    let status = drifted
        .drain_status(false)
        .await
        .expect("read drain status");
    assert_eq!(status.parked_turns, 1);
    #[cfg(feature = "otel-trace")]
    assert!(
        metrics.counter_value("lash.parked_work.count") >= 1,
        "drain status reports the live parked count"
    );
    #[cfg(feature = "otel-trace")]
    assert!(
        metrics.counter_value("lash.parked_work.oldest_age") >= 1,
        "drain status reports the oldest park's age"
    );
    Ok(())
}

/// The tool the cell called was dispatched but its result never recorded,
/// and the redrive build moved or removed it, or changed how it dispatches
/// (FIG-3587): the call would reach the drifted tool live, so every redrive
/// parks with the binding-drift refusal naming it and dispatches nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cell_whose_tool_drifted_before_its_result_parks_on_every_redrive() -> Result<()> {
    for (session_id, probe_id, drift, word) in [
        ("drift-mov1", "drift-prb1", Probe::Id("probe_v2"), "missing"),
        ("drift-ret1", "drift-prb4", Probe::Retried, "changed"),
        ("drift-rem1", "drift-prb2", Probe::Removed, "missing"),
    ] {
        let backend = Backend::open().await;
        let attempt_key = backend.first_attempt_key(probe_id, session_id).await;
        backend.abort_after_dispatch(session_id, &attempt_key).await;
        let dispatched = backend.executions.load(Ordering::SeqCst);
        let asked = backend.provider_calls.load(Ordering::SeqCst);

        let drifted = backend.core_for(drift);
        for _ in 0..2 {
            let parked = redrive(&drifted, session_id).await;
            assert!(
                matches!(
                    parked.reason,
                    lash_core::store::ParkReason::BindingDrift { .. }
                ),
                "{drift:?}"
            );
            assert_parked(&backend, &drifted, session_id, &parked).await;
            let park = backend.park_of(session_id).await.expect("parked");
            let message = park.reason.message();
            assert!(
                message.contains("`tools.probe`")
                    && message.contains("tool:probe")
                    && message.contains(word),
                "the park names the binding and how it drifted: {message}"
            );
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
    }
    Ok(())
}

/// The tool's result was recorded before the crash, and the redrive build
/// removed or redescribed it (FIG-3587): the cell links against its recorded
/// binding, replays the result, and the turn completes with nothing
/// dispatched.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cell_whose_tool_drifted_after_its_result_completes_its_turn() -> Result<()> {
    for (session_id, probe_id, drift) in [
        ("done-mov01", "done-prb00", Probe::Id("probe_v2")),
        ("done-ret01", "done-prb03", Probe::Retried),
        ("done-rem01", "done-prb01", Probe::Removed),
        ("done-chg01", "done-prb02", Probe::Described("Reworded.")),
    ] {
        let backend = Backend::open().await;
        let attempt_key = backend.first_attempt_key(probe_id, session_id).await;
        let seal_key = attempt_key.replace(":lk2:0000000000:attempt:1", ":lk2:~seal");
        backend
            .abort_at(session_id, EffectJournalFaultPoint::Claim, &seal_key)
            .await;
        let dispatched = backend.executions.load(Ordering::SeqCst);
        let asked = backend.provider_calls.load(Ordering::SeqCst);

        let drifted = backend.core_for(drift);
        let session = drifted
            .session(session_id)
            .open()
            .await
            .expect("open the session");
        let output = session
            .turn(TurnInput::text("call the probe"))
            .turn_id(TURN)
            .run()
            .await
            .unwrap_or_else(|error| panic!("{drift:?}: the redrive completes: {error:?}"));
        assert!(
            output.is_success(),
            "{drift:?}: the redrive completes: {:?}",
            output.result.errors
        );
        assert_eq!(
            backend.executions.load(Ordering::SeqCst),
            dispatched,
            "{drift:?}: the recorded result is served"
        );
        assert_eq!(
            backend.provider_calls.load(Ordering::SeqCst),
            asked,
            "{drift:?}: the model call replays from the journaled prompt"
        );
        assert!(backend.park_of(session_id).await.is_none());
    }
    Ok(())
}

/// A reworded descriptor never parks (FIG-3587): the prompt is served from
/// the journal and the binding still dispatches the same way, so a call whose
/// result was never recorded runs the tool live and the turn completes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_redescribed_tool_never_parks() -> Result<()> {
    const SESSION: &str = "desc-real1";
    let backend = Backend::open().await;
    let attempt_key = backend.first_attempt_key("desc-prob1", SESSION).await;
    backend.abort_after_dispatch(SESSION, &attempt_key).await;
    let dispatched = backend.executions.load(Ordering::SeqCst);

    let redescribed = backend.core_for(Probe::Described("Reworded at length."));
    let session = redescribed.session(SESSION).open().await?;
    let output = session
        .turn(TurnInput::text("call the probe"))
        .turn_id(TURN)
        .run()
        .await?;
    assert!(output.is_success(), "{:?}", output.result.errors);
    assert_eq!(
        backend.executions.load(Ordering::SeqCst),
        dispatched + 1,
        "the unrecorded call runs once, live"
    );
    assert!(backend.park_of(SESSION).await.is_none());
    Ok(())
}

/// FIG-3571 end to end: a turn admitted under another executable generation
/// than this build runs — one a previous build recorded, or none, as an
/// admission a pre-cutover build journaled records — is refused at its
/// admission on every redrive, typed, before any model, tool or provider
/// effect, and parks carrying both generations so drain status can count it
/// per generation.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_turn_admitted_under_another_generation_parks_before_any_effect() -> Result<()> {
    let current = lash_lashlang_runtime::lashlang_cell_generation();
    let retired = lash_core::ExecutableGeneration::new("blake3:retired");
    for (session_id, probe_id, recorded) in [
        ("gen-retired", "gen-probe01", Some(retired)),
        ("gen-unstamp", "gen-probe02", None),
    ] {
        let backend = Backend::open().await;
        let attempt_key = backend.first_attempt_key(probe_id, session_id).await;
        backend.abort_after_dispatch(session_id, &attempt_key).await;
        let dispatched = backend.executions.load(Ordering::SeqCst);
        let asked = backend.provider_calls.load(Ordering::SeqCst);

        // The admission's stamp rides the drive root's recorded input claim,
        // keyed `drive-claim:{root}` in the root's scope: the session is
        // named by the claim it recorded, not by its replay key.
        let restamped = backend
            .journal()
            .execute(
                "UPDATE runtime_effect_replay
                    SET outcome_json = replace(outcome_json, ?2, ?3)
                  WHERE replay_key LIKE 'drive-claim:%'
                    AND outcome_json LIKE ?1 AND outcome_json LIKE ?4",
                [
                    format!("%\"session_id\":\"{session_id}\"%"),
                    format!("\"generation\":\"{current}\""),
                    format!(
                        "\"generation\":{}",
                        serde_json::to_string(&recorded).expect("encode the recorded generation")
                    ),
                    format!("%\"generation\":\"{current}\"%"),
                ],
            )
            .expect("restamp the admission's generation");
        assert_eq!(restamped, 1, "the turn journaled one stamped admission");

        let core = backend.core("probe");
        for _ in 0..2 {
            let parked = redrive(&core, session_id).await;
            assert!(matches!(
                parked.reason,
                lash_core::store::ParkReason::RetiredGeneration { .. }
            ));
            assert_parked(&backend, &core, session_id, &parked).await;
            let park = backend.park_of(session_id).await.expect("parked");
            assert_eq!(
                park.reason,
                lash_core::store::ParkReason::retired_generation(
                    lash_core::ExecutableGenerationRefusal {
                        found: recorded.clone(),
                        current: Some(current.clone()),
                    }
                ),
                "the park carries both generations"
            );
            assert_eq!(
                backend.executions.load(Ordering::SeqCst),
                dispatched,
                "no tool ran"
            );
            assert_eq!(
                backend.provider_calls.load(Ordering::SeqCst),
                asked,
                "no model was asked"
            );
        }
    }
    Ok(())
}

const DRAIN: &str = "generation-drain";

impl Backend {
    /// Runs one queued input on `session_id` through the drain `DRAIN`.
    async fn drain(
        &self,
        session_id: &str,
    ) -> Result<crate::turn::QueuedTurnDrain<crate::TurnOutput>> {
        let core = self.core("probe");
        let session = core.session(session_id).open().await?;
        if session.durable().pending_queued_run().await?.is_none() {
            session
                .durable()
                .enqueue(TurnInput::text("call the probe"))
                .id(format!("{session_id}-input"))
                .send()
                .await?;
        }
        session.queued_turn().drain_id(DRAIN).run().await
    }

    /// Rewrites the generation the pending queued run of `session_id` was
    /// admitted under, as a run a previous build admitted records it.
    fn restamp_queued_run(
        &self,
        session_id: &str,
        current: &lash_core::ExecutableGeneration,
        recorded: Option<&lash_core::ExecutableGeneration>,
    ) {
        let core = rusqlite::Connection::open(
            self.directory
                .path()
                .join(lash_sqlite_store::SqliteDatabase::DurableCore.file_name()),
        )
        .expect("open the durable core");
        let restamped = core
            .execute(
                "UPDATE queued_runs SET admission_json = replace(admission_json, ?2, ?3)
                  WHERE session_id = ?1 AND status = 'pending'",
                [
                    session_id.to_string(),
                    format!("\"generation\":\"{current}\""),
                    format!(
                        "\"generation\":{}",
                        serde_json::to_string(&recorded).expect("encode the recorded generation")
                    ),
                ],
            )
            .expect("restamp the queued run's generation");
        assert_eq!(restamped, 1, "the drain left one pending run");
    }
}

/// FIG-3571 for a queue drain: a pending queued run admitted under another
/// executable generation resumes only to be refused, typed, before the run
/// drives anything, and its turn parks carrying both generations.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_queued_run_admitted_under_another_generation_parks_before_any_effect() -> Result<()> {
    let current = lash_lashlang_runtime::lashlang_cell_generation();
    let retired = lash_core::ExecutableGeneration::new("blake3:retired");
    for (session_id, probe_id, recorded) in [
        ("qgen-retired", "qgen-probe01", Some(retired)),
        ("qgen-unstamp", "qgen-probe02", None),
    ] {
        let backend = Backend::open().await;
        backend
            .drain(probe_id)
            .await
            .expect("the probe drain completes");
        let attempt_key = backend
            .keys_of(probe_id)
            .into_iter()
            .find(|key| key.ends_with(":lk2:0000000000:attempt:1"))
            .expect("the probe's cell journaled its tool attempt")
            .replace(probe_id, session_id);
        let faults = backend.backend.effect_host().effect_journal_faults();
        faults.fail_next(EffectJournalFaultPoint::Finalize, &attempt_key);
        let aborted = backend.drain(session_id).await;
        assert!(
            faults.fired(),
            "the armed finalize fault fired: {aborted:?}"
        );
        let dispatched = backend.executions.load(Ordering::SeqCst);
        let asked = backend.provider_calls.load(Ordering::SeqCst);
        backend.restamp_queued_run(session_id, &current, recorded.as_ref());

        for _ in 0..2 {
            let refused = backend.drain(session_id).await;
            let EmbedError::Runtime(error) = refused.expect_err("the resumed run is refused")
            else {
                panic!("the refusal is the typed runtime error");
            };
            assert_eq!(error.code, lash_core::RuntimeErrorCode::RetiredGeneration);
            let park = backend
                .park_of(session_id)
                .await
                .expect("the run's turn parked");
            assert_eq!(
                park.reason,
                lash_core::store::ParkReason::retired_generation(
                    lash_core::ExecutableGenerationRefusal {
                        found: recorded.clone(),
                        current: Some(current.clone()),
                    }
                ),
                "the park carries both generations"
            );
            assert_eq!(
                backend.executions.load(Ordering::SeqCst),
                dispatched,
                "no tool ran"
            );
            assert_eq!(
                backend.provider_calls.load(Ordering::SeqCst),
                asked,
                "no model was asked"
            );
        }
    }
    Ok(())
}

const SPAWN_CELL: &str = r#"<typescript>
const child = await agents.spawn({
  capability: "default",
  task: "Finish `{ len: chunk.length }` using the seeded `chunk` variable.",
  seed: { chunk: ["a", "b"] },
  output: { len: "int" }
});
finish(child);
</typescript>"#;
const CHILD_CELL: &str = "<typescript>\nfinish({ len: chunk.length });\n</typescript>";

impl Backend {
    /// A core whose `agents.spawn` offers `capabilities`, answering the parent
    /// with [`SPAWN_CELL`] and the spawned child with [`CHILD_CELL`].
    fn spawn_core(&self, capabilities: &[&'static str]) -> LashCore {
        let calls = Arc::clone(&self.provider_calls);
        let provider = crate::testing::TestProvider::builder()
            .kind("replay-park")
            .complete(move |_| {
                let calls = Arc::clone(&calls);
                async move {
                    let call = calls.fetch_add(1, Ordering::SeqCst);
                    Ok(text_response(if call.is_multiple_of(2) {
                        SPAWN_CELL
                    } else {
                        CHILD_CELL
                    }))
                }
            })
            .build()
            .into_handle();
        let registry = capabilities.iter().fold(
            lash_subagents::CapabilityRegistry::new(),
            |registry, name| {
                registry.with(Arc::new(lash_subagents::StaticCapability::new(
                    *name,
                    SessionSpec::inherit(),
                )))
            },
        );
        explicit_ephemeral_facets(rlm_core_builder_over(self.backend.clone().into()))
            .provider(provider)
            .model(mock_model_spec())
            .plugin(Arc::new(lash_subagents::SubagentsPluginFactory::new(
                Arc::new(registry),
                lash_core::lifetime::starter,
            )))
            .build(crate::testing::runtime_lease_owner())
            .expect("file-backed SQLite RLM backend with subagents")
    }
}

/// A completed cell whose `agents.spawn` binding drifted — the redeploy
/// offers another capability, which moves the orchestrating tool's schema —
/// replays down the orchestrating path against its recorded nested effects
/// (FIG-3587): the turn completes with nothing dispatched and no model call
/// re-issued.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_completed_spawn_whose_capabilities_changed_replays() -> Result<()> {
    const SESSION: &str = "spawn-real";
    const PROBE: &str = "spawn-prob";
    let backend = Backend::open().await;
    let probe = backend.spawn_core(&["default"]);
    probe
        .session(PROBE)
        .open()
        .await?
        .turn(TurnInput::text("spawn"))
        .turn_id(TURN)
        .run()
        .await?;
    let seal_key = backend
        .keys_of(PROBE)
        .into_iter()
        .find(|key| key.starts_with(&format!("{PROBE}:")) && key.ends_with(":lk2:~seal"))
        .expect("the probe's cell sealed its run")
        .replace(PROBE, SESSION);

    let faults = backend.backend.effect_host().effect_journal_faults();
    faults.fail_next(EffectJournalFaultPoint::Claim, &seal_key);
    let crashing = backend.spawn_core(&["default"]);
    let error = crashing
        .session(SESSION)
        .open()
        .await?
        .turn(TurnInput::text("spawn"))
        .turn_id(TURN)
        .run()
        .await
        .expect_err("the seal fault aborts the turn after the spawn completed");
    assert!(faults.fired(), "the armed seal fault fired: {error:?}");
    let asked = backend.provider_calls.load(Ordering::SeqCst);

    let redeployed = backend.spawn_core(&["default", "reviewer"]);
    let output = redeployed
        .session(SESSION)
        .open()
        .await?
        .turn(TurnInput::text("spawn"))
        .turn_id(TURN)
        .run()
        .await?;
    assert!(output.is_success(), "{:?}", output.result.errors);
    assert_eq!(
        backend.provider_calls.load(Ordering::SeqCst),
        asked,
        "the parent's and the child's model calls replay from the journal"
    );
    assert!(backend.park_of(SESSION).await.is_none());
    Ok(())
}

/// The model's native call on `probe`, then its answer once the result is in.
fn native_probe_response(request: &LlmRequest) -> LlmResponse {
    let answered = request.messages.iter().any(|message| {
        message
            .blocks
            .iter()
            .any(|block| matches!(block, LlmContentBlock::ToolResult { .. }))
    });
    if answered {
        return text_response("probed");
    }
    LlmResponse {
        parts: vec![LlmOutputPart::ToolCall {
            call_id: "native-probe-1".to_string(),
            tool_name: "probe".to_string(),
            input_json: "{}".to_string(),
            replay: None,
        }],
        response_metadata: Default::default(),
        ..LlmResponse::default()
    }
}

impl Backend {
    /// A standard-protocol core of the build that registers `probe`: the
    /// model calls it natively, so the call is a turn tool call, not a cell's.
    fn native_core_for(&self, probe: Probe) -> LashCore {
        let calls = Arc::clone(&self.provider_calls);
        let provider = crate::testing::TestProvider::builder()
            .kind("replay-park")
            .complete(move |request| {
                calls.fetch_add(1, Ordering::SeqCst);
                let response = native_probe_response(&request);
                async move { Ok(response) }
            })
            .build()
            .into_handle();
        explicit_ephemeral_facets(LashCore::standard_builder(
            self.backend.clone().into(),
            crate::TurnBudget::Unbounded,
        ))
        .provider(provider)
        .model(mock_model_spec())
        .tools(Arc::new(ProbeTool {
            probe,
            executions: Arc::clone(&self.executions),
        }))
        .build(crate::testing::runtime_lease_owner())
        .expect("file-backed SQLite standard backend")
    }

    /// The journal key in `session_id`'s turn matching `pick`, found by
    /// running the same turn to completion in a same-length probe session
    /// first (see [`Self::first_attempt_key`]).
    async fn native_key(
        &self,
        probe: &str,
        session_id: &str,
        pick: impl Fn(&str) -> bool,
    ) -> String {
        assert_eq!(probe.len(), session_id.len(), "same-length session ids");
        let core = self.native_core_for(Probe::Id("probe"));
        let output = core
            .session(probe)
            .open()
            .await
            .expect("open the probe")
            .send(TurnInput::text("call the probe"))
            .id(TURN)
            .output()
            .await
            .expect("the probe turn completes");
        assert!(output.is_success(), "{:?}", output.result.errors);
        let keys = self.keys_of(probe);
        keys.iter()
            .find(|key| pick(key))
            .unwrap_or_else(|| panic!("no probe key matched: {keys:#?}"))
            .replace(probe, session_id)
    }

    /// Runs `session_id`'s native turn with a journal fault at `point` on
    /// `key`, under the build that registers `probe` as first deployed.
    async fn native_abort_at(&self, session_id: &str, point: EffectJournalFaultPoint, key: &str) {
        let faults = self.backend.effect_host().effect_journal_faults();
        faults.fail_next(point, key);
        let error = self
            .native_core_for(Probe::Id("probe"))
            .session(session_id)
            .open()
            .await
            .expect("open the session")
            .send(TurnInput::text("call the probe"))
            .id(TURN)
            .output()
            .await
            .expect_err("the journal fault aborts the turn");
        assert!(faults.fired(), "the armed fault fired: {error:?}");
    }

    async fn native_parked(&self, probe: Probe, session_id: &str) -> crate::ParkedTurn {
        let session = self
            .native_core_for(probe)
            .session(session_id)
            .open()
            .await
            .expect("open the session");
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(60),
            session.root(TURN).outcome(),
        )
        .await
        .expect("the native redrive answers")
        .expect("the native root answers its park");
        assert!(outcome.output.is_none());
        let crate::TurnStatus::Parked(parked) = outcome.status else {
            panic!("the native root must park: {outcome:?}");
        };
        parked
    }

    /// Redrives `session_id`'s native turn under the build registering
    /// `probe` and returns its outcome.
    async fn native_redrive(
        &self,
        probe: Probe,
        session_id: &str,
    ) -> std::result::Result<crate::TurnOutput, EmbedError> {
        self.native_core_for(probe)
            .session(session_id)
            .open()
            .await
            .expect("open the session")
            .turn(TurnInput::text("call the probe"))
            .turn_id(TURN)
            .run()
            .await
    }
}

/// The turn's first model call: a fault on its finalize aborts the turn after
/// its execution-environment sync recorded the tool surface and before the
/// model's call on `probe` exists, so the redrive needs that call live.
fn is_first_model_call(key: &str) -> bool {
    key.contains(":0:llm_call:")
}

/// A native tool call whose tool was only reworded since the turn recorded
/// its tool surface never parks (FIG-3672 P7b): drift is judged per tool on
/// what decides how a call links and dispatches, and the description only
/// reaches the prompt, which the redrive serves from the journal. The call
/// whose result the journal does not hold runs live, and the turn completes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_native_call_on_a_reworded_tool_never_parks() -> Result<()> {
    const SESSION: &str = "native-dsc1";
    let backend = Backend::open().await;
    let model_key = backend
        .native_key("native-prb1", SESSION, is_first_model_call)
        .await;
    backend
        .native_abort_at(SESSION, EffectJournalFaultPoint::Finalize, &model_key)
        .await;
    let dispatched = backend.executions.load(Ordering::SeqCst);

    let output = backend
        .native_redrive(Probe::Described("Reworded at length."), SESSION)
        .await
        .expect("the reworded redrive completes");
    assert!(output.is_success(), "{:?}", output.result.errors);
    assert_eq!(
        backend.executions.load(Ordering::SeqCst),
        dispatched + 1,
        "the unrecorded call runs once, live"
    );
    assert!(backend.park_of(SESSION).await.is_none());
    Ok(())
}

/// A native tool call on a tool whose dispatch changed (its retry policy)
/// or which is gone, whose result the journal does not hold, would reach the
/// drifted tool live: every redrive parks with the binding-drift refusal
/// naming the call and dispatches nothing (FIG-3672 P7b).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_native_call_on_a_drifted_tool_needed_live_parks() -> Result<()> {
    for (session_id, probe_id, drift, word) in [
        ("native-ret1", "native-prb2", Probe::Retried, "changed in"),
        ("native-rem1", "native-prb3", Probe::Removed, "missing from"),
    ] {
        let backend = Backend::open().await;
        let model_key = backend
            .native_key(probe_id, session_id, is_first_model_call)
            .await;
        backend
            .native_abort_at(session_id, EffectJournalFaultPoint::Finalize, &model_key)
            .await;
        let dispatched = backend.executions.load(Ordering::SeqCst);

        for _ in 0..2 {
            let parked = backend.native_parked(drift, session_id).await;
            assert!(
                matches!(
                    parked.reason,
                    lash_core::store::ParkReason::BindingDrift { .. }
                ),
                "{drift:?}"
            );
            let park = backend.park_of(session_id).await.expect("parked");
            assert!(
                matches!(
                    park.reason,
                    lash_core::store::ParkReason::BindingDrift { .. }
                ),
                "{drift:?}: {park:?}"
            );
            let message = park.reason.message();
            assert!(
                message.contains("native-probe-1")
                    && message.contains("tool:probe")
                    && message.contains(word),
                "the park names the call, its tool and how it drifted: {message}"
            );
            assert_eq!(
                backend.executions.load(Ordering::SeqCst),
                dispatched,
                "{drift:?}: a parked redrive dispatches nothing"
            );
        }
    }
    Ok(())
}

/// A native tool call whose result the journal holds is served whatever
/// happened to its tool since (FIG-3672 P7b): reworded, retried, or removed,
/// the turn completes with nothing dispatched.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_native_call_whose_result_is_recorded_is_served_whatever_its_tool_became() -> Result<()> {
    for (session_id, probe_id, drift) in [
        ("native-don1", "native-prb4", Probe::Described("Reworded.")),
        ("native-don2", "native-prb5", Probe::Retried),
        ("native-don3", "native-prb6", Probe::Removed),
    ] {
        let backend = Backend::open().await;
        // The second model call, after the tool's result is in.
        let answer_key = backend
            .native_key(probe_id, session_id, |key| {
                key.contains(":llm_call:") && !key.contains(":0:llm_call:")
            })
            .await;
        backend
            .native_abort_at(session_id, EffectJournalFaultPoint::Claim, &answer_key)
            .await;
        let dispatched = backend.executions.load(Ordering::SeqCst);

        let output = backend
            .native_redrive(drift, session_id)
            .await
            .unwrap_or_else(|error| panic!("{drift:?}: the redrive completes: {error:?}"));
        assert!(output.is_success(), "{drift:?}: {:?}", output.result.errors);
        assert_eq!(
            backend.executions.load(Ordering::SeqCst),
            dispatched,
            "{drift:?}: the recorded result is served"
        );
        assert!(backend.park_of(session_id).await.is_none(), "{drift:?}");
    }
    Ok(())
}

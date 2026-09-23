//! FIG-3464: the durable per-effect summary, rebuilt from paged
//! `Processes::events` and checked against the effect replay rows, bounded on
//! the write side, and recovered by redrive after a crash between the replay
//! row and the summary append.

// FIG-2971: this file is test/tooling/host code; ambient fs/env/process
// access is sanctioned here (the workspace clippy ban targets production
// library code).
#![allow(clippy::disallowed_methods)]

use super::*;
use lash_core::ProcessEffectOutcomeClass;

fn summary_catalog() -> lashlang::LashlangHostCatalog {
    let mut catalog = programs::process_control_catalog();
    catalog
        .add_module_operation_contract(
            [lashlang::LANGUAGE_RUNTIME_MODULE_PATH],
            lashlang::LANGUAGE_RUNTIME_RESOURCE_TYPE,
            lashlang::LANGUAGE_RUNTIME_NOW_OPERATION,
            "typescript.runtime.now",
            &lashlang::OperationContract::new(
                serde_json::json!({}),
                serde_json::json!({ "type": "number" }),
            ),
        )
        .expect("register TypeScript runtime operation");
    lashlang::add_trigger_resource_operations(&mut catalog).expect("register trigger operations");
    catalog
}

fn now_call() -> lashlang::Expr {
    b::module_call(&[lashlang::LANGUAGE_RUNTIME_MODULE_PATH], "now", Vec::new())
}

fn list_triggers() -> lashlang::Expr {
    b::module_call(&["triggers"], "list", vec![b::record(Vec::new())])
}

fn disable_missing_trigger() -> lashlang::Expr {
    b::module_call(
        &["triggers"],
        "disable",
        vec![b::record(vec![
            ("subscription_key", b::string("missing-subscription")),
            ("expected_revision", b::num(1.0)),
        ])],
    )
}

/// The durable stores one host runs over, reopened fresh for each host so a
/// "crash" drops every in-memory handle.
#[derive(Clone)]
enum SummaryBackend {
    Sqlite(DurableAdmissionPaths),
    /// A database URL, and the directory attachments live in.
    Postgres(String, std::path::PathBuf),
}

/// The durable stores one host is built over.
struct SummaryStores {
    registry: Arc<dyn lash_core::ProcessRegistry>,
    env_store: Arc<dyn lash_core::ProcessExecutionEnvStore>,
    trigger_store: Arc<dyn lash_core::TriggerStore>,
    effect_host: Arc<dyn lash_core::EffectHost>,
    store_factory: Arc<dyn lash_core::SessionStoreFactory>,
    attachments: std::path::PathBuf,
}

struct SummaryHost {
    core: LashCore,
    sink: CollectingProcessEventSink,
    faults: Option<Arc<lash_core::EffectSummaryAppendFaults>>,
}

impl SummaryBackend {
    async fn artifact_store(&self) -> Arc<dyn lash_lashlang_runtime::LashlangArtifactStore> {
        match self {
            Self::Sqlite(paths) => Arc::new(
                lash_sqlite_store::Store::open(&paths.artifacts)
                    .await
                    .expect("open SQLite artifact store"),
            ),
            Self::Postgres(url, _) => Arc::new(
                lash_postgres_store::PostgresStorage::connect(url)
                    .await
                    .expect("connect PostgreSQL storage")
                    .lashlang_artifact_store(),
            ),
        }
    }

    /// Opens one host. With `fault`, the host's registry refuses the first
    /// `count` appends of that effect-summary kind.
    async fn host(&self, owner: &str, fault: Option<(&'static str, usize)>) -> SummaryHost {
        let sink = CollectingProcessEventSink::default();
        let provider = mock_provider();
        let provider_id = provider.kind().to_string();
        let artifact = self.artifact_store().await;
        let SummaryStores {
            registry,
            env_store,
            trigger_store,
            effect_host,
            store_factory,
            attachments,
        } = match self {
            Self::Sqlite(paths) => SummaryStores {
                registry: Arc::new(
                    lash_sqlite_store::SqliteProcessRegistry::open(
                        &paths.processes,
                        &paths.sessions,
                    )
                    .await
                    .expect("open SQLite process registry"),
                ),
                env_store: Arc::new(
                    lash_sqlite_store::Store::open(&paths.artifacts)
                        .await
                        .expect("open SQLite process-environment store"),
                ),
                trigger_store: Arc::new(
                    lash_sqlite_store::SqliteTriggerStore::open(&paths.triggers)
                        .await
                        .expect("open SQLite trigger store"),
                ),
                effect_host: Arc::new(
                    lash_sqlite_store::SqliteEffectHost::open(&paths.effects)
                        .await
                        .expect("open SQLite effect journal"),
                ),
                store_factory: Arc::new(
                    lash_sqlite_store::SqliteSessionStoreFactory::new_with_process_registry(
                        &paths.sessions,
                        &paths.processes,
                    ),
                ),
                attachments: paths.attachments.clone(),
            },
            Self::Postgres(url, attachments) => {
                let storage = lash_postgres_store::PostgresStorage::connect(url)
                    .await
                    .expect("connect PostgreSQL storage");
                SummaryStores {
                    registry: Arc::new(storage.process_registry()),
                    env_store: Arc::new(storage.process_env_store()),
                    trigger_store: Arc::new(storage.trigger_store()),
                    effect_host: Arc::new(storage.effect_host()),
                    store_factory: Arc::new(
                        storage.session_store_factory_with_shared_process_registry(),
                    ),
                    attachments: attachments.join(owner),
                }
            }
        };
        let (registry, faults) = match fault {
            Some((event_type, count)) => {
                let faults = Arc::new(lash_core::EffectSummaryAppendFaults::new(
                    registry, event_type, count,
                ));
                (
                    Arc::clone(&faults) as Arc<dyn lash_core::ProcessRegistry>,
                    Some(faults),
                )
            }
            None => (registry, None),
        };
        let core = LashCore::rlm_builder(
            crate::TurnBudget::Unbounded,
            lash_protocol_rlm::RlmProtocolPluginFactory::new(
                lash_protocol_rlm::RlmProtocolPluginConfig::builder()
                    .channel(lash_protocol_rlm::RlmChannel::Cell)
                    .instruction_limit(lash_protocol_rlm::InstructionBound::instructions(1_000_000))
                    .wall_clock(lash_protocol_rlm::WallClockBound::secs(30))
                    .memory_limit(lash_protocol_rlm::MemoryBound::mebibytes(64))
                    .build(),
                artifact,
            ),
        )
        .session_spec(
            crate::SessionSpec::new()
                .provider_id(provider_id)
                .turn_budget(crate::TurnBudget::Unbounded),
        )
        .provider(provider)
        .model(mock_model_spec())
        .store_factory(store_factory)
        .attachment_store(Arc::new(crate::persistence::FileAttachmentStore::new(
            attachments,
        )))
        .commit_budget(crate::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(crate::QueuedWorkBatchingConfig::new(1))
        .process_env_store(env_store)
        .process_registry(registry)
        .trigger_store(trigger_store)
        .plugin(Arc::new(
            lash_plugin_process_controls::SessionProcessAdminPluginFactory::new(),
        ))
        .effect_host(effect_host)
        .process_event_sink(Arc::new(sink.clone()))
        .without_queued_work()
        .build(lash_core::LeaseOwnerIdentity::opaque(
            owner,
            format!("{owner}:incarnation"),
        ))
        .expect("build effect-summary host");
        SummaryHost { core, sink, faults }
    }

    /// Completed replay rows of the process's own effects, by replay key.
    async fn replay_rows(&self, process_id: &ProcessId) -> BTreeMap<String, String> {
        let pattern = format!("%{process_id}%");
        match self {
            Self::Sqlite(paths) => {
                let connection = rusqlite::Connection::open(&paths.effects)
                    .expect("open SQLite effect replay database");
                let mut query = connection
                    .prepare(
                        "SELECT replay_key, outcome_json FROM runtime_effect_replay \
                         WHERE scope_id LIKE ?1 AND status = 'completed'",
                    )
                    .expect("prepare replay-row query");
                query
                    .query_map([pattern], |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                    })
                    .expect("query replay rows")
                    .collect::<std::result::Result<BTreeMap<_, _>, _>>()
                    .expect("read replay rows")
            }
            Self::Postgres(url, _) => {
                let storage = lash_postgres_store::PostgresStorage::connect(url)
                    .await
                    .expect("connect PostgreSQL storage");
                sqlx::query_as::<_, (String, String)>(
                    "SELECT replay_key, outcome_json FROM lash_runtime_effect_replay \
                     WHERE scope_id LIKE $1 AND status = 'completed'",
                )
                .bind(pattern)
                .fetch_all(storage.pool())
                .await
                .expect("read PostgreSQL replay rows")
                .into_iter()
                .collect()
            }
        }
    }
}

/// The per-effect table a host rebuilds from `Processes::events` alone, two
/// events per page, with every effect-summary event it folded.
async fn paged_summary(
    core: &LashCore,
    process_id: &ProcessId,
) -> (
    lash_core::ProcessEffectSummary,
    Vec<(String, serde_json::Value)>,
) {
    let mut table = lash_core::ProcessEffectSummary::default();
    let mut folded = Vec::new();
    let mut continuation = None;
    let mut pages = 0;
    loop {
        let outcome = core
            .processes()
            .events(
                process_id,
                std::num::NonZeroUsize::new(2).expect("non-zero page size"),
                lash_core::ProcessEventQueryMode::Full,
                continuation,
            )
            .await
            .expect("read an event page");
        let lash_core::ProcessEventReadOutcome::Retained(page) = outcome else {
            panic!("effect-summary process history must be retained");
        };
        pages += 1;
        let lash_core::ProcessEventPageEvents::Full(events) = page.events else {
            panic!("a full page request includes payloads");
        };
        assert!(events.len() <= 2);
        for event in events {
            table
                .fold_event(&event.event_type, &event.payload)
                .expect("fold a paged effect-summary event");
            if event.event_type.starts_with("process.effect_") {
                folded.push((event.event_type, event.payload));
            }
        }
        continuation = match page.more {
            lash_core::ProcessEventPageMore::Complete => break,
            lash_core::ProcessEventPageMore::More { continuation } => Some(continuation),
        };
    }
    assert!(pages >= 2, "the rebuild must cross a page boundary");
    (table, folded)
}

async fn start_process(
    host: &SummaryHost,
    backend: &SummaryBackend,
    process_id: &ProcessId,
    program: lashlang::Program,
) {
    let artifact = backend.artifact_store().await;
    let process =
        LinkedTestProcess::new_with_catalog(artifact.as_ref(), program, "main", summary_catalog())
            .await;
    let mut start_request = process.start_request(process_id);
    start_request.originator = lash_core::ProcessOriginator::host_scoped("effect-summary-test");
    host.core
        .processes()
        .start(
            start_request,
            runtime_operation_scope(&host.core, format!("start-{process_id}")),
        )
        .await
        .expect("start the effect-summary process");
}

/// Drives a process whose effect-summary appends of kind `fault` "crash" on
/// this host: the replay row is committed, the append is refused, the worker
/// reports a retryable fault and leaves the row claimable, and the host dies.
/// A fresh host over the same stores then recovers the process to its
/// terminal state.
async fn run_through_append_crash(
    backend: &SummaryBackend,
    process_id: &ProcessId,
    program: lashlang::Program,
    fault: &'static str,
    terminal: lash_core::ProcessStatus,
) -> SummaryHost {
    let crashed = backend
        .host("effect-summary-crashed", Some((fault, usize::MAX)))
        .await;
    start_process(&crashed, backend, process_id, program).await;
    let worker_fault = wait_for_worker_fault(&crashed.sink, process_id).await;
    assert!(
        matches!(
            worker_fault,
            lash_core::facade_support::ProcessWorkerFault::RecoveryRunFailed { ref error, .. }
                if error.contains("injected crash before the")
        ),
        "a failed effect-summary append is a retryable incorporation fault: {worker_fault:?}"
    );
    assert!(crashed.faults.as_ref().expect("fault decorator").injected() >= 1);
    let claimable = wait_for_process(&crashed.core, process_id, "claimable redrive", |process| {
        process.lifecycle == lash_core::ProcessStatus::Running && process.lease_holder.is_none()
    })
    .await;
    assert!(
        claimable.error.is_none(),
        "the program never observed the failed append: {claimable:?}"
    );
    drop(crashed);

    let recovered = backend.host("effect-summary-recovered", None).await;
    let worker = lash_core_worker::DurableProcessWorker::new(
        recovered
            .core
            .durable_process_worker_config()
            .expect("worker config"),
    )
    .expect("durable process worker");
    let drive = worker
        .drive_pending_processes()
        .await
        .expect("redrive the crashed process");
    assert_eq!(drive.admitted, vec![process_id.clone()]);
    let settled = wait_for_process(
        &recovered.core,
        process_id,
        "recovered terminal",
        |process| process.lifecycle.is_terminal(),
    )
    .await;
    assert_eq!(settled.lifecycle, terminal, "{settled:?}");
    recovered
}

/// Effect-summary events by replay key; a duplicate append would collide.
fn events_by_key(folded: &[(String, serde_json::Value)]) -> BTreeMap<String, serde_json::Value> {
    let mut by_key = BTreeMap::new();
    for (event_type, payload) in folded {
        let key = if event_type == lash_core::PROCESS_EFFECT_OUTCOME_EVENT_TYPE {
            payload["replay_key"]
                .as_str()
                .expect("an effect outcome names its replay key")
                .to_string()
        } else {
            event_type.clone()
        };
        assert!(
            by_key.insert(key.clone(), payload.clone()).is_none(),
            "`{key}` was appended twice"
        );
    }
    by_key
}

#[tokio::test]
async fn paged_process_effect_summary_matches_durable_replay_rows() -> Result<()> {
    let temp = tempfile::tempdir().expect("effect-summary tempdir");
    let backend = SummaryBackend::Sqlite(DurableAdmissionPaths::new(temp.path()));
    let host = backend.host("effect-summary-host", None).await;
    let process_id = ProcessId::from("effect-summary-facade");
    start_process(
        &host,
        &backend,
        &process_id,
        b::module(
            vec![
                b::process("child", Vec::new(), b::finish(b::string("child done"))),
                b::process(
                    "main",
                    Vec::new(),
                    b::block(vec![
                        b::assign("child", b::start("child", Vec::new())),
                        b::assign("clock", now_call()),
                        b::sleep_until(b::num(0.0)),
                        b::assign("listed", list_triggers()),
                        disable_missing_trigger(),
                        b::finish(b::var("clock")),
                    ]),
                ),
            ],
            Vec::new(),
        ),
    )
    .await;
    let terminal =
        wait_for_terminal(&host.core, &process_id, lash_core::ProcessStatus::Failed).await;

    let (table, folded) = paged_summary(&host.core, &process_id).await;
    let occurrences = table
        .nodes()
        .flat_map(|node| node.occurrences.iter())
        .collect::<Vec<_>>();
    assert_eq!(
        occurrences.len(),
        5,
        "tool, TypeScript, sleep and two trigger outcomes: {occurrences:?}; terminal={terminal:?}"
    );
    assert!(
        folded
            .iter()
            .all(|(event_type, _)| event_type == lash_core::PROCESS_EFFECT_OUTCOME_EVENT_TYPE),
        "no node exceeded the cap, so no omission record"
    );

    let replay_rows = backend.replay_rows(&process_id).await;
    let mut expected_rows = std::collections::BTreeSet::new();
    for occurrence in &occurrences {
        // The tool coordinator journals each atomic attempt beneath the
        // call's command key (FIG-3586); every other effect journals its
        // replay key directly.
        let row_key = if occurrence.operation == "tool:start_process" {
            format!("{}:attempt:1", occurrence.replay_key)
        } else {
            occurrence.replay_key.clone()
        };
        let encoded = replay_rows
            .get(&row_key)
            .unwrap_or_else(|| panic!("missing durable replay row for {row_key}"));
        expected_rows.insert(row_key);
        let outcome: lash_core::RuntimeEffectOutcome =
            serde_json::from_str(encoded).expect("decode effect replay outcome");
        match occurrence.operation.as_str() {
            "tool:start_process" => {
                assert_eq!(occurrence.outcome_class, ProcessEffectOutcomeClass::Success);
                assert!(occurrence.code.is_none());
                assert!(
                    matches!(outcome, lash_core::RuntimeEffectOutcome::ToolAttempt { launch, .. } if matches!(&*launch, lash_core::ToolAttemptLaunch::Done { record, .. } if matches!(&record.output.outcome, lash_core::ToolCallOutcome::Success(_))))
                );
            }
            "typescript.runtime" => {
                assert_eq!(occurrence.outcome_class, ProcessEffectOutcomeClass::Success);
                assert!(occurrence.code.is_none());
                assert!(matches!(
                    outcome,
                    lash_core::RuntimeEffectOutcome::LanguageRuntimeValue { .. }
                ));
            }
            "sleep" => {
                assert_eq!(occurrence.outcome_class, ProcessEffectOutcomeClass::Success);
                assert!(occurrence.code.is_none());
                assert!(matches!(outcome, lash_core::RuntimeEffectOutcome::Sleep));
            }
            "triggers.list" => {
                assert_eq!(occurrence.outcome_class, ProcessEffectOutcomeClass::Success);
                assert!(occurrence.code.is_none());
                assert!(
                    matches!(outcome, lash_core::RuntimeEffectOutcome::Trigger { result } if result.is_ok())
                );
            }
            "triggers.disable" => {
                assert_eq!(occurrence.outcome_class, ProcessEffectOutcomeClass::Failure);
                let lash_core::RuntimeEffectOutcome::Trigger { result } = outcome else {
                    panic!("a trigger row records a trigger outcome");
                };
                let error = result.expect_err("the disable row records the refusal");
                assert_eq!(
                    occurrence.code.as_ref(),
                    Some(&error.failure_code()),
                    "the summary code is the code of the recorded failure"
                );
                assert_eq!(
                    occurrence
                        .code
                        .as_ref()
                        .map(|code| code.namespaced())
                        .as_deref(),
                    Some("lash:trigger_conflict")
                );
            }
            operation => panic!("unexpected effect operation {operation}"),
        }
    }
    let program_effect_rows = replay_rows
        .keys()
        .filter(|key| is_program_effect_row(key))
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        program_effect_rows, expected_rows,
        "every completed replay row of the process has exactly one summary occurrence"
    );
    Ok(())
}

/// Whether a completed replay row records an effect the program's own nodes
/// performed: a resource leaf, a tool attempt, or a sleep. The rest in the
/// process scope are sub-effects of those (a tool's presentation step and the
/// Start it issued) or the host's own Start of this process.
fn is_program_effect_row(key: &str) -> bool {
    if key.starts_with("lashlang:") {
        // A tool attempt journals at `{command}:attempt:{a}` (FIG-3586); what
        // it issued in turn — its retry sleep, the Start it made — journals
        // beneath that key and is not a node's own effect.
        return !key.ends_with(":present")
            && key
                .rsplit_once(":attempt:")
                .is_none_or(|(_, attempt)| attempt.parse::<u32>().is_ok());
    }
    key.starts_with("process:process:") && key.contains(":sleep:")
}

fn capped_program() -> lashlang::Program {
    let iterations = (0..10).map(|index| b::num(f64::from(index))).collect();
    b::module(
        vec![b::process(
            "main",
            Vec::new(),
            b::block(vec![
                b::for_in(
                    "index",
                    b::list(iterations),
                    b::block(vec![
                        b::assign("clock", now_call()),
                        b::try_expr(
                            disable_missing_trigger(),
                            Some(b::catch("refused", b::null())),
                            None,
                        ),
                    ]),
                ),
                b::finish(b::string("done")),
            ]),
        )],
        Vec::new(),
    )
}

#[tokio::test]
async fn effect_summary_is_bounded_on_the_write_side_and_a_redrive_rewrites_it_identically()
-> Result<()> {
    let process_id = ProcessId::from("effect-summary-capped");

    let clean_dir = tempfile::tempdir().expect("clean tempdir");
    let clean = SummaryBackend::Sqlite(DurableAdmissionPaths::new(clean_dir.path()));
    let clean_host = clean.host("effect-summary-clean", None).await;
    start_process(&clean_host, &clean, &process_id, capped_program()).await;
    wait_for_terminal(
        &clean_host.core,
        &process_id,
        lash_core::ProcessStatus::Completed,
    )
    .await;
    let (clean_table, clean_events) = paged_summary(&clean_host.core, &process_id).await;

    let cap = usize::try_from(lash_core::PROCESS_EFFECT_OCCURRENCE_CAP).expect("small cap");
    assert_eq!(clean_table.nodes().len(), 2);
    for node in clean_table.nodes() {
        assert_eq!(
            node.occurrences.len(),
            cap,
            "the writer records the cap, no more"
        );
        assert_eq!(
            node.occurrences
                .iter()
                .map(|occurrence| occurrence.occurrence)
                .collect::<Vec<_>>(),
            (1..=lash_core::PROCESS_EFFECT_OCCURRENCE_CAP).collect::<Vec<_>>()
        );
        let class = node.occurrences[0].outcome_class;
        let omitted = match class {
            ProcessEffectOutcomeClass::Success => node.omitted.success,
            ProcessEffectOutcomeClass::Failure => node.omitted.failure,
            ProcessEffectOutcomeClass::Cancelled => node.omitted.cancelled,
        };
        assert_eq!(omitted, 2, "an omitted {class:?} keeps its class");
        assert_eq!(node.omitted.total(), 2);
    }
    assert_eq!(
        clean_events.len(),
        2 * cap + 1,
        "the durable log holds the capped occurrences and one omission record"
    );

    // The omission record's append crashes; the redrive re-derives every
    // count from the recorded effects and writes the identical record.
    let crash_dir = tempfile::tempdir().expect("crash tempdir");
    let crashed = SummaryBackend::Sqlite(DurableAdmissionPaths::new(crash_dir.path()));
    let recovered = run_through_append_crash(
        &crashed,
        &process_id,
        capped_program(),
        lash_core::PROCESS_EFFECT_OMISSIONS_EVENT_TYPE,
        lash_core::ProcessStatus::Completed,
    )
    .await;
    let (recovered_table, recovered_events) = paged_summary(&recovered.core, &process_id).await;
    assert_eq!(recovered_table, clean_table);
    assert_eq!(
        events_by_key(&recovered_events),
        events_by_key(&clean_events)
    );
    Ok(())
}

/// `batch_first` puts an aggregate's leaves before the single call, so the
/// crashed append is a batch leaf; otherwise it is the single call.
fn crash_window_program(batch_first: bool) -> lashlang::Program {
    let batch = b::assign(
        "batch",
        b::await_expr(b::record(vec![
            ("listed", list_triggers()),
            ("again", list_triggers()),
        ])),
    );
    let single = b::assign("clock", now_call());
    let (first, second) = if batch_first {
        (batch, single)
    } else {
        (single, batch)
    };
    b::module(
        vec![b::process(
            "main",
            Vec::new(),
            b::block(vec![
                first,
                second,
                b::finish(b::record(vec![
                    ("listed", b::field(b::var("batch"), "listed")),
                    ("again", b::field(b::var("batch"), "again")),
                    ("clock", b::var("clock")),
                ])),
            ]),
        )],
        Vec::new(),
    )
}

async fn assert_crash_window_recovers_once(backend: &SummaryBackend, batch_first: bool) {
    let process_id = ProcessId::from(format!("effect-summary-crash-batch-first-{batch_first}"));
    let recovered = run_through_append_crash(
        backend,
        &process_id,
        crash_window_program(batch_first),
        lash_core::PROCESS_EFFECT_OUTCOME_EVENT_TYPE,
        lash_core::ProcessStatus::Completed,
    )
    .await;
    let output = recovered
        .core
        .processes()
        .await_output(&process_id)
        .await
        .expect("await the recovered output");
    let lash_core::ProcessAwaitOutput::Settled { output } = output else {
        panic!("the recovered process settles: {output:?}");
    };
    assert!(
        output.is_success(),
        "the program outcome is unchanged: {output:?}"
    );
    let value = output.value_for_projection();

    let (table, folded) = paged_summary(&recovered.core, &process_id).await;
    let by_key = events_by_key(&folded);
    assert_eq!(
        by_key.len(),
        3,
        "two batch leaves and the single call, each appended exactly once"
    );
    let replay_rows = backend.replay_rows(&process_id).await;
    for key in by_key.keys() {
        assert!(
            replay_rows.contains_key(key),
            "the summary names a recorded effect: {key}"
        );
    }
    let clock_row = table
        .nodes()
        .flat_map(|node| node.occurrences.iter())
        .find(|occurrence| occurrence.operation == "typescript.runtime")
        .expect("the clock call is summarised");
    let recorded: lash_core::RuntimeEffectOutcome = serde_json::from_str(
        replay_rows
            .get(&clock_row.replay_key)
            .expect("the clock call has a replay row"),
    )
    .expect("decode the clock row");
    let lash_core::RuntimeEffectOutcome::LanguageRuntimeValue { value: recorded } = recorded else {
        panic!("the clock row records a runtime value");
    };
    assert_eq!(
        value["clock"], recorded,
        "the redrive replayed the recorded clock instead of sampling it again"
    );
    assert_eq!(value["listed"], serde_json::json!([]));
    assert_eq!(value["again"], serde_json::json!([]));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_crash_between_replay_row_and_summary_append_redrives_once() -> Result<()> {
    for batch_first in [false, true] {
        let temp = tempfile::tempdir().expect("crash-window tempdir");
        let backend = SummaryBackend::Sqlite(DurableAdmissionPaths::new(temp.path()));
        assert_crash_window_recovers_once(&backend, batch_first).await;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_crash_between_replay_row_and_summary_append_redrives_once() -> Result<()> {
    use sqlx::Connection as _;

    let database_url = match std::env::var("LASH_POSTGRES_DATABASE_URL") {
        Ok(url) if !url.is_empty() => url,
        _ if std::env::var("LASH_REQUIRE_POSTGRES").as_deref() == Ok("1") => {
            panic!("LASH_POSTGRES_DATABASE_URL is required")
        }
        _ => {
            eprintln!("skipping PostgreSQL effect-summary crash window: database URL is not set");
            return Ok(());
        }
    };
    let mut lock = sqlx::PgConnection::connect(&database_url)
        .await
        .expect("connect PostgreSQL test advisory lock");
    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(0x4c41_5348_5f50_4754_i64)
        .execute(&mut lock)
        .await
        .expect("acquire PostgreSQL test advisory lock");
    let storage = lash_postgres_store::PostgresStorage::connect(&database_url).await?;
    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT tablename FROM pg_tables
         WHERE schemaname = 'public'
           AND tablename LIKE 'lash\\_%'
           AND tablename NOT IN ('lash_schema_versions', 'lash_await_event_meta')
         ORDER BY tablename",
    )
    .fetch_all(storage.pool())
    .await
    .expect("list PostgreSQL tables");
    sqlx::query(&format!(
        "TRUNCATE {} RESTART IDENTITY CASCADE",
        tables.join(", ")
    ))
    .execute(storage.pool())
    .await
    .expect("reset PostgreSQL tables");
    sqlx::query(
        "INSERT INTO lash_process_change_clock (singleton, current_seq)
         VALUES (TRUE, 0)
         ON CONFLICT (singleton) DO UPDATE SET current_seq = 0",
    )
    .execute(storage.pool())
    .await
    .expect("reset PostgreSQL process change clock");
    drop(storage);

    let attachments = tempfile::tempdir().expect("PostgreSQL attachment tempdir");
    let backend = SummaryBackend::Postgres(database_url, attachments.path().to_path_buf());
    for batch_first in [false, true] {
        assert_crash_window_recovers_once(&backend, batch_first).await;
    }
    Ok(())
}

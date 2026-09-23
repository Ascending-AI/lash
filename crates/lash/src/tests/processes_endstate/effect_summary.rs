use super::*;
use lash_core::ProcessEffectOutcomeClass;

const TYPESCRIPT_RUNTIME_MODULE_PATH: &str = "__typescript_runtime";
const TYPESCRIPT_RUNTIME_RESOURCE_TYPE: &str = "typescript.Runtime";
const TYPESCRIPT_RUNTIME_NOW_OPERATION: &str = "now";

#[tokio::test]
async fn paged_process_effect_summary_matches_durable_replay_rows() -> Result<()> {
    let temp = tempfile::tempdir().expect("effect-summary tempdir");
    let paths = DurableAdmissionPaths::new(temp.path());
    let durable_store = Arc::new(
        lash_sqlite_store::Store::open(&paths.artifacts)
            .await
            .expect("open effect-summary artifact store"),
    );
    let artifact_store = Arc::new(SwitchableArtifactStore::new(durable_store));
    let registry = Arc::new(
        lash_sqlite_store::SqliteProcessRegistry::open(&paths.processes, &paths.sessions)
            .await
            .expect("open effect-summary process registry"),
    );
    let mut catalog = programs::process_control_catalog();
    catalog
        .add_module_operation_contract(
            [TYPESCRIPT_RUNTIME_MODULE_PATH],
            TYPESCRIPT_RUNTIME_RESOURCE_TYPE,
            TYPESCRIPT_RUNTIME_NOW_OPERATION,
            "typescript.runtime.now",
            &lashlang::OperationContract::new(
                serde_json::json!({}),
                serde_json::json!({ "type": "number" }),
            ),
        )
        .expect("register TypeScript runtime operation");
    lashlang::add_trigger_resource_operations(&mut catalog).expect("register trigger operations");
    let process = LinkedTestProcess::new_with_catalog(
        artifact_store.as_ref(),
        b::module(
            vec![
                b::process("child", Vec::new(), b::finish(b::string("child done"))),
                b::process(
                    "main",
                    Vec::new(),
                    b::block(vec![
                        b::assign("child", b::start("child", Vec::new())),
                        b::assign(
                            "clock",
                            b::module_call(&[TYPESCRIPT_RUNTIME_MODULE_PATH], "now", Vec::new()),
                        ),
                        b::sleep_until(b::num(0.0)),
                        b::assign(
                            "listed",
                            b::module_call(&["triggers"], "list", vec![b::record(Vec::new())]),
                        ),
                        b::module_call(
                            &["triggers"],
                            "disable",
                            vec![b::record(vec![
                                ("subscription_key", b::string("missing-subscription")),
                                ("expected_revision", b::num(1.0)),
                            ])],
                        ),
                        b::finish(b::var("clock")),
                    ]),
                ),
            ],
            Vec::new(),
        ),
        "main",
        catalog,
    )
    .await;
    let core = durable_admission_core(
        &paths,
        artifact_store,
        registry,
        CollectingProcessEventSink::default(),
        "effect-summary-host",
    )
    .await?;
    let process_id = ProcessId::from("effect-summary-facade");
    let mut start_request = process.start_request(&process_id);
    start_request.originator = lash_core::ProcessOriginator::host_scoped("effect-summary-test");
    core.processes()
        .start(
            start_request,
            runtime_operation_scope(&core, "effect-summary-start"),
        )
        .await?;
    let terminal = wait_for_terminal(&core, &process_id, lash_core::ProcessStatus::Failed).await;

    let mut table = lash_core::ProcessEffectSummary::default();
    let mut continuation = None;
    let mut page_count = 0;
    loop {
        let outcome = core
            .processes()
            .events(
                &process_id,
                std::num::NonZeroUsize::new(2).expect("non-zero page size"),
                lash_core::ProcessEventQueryMode::Full,
                continuation,
            )
            .await?;
        let lash_core::ProcessEventReadOutcome::Retained(page) = outcome else {
            panic!("effect-summary process history must be retained");
        };
        page_count += 1;
        let lash_core::ProcessEventPageEvents::Full(events) = page.events else {
            panic!("full page request must include payloads");
        };
        assert!(events.len() <= 2);
        for event in events {
            if event.event_type == lash_core::PROCESS_EFFECT_OUTCOME_EVENT_TYPE {
                table
                    .fold_outcome(
                        lash_core::ProcessEffectSummaryOccurrence::decode(event.payload)
                            .expect("decode paged effect outcome"),
                        lash_core::ProcessEffectSummaryConfig::default(),
                    )
                    .expect("fold paged effect outcome");
            }
        }
        continuation = match page.more {
            lash_core::ProcessEventPageMore::Complete => break,
            lash_core::ProcessEventPageMore::More { continuation } => Some(continuation),
        };
    }
    assert!(
        page_count >= 2,
        "the facade must actually cross a page boundary"
    );
    let occurrences = table
        .nodes()
        .flat_map(|node| node.occurrences.iter())
        .collect::<Vec<_>>();
    assert_eq!(
        occurrences.len(),
        5,
        "tool, TypeScript, sleep and two trigger outcomes: {occurrences:?}; terminal={terminal:?}"
    );
    assert_eq!(
        occurrences
            .iter()
            .filter(|row| row.outcome_class == ProcessEffectOutcomeClass::Failure)
            .count(),
        1
    );

    let connection =
        rusqlite::Connection::open(&paths.effects).expect("open effect-summary replay database");
    let mut query = connection.prepare(
        "SELECT replay_key, status, outcome_json FROM runtime_effect_replay WHERE scope_id LIKE '%effect-summary-facade%'",
    )
    .expect("prepare effect-summary replay query");
    let replay_rows = query
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        })
        .expect("query effect-summary replay rows")
        .map(|row| row.map(|(key, status, encoded)| (key, (status, encoded))))
        .collect::<std::result::Result<BTreeMap<_, _>, _>>();
    drop(query);
    let replay_rows = replay_rows.expect("read effect-summary replay rows");
    for occurrence in occurrences {
        // The tool coordinator journals each atomic attempt beneath the stable
        // call ID; trigger and TypeScript operations journal the ID directly.
        let row_key = if occurrence.operation == "tool:start_process" {
            format!("tool:{}:attempt:1", occurrence.replay_key)
        } else {
            occurrence.replay_key.clone()
        };
        let (status, encoded) = replay_rows
            .get(&row_key)
            .unwrap_or_else(|| panic!("missing durable replay row for {row_key}"));
        assert_eq!(status, "completed");
        let outcome: lash_core::RuntimeEffectOutcome =
            serde_json::from_str(encoded.as_deref().expect("completed replay has outcome"))
                .expect("decode effect replay outcome");
        match occurrence.operation.as_str() {
            "tool:start_process" => {
                assert_eq!(occurrence.outcome_class, ProcessEffectOutcomeClass::Success);
                assert!(
                    matches!(outcome, lash_core::RuntimeEffectOutcome::ToolAttempt { launch, .. } if matches!(&*launch, lash_core::ToolAttemptLaunch::Done { record, .. } if matches!(&record.output.outcome, lash_core::ToolCallOutcome::Success(_))))
                );
            }
            "now" => {
                assert_eq!(occurrence.outcome_class, ProcessEffectOutcomeClass::Success);
                assert!(matches!(
                    outcome,
                    lash_core::RuntimeEffectOutcome::LanguageRuntimeValue { .. }
                ));
            }
            "sleep_until" => {
                assert_eq!(occurrence.outcome_class, ProcessEffectOutcomeClass::Success);
                assert!(occurrence.code.is_none());
                assert!(matches!(outcome, lash_core::RuntimeEffectOutcome::Sleep));
            }
            "triggers.list" => {
                assert_eq!(occurrence.outcome_class, ProcessEffectOutcomeClass::Success);
                assert!(
                    matches!(outcome, lash_core::RuntimeEffectOutcome::Trigger { result } if result.is_ok())
                );
            }
            "triggers.disable" => {
                assert_eq!(occurrence.outcome_class, ProcessEffectOutcomeClass::Failure);
                assert!(occurrence.code.is_some());
                assert!(
                    matches!(outcome, lash_core::RuntimeEffectOutcome::Trigger { result } if result.is_err())
                );
            }
            operation => panic!("unexpected effect operation {operation}"),
        }
    }
    Ok(())
}

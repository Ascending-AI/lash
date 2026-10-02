// FIG-2971: this file is test/tooling/host code; ambient fs/env/process
// access is sanctioned here (the workspace clippy ban targets production
// library code).
#![allow(clippy::disallowed_methods)]

use super::*;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

mod service_journals;
use service_journals::HandlerJournals;

const CORPUS_ROOT_ENV: &str = "LASH_REPLAY_CORPUS_ROOT";
const REGENERATE_ENV: &str = "LASH_REGENERATE";
const FORMAT_NOTE: &str =
    "lash-restate RecordedRuntimeEffect JSON v1; map keys are Restate effect names";
const SERVICE_FORMAT_NOTE: &str = "lash-restate service handler journals v1; per handler, each distinct ordered command \
     sequence its invocations wrote on the server double, minted ids elided as #";

#[derive(Debug, Serialize, serde::Deserialize)]
struct ReplayCorpusFixture {
    scenario: String,
    recorded_at_git_sha: String,
    format: String,
    journal_logic_epoch: u32,
    journal_steps: Vec<String>,
    records: BTreeMap<String, RecordedRuntimeEffect>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    process_command_facts: BTreeMap<String, serde_json::Value>,
}

/// One lash Restate service's journals, recorded from its real handlers on
/// the server double (FIG-4805).
#[derive(Debug, Serialize, serde::Deserialize)]
struct ServiceJournalFixture {
    scenario: String,
    service: String,
    recorded_at_git_sha: String,
    format: String,
    journal_logic_epoch: u32,
    handlers: HandlerJournals,
}

#[derive(Clone, Copy)]
struct Scenario {
    name: &'static str,
}

const SCENARIOS: &[Scenario] = &[
    Scenario {
        name: "lashlang-effect-summary",
    },
    Scenario {
        name: "scalar-lashlang-tool-attempt",
    },
    Scenario {
        name: "sleep-envelope",
    },
];

/// The stable name of every service lash serves: the corpus holds one
/// scenario for each.
fn lash_service_names() -> BTreeSet<String> {
    crate::services::LASH_SERVICES
        .iter()
        .map(|service| service.base_name().to_string())
        .collect()
}

fn service_scenario_name(service: &str) -> String {
    format!("service-{service}")
}

/// Every scenario the corpus owes a fixture: the controller scenarios and
/// one per lash service.
fn registered_scenario_names() -> Vec<String> {
    let mut names = SCENARIOS
        .iter()
        .map(|scenario| scenario.name.to_string())
        .chain(
            lash_service_names()
                .iter()
                .map(|service| service_scenario_name(service)),
        )
        .collect::<Vec<_>>();
    names.sort();
    names
}

/// The registered name of every `#[restate_sdk::object]` and
/// `#[restate_sdk::workflow]` the crate declares outside its tests: the
/// trait's `#[name]`, or the trait's own name.
fn restate_services_declared_in_the_source() -> BTreeSet<String> {
    fn visit(directory: &Path, tests: &Path, services: &mut BTreeSet<String>) {
        let mut entries = std::fs::read_dir(directory)
            .expect("read a source directory")
            .map(|entry| entry.expect("read a source entry").path())
            .collect::<Vec<_>>();
        entries.sort();
        for path in entries {
            if path == tests || path == tests.with_extension("rs") {
                continue;
            }
            if path.is_dir() {
                visit(&path, tests, services);
                continue;
            }
            if path.extension().is_none_or(|extension| extension != "rs") {
                continue;
            }
            let source = std::fs::read_to_string(&path).expect("read a source file");
            let mut lines = source.lines().map(str::trim);
            while let Some(line) = lines.next() {
                if !line.starts_with("#[restate_sdk::object")
                    && !line.starts_with("#[restate_sdk::workflow")
                {
                    continue;
                }
                let name = lines
                    .by_ref()
                    .find_map(|line| {
                        if let Some(named) = line.strip_prefix("#[name = \"") {
                            return named.split('"').next();
                        }
                        let (_, declared) = line.split_once("trait ")?;
                        declared.split([' ', '{', '<', ':']).next()
                    })
                    .unwrap_or_else(|| {
                        panic!("{}: a service macro names no trait", path.display())
                    });
                assert!(
                    services.insert(name.to_string()),
                    "{}: a second service named `{name}`",
                    path.display()
                );
            }
        }
    }
    let source = crate_dir().join("src");
    let mut services = BTreeSet::new();
    visit(&source, &source.join("tests"), &mut services);
    services
}

/// The corpus's service list is derived, never hand-kept (FIG-4805): every
/// `#[restate_sdk::object|workflow]` the crate declares is a lash service,
/// and every lash service has a recorded scenario.
#[test]
fn every_restate_service_in_the_source_has_a_recorded_scenario() {
    assert_eq!(
        restate_services_declared_in_the_source(),
        lash_service_names(),
        "the services the source declares are the services lash serves"
    );
    let fixtures = fixture_scenario_names();
    for service in lash_service_names() {
        assert!(
            fixtures.contains(&service_scenario_name(&service)),
            "Restate service `{service}` has no replay corpus scenario: record one with \
             {REGENERATE_ENV}=1 (crates/lash-restate/testdata/README.md)"
        );
        let fixture = read_service_fixture(&service);
        assert_eq!(fixture.service, service);
        assert_eq!(fixture.scenario, service_scenario_name(&service));
        assert_eq!(fixture.format, SERVICE_FORMAT_NOTE);
        assert!(
            !fixture.recorded_at_git_sha.is_empty(),
            "{service} must name the commit it was recorded from"
        );
    }
}

/// Every lash service's recorded journals against what its real handlers
/// write now, on the server double over SQLite memory: a step added to,
/// removed from or reordered in a handler diverges here.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn replay_corpus_service_journals_match_the_real_handlers() {
    let recorded = Box::pin(service_journals::record()).await;
    assert_eq!(
        recorded.served,
        lash_service_names(),
        "the workload's deployment serves every lash service"
    );
    for service in lash_service_names() {
        let current = recorded
            .journals
            .get(&service)
            .unwrap_or_else(|| panic!("the workload ran no handler of `{service}`"));
        let result = replay_service_fixture(
            read_service_fixture(&service),
            current,
            crate::JOURNAL_LOGIC_EPOCH,
        )
        .unwrap_or_else(|error| {
            panic!(
                "{error}: {}",
                std::error::Error::source(&error).expect("a divergence names its cause")
            )
        });
        match result {
            ReplayComparison::Compared => println!("{service}: handler journals compared"),
            ReplayComparison::DifferentGeneration { recorded, current } => println!(
                "{service}: different generation, not compared (recorded epoch {recorded}, current {current})",
            ),
        }
    }
    println!(
        "release journal replay: {} service scenarios",
        lash_service_names().len()
    );
}

fn replay_service_fixture(
    fixture: ServiceJournalFixture,
    current: &HandlerJournals,
    current_epoch: u32,
) -> Result<ReplayComparison, ReplayDivergence> {
    assert_ne!(
        fixture.journal_logic_epoch, 0,
        "capture epoch must be positive"
    );
    if fixture.journal_logic_epoch != current_epoch {
        return Ok(ReplayComparison::DifferentGeneration {
            recorded: fixture.journal_logic_epoch,
            current: current_epoch,
        });
    }
    let differing = fixture
        .handlers
        .keys()
        .chain(current.keys())
        .filter(|handler| fixture.handlers.get(*handler) != current.get(*handler))
        .cloned()
        .collect::<BTreeSet<_>>();
    if differing.is_empty() {
        return Ok(ReplayComparison::Compared);
    }
    let only = |journals: &HandlerJournals| {
        journals
            .iter()
            .filter(|(handler, _)| differing.contains(*handler))
            .map(|(handler, journals)| (handler.clone(), journals.clone()))
            .collect()
    };
    Err(ReplayDivergence(ReplayFailure::ServiceJournals {
        service: fixture.service,
        recorded: only(&fixture.handlers),
        current: only(current),
    }))
}

#[tokio::test]
async fn replay_corpus_fixtures_match_current_controller() {
    let fixture_names = fixture_scenario_names();
    let registered_names = registered_scenario_names();
    assert_eq!(
        fixture_names, registered_names,
        "every committed replay fixture must have exactly one registered scenario"
    );

    for scenario in SCENARIOS {
        let fixture = read_fixture(*scenario);
        assert_eq!(fixture.scenario, scenario.name);
        assert_eq!(fixture.format, FORMAT_NOTE);
        assert!(
            !fixture.recorded_at_git_sha.is_empty(),
            "{} must name the commit it was recorded from",
            scenario.name
        );

        let result = replay_fixture(*scenario, fixture, crate::JOURNAL_LOGIC_EPOCH)
            .await
            .unwrap_or_else(|error| panic!("{error}"));
        match result {
            ReplayComparison::Compared => println!("{}: journal replay compared", scenario.name),
            ReplayComparison::DifferentGeneration { recorded, current } => println!(
                "{}: different generation, not compared (recorded epoch {recorded}, current {current})",
                scenario.name,
            ),
        }
    }
    println!("release journal replay: {} scenarios", SCENARIOS.len());
}

#[derive(Debug, PartialEq, Eq)]
enum ReplayComparison {
    Compared,
    DifferentGeneration { recorded: u32, current: u32 },
}

#[derive(Debug, thiserror::Error)]
#[error("journal logic changed: bump JOURNAL_LOGIC_EPOCH")]
struct ReplayDivergence(#[source] ReplayFailure);

#[derive(Debug, thiserror::Error)]
enum ReplayFailure {
    #[error(transparent)]
    Effect(#[from] lash_core::RuntimeEffectControllerError),
    #[error(transparent)]
    Process(#[from] lash_core::PluginError),
    #[error("recorded steps {recorded:?}, current steps {current:?}")]
    Steps {
        recorded: Vec<String>,
        current: Vec<String>,
    },
    #[error("missing recorded effect `{0}`")]
    MissingEffect(String),
    #[error("service `{service}` handlers recorded {recorded:#?}, current {current:#?}")]
    ServiceJournals {
        service: String,
        recorded: HandlerJournals,
        current: HandlerJournals,
    },
}

async fn replay_fixture(
    scenario: Scenario,
    fixture: ReplayCorpusFixture,
    current_epoch: u32,
) -> Result<ReplayComparison, ReplayDivergence> {
    assert_ne!(
        fixture.journal_logic_epoch, 0,
        "capture epoch must be positive"
    );
    if fixture.journal_logic_epoch != current_epoch {
        return Ok(ReplayComparison::DifferentGeneration {
            recorded: fixture.journal_logic_epoch,
            current: current_epoch,
        });
    }
    let context = Arc::new(ReplayableRecordingContext::default());
    context.install_recorded_runtime_effects(fixture.records);
    context.install_recorded_process_command_facts(fixture.process_command_facts);
    context.start_replay();
    Box::pin(drive_scenario(scenario, Arc::clone(&context), true))
        .await
        .map_err(ReplayDivergence)?;
    let current = context.runs();
    if fixture.journal_steps != current {
        return Err(ReplayDivergence(ReplayFailure::Steps {
            recorded: fixture.journal_steps,
            current,
        }));
    }
    Ok(ReplayComparison::Compared)
}

fn added_step_fixture() -> ReplayCorpusFixture {
    let scenario = Scenario {
        name: "scalar-lashlang-tool-attempt",
    };
    let mut fixture: serde_json::Value =
        serde_json::from_slice(&std::fs::read(fixture_path(scenario)).expect("read fixture copy"))
            .expect("fixture JSON");
    let records = fixture["records"].as_object_mut().expect("record map");
    let record = records
        .values()
        .next()
        .expect("scalar recorded step")
        .clone();
    records.insert("lash:release-journal-added-step".to_string(), record);
    // A fixture copy with one more ctx.run result than the current controller.
    fixture["journal_logic_epoch"] = serde_json::json!(crate::JOURNAL_LOGIC_EPOCH);
    fixture["journal_steps"] = serde_json::json!([
        "lash:session:turn:1:0:tool_attempt:scalar-lashlang-tool-attempt",
        "lash:release-journal-added-step",
    ]);
    serde_json::from_value(fixture).expect("decode fixture copy")
}

#[tokio::test]
async fn an_added_run_step_with_unchanged_epoch_requires_the_exact_bump_message() {
    let error = replay_fixture(
        Scenario {
            name: "scalar-lashlang-tool-attempt",
        },
        added_step_fixture(),
        crate::JOURNAL_LOGIC_EPOCH,
    )
    .await
    .expect_err("an added ctx.run step must diverge");
    assert_eq!(
        error.to_string(),
        "journal logic changed: bump JOURNAL_LOGIC_EPOCH"
    );
}

#[tokio::test]
async fn an_epoch_bump_does_not_compare_the_added_step_journal() {
    assert_eq!(
        replay_fixture(
            Scenario {
                name: "scalar-lashlang-tool-attempt"
            },
            added_step_fixture(),
            crate::JOURNAL_LOGIC_EPOCH + 1,
        )
        .await
        .expect("another generation is routed separately"),
        ReplayComparison::DifferentGeneration {
            recorded: crate::JOURNAL_LOGIC_EPOCH,
            current: crate::JOURNAL_LOGIC_EPOCH + 1,
        },
    );
}

/// A service fixture and the journals of a build whose `drive` handler
/// journals one more step than the fixture recorded.
fn service_fixture_and_an_added_step() -> (ServiceJournalFixture, HandlerJournals) {
    let journal = |steps: &[&str]| {
        BTreeMap::from([(
            "drive".to_string(),
            BTreeSet::from([steps
                .iter()
                .map(|step| step.to_string())
                .collect::<Vec<_>>()]),
        )])
    };
    let fixture = ServiceJournalFixture {
        scenario: service_scenario_name("LashSession"),
        service: "LashSession".to_string(),
        recorded_at_git_sha: "self-test".to_string(),
        format: SERVICE_FORMAT_NOTE.to_string(),
        journal_logic_epoch: crate::JOURNAL_LOGIC_EPOCH,
        handlers: journal(&["InputCommand", "RunCommand lash.drive.leg", "OutputCommand"]),
    };
    let current = journal(&[
        "InputCommand",
        "RunCommand lash.drive.leg",
        "RunCommand lash:release-journal-added-step",
        "OutputCommand",
    ]);
    (fixture, current)
}

#[test]
fn an_added_handler_step_with_unchanged_epoch_requires_the_exact_bump_message() {
    let (fixture, current) = service_fixture_and_an_added_step();
    let error = replay_service_fixture(fixture, &current, crate::JOURNAL_LOGIC_EPOCH)
        .expect_err("an added ctx.run step must diverge");
    assert_eq!(
        error.to_string(),
        "journal logic changed: bump JOURNAL_LOGIC_EPOCH"
    );
}

#[test]
fn an_epoch_bump_does_not_compare_the_added_handler_step() {
    let (fixture, current) = service_fixture_and_an_added_step();
    assert_eq!(
        replay_service_fixture(fixture, &current, crate::JOURNAL_LOGIC_EPOCH + 1)
            .expect("another generation is routed separately"),
        ReplayComparison::DifferentGeneration {
            recorded: crate::JOURNAL_LOGIC_EPOCH,
            current: crate::JOURNAL_LOGIC_EPOCH + 1,
        },
    );
}

#[test]
fn every_replay_fixture_records_its_capture_epoch_and_step_order() {
    for scenario in SCENARIOS {
        let fixture: serde_json::Value = serde_json::from_slice(
            &std::fs::read(fixture_path(*scenario)).expect("read replay fixture"),
        )
        .expect("fixture JSON");
        assert!(
            fixture["journal_logic_epoch"]
                .as_u64()
                .is_some_and(|epoch| epoch > 0)
        );
        assert!(
            fixture["journal_steps"]
                .as_array()
                .is_some_and(|steps| !steps.is_empty())
        );
    }
    for service in lash_service_names() {
        let fixture = read_service_fixture(&service);
        assert!(fixture.journal_logic_epoch > 0);
        assert!(
            !fixture.handlers.is_empty()
                && fixture.handlers.values().all(|journals| {
                    !journals.is_empty() && journals.iter().all(|steps| !steps.is_empty())
                }),
            "{service} records the steps of every handler it ran"
        );
    }
}

#[test]
fn replay_corpus_root_uses_the_selected_directory() {
    const PROBE: &str = "LASH_REPLAY_ROOT_PROBE";
    if std::env::var_os(PROBE).is_some() {
        assert_eq!(
            fixture_root(),
            PathBuf::from(std::env::var_os(CORPUS_ROOT_ENV).unwrap())
        );
        return;
    }
    let root = tempfile::tempdir_in(crate_dir()).expect("isolated corpus root");
    let output = Command::new(std::env::current_exe().expect("test binary"))
        .args([
            "--exact",
            "tests::replay_corpus::replay_corpus_root_uses_the_selected_directory",
            "--nocapture",
        ])
        .env(PROBE, "1")
        .env(CORPUS_ROOT_ENV, root.path())
        .output()
        .expect("run corpus-root probe");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "regenerates crates/lash-restate/testdata/replay-corpus"]
async fn regenerate_replay_corpus_fixtures() {
    assert_eq!(
        std::env::var(REGENERATE_ENV).as_deref(),
        Ok("1"),
        "set {REGENERATE_ENV}=1 to acknowledge replacing the committed replay corpus"
    );
    let git_sha = recorded_at_git_sha();

    for scenario in SCENARIOS {
        let context = Arc::new(ReplayableRecordingContext::default());
        Box::pin(drive_scenario(*scenario, Arc::clone(&context), false))
            .await
            .expect("record scenario");
        let fixture = ReplayCorpusFixture {
            scenario: scenario.name.to_string(),
            recorded_at_git_sha: git_sha.clone(),
            format: FORMAT_NOTE.to_string(),
            journal_logic_epoch: crate::JOURNAL_LOGIC_EPOCH,
            journal_steps: context.runs(),
            records: context.recorded_runtime_effects(),
            process_command_facts: context.recorded_process_command_facts(),
        };
        let path = fixture_path(*scenario);
        std::fs::create_dir_all(path.parent().expect("fixture parent"))
            .expect("create replay corpus scenario directory");
        std::fs::write(path, json_with_newline(&fixture)).expect("write replay corpus fixture");
    }

    let mut recorded = Box::pin(service_journals::record()).await;
    for service in lash_service_names() {
        let fixture = ServiceJournalFixture {
            scenario: service_scenario_name(&service),
            recorded_at_git_sha: git_sha.clone(),
            format: SERVICE_FORMAT_NOTE.to_string(),
            journal_logic_epoch: crate::JOURNAL_LOGIC_EPOCH,
            handlers: recorded
                .journals
                .remove(&service)
                .unwrap_or_else(|| panic!("the workload ran no handler of `{service}`")),
            service: service.clone(),
        };
        let path = service_fixture_path(&service);
        std::fs::create_dir_all(path.parent().expect("fixture parent"))
            .expect("create replay corpus scenario directory");
        std::fs::write(path, json_with_newline(&fixture)).expect("write replay corpus fixture");
    }
}

async fn drive_scenario(
    scenario: Scenario,
    context: Arc<ReplayableRecordingContext>,
    replaying: bool,
) -> Result<(), ReplayFailure> {
    match scenario.name {
        "sleep-envelope" => drive_sleep_envelope(context, replaying),
        "scalar-lashlang-tool-attempt" => {
            Box::pin(drive_scalar_lashlang_tool_attempt(context, replaying)).await
        }
        "lashlang-effect-summary" => {
            Box::pin(drive_lashlang_effect_summary(context, replaying)).await
        }
        other => panic!("unimplemented replay corpus scenario `{other}`"),
    }
}

fn drive_sleep_envelope(
    context: Arc<ReplayableRecordingContext>,
    replaying: bool,
) -> Result<(), ReplayFailure> {
    let envelope = test_sleep_envelope(1);
    let effect_name = restate_effect_name(&envelope.invocation);
    let canonical = envelope.canonical_form().expect("canonical sleep envelope");

    if !replaying {
        context.install_recorded_runtime_effects(BTreeMap::from([(
            effect_name.clone(),
            RecordedRuntimeEffect {
                envelope: Arc::new(canonical.clone()),
                outcome: Ok(RuntimeEffectOutcome::Sleep),
                usage: None,
            },
        )]));
    }

    context.runs.lock_recover().push(effect_name.clone());
    let recorded = context
        .recorded_runtime_effect(&effect_name)
        .ok_or(ReplayFailure::MissingEffect(effect_name))?;
    let outcome = validate_recorded_effect_envelope(recorded, &canonical, None)??;
    assert!(matches!(outcome, RuntimeEffectOutcome::Sleep));
    Ok(())
}

async fn drive_scalar_lashlang_tool_attempt(
    context: Arc<ReplayableRecordingContext>,
    replaying: bool,
) -> Result<(), ReplayFailure> {
    let call_id = "replay-scalar-call";
    let tool_name = "replay_scalar_counter";
    let envelope = RuntimeEffectEnvelope::new(
        runtime_invocation(
            RuntimeEffectKind::ToolAttempt,
            "scalar-lashlang-tool-attempt",
        ),
        RuntimeEffectCommand::ToolAttempt {
            call: Box::new(prepared_tool_call_with(call_id, tool_name)),
            execution_grant: None,
            attempt: 1,
            max_attempts: 1,
        },
    );
    let effect_name = restate_effect_name(&envelope.invocation);
    let local_runs = Arc::new(AtomicUsize::new(0));
    let controller = RestateRuntimeEffectController::new_for_test(Arc::clone(&context));
    let outcome = controller
        .execute_effect(
            envelope,
            RuntimeEffectLocalExecutor::testing({
                let local_runs = Arc::clone(&local_runs);
                move |_envelope| async move {
                    local_runs.fetch_add(1, Ordering::SeqCst);
                    Ok(RuntimeEffectOutcome::ToolAttempt {
                        launch: Box::new(lash_core::ToolAttemptLaunch::Done {
                            record: Box::new(completed_tool_record(call_id, tool_name)),
                            intents: lash_core::ToolIntents::default(),
                        }),
                        triggers: Vec::new(),
                        capture: None,
                    })
                }
            }),
        )
        .await?;

    let RuntimeEffectOutcome::ToolAttempt { launch, .. } = outcome else {
        panic!("recorded scalar scenario returned the wrong outcome");
    };
    assert!(matches!(
        *launch,
        lash_core::ToolAttemptLaunch::Done { ref record, .. }
            if record.call_id == lash_core::ToolCallId::fixture(call_id) && record.tool == tool_name
    ));
    assert_eq!(
        local_runs.load(Ordering::SeqCst),
        usize::from(!replaying),
        "replay must return the journaled scalar ToolAttempt without re-executing it"
    );
    assert_eq!(context.runs(), vec![effect_name]);
    Ok(())
}

/// FIG-3464: a Lashlang process whose tool call is journaled in the Restate
/// invocation. Replaying the committed journal answers the call without
/// running the tool, and the durable effect summary the terminal batch
/// carries (FIG-3571) is derived from the journaled outcome — the record a
/// redrive after an interruption rebuilds.
async fn drive_lashlang_effect_summary(
    context: Arc<ReplayableRecordingContext>,
    replaying: bool,
) -> Result<(), ReplayFailure> {
    // The journal keys its effects by the process id, so the recording and
    // every replay register under the same sequential test id.
    let registry = sequential_process_registry();
    let registration = super::process_effect_summary::counting_lashlang_registration().await;
    let process_id = registry
        .register_process(registration.clone())
        .await
        .expect("register the effect-summary process")
        .id;
    let executions = Arc::new(AtomicUsize::new(0));
    let outcome = super::process_effect_summary::run_invocation(
        Arc::clone(&registry),
        &executions,
        &context,
        &process_id,
        &registration,
    )
    .await?;
    let lash_core::ProcessRunOutcome::Terminal { output, prelude } = outcome else {
        panic!("the effect-summary invocation terminates");
    };
    assert!(matches!(
        output.as_ref(),
        ProcessAwaitOutput::Settled { output } if output.value_for_projection() == serde_json::json!(1)
    ));
    assert_eq!(
        executions.load(Ordering::SeqCst),
        usize::from(!replaying),
        "replay answers the tool call from the journal"
    );
    assert!(
        super::process_effect_summary::effect_outcomes(&registry, &process_id)
            .await
            .is_empty(),
        "the run commits its summary with its terminal, not as it goes"
    );
    let outcomes = prelude
        .into_iter()
        .filter(|request| request.event_type == lash_core::PROCESS_EFFECT_OUTCOME_EVENT_TYPE)
        .collect::<Vec<_>>();
    assert_eq!(outcomes.len(), 1, "one summary record per journaled effect");
    let summary = lash_core::ProcessEffectOccurrence::decode(
        outcomes[0].payload.clone(),
        lash_core::FleetFormat::current(),
    )
    .expect("decode the summary record");
    assert_eq!(summary.operation, "tool:recovery_count");
    assert_eq!(summary.occurrence, 1);
    assert_eq!(
        summary.outcome_class,
        lash_core::ProcessEffectOutcomeClass::Success
    );
    assert!(
        context
            .recorded_runtime_effects()
            .keys()
            .any(|name| name.contains(&summary.replay_key)),
        "the summary names the journaled effect: {}",
        summary.replay_key
    );
    Ok(())
}

fn read_fixture(scenario: Scenario) -> ReplayCorpusFixture {
    serde_json::from_slice(
        &std::fs::read(fixture_path(scenario)).expect("read committed replay corpus fixture"),
    )
    .expect("decode committed replay corpus fixture")
}

fn read_service_fixture(service: &str) -> ServiceJournalFixture {
    let path = service_fixture_path(service);
    serde_json::from_slice(&std::fs::read(&path).unwrap_or_else(|error| {
        panic!(
            "Restate service `{service}` has no replay corpus scenario at {}: {error}",
            path.display()
        )
    }))
    .expect("decode committed service journal fixture")
}

fn service_fixture_path(service: &str) -> PathBuf {
    fixture_root()
        .join(service_scenario_name(service))
        .join("journal.json")
}

fn fixture_scenario_names() -> Vec<String> {
    let mut names = std::fs::read_dir(fixture_root())
        .expect("read committed replay corpus directory")
        .map(|entry| {
            let entry = entry.expect("read replay corpus entry");
            assert!(
                entry.path().is_dir(),
                "replay corpus entries must be directories"
            );
            entry
                .file_name()
                .into_string()
                .expect("replay corpus scenario names must be UTF-8")
        })
        .collect::<Vec<_>>();
    names.sort();
    names
}

fn fixture_path(scenario: Scenario) -> PathBuf {
    fixture_root().join(scenario.name).join("journal.json")
}

fn fixture_root() -> PathBuf {
    std::env::var_os(CORPUS_ROOT_ENV)
        .map_or_else(|| crate_dir().join("testdata/replay-corpus"), PathBuf::from)
}

/// The crate's source directory in a Buck2 test workspace or Cargo checkout.
fn crate_dir() -> PathBuf {
    std::env::var_os("BUILD_WORKSPACE_DIRECTORY").map_or_else(
        || Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf(),
        |root| PathBuf::from(root).join("crates/lash-restate"),
    )
}

fn recorded_at_git_sha() -> String {
    let output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(crate_dir())
        .output()
        .expect("run git rev-parse for replay corpus metadata");
    assert!(output.status.success(), "git rev-parse HEAD must succeed");
    String::from_utf8(output.stdout)
        .expect("git SHA must be UTF-8")
        .trim()
        .to_string()
}

fn json_with_newline(value: &impl Serialize) -> Vec<u8> {
    let mut json = serde_json::to_vec_pretty(value).expect("encode deterministic fixture JSON");
    json.push(b'\n');
    json
}

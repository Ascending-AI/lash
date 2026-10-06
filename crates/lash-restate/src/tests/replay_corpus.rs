// FIG-2971: this file is test/tooling/host code; ambient fs/env/process
// access is sanctioned here (the workspace clippy ban targets production
// library code).
#![allow(clippy::disallowed_methods)]

use super::*;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

pub(super) mod run_scenarios;
pub(super) mod service_journals;
use service_journals::HandlerJournals;

const CORPUS_ROOT_ENV: &str = "LASH_REPLAY_CORPUS_ROOT";
const REGENERATE_ENV: &str = "LASH_REGENERATE";

/// One scenario's journal, as the build of generation `generation` wrote it.
///
/// The fixture carries no provenance of its own: a release corpus names its
/// source commit once, in its capture manifest.
#[derive(Debug, Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplayCorpusFixture {
    scenario: String,
    /// The drain generation `G` of the capturing build. Builds sharing it
    /// replay this journal; any other build routes it to a drain.
    generation: lash_core::engine::BuildGeneration,
    /// The journal, in the order the handler ran its steps.
    journal: Vec<JournalEntry>,
}

/// One journaled step: its Restate name and the entry the build wrote under it.
#[derive(Clone, Debug, Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct JournalEntry {
    name: String,
    body: JournalBody,
}

/// What a step journaled, verbatim: a runtime effect's stamped record or a
/// process command's fact.
#[derive(Clone, Debug, Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum JournalBody {
    Effect(serde_json::Value),
    ProcessCommandFact(serde_json::Value),
}

impl JournalBody {
    fn journaled(name: &str, bytes: &[u8]) -> Self {
        let value = serde_json::from_slice(bytes).expect("a journaled entry is JSON");
        if crate::controller::is_process_command_journal_name(name) {
            Self::ProcessCommandFact(value)
        } else {
            Self::Effect(value)
        }
    }

    fn bytes(&self) -> Vec<u8> {
        let (Self::Effect(value) | Self::ProcessCommandFact(value)) = self;
        serde_json::to_vec(value).expect("encode a journaled entry")
    }
}

/// The corpus's standard-protocol composition, bound by a real core before
/// its generation can be used to stamp or compare a journal.
pub(super) async fn current_generation() -> lash_core::engine::BuildGeneration {
    let connection = RestateConnection::new("https://restate.invalid");
    let backend = lash_core::Backend::new(Arc::new(RestateEngine::new(
        Arc::new(
            lash_sqlite_store::SqliteStoreSet::memory()
                .await
                .expect("corpus stores"),
        ),
        RestateConfig::new(connection.clone(), connection, test_restate_authority_id()),
    )));
    let core =
        service_journals::build_core(backend, &Arc::new(tokio::sync::Semaphore::new(0)), None);
    core.build_generation().clone()
}

/// The journal a recording left behind, in step order.
fn recorded_journal(context: &ReplayableRecordingContext) -> Vec<JournalEntry> {
    let records = context.records.lock_recover();
    let journal = context
        .runs()
        .into_iter()
        .map(|name| {
            let bytes = records
                .get(&name)
                .unwrap_or_else(|| panic!("step `{name}` journaled no entry"));
            JournalEntry {
                body: JournalBody::journaled(&name, bytes),
                name,
            }
        })
        .collect::<Vec<_>>();
    assert_eq!(
        journal.len(),
        records.len(),
        "every journaled entry belongs to exactly one step"
    );
    journal
}

fn journal_steps(journal: &[JournalEntry]) -> Vec<String> {
    journal.iter().map(|entry| entry.name.clone()).collect()
}

/// One lash Restate service's journals, recorded from its real handlers on
/// the server double (FIG-4805).
#[derive(Debug, Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ServiceJournalFixture {
    scenario: String,
    service: String,
    generation: lash_core::engine::BuildGeneration,
    handlers: HandlerJournals,
}

/// One run-behaviour scenario's whole deployment: every lash service's
/// handler journals, recorded from the real handlers on the server double
/// (FIG-4902).
#[derive(Debug, Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RunScenarioFixture {
    scenario: String,
    generation: lash_core::engine::BuildGeneration,
    /// Every lash service's handler journals, by stable service name.
    services: BTreeMap<String, HandlerJournals>,
}

#[derive(Clone, Copy)]
struct Scenario {
    name: &'static str,
}

const SCENARIOS: &[Scenario] = &[
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

/// Every scenario the corpus owes a fixture: the controller scenarios, one
/// per lash service, and the run-behaviour scenarios.
fn registered_scenario_names() -> Vec<String> {
    let mut names = SCENARIOS
        .iter()
        .map(|scenario| scenario.name.to_string())
        .chain(
            lash_service_names()
                .iter()
                .map(|service| service_scenario_name(service)),
        )
        .chain(
            run_scenarios::RUN_SCENARIOS
                .iter()
                .map(|name| name.to_string()),
        )
        .collect::<Vec<_>>();
    names.sort();
    names
}

/// The registered name of every Restate service, object and workflow
/// the crate declares outside its tests: the
/// trait's `#[name]`, or the trait's own name.
fn restate_services_declared_in_the_source() -> BTreeSet<String> {
    fn visit(directory: &Path, services: &mut BTreeSet<String>) {
        let mut entries = std::fs::read_dir(directory)
            .expect("read a source directory")
            .map(|entry| entry.expect("read a source entry").path())
            .collect::<Vec<_>>();
        entries.sort();
        for path in entries {
            if path.file_name().is_some_and(|name| {
                name == "tests"
                    || name == "tests.rs"
                    || name.to_string_lossy().ends_with("_tests.rs")
            }) {
                continue;
            }
            if path.is_dir() {
                visit(&path, services);
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
                    && !line.starts_with("#[restate_sdk::service")
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
    visit(&source, &mut services);
    services
}

/// The corpus's service list is derived, never hand-kept (FIG-4805): every
/// `#[restate_sdk::service|object|workflow]` the crate declares is a lash service,
/// and every lash service has a recorded scenario.
#[test]
fn every_restate_service_in_the_source_has_a_recorded_scenario() {
    assert_eq!(
        restate_services_declared_in_the_source(),
        lash_service_names(),
        "the services the source declares are the services lash serves"
    );
    let fixtures = fixture_scenario_names();
    let generation = read_fixture(SCENARIOS[0]).generation;
    for service in lash_service_names() {
        assert!(
            fixtures.contains(&service_scenario_name(&service)),
            "Restate service `{service}` has no replay corpus scenario: record one with \
             {REGENERATE_ENV}=1 (crates/lash-restate/testdata/README.md)"
        );
        let fixture = read_service_fixture(&service);
        assert_eq!(fixture.service, service);
        assert_eq!(fixture.scenario, service_scenario_name(&service));
        assert_eq!(
            fixture.generation, generation,
            "{service} belongs to the corpus's generation"
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
            &recorded.generation,
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
                "{service}: different generation, not compared (recorded {recorded}, current {current})",
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
    generation: &lash_core::engine::BuildGeneration,
) -> Result<ReplayComparison, ReplayDivergence> {
    if fixture.generation != *generation {
        return Ok(ReplayComparison::DifferentGeneration {
            recorded: fixture.generation,
            current: generation.clone(),
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

/// Every run-behaviour scenario's recorded journals against what its real
/// handlers write now, on a fresh server double over SQLite memory — and the
/// scenario's semantic assertion runs again every recording.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn replay_corpus_run_scenarios_match_the_real_handlers() {
    let generation = current_generation().await;
    for scenario in run_scenarios::RUN_SCENARIOS {
        let current = run_scenarios::record(scenario).await;
        let result = replay_run_fixture(read_run_fixture(scenario), &current, &generation)
            .unwrap_or_else(|error| {
                panic!(
                    "{error}: {}",
                    std::error::Error::source(&error).expect("a divergence names its cause")
                )
            });
        match result {
            ReplayComparison::Compared => println!("{scenario}: handler journals compared"),
            ReplayComparison::DifferentGeneration { recorded, current } => println!(
                "{scenario}: different generation, not compared (recorded {recorded}, current {current})",
            ),
        }
    }
    println!(
        "release journal replay: {} run scenarios",
        run_scenarios::RUN_SCENARIOS.len()
    );
}

fn replay_run_fixture(
    fixture: RunScenarioFixture,
    current: &BTreeMap<String, HandlerJournals>,
    generation: &lash_core::engine::BuildGeneration,
) -> Result<ReplayComparison, ReplayDivergence> {
    if fixture.generation != *generation {
        return Ok(ReplayComparison::DifferentGeneration {
            recorded: fixture.generation,
            current: generation.clone(),
        });
    }
    let differing = fixture
        .services
        .keys()
        .chain(current.keys())
        .filter(|service| fixture.services.get(*service) != current.get(*service))
        .cloned()
        .collect::<BTreeSet<_>>();
    if differing.is_empty() {
        return Ok(ReplayComparison::Compared);
    }
    let only = |journals: &BTreeMap<String, HandlerJournals>| {
        journals
            .iter()
            .filter(|(service, _)| differing.contains(*service))
            .map(|(service, journals)| (service.clone(), journals.clone()))
            .collect()
    };
    Err(ReplayDivergence(ReplayFailure::RunScenario {
        scenario: fixture.scenario,
        recorded: only(&fixture.services),
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

        let result = replay_fixture(*scenario, fixture, &current_generation().await)
            .await
            .unwrap_or_else(|error| panic!("{error}"));
        match result {
            ReplayComparison::Compared => println!("{}: journal replay compared", scenario.name),
            ReplayComparison::DifferentGeneration { recorded, current } => println!(
                "{}: different generation, not compared (recorded {recorded}, current {current})",
                scenario.name,
            ),
        }
    }
    println!("release journal replay: {} scenarios", SCENARIOS.len());
}

#[derive(Debug, PartialEq, Eq)]
enum ReplayComparison {
    Compared,
    DifferentGeneration {
        recorded: lash_core::engine::BuildGeneration,
        current: lash_core::engine::BuildGeneration,
    },
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
    #[error("run scenario `{scenario}` recorded {recorded:#?}, current {current:#?}")]
    RunScenario {
        scenario: String,
        recorded: BTreeMap<String, HandlerJournals>,
        current: BTreeMap<String, HandlerJournals>,
    },
}

async fn replay_fixture(
    scenario: Scenario,
    fixture: ReplayCorpusFixture,
    current: &lash_core::engine::BuildGeneration,
) -> Result<ReplayComparison, ReplayDivergence> {
    if fixture.generation != *current {
        return Ok(ReplayComparison::DifferentGeneration {
            recorded: fixture.generation,
            current: current.clone(),
        });
    }
    let recorded = journal_steps(&fixture.journal);
    let context = Arc::new(ReplayableRecordingContext::default());
    *context.records.lock_recover() = fixture
        .journal
        .into_iter()
        .map(|entry| (entry.name, entry.body.bytes()))
        .collect();
    context.start_replay();
    Box::pin(drive_scenario(scenario, Arc::clone(&context), true))
        .await
        .map_err(ReplayDivergence)?;
    let current = context.runs();
    if recorded != current {
        return Err(ReplayDivergence(ReplayFailure::Steps { recorded, current }));
    }
    Ok(ReplayComparison::Compared)
}

/// A copy of a committed journal with one more step than this build's
/// handler runs, stamped with this build's generation.
async fn added_step_fixture() -> ReplayCorpusFixture {
    let mut fixture = read_fixture(Scenario {
        name: "scalar-lashlang-tool-attempt",
    });
    fixture.generation = current_generation().await;
    let added = JournalEntry {
        name: "lash:release-journal-added-step".to_string(),
        body: fixture.journal[0].body.clone(),
    };
    fixture.journal.push(added);
    fixture
}

#[tokio::test]
async fn an_added_run_step_with_unchanged_epoch_requires_the_exact_bump_message() {
    let error = replay_fixture(
        Scenario {
            name: "scalar-lashlang-tool-attempt",
        },
        added_step_fixture().await,
        &current_generation().await,
    )
    .await
    .expect_err("an added ctx.run step must diverge");
    assert_eq!(
        error.to_string(),
        "journal logic changed: bump JOURNAL_LOGIC_EPOCH"
    );
}

#[tokio::test]
async fn another_generation_does_not_compare_the_added_step_journal() {
    let other = lash_core::engine::BuildGeneration::for_test("another-build");
    assert_ne!(other, current_generation().await);
    assert_eq!(
        replay_fixture(
            Scenario {
                name: "scalar-lashlang-tool-attempt"
            },
            added_step_fixture().await,
            &other,
        )
        .await
        .expect("another generation is routed separately"),
        ReplayComparison::DifferentGeneration {
            recorded: current_generation().await,
            current: other,
        },
    );
}

/// A service fixture and the journals of a build whose `shift` handler
/// journals one more step than the fixture recorded.
async fn service_fixture_and_an_added_step() -> (ServiceJournalFixture, HandlerJournals) {
    let journal = |steps: &[&str]| {
        BTreeMap::from([(
            "shift".to_string(),
            BTreeSet::from([steps
                .iter()
                .map(|step| step.to_string())
                .collect::<Vec<_>>()]),
        )])
    };
    let fixture = ServiceJournalFixture {
        scenario: service_scenario_name("LashSession"),
        service: "LashSession".to_string(),
        generation: current_generation().await,
        handlers: journal(&["InputCommand", "RunCommand lash.shift.leg", "OutputCommand"]),
    };
    let current = journal(&[
        "InputCommand",
        "RunCommand lash.shift.leg",
        "RunCommand lash:release-journal-added-step",
        "OutputCommand",
    ]);
    (fixture, current)
}

#[tokio::test]
async fn an_added_handler_step_with_unchanged_epoch_requires_the_exact_bump_message() {
    let (fixture, current) = service_fixture_and_an_added_step().await;
    let error = replay_service_fixture(fixture, &current, &current_generation().await)
        .expect_err("an added ctx.run step must diverge");
    assert_eq!(
        error.to_string(),
        "journal logic changed: bump JOURNAL_LOGIC_EPOCH"
    );
}

#[tokio::test]
async fn another_generation_does_not_compare_the_added_handler_step() {
    let (fixture, current) = service_fixture_and_an_added_step().await;
    let other = lash_core::engine::BuildGeneration::for_test("another-service-build");
    assert_ne!(other, fixture.generation);
    assert_eq!(
        replay_service_fixture(fixture, &current, &other)
            .expect("another generation is routed separately"),
        ReplayComparison::DifferentGeneration {
            recorded: current_generation().await,
            current: other,
        },
    );
}

#[test]
fn every_replay_fixture_records_one_generation_and_an_ordered_journal() {
    let fixtures = SCENARIOS
        .iter()
        .map(|scenario| read_fixture(*scenario))
        .collect::<Vec<_>>();
    for fixture in &fixtures {
        assert!(!fixture.journal.is_empty(), "{}", fixture.scenario);
        assert_eq!(
            fixture.generation, fixtures[0].generation,
            "one corpus is one build's journals"
        );
        let mut names = journal_steps(&fixture.journal);
        names.sort();
        names.dedup();
        assert_eq!(
            names.len(),
            fixture.journal.len(),
            "{}: a step journals once",
            fixture.scenario
        );
    }
    for service in lash_service_names() {
        let fixture = read_service_fixture(&service);
        assert_eq!(
            fixture.generation, fixtures[0].generation,
            "{service} is in the same generation"
        );
        assert!(
            !fixture.handlers.is_empty()
                && fixture.handlers.values().all(|journals| {
                    !journals.is_empty() && journals.iter().all(|steps| !steps.is_empty())
                }),
            "{service} records the steps of every handler it ran"
        );
    }
    for scenario in run_scenarios::RUN_SCENARIOS {
        let fixture = read_run_fixture(scenario);
        assert_eq!(fixture.scenario, *scenario);
        assert_eq!(
            fixture.generation, fixtures[0].generation,
            "{scenario} is in the same generation"
        );
        assert!(
            !fixture.services.is_empty()
                && fixture.services.values().all(|journals| {
                    !journals.is_empty()
                        && journals
                            .values()
                            .all(|set| !set.is_empty() && set.iter().all(|steps| !steps.is_empty()))
                }),
            "{scenario} records the steps of every handler it ran"
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
    for scenario in SCENARIOS {
        let context = Arc::new(ReplayableRecordingContext::default());
        Box::pin(drive_scenario(*scenario, Arc::clone(&context), false))
            .await
            .expect("record scenario");
        let fixture = ReplayCorpusFixture {
            scenario: scenario.name.to_string(),
            generation: current_generation().await,
            journal: recorded_journal(&context),
        };
        let path = fixture_path(*scenario);
        std::fs::create_dir_all(path.parent().expect("fixture parent"))
            .expect("create replay corpus scenario directory");
        std::fs::write(path, json_with_newline(&fixture)).expect("write replay corpus fixture");
    }

    let mut recorded = Box::pin(service_journals::record()).await;
    assert_eq!(
        recorded.generation,
        current_generation().await,
        "one corpus uses one core composition"
    );
    for service in lash_service_names() {
        let fixture = ServiceJournalFixture {
            scenario: service_scenario_name(&service),
            generation: recorded.generation.clone(),
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

    for scenario in run_scenarios::RUN_SCENARIOS {
        let fixture = RunScenarioFixture {
            scenario: scenario.to_string(),
            generation: recorded.generation.clone(),
            services: run_scenarios::record(scenario).await,
        };
        let path = run_fixture_path(scenario);
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

fn read_run_fixture(scenario: &str) -> RunScenarioFixture {
    let path = run_fixture_path(scenario);
    serde_json::from_slice(&std::fs::read(&path).unwrap_or_else(|error| {
        panic!(
            "run scenario `{scenario}` has no replay corpus fixture at {}: {error}",
            path.display()
        )
    }))
    .expect("decode committed run scenario fixture")
}

fn run_fixture_path(scenario: &str) -> PathBuf {
    fixture_root().join(scenario).join("journal.json")
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

fn json_with_newline(value: &impl Serialize) -> Vec<u8> {
    let mut json = serde_json::to_vec_pretty(value).expect("encode deterministic fixture JSON");
    json.push(b'\n');
    json
}

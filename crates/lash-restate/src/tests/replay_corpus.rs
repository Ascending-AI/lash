// FIG-2971: this file is test/tooling/host code; ambient fs/env/process
// access is sanctioned here (the workspace clippy ban targets production
// library code).
#![allow(clippy::disallowed_methods)]

use super::*;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

const CORPUS_ROOT_ENV: &str = "LASH_REPLAY_CORPUS_ROOT";
const REGENERATE_ENV: &str = "LASH_REGENERATE_REPLAY_CORPUS";

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
        if super::recording_context::is_process_command_journal_fact(name) {
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

/// The generation the replay compares journals under: the facade's `G` for
/// this build, the same value a deployment registers and routes on.
fn current_generation() -> lash_core::engine::BuildGeneration {
    lash::formats::build_generation()
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

#[tokio::test]
async fn replay_corpus_fixtures_match_current_controller() {
    let fixture_names = fixture_scenario_names();
    let registered_names = SCENARIOS
        .iter()
        .map(|scenario| scenario.name.to_string())
        .collect::<Vec<_>>();
    assert_eq!(
        fixture_names, registered_names,
        "every committed replay fixture must have exactly one registered scenario"
    );

    for scenario in SCENARIOS {
        let fixture = read_fixture(*scenario);
        assert_eq!(fixture.scenario, scenario.name);

        let result = replay_fixture(*scenario, fixture, &current_generation())
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
fn added_step_fixture() -> ReplayCorpusFixture {
    let mut fixture = read_fixture(Scenario {
        name: "scalar-lashlang-tool-attempt",
    });
    fixture.generation = current_generation();
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
        added_step_fixture(),
        &current_generation(),
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
    assert_ne!(other, current_generation());
    assert_eq!(
        replay_fixture(
            Scenario {
                name: "scalar-lashlang-tool-attempt"
            },
            added_step_fixture(),
            &other,
        )
        .await
        .expect("another generation is routed separately"),
        ReplayComparison::DifferentGeneration {
            recorded: current_generation(),
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

#[tokio::test]
#[ignore = "writes committed replay fixtures; set LASH_REGENERATE_REPLAY_CORPUS=1"]
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
            generation: current_generation(),
            journal: recorded_journal(&context),
        };
        let path = fixture_path(*scenario);
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

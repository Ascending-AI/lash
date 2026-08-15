use super::*;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

const REGENERATE_ENV: &str = "LASH_REGENERATE_REPLAY_CORPUS";
const FORMAT_NOTE: &str =
    "lash-restate RecordedRuntimeEffect JSON v1; map keys are Restate effect names";

#[derive(Debug, Serialize, serde::Deserialize)]
struct ReplayCorpusFixture {
    scenario: String,
    recorded_at_git_sha: String,
    format: String,
    records: BTreeMap<String, RecordedRuntimeEffect>,
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
        assert_eq!(fixture.format, FORMAT_NOTE);
        assert!(
            !fixture.recorded_at_git_sha.is_empty(),
            "{} must name the commit it was recorded from",
            scenario.name
        );

        let context = Arc::new(ReplayableRecordingContext::default());
        context.install_recorded_runtime_effects(fixture.records);
        context.start_replay();
        drive_scenario(*scenario, context, true).await;
    }
}

#[tokio::test]
#[ignore = "writes committed replay fixtures; set LASH_REGENERATE_REPLAY_CORPUS=1"]
async fn regenerate_replay_corpus_fixtures() {
    assert_eq!(
        std::env::var(REGENERATE_ENV).as_deref(),
        Ok("1"),
        "set {REGENERATE_ENV}=1 to acknowledge replacing the committed replay corpus"
    );
    let git_sha = recorded_at_git_sha();

    for scenario in SCENARIOS {
        let context = Arc::new(ReplayableRecordingContext::default());
        drive_scenario(*scenario, Arc::clone(&context), false).await;
        let fixture = ReplayCorpusFixture {
            scenario: scenario.name.to_string(),
            recorded_at_git_sha: git_sha.clone(),
            format: FORMAT_NOTE.to_string(),
            records: context.recorded_runtime_effects(),
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
) {
    match scenario.name {
        "sleep-envelope" => drive_sleep_envelope(context, replaying),
        "scalar-lashlang-tool-attempt" => {
            drive_scalar_lashlang_tool_attempt(context, replaying).await;
        }
        other => panic!("unimplemented replay corpus scenario `{other}`"),
    }
}

fn drive_sleep_envelope(context: Arc<ReplayableRecordingContext>, replaying: bool) {
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

    let recorded = context
        .recorded_runtime_effect(&effect_name)
        .unwrap_or_else(|| panic!("missing recorded effect `{effect_name}`"));
    let outcome = validate_recorded_effect_envelope(recorded, &canonical, None)
        .expect("sleep envelope must match the committed recording")
        .expect("recorded sleep must have succeeded");
    assert!(matches!(outcome, RuntimeEffectOutcome::Sleep));
}

async fn drive_scalar_lashlang_tool_attempt(
    context: Arc<ReplayableRecordingContext>,
    replaying: bool,
) {
    let call_id = "replay-scalar-call";
    let tool_name = "replay_scalar_counter";
    let envelope = RuntimeEffectEnvelope::new(
        runtime_invocation(
            RuntimeEffectKind::ToolAttempt,
            "scalar-lashlang-tool-attempt",
        ),
        RuntimeEffectCommand::ToolAttempt {
            call: prepared_tool_call_with(call_id, tool_name),
            execution_grant: None,
            attempt: 1,
            max_attempts: 1,
        },
    );
    let effect_name = restate_effect_name(&envelope.invocation);
    let local_runs = Arc::new(AtomicUsize::new(0));
    let controller = RestateRuntimeEffectController::new(Arc::clone(&context));
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
                    })
                }
            }),
        )
        .await
        .expect("scalar Lashlang tool attempt must match the committed recording");

    let RuntimeEffectOutcome::ToolAttempt { launch, .. } = outcome else {
        panic!("recorded scalar scenario returned the wrong outcome");
    };
    assert!(matches!(
        *launch,
        lash_core::ToolAttemptLaunch::Done { ref record, .. }
            if record.call_id.as_deref() == Some(call_id) && record.tool == tool_name
    ));
    assert_eq!(
        local_runs.load(Ordering::SeqCst),
        usize::from(!replaying),
        "replay must return the journaled scalar ToolAttempt without re-executing it"
    );
    assert_eq!(context.runs(), vec![effect_name]);
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
    Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/replay-corpus")
}

fn recorded_at_git_sha() -> String {
    let output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
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

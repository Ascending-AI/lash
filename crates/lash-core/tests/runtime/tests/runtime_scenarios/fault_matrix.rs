// FIG-2971: this file is test/tooling/host code; ambient fs/env/process
// access is sanctioned here (the workspace clippy ban targets production
// library code).
#![allow(clippy::disallowed_methods)]

#[derive(Clone, Copy, Debug)]
struct CargoTestEvidence {
    package: &'static str,
    test_target: Option<&'static str>,
    filter: Option<&'static str>,
}

#[derive(Clone, Copy, Debug)]
enum FaultEvidence {
    RuntimeScenario,
    CargoTest(CargoTestEvidence),
    Blocked,
}

#[derive(Clone, Copy, Debug)]
struct DurableFaultMatrixRow {
    id: &'static str,
    evidence: FaultEvidence,
}

const DURABLE_FAULT_MATRIX: &[DurableFaultMatrixRow] = &[
    // L9f (FIG-5184) implements its crash-point matrix on the durable
    // harness.
    DurableFaultMatrixRow {
        id: "crash-reopen-runtime-rebuild",
        evidence: FaultEvidence::Blocked,
    },
    DurableFaultMatrixRow {
        id: "duplicate-turn-input-source-key",
        evidence: FaultEvidence::RuntimeScenario,
    },
    DurableFaultMatrixRow {
        id: "provider-retry-exhaustion",
        evidence: FaultEvidence::CargoTest(CargoTestEvidence {
            package: "lash-internal-core",
            test_target: None,
            filter: Some("retryable_llm_failures_exhaust_and_fail_turn"),
        }),
    },
    DurableFaultMatrixRow {
        id: "protocol-provider-failure",
        evidence: FaultEvidence::CargoTest(CargoTestEvidence {
            package: "lash-internal-protocol-standard",
            test_target: Some("protocol_scenarios"),
            filter: Some("standard_protocol_scenario_provider_error_stops_without_checkpoint"),
        }),
    },
    DurableFaultMatrixRow {
        id: "checkpoint-redrive-cancel",
        evidence: FaultEvidence::RuntimeScenario,
    },
    DurableFaultMatrixRow {
        id: "lease-release-advisory",
        evidence: FaultEvidence::RuntimeScenario,
    },
    DurableFaultMatrixRow {
        id: "stale-lease-ttl",
        evidence: FaultEvidence::RuntimeScenario,
    },
    DurableFaultMatrixRow {
        id: "stale-shift-fence-writes-nothing",
        evidence: FaultEvidence::CargoTest(CargoTestEvidence {
            package: "lash-internal-sqlite-store",
            test_target: Some("conformance_memory"),
            filter: Some("a_stale_fence_writes_nothing"),
        }),
    },
    DurableFaultMatrixRow {
        id: "settlement-predicated-on-the-run",
        evidence: FaultEvidence::CargoTest(CargoTestEvidence {
            package: "lash-internal-sqlite-store",
            test_target: Some("conformance_memory"),
            filter: Some("settlement_is_predicated_on_the_run"),
        }),
    },
    DurableFaultMatrixRow {
        id: "trigger-delivery-reserve-start-crash-window",
        evidence: FaultEvidence::CargoTest(CargoTestEvidence {
            package: "lash-internal-sqlite-store",
            test_target: Some("conformance"),
            filter: Some("trigger_delivery_recovery"),
        }),
    },
    DurableFaultMatrixRow {
        id: "trigger-delivery-prune-orphan-retention",
        evidence: FaultEvidence::CargoTest(CargoTestEvidence {
            package: "lash-internal-sqlite-store",
            test_target: Some("conformance"),
            filter: Some("trigger_capture_route_and_compaction_refusal_matrix"),
        }),
    },
    DurableFaultMatrixRow {
        id: "sqlite-backend-conformance",
        evidence: FaultEvidence::CargoTest(CargoTestEvidence {
            package: "lash-internal-sqlite-store",
            test_target: Some("conformance"),
            filter: None,
        }),
    },
    DurableFaultMatrixRow {
        id: "postgres-backend-conformance",
        evidence: FaultEvidence::Blocked,
    },
];

#[test]
fn durable_fault_matrix_fast_gate_executes_all_nonblocked_evidence() {
    let scenario_commands = run_fast_gate_with_fake_cargo("scenario-harnesses");
    assert!(
        scenario_commands.iter().any(|command| {
            command
                == &[
                    "test",
                    "-p",
                    "lash-internal-core",
                    "--locked",
                    "runtime_scenario",
                ]
        }),
        "fast gate must execute RuntimeScenario evidence rows"
    );

    let fault_matrix_commands = run_fast_gate_with_fake_cargo("fault-matrix");
    for row in DURABLE_FAULT_MATRIX {
        match row.evidence {
            FaultEvidence::RuntimeScenario | FaultEvidence::Blocked => {}
            FaultEvidence::CargoTest(evidence) => {
                let command = fault_matrix_commands
                    .iter()
                    .find(|command| command_executes_evidence(command, evidence));
                assert!(
                    command.is_some(),
                    "{} is non-blocked CargoTest evidence but is not executed by scripts/confidence-gate.sh fast:fault-matrix; observed commands: {fault_matrix_commands:?}",
                    row.id
                );
            }
        }
    }
}

/// The Confidence `coverage` and `mutation-packages-rotating` stages export
/// `LASH_CONFIDENCE_STAGE`, and `scripts/confidence-gate.sh` sources
/// `scripts/ci/confidence-stage.sh` whenever it is set; that file exits 2 for
/// any selector but `full`. This probe runs `fast:<shard>`, so an inherited
/// stage selector turned all six routing tests into
/// "Confidence stages require the unscoped full selector" in exactly the two
/// stages that run them, and no Confidence run has completed since the stage
/// split. The gate variables must not reach the child, and scrubbing them must
/// not change what the probe observes.
#[test]
fn durable_fault_matrix_gate_probe_ignores_inherited_confidence_stage_routing() {
    let clean = run_fast_gate_with_fake_cargo("fault-matrix");
    assert!(
        !clean.is_empty(),
        "the fault-matrix shard must record cargo commands"
    );

    let under_a_stage = run_fast_gate_with_fake_cargo_inheriting(
        "fault-matrix",
        &[
            ("LASH_CONFIDENCE_STAGE", "coverage"),
            ("LASH_CONFIDENCE_PACKAGE", "lash-internal-core"),
            ("LASH_SIM_SHARD", "1/9"),
        ],
    );
    assert_eq!(
        under_a_stage, clean,
        "a Confidence stage environment must not change fast:fault-matrix routing"
    );
}

const REAL_CARGO_FILTER_CHUNKS: usize = 5;
// Raising the chunk count must add a matching test and bump this pin.
const _: () = assert!(REAL_CARGO_FILTER_CHUNKS == 5);

#[test]
fn durable_fault_matrix_real_cargo_filters_chunk_0() {
    assert_real_cargo_filter_chunk_selects_tests(0);
}

#[test]
fn durable_fault_matrix_real_cargo_filters_chunk_1() {
    assert_real_cargo_filter_chunk_selects_tests(1);
}

#[test]
fn durable_fault_matrix_real_cargo_filters_chunk_2() {
    assert_real_cargo_filter_chunk_selects_tests(2);
}

#[test]
fn durable_fault_matrix_real_cargo_filters_chunk_3() {
    assert_real_cargo_filter_chunk_selects_tests(3);
}

#[test]
fn durable_fault_matrix_real_cargo_filters_chunk_4() {
    assert_real_cargo_filter_chunk_selects_tests(4);
}

fn assert_real_cargo_filter_chunk_selects_tests(chunk_index: usize) {
    assert!(chunk_index < REAL_CARGO_FILTER_CHUNKS);
    let fault_matrix_commands = run_fast_gate_with_fake_cargo("fault-matrix");
    let mut cargo_evidence_index = 0;

    for row in DURABLE_FAULT_MATRIX {
        let FaultEvidence::CargoTest(evidence) = row.evidence else {
            continue;
        };
        let row_chunk = cargo_evidence_index % REAL_CARGO_FILTER_CHUNKS;
        cargo_evidence_index += 1;
        if row_chunk != chunk_index {
            continue;
        }

        let command = fault_matrix_commands
            .iter()
            .find(|command| command_executes_evidence(command, evidence))
            .unwrap_or_else(|| {
                panic!(
                    "{} is non-blocked CargoTest evidence but is not executed by scripts/confidence-gate.sh fast:fault-matrix; observed commands: {fault_matrix_commands:?}",
                    row.id
                )
            });
        assert_real_cargo_filter_selects_tests(command, row.id);
    }
}

fn assert_real_cargo_filter_selects_tests(command: &[String], row_id: &str) {
    let repo_root = repository_root();
    let output =
        std::process::Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
            .args(command)
            .args(["--", "--list"])
            .current_dir(repo_root)
            .output()
            .unwrap_or_else(|err| panic!("execute real cargo list probe for {row_id}: {err}"));
    assert!(
        output.status.success(),
        "real cargo list probe failed for {row_id}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let selected = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| line.ends_with(": test"))
        .count();
    assert!(
        selected > 0,
        "confidence-gate command for {row_id} selected zero tests under real cargo: {command:?}"
    );
}

#[test]
fn durable_fault_matrix_target_name_is_not_a_test_filter() {
    let evidence = CargoTestEvidence {
        package: "lash-internal-sqlite-store",
        test_target: Some("conformance"),
        filter: Some("conformance"),
    };
    let mut command = [
        "test",
        "-p",
        "lash-internal-sqlite-store",
        "--locked",
        "--test",
        "conformance",
        "trigger_capture_route_and_compaction_refusal_matrix",
    ]
    .map(str::to_string)
    .to_vec();
    assert!(
        !command_executes_evidence(&command, evidence),
        "the integration target name must not satisfy a different test filter"
    );
    command.pop();
    assert!(
        !command_executes_evidence(&command, evidence),
        "an unfiltered target must not satisfy a named test filter"
    );
    assert!(command_executes_evidence(
        &command,
        CargoTestEvidence {
            filter: None,
            ..evidence
        }
    ));
    command.push("conformance".to_string());
    command.push("--locked".to_string());
    assert!(command_executes_evidence(&command, evidence));
}

fn command_executes_evidence(command: &[String], evidence: CargoTestEvidence) -> bool {
    if command.first().map(String::as_str) != Some("test")
        || !command
            .windows(2)
            .any(|pair| pair[0] == "-p" && pair[1] == evidence.package)
        || cargo_test_filter(command) != evidence.filter
    {
        return false;
    }
    evidence.test_target.is_none_or(|target| {
        command
            .windows(2)
            .any(|pair| pair[0] == "--test" && pair[1] == target)
    })
}

fn cargo_test_filter(command: &[String]) -> Option<&str> {
    let mut args = command.iter().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-p" | "--package" | "--test" | "--features" | "--target" | "--profile"
            | "--manifest-path" => {
                args.next();
            }
            "--" => break,
            option if option.starts_with('-') => {}
            filter => return Some(filter),
        }
    }
    None
}

fn run_fast_gate_with_fake_cargo(shard: &str) -> Vec<Vec<String>> {
    run_fast_gate_with_fake_cargo_inheriting(shard, &[])
}

/// `inherited` is applied to the child environment before the gate variables
/// are scrubbed from it. A variable set here and a variable inherited from the
/// parent process land in the same child environment map, so a run that still
/// observes one proves the scrub, and `env_remove` after `env` is what removes
/// it in both cases.
fn run_fast_gate_with_fake_cargo_inheriting(
    shard: &str,
    inherited: &[(&str, &str)],
) -> Vec<Vec<String>> {
    use std::os::unix::fs::PermissionsExt;

    let repo_root = repository_root();
    let temp = tempfile::tempdir().expect("confidence-gate probe tempdir");
    let cargo_dir = temp.path().join(".cargo/bin");
    std::fs::create_dir_all(&cargo_dir).expect("create fake cargo directory");
    let cargo_path = cargo_dir.join("cargo");
    // The recorder is a declared test input, so it is present in a Cargo
    // checkout and in a hermetic action's runfiles alike.
    std::fs::copy(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-cargo.sh"),
        &cargo_path,
    )
    .expect("install fake cargo from fixture");
    let mut permissions = std::fs::metadata(&cargo_path)
        .expect("stat fake cargo")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&cargo_path, permissions).expect("make fake cargo executable");
    let log_path = temp.path().join("cargo.log");
    // The gate refuses to route without a built VM worker, and wraps a
    // PostgreSQL store command in a throwaway database service when it has no
    // database. The probe only records which commands the gate would run and
    // never runs a test: the worker must exist but is never executed, so it
    // points at the recorder, where any execution would show up as an
    // unexpected command, and a stated database keeps every command on the
    // recorder rather than a started service.
    let worker_path = cargo_path.clone();
    let out_dir = temp.path().join("confidence");

    // The gate itself prepends `$HOME/.cargo/bin`; prepending the fixture
    // directory to PATH as well keeps the recorder ahead of any real Cargo on
    // the runner regardless of how the gate resolves it.
    let path = match std::env::var_os("PATH") {
        Some(existing) => {
            let mut entries = vec![cargo_dir.clone()];
            entries.extend(std::env::split_paths(&existing));
            std::env::join_paths(entries).expect("join fake cargo PATH")
        }
        None => cargo_dir.clone().into_os_string(),
    };

    let mut command = std::process::Command::new("bash");
    command
        .arg(repo_root.join("scripts/confidence-gate.sh"))
        .arg(format!("fast:{shard}"))
        .current_dir(repo_root)
        .env("HOME", temp.path());
    for (name, value) in inherited {
        command.env(name, value);
    }
    // The probe executes the gate with a `fast:<shard>` selector. Every
    // Confidence stage exports these, and the gate routes on them: a stage
    // selector makes it source `scripts/ci/confidence-stage.sh`, which refuses
    // anything but the unscoped `full` lane, and the sim variables re-shard a
    // generated lane the probe is not asking for. None of them describe the
    // routing under test, so the child never sees them.
    let output = command
        .env_remove("LASH_CONFIDENCE_STAGE")
        .env_remove("LASH_CONFIDENCE_PACKAGE")
        .env_remove("LASH_SIM_SHARD")
        .env("PATH", path)
        .env("LASH_VM_WORKER", &worker_path)
        .env(
            "LASH_POSTGRES_DATABASE_URL",
            "postgres://fault-matrix-probe.invalid/lash",
        )
        .env("LASH_FAKE_CARGO_LOG", &log_path)
        .env("LASH_CONFIDENCE_OUT_DIR", &out_dir)
        .output()
        .expect("execute confidence gate with fake cargo");
    assert!(
        output.status.success(),
        "confidence gate routing probe failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let log = std::fs::read_to_string(log_path).expect("read fake cargo command log");
    let mut commands = Vec::new();
    let mut current = None;
    for line in log.lines() {
        match line {
            "BEGIN" => current = Some(Vec::new()),
            "END" => commands.push(current.take().expect("command begin before end")),
            arg => current
                .as_mut()
                .expect("command argument inside begin/end")
                .push(arg.to_string()),
        }
    }
    assert!(current.is_none(), "unterminated fake cargo command log");
    commands
}

/// The repository root, both under Cargo (an absolute path two levels above
/// the crate) and under Buck2, where `CARGO_MANIFEST_DIR` is the
/// runfiles-relative package directory and the run is the working directory.
fn repository_root() -> &'static std::path::Path {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("lash-core has repository root two ancestors above");
    if root.as_os_str().is_empty() {
        std::path::Path::new(".")
    } else {
        root
    }
}

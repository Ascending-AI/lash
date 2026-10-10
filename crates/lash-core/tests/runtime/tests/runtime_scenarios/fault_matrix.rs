// FIG-2971: this file is test/tooling/host code; ambient fs/env/process
// access is sanctioned here (the workspace clippy ban targets production
// library code).
#![allow(clippy::disallowed_methods)]

#[derive(Clone, Copy, Debug)]
enum CargoTestTarget {
    Library(&'static str),
    Integration(&'static str),
}

impl CargoTestTarget {
    fn binary_name(self) -> &'static str {
        match self {
            Self::Library(name) | Self::Integration(name) => name,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct CargoTestEvidence {
    package: &'static str,
    target: CargoTestTarget,
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
            package: "lash-runtime",
            target: CargoTestTarget::Library("lash"),
            filter: Some("tests::turn_checkpoints::retryable_llm_failures_exhaust_and_fail_turn"),
        }),
    },
    DurableFaultMatrixRow {
        id: "protocol-provider-failure",
        evidence: FaultEvidence::CargoTest(CargoTestEvidence {
            package: "lash-internal-protocol-standard",
            target: CargoTestTarget::Integration("protocol_scenarios"),
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
        id: "stale-epoch-writes-nothing",
        evidence: FaultEvidence::CargoTest(CargoTestEvidence {
            package: "lash-internal-sqlite-store",
            target: CargoTestTarget::Library("lash_sqlite_store"),
            filter: Some("durable::tests::no_write_through_a_stale_epoch::memory"),
        }),
    },
    DurableFaultMatrixRow {
        id: "terminal-preserves-next-run-mail",
        evidence: FaultEvidence::CargoTest(CargoTestEvidence {
            package: "lash-internal-durable-test",
            target: CargoTestTarget::Integration("steering_crash_laws"),
            filter: Some("a_withheld_steer_killed_at_every_label_runs_once_next_on_sqlite_memory"),
        }),
    },
    DurableFaultMatrixRow {
        id: "sqlite-backend-conformance",
        evidence: FaultEvidence::CargoTest(CargoTestEvidence {
            package: "lash-internal-sqlite-store",
            target: CargoTestTarget::Integration("conformance"),
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
        under_a_stage.as_slice(),
        clean,
        "a Confidence stage environment must not change fast:fault-matrix routing"
    );
}

/// Keep the Cargo-owned probes in one process: nextest runs separate tests
/// in separate processes, so splitting rows repeated gate setup and contended
/// on the same Cargo build lock. Share the unfiltered catalogue as well. Every row still has its own nonempty check.
#[test]
fn durable_fault_matrix_real_cargo_filters_select_tests() {
    let fault_matrix_commands = run_fast_gate_with_fake_cargo("fault-matrix");
    let catalogue = real_cargo_test_catalogue();
    let mut failures = Vec::new();
    for row in DURABLE_FAULT_MATRIX {
        let FaultEvidence::CargoTest(evidence) = row.evidence else {
            continue;
        };
        let command = fault_matrix_commands
            .iter()
            .find(|command| command_executes_evidence(command, evidence));
        match command {
            Some(command) => {
                if let Err(error) =
                    real_cargo_filter_selects_tests(&catalogue, command, evidence, row.id)
                {
                    failures.push(error);
                }
            }
            None => failures.push(format!(
                "{} has no gate command; observed: {fault_matrix_commands:?}",
                row.id
            )),
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// One Cargo-owned catalogue for all rows. Nextest lists libtest's full names
/// once after one workspace build; package and target identity stay attached
/// to each suite, so a name in another crate cannot satisfy a stale command.
fn real_cargo_test_catalogue() -> serde_json::Map<String, serde_json::Value> {
    // Keep workspace feature resolution, but build/list only the evidence
    // binaries. Every test name within those binaries is still catalogued.
    let targets = DURABLE_FAULT_MATRIX
        .iter()
        .filter_map(|row| match row.evidence {
            FaultEvidence::CargoTest(evidence) => Some(format!(
                "(package(={}) & binary(={}))",
                evidence.package,
                evidence.target.binary_name()
            )),
            FaultEvidence::RuntimeScenario | FaultEvidence::Blocked => None,
        })
        .collect::<Vec<_>>()
        .join(" | ");
    let output =
        std::process::Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
            .args([
                "nextest",
                "list",
                "--workspace",
                "--all-targets",
                "--locked",
                "--profile",
                "default",
                "--ignore-default-filter",
                "--message-format",
                "json",
            ])
            .args(["--filterset", &targets])
            .current_dir(repository_root())
            .output()
            .expect("execute real Cargo test catalogue probe");
    assert!(
        output.status.success(),
        "real Cargo test catalogue failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let catalogue: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("nextest's JSON test catalogue");
    catalogue["rust-suites"]
        .as_object()
        .expect("catalogued Rust suites")
        .clone()
}

fn real_cargo_filter_selects_tests(
    catalogue: &serde_json::Map<String, serde_json::Value>,
    command: &[String],
    evidence: CargoTestEvidence,
    row_id: &str,
) -> Result<(), String> {
    let selected = catalogue
        .values()
        .filter(|suite| {
            suite["package-name"].as_str() == Some(evidence.package)
                && suite["binary-name"].as_str() == Some(evidence.target.binary_name())
                && suite["kind"].as_str()
                    == Some(match evidence.target {
                        CargoTestTarget::Library(_) => "lib",
                        CargoTestTarget::Integration(_) => "test",
                    })
        })
        .flat_map(|suite| suite["testcases"].as_object().expect("suite's test names"))
        .filter(|(name, test)| {
            test["kind"].as_str() == Some("test")
                && cargo_test_filter(command).is_none_or(|filter| name.contains(filter))
        })
        .count();
    eprintln!("fault-matrix {row_id}: selected={selected}");
    if selected == 0 {
        return Err(format!(
            "confidence-gate command for {row_id} selected zero tests under real cargo: {command:?}"
        ));
    }
    Ok(())
}

#[test]
fn durable_fault_matrix_target_name_is_not_a_test_filter() {
    let evidence = CargoTestEvidence {
        package: "lash-internal-sqlite-store",
        target: CargoTestTarget::Integration("conformance"),
        filter: Some("conformance"),
    };
    let mut command = [
        "test",
        "-p",
        "lash-internal-sqlite-store",
        "--locked",
        "--test",
        "conformance",
        "settlement_is_predicated_on_the_run",
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
    let library = CargoTestEvidence {
        target: CargoTestTarget::Library("lash_sqlite_store"),
        ..evidence
    };
    assert!(!command_executes_evidence(&command, library));
    command.push("--lib".to_owned());
    assert!(command_executes_evidence(&command, library));
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
    match evidence.target {
        CargoTestTarget::Library(_) => command.iter().any(|arg| arg == "--lib"),
        CargoTestTarget::Integration(target) => command
            .windows(2)
            .any(|pair| pair[0] == "--test" && pair[1] == target),
    }
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

fn run_fast_gate_with_fake_cargo(shard: &str) -> &'static [Vec<String>] {
    use std::sync::OnceLock;

    static SCENARIOS: OnceLock<Vec<Vec<String>>> = OnceLock::new();
    static FAULTS: OnceLock<Vec<Vec<String>>> = OnceLock::new();
    let commands = match shard {
        "scenario-harnesses" => &SCENARIOS,
        "fault-matrix" => &FAULTS,
        _ => panic!("uncatalogued gate probe shard: {shard}"),
    };
    commands.get_or_init(|| run_fast_gate_with_fake_cargo_inheriting(shard, &[]))
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
        .env(concat!("LASH_", "VM_WORKER"), &worker_path)
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

#!/usr/bin/env python3
from __future__ import annotations

import functools
import json
import os
import pathlib
import re
import runpy
import subprocess
import sys
import tempfile
import textwrap
import tomllib
import unittest

import yaml


ROOT = pathlib.Path(__file__).resolve().parent.parent
WORKFLOW = ROOT / ".github" / "workflows" / "ci.yml"
PERF_WORKFLOW = ROOT / ".github" / "workflows" / "perf.yml"
RELEASE_WORKFLOW = ROOT / ".github" / "workflows" / "release.yml"
RELEASE_CACHE_WORKFLOW = ROOT / ".github" / "workflows" / "release-cache.yml"
SEAL_CACHE_WORKFLOW = ROOT / ".github" / "workflows" / "seal-cache.yml"
MOLD_RUSTFLAGS = "-C link-arg=-fuse-ld=mold"
GATE = ROOT / "scripts" / "confidence-gate.sh"
PUSH_GATE = ROOT / "scripts" / "push-gate.sh"
STORE_TESTS = ROOT / "scripts" / "ci" / "store-tests.sh"
FEATURE_COVERAGE = ROOT / "scripts" / "feature-coverage.toml"
LANE_TABLE = ROOT / "tools" / "buck2" / "target-inventory.json"


def feature_lane_table() -> dict[str, list[str]]:
    """The generated lane -> Buck2 label table."""
    return json.loads(LANE_TABLE.read_text(encoding="utf-8"))["feature_lanes"]
PRE_COMMIT_CONFIG = ROOT / ".pre-commit-config.yaml"
PERF_SCENARIOS_RS = ROOT / "crates" / "lash-perf" / "src" / "runtime_perf" / "scenarios.rs"
PERF_PHASE_PROBE_RS = (
    ROOT
    / "crates"
    / "lash-perf"
    / "src"
    / "runtime_perf"
    / "measurement"
    / "phase_probe.rs"
)
CARGO_TOML = ROOT / "Cargo.toml"
JUSTFILE = ROOT / "justfile"
# The two micro lanes (sim unit/oracle + perf-guard identity) share one shard.
FAST_SHARDS = [
    "scenario-harnesses",
    "fault-matrix",
    "sim-unit-perf-guards",
    "sim-generated",
    "minimizer-fixtures",
]


def render_just_recipe(name: str, *arguments: str) -> str:
    """The shebang recipe `name` with `arguments` bound, as `just` would run it.

    The repository-gates job has no `just`, so recipe behaviour is exercised
    from the justfile itself: positional arguments, then the header's quoted
    defaults, fill each `{{parameter}}`. Any other recipe shape fails the
    render instead of running something `just` would not.
    """
    text = JUSTFILE.read_text(encoding="utf-8")
    header = re.search(
        rf"(?m)^{re.escape(name)}((?:[ \t]+\w+='[^']*')*)[ \t]*:[ \t]*\n", text
    )
    if header is None:
        raise AssertionError(f"justfile has no recipe {name} with quoted defaults")
    parameters = re.findall(r"(\w+)='([^']*)'", header[1])
    if len(arguments) > len(parameters):
        raise AssertionError(f"{name} takes at most {len(parameters)} arguments")
    values = dict(parameters) | dict(zip((key for key, _ in parameters), arguments))
    body = []
    for line in text[header.end() :].splitlines():
        if line and not line[0].isspace():
            break
        body.append(line)
    script = textwrap.dedent("\n".join(body)).strip("\n") + "\n"
    if not script.startswith("#!/usr/bin/env bash\n"):
        raise AssertionError(f"{name} is not a bash shebang recipe")
    rendered = re.sub(r"\{\{\s*(\w+)\s*\}\}", lambda match: values[match[1]], script)
    if "{{" in rendered:
        raise AssertionError(f"{name} uses an interpolation the render does not support")
    return rendered


@functools.lru_cache(maxsize=None)
def _store_tests_stub_bin() -> str:
    """A PATH entry that records build commands while running inventory reads."""
    directory = pathlib.Path(tempfile.mkdtemp(prefix="store-tests-stub-"))
    for tool in ("buck2", "cargo"):
        stub = directory / tool
        stub.write_text(
            f'#!/usr/bin/env bash\nprintf "%s\\n" "{tool} $*"\n', encoding="utf-8"
        )
        stub.chmod(0o755)
    hermetic = directory / "hermetic-build"
    hermetic.write_text(
        '#!/usr/bin/env bash\nprintf "%s\\n" "buck2 $*"\n', encoding="utf-8"
    )
    hermetic.chmod(0o755)
    python = directory / "python3"
    python.write_text(
        "#!/usr/bin/env bash\n"
        'if [[ "${1:-}" == "-" ]]; then\n'
        f'  exec "{sys.executable}" "$@"\n'
        "fi\n"
        'printf "%s\\n" "python3 $*"\n',
        encoding="utf-8",
    )
    python.chmod(0o755)
    return str(directory)


@functools.lru_cache(maxsize=None)
def store_suite_branches(suite: str) -> tuple[str, str]:
    """Returns the (Buck2, Cargo) commands one `store-tests.sh` suite renders.

    The service jobs dispatch to that script rather than inlining a command, so
    a pin on a command or a test name has to follow the name into the branch
    that actually runs it -- and has to hold on BOTH branches, because an
    untrusted event (fork or Dependabot PR) gets no cache credentials and takes
    the Cargo half.

    This runs the script with `buck2` and `cargo` stubbed to echo their argv,
    rather than splitting the arm's text on `else`. Uniform suites are
    rendered from one table instead of written twice, so there is no `else` to
    split on -- and reading what the script actually invokes is the stronger
    check for the three shaped arms too: `pg-store` and `s3-store` expand a
    generated label file that text-splitting could only see as `labels s3`.
    """
    rendered = []
    for trusted in ("true", "false"):
        environment = {
            key: value
            for key, value in os.environ.items()
            if key != "GITHUB_ACTIONS"
        }
        environment["PATH"] = (
            f"{_store_tests_stub_bin()}{os.pathsep}{environment['PATH']}"
        )
        environment["BUCK2_TRUSTED"] = trusted
        environment["HERMETIC_BUILD"] = str(
            pathlib.Path(_store_tests_stub_bin()) / "hermetic-build"
        )
        # `scripts/ci/with-service.sh` exports the slot count to every suite
        # it wraps; the sharded PostgreSQL suite refuses to run without it.
        environment["LASH_POSTGRES_SLOT_COUNT"] = "4"
        result = subprocess.run(
            ["bash", str(STORE_TESTS), suite],
            cwd=ROOT,
            env=environment,
            capture_output=True,
            text=True,
            check=True,
        )
        commands = []
        verifier_prefix = "python3 tools/buck2/libtest_selection.py "
        for line in result.stdout.splitlines():
            if line.startswith(verifier_prefix):
                dialect = "buck2" if trusted == "true" else "cargo"
                if not commands or not line.startswith(f"{verifier_prefix}{dialect} "):
                    raise AssertionError(f"unexpected selection verifier: {line}")
                if dialect == "cargo" and not line.endswith(commands[-1]):
                    raise AssertionError(f"verifier lost the Cargo selection: {line}")
                continue
            if not line.startswith(("buck2 test ", "cargo ")):
                raise AssertionError(f"unexpected store suite output: {line}")
            commands.append(line)
        dialect = "buck2" if trusted == "true" else "cargo"
        expected_verifiers = len(commands) if trusted == "true" else sum(
            command.startswith("cargo test ") for command in commands
        )
        actual_verifiers = sum(
            line.startswith(f"{verifier_prefix}{dialect} ")
            for line in result.stdout.splitlines()
        )
        if actual_verifiers != expected_verifiers:
            raise AssertionError(f"{suite} lost selection verification: {result.stdout}")
        rendered.append("\n".join(commands) + "\n")
    return rendered[0], rendered[1]


def store_suite_for_step(step: str) -> str:
    """Reads the suite name out of a `run: bash scripts/ci/store-tests.sh <s>`."""
    match = re.search(r"scripts/ci/store-tests\.sh\s+(\S+)", step)
    if match is None:
        raise AssertionError(f"step does not dispatch to store-tests.sh:\n{step}")
    return match.group(1)


def isolated_store_fixture(directory: pathlib.Path) -> tuple[pathlib.Path, pathlib.Path]:
    """Copy the store dispatcher beside a recording hermetic-build executable."""
    root = directory / "repo"
    script = root / "scripts/ci/store-tests.sh"
    script.parent.mkdir(parents=True)
    script.write_bytes(STORE_TESTS.read_bytes())
    script.chmod(0o755)
    calls = directory / "calls.jsonl"
    driver = root / "scripts/hermetic-build.sh"
    driver.write_text(
        """#!/usr/bin/env python3
import json
import os
import sys

entry = {
    "argv": sys.argv[1:],
    "environment": {
        name: os.environ.get(name)
        for name in (
            "LASH_POSTGRES_DATABASE_URL",
            "LASH_CROSS_BACKEND_CASES",
        )
    },
}
with open(os.environ["STORE_TEST_CALLS"], "a", encoding="utf-8") as output:
    output.write(json.dumps(entry) + "\\n")
""",
        encoding="utf-8",
    )
    driver.chmod(0o755)
    return root, calls


def run_isolated_store_suite(
    root: pathlib.Path,
    calls: pathlib.Path,
    suite: str,
    runner_temp: pathlib.Path,
    extra_environment: dict[str, str],
) -> list[dict]:
    before = calls.read_text(encoding="utf-8").splitlines() if calls.exists() else []
    environment = os.environ | {
        "BUCK2_TRUSTED": "true",
        "RUNNER_TEMP": str(runner_temp),
        "STORE_TEST_CALLS": str(calls),
        "LIBTEST_SELECTION_PYTHON": "true",
    } | extra_environment
    subprocess.run(
        ["bash", str(root / "scripts/ci/store-tests.sh"), suite],
        cwd=root,
        env=environment,
        check=True,
        capture_output=True,
        text=True,
    )
    after = calls.read_text(encoding="utf-8").splitlines()
    return [json.loads(line) for line in after[len(before):]]


# Use CI's executable discovery while checking its coverage independently.
REPOSITORY_GATE_DISCOVERY = runpy.run_path(
    str(ROOT / "scripts/ci/repository_gate_commands.py")
)
SELF_TEST_DIRECTORIES = ("scripts", "tools/buck2", "tools/buck2/tests")
BUCK2_TEST_NAMED_TOOLS = frozenset(
    path for path in REPOSITORY_GATE_DISCOVERY["EXCLUSIONS"]
    if path.startswith("tools/buck2/")
)


def discovered_self_tests() -> list[str]:
    return REPOSITORY_GATE_DISCOVERY["self_test_paths"](ROOT)


def workflow_job_block(workflow: str, job_id: str) -> str:
    marker = f"  {job_id}:\n"
    start = workflow.index(marker)
    next_job = re.search(r"^  [A-Za-z0-9_-]+:\n", workflow[start + len(marker) :], re.MULTILINE)
    if next_job is None:
        return workflow[start:]
    return workflow[start : start + len(marker) + next_job.start()]


def workflow_step_block(job_block: str, step_name: str) -> str:
    marker = f"      - name: {step_name}\n"
    start = job_block.index(marker)
    next_step = re.search(
        r"^      - name: ", job_block[start + len(marker) :], re.MULTILINE
    )
    if next_step is None:
        return job_block[start:]
    return job_block[start : start + len(marker) + next_step.start()]


def shell_function_body(script: str, function_name: str) -> str:
    start_match = re.search(
        rf"^{re.escape(function_name)}\(\) \{{\n", script, re.MULTILINE
    )
    if start_match is None:
        raise AssertionError(f"missing shell function {function_name}")
    next_function = re.search(
        r"^[a-zA-Z_][a-zA-Z0-9_]*\(\) \{\n",
        script[start_match.end() :],
        re.MULTILINE,
    )
    if next_function is None:
        return script[start_match.end() :]
    return script[start_match.end() : start_match.end() + next_function.start()]


def shell_function_definition(script: str, function_name: str) -> str:
    match = re.search(
        rf"^{re.escape(function_name)}\(\) \{{\n.*?^\}}\n",
        script,
        re.MULTILINE | re.DOTALL,
    )
    if match is None:
        raise AssertionError(f"missing shell function {function_name}")
    return match.group(0)


def shell_logical_commands(script: str) -> list[str]:
    commands: list[str] = []
    current = ""
    for line in script.splitlines():
        stripped = line.strip()
        current = f"{current} {stripped}".strip()
        if current.endswith("\\"):
            current = current[:-1].rstrip()
            continue
        if current:
            commands.append(current)
        current = ""
    if current:
        commands.append(current)
    return commands


class ConfidenceGateCiContractTest(unittest.TestCase):


    def test_generated_stage_partitions_full_budget_without_search(self):
        function = shell_function_definition(GATE.read_text(), "run_sim_generated_lane")
        shell = 'step() { :; }\ncargo() { printf "%s\\n" "$@"; }\nrun_sim_search_lane() { echo SEARCH; }\n' + function + '\nlane=full\nout_dir=/tmp/evidence\nrun_sim_generated_lane'
        for shard in range(1, 10):
            env = {k: v for k, v in os.environ.items() if not k.startswith("LASH_SIM_")}
            env.update(LASH_CONFIDENCE_STAGE="generated", LASH_SIM_SHARD=f"{shard}/9")
            result = subprocess.run(["bash", "-euc", shell], env=env, capture_output=True, text=True)
            self.assertEqual(0, result.returncode, result.stderr)
            self.assertEqual(["run", "-p", "lash-sim", "--locked", "--", "run", "--out", "/tmp/evidence/sim",
                              "--profile", "full-random", "--shard", f"{shard}/9"], result.stdout.splitlines())


    def test_seal_cache_writer_mirrors_the_seal_lane(self) -> None:
        # seal-cache.yml is the only main-scoped writer of the seal lane's
        # rust-cache key. rust-cache builds that key from `shared-key`, the
        # `workspaces` target dir, the toolchain and every CARGO*/RUST* env
        # var, so the writer and the reader must agree on all of them or the
        # lane restores nothing and pays the cold trybuild build. The
        # generation suffix is the only way to retire a tree whose shape
        # changed under an unchanged lockfile: rust-cache never re-saves a
        # key it restored in full.
        ci = yaml.safe_load(WORKFLOW.read_text(encoding="utf-8"))
        writer = yaml.safe_load(SEAL_CACHE_WORKFLOW.read_text(encoding="utf-8"))
        lane = next(
            s for s in ci["jobs"]["check"]["steps"]
            if "rust-cache@" in s.get("uses", "")
        )
        (writer_job,) = writer["jobs"].values()
        saver = next(s for s in writer_job["steps"] if "rust-cache@" in s.get("uses", ""))
        self.assertEqual(lane["uses"], saver["uses"])
        # The lane restores only: a save from a pull request lands in that
        # PR's own branch scope, which nothing else can read.
        self.assertIs(False, lane["with"]["save-if"])
        restore = {key: value for key, value in lane["with"].items() if key != "save-if"}
        self.assertEqual(restore, saver["with"])
        self.assertRegex(saver["with"]["shared-key"], r"^linux-seal-[0-9]+$")
        self.assertNotIn("save-if", saver["with"])
        self.assertEqual("${{ github.workspace }}/target-seal", ci["jobs"]["check"]["env"]["CARGO_TARGET_DIR"])
        self.assertEqual(ci["jobs"]["check"]["env"]["CARGO_TARGET_DIR"], writer_job["env"]["CARGO_TARGET_DIR"])
        for name in ("CARGO_TERM_COLOR", "LASH_CI_FEATURES", "RUSTFLAGS"):
            self.assertEqual(ci["env"][name], writer["env"][name], name)


    def test_active_test_callers_use_the_test_interface(self) -> None:
        def obsolete_test_run(source: str) -> re.Match[str] | None:
            shell_continuations_collapsed = source.replace("\\\n", " ")
            return re.search(
                r"\bkiln run\s+[^\n]*:[^\s]*__test\b",
                shell_continuations_collapsed,
            )

        self.assertIsNotNone(
            obsolete_test_run("kiln " + "run \\\n  //pkg:target__test")
        )
        tracked = subprocess.check_output(
            ["git", "ls-files", "-z"], cwd=ROOT
        ).decode().split("\0")
        guidance = [
            ROOT / relative
            for relative in tracked
            if relative
            and (
                relative in {"justfile", ".gitignore"}
                or pathlib.PurePosixPath(relative).suffix
                in {".md", ".py", ".rs", ".sh"}
            )
        ]
        sources = {
            str(path.relative_to(ROOT)): path.read_text(encoding="utf-8")
            for path in guidance
        }
        for path, source in sources.items():
            with self.subTest(path=path):
                self.assertIsNone(
                    obsolete_test_run(source),
                    f"{path} tries to run a Buck2 test wrapper as a binary",
                )

    def test_soak_callers_forward_only_declared_runtime_inputs(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            temporary = pathlib.Path(directory)
            calls = temporary / "calls.jsonl"
            kiln = temporary / "kiln"
            kiln.write_text(
                "#!/usr/bin/env python3\n"
                "import json, os, sys\n"
                "with open(os.environ['KILN_STUB_LOG'], 'a') as output:\n"
                "    output.write(json.dumps(sys.argv[1:]) + '\\n')\n",
                encoding="utf-8",
            )
            kiln.chmod(0o755)
            environment = dict(os.environ)
            environment["PATH"] = str(temporary) + os.pathsep + environment["PATH"]
            environment["KILN_STUB_LOG"] = str(calls)
            environment.pop("LASH_POSTGRES_DATABASE_URL", None)
            for name in (
                "LASH_STORE_CONTRACT_PROPTEST_SEED",
                "LASH_SESSION_GRAPH_PROPTEST_SEED",
                "LASH_CHAOS_SOAK_EPOCHS",
                "LASH_CHAOS_SOAK_STEPS",
            ):
                environment.pop(name, None)

            def run(
                *arguments: str, **extra: str
            ) -> tuple[subprocess.CompletedProcess, list[list[str]]]:
                calls.unlink(missing_ok=True)
                recipe = temporary / "recipe"
                recipe.write_text(render_just_recipe(*arguments), encoding="utf-8")
                recipe.chmod(0o755)
                result = subprocess.run(
                    [str(recipe), *arguments[1:]],
                    cwd=ROOT,
                    env=environment | extra,
                    capture_output=True,
                    text=True,
                    timeout=30,
                )
                rows = (
                    [
                        json.loads(line)
                        for line in calls.read_text(encoding="utf-8").splitlines()
                    ]
                    if calls.exists()
                    else []
                )
                return result, rows

            result, rows = run(
                "store-contract-soak",
                "7",
                LASH_STORE_CONTRACT_PROPTEST_SEED="store-secret-seed",
                LASH_SESSION_GRAPH_PROPTEST_SEED="session-secret-seed",
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(len(rows), 6)
            for arguments in rows:
                self.assertEqual(arguments[0], "test")
                self.assertIn("--test_timeout=1200", arguments)
                self.assertIn("--test_output=all", arguments)
                self.assertIn("--test_arg=--nocapture", arguments)
                self.assertNotIn("--local-test-execution", arguments)
                self.assertNotIn("--no-test-cache", arguments)
                self.assertNotIn("store-secret-seed", arguments)
                self.assertNotIn("session-secret-seed", arguments)
            self.assertEqual(
                sum(
                    "--test_env=LASH_STORE_CONTRACT_PROPTEST_CASES=7" in row
                    for row in rows
                ),
                3,
            )
            self.assertEqual(
                sum(
                    "--test_env=LASH_STORE_CONTRACT_PROPTEST_SEED" in row
                    for row in rows
                ),
                3,
            )
            self.assertEqual(
                sum(
                    "--test_env=LASH_SESSION_GRAPH_PROPTEST_SEED" in row
                    for row in rows
                ),
                3,
            )
            self.assertEqual(
                sum(
                    "--test_env=LASH_SESSION_GRAPH_PROPTEST_CASES=7" in row
                    for row in rows
                ),
                3,
            )

            database = "postgres://fixture.invalid/database"
            result, rows = run("cross-backend-store-soak", "5", "17")
            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(rows, [])
            result, rows = run(
                "cross-backend-store-soak",
                "5",
                "17",
                LASH_POSTGRES_DATABASE_URL=database,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(len(rows), 1)
            for argument in (
                "--local-test-execution",
                "--no-test-cache",
                "--test_env=LASH_POSTGRES_DATABASE_URL",
                "--test_env=LASH_CROSS_BACKEND_CASES=5",
                "--test_env=LASH_CROSS_BACKEND_SEED=17",
                "--test_arg=--include-ignored",
            ):
                self.assertIn(argument, rows[0])
            self.assertNotIn(database, rows[0])

    def test_perf_failure_artifacts_are_attempt_qualified(self) -> None:
        workflow = WORKFLOW.read_text(encoding="utf-8")
        perf_workflow = PERF_WORKFLOW.read_text(encoding="utf-8")
        gate = GATE.read_text(encoding="utf-8")

        self.assertIn("if: always()", perf_workflow)
        self.assertIn(
            "perf-guard-full-attempt-${{ github.run_attempt }}", perf_workflow
        )


    def test_every_script_self_test_is_run_by_ci(self) -> None:
        workflow = WORKFLOW.read_text(encoding="utf-8")
        repo_gates = workflow_job_block(workflow, "repo-gates")
        script_step = workflow_step_block(repo_gates, "Test repository scripts")
        self.assertIn("run-gate-commands.sh --jobs 4 --discover <<'GATES'", script_step)
        runner = (ROOT / "scripts/ci/run-gate-commands.sh").read_text()
        local = (ROOT / "scripts/ci/repository-gates.sh").read_text()
        for entrypoint in (runner, local):
            self.assertIn('python3 "$repo/scripts/ci/repository_gate_commands.py"', entrypoint)
        candidates = {
            path.relative_to(ROOT).as_posix()
            for directory in SELF_TEST_DIRECTORIES
            for pattern in ("test_*.py", "test-*.sh")
            for path in (ROOT / directory).glob(pattern)
            if path.is_file()
        }
        exclusions = REPOSITORY_GATE_DISCOVERY["EXCLUSIONS"]
        self.assertEqual(set(discovered_self_tests()), candidates - exclusions.keys())
        for path, reason in exclusions.items():
            with self.subTest(exclusion=path):
                self.assertIn(path, candidates, "stale exclusion")
                self.assertTrue(reason.strip(), "every exclusion needs a reason")
        self.assertEqual(
            set(exclusions) - BUCK2_TEST_NAMED_TOOLS,
            set(),
        )
        self.assertGreater(len(candidates), 5, "discovery found nothing")

    def test_new_self_tests_execute_without_a_workflow_edit(self) -> None:
        import shutil

        with tempfile.TemporaryDirectory() as raw:
            root = pathlib.Path(raw)
            for directory in SELF_TEST_DIRECTORIES:
                (root / directory).mkdir(parents=True, exist_ok=True)
            (root / "scripts/ci").mkdir()
            for name in ("repository_gate_commands.py", "run-gate-commands.sh", "repository-gates.sh"):
                shutil.copyfile(ROOT / "scripts/ci" / name, root / "scripts/ci" / name)
            (root / ".github/workflows").mkdir(parents=True)
            (root / ".github/workflows/ci.yml").write_text(
                "bash scripts/ci/run-gate-commands.sh --jobs 4 --discover <<'GATES'\n"
                "python3 scripts/check_fixture.py\npython3 scripts/test_foo.py\nGATES\n"
            )
            (root / "scripts/check_fixture.py").write_text("print('fixture check passed')\n")
            new_tests = [f"{directory}/test_foo.py" for directory in SELF_TEST_DIRECTORIES]
            new_tests += ["scripts/test-foo.sh", "scripts/test-agent-workbench-dev-reset.sh"]
            for path in new_tests:
                (root / path).write_text(
                    f"print('{path} executed')\n" if path.endswith(".py")
                    else f"printf '%s\\n' '{path} executed'\n"
                )
            # Running a production tool by mistake must fail this fixture.
            (root / "tools/buck2/test_runner.py").write_text("raise SystemExit(99)\n")
            for arguments, stdin, expected in (
                (["scripts/ci/run-gate-commands.sh", "--jobs", "4", "--discover"],
                 "python3 scripts/check_fixture.py\npython3 scripts/test_foo.py\n", new_tests),
                (["scripts/ci/repository-gates.sh"], "", new_tests[:-1]),
                (["scripts/ci/repository-gates.sh", "--all"], "", new_tests),
            ):
                with self.subTest(entrypoint=arguments):
                    result = subprocess.run(
                        ["bash", *arguments], cwd=root, input=stdin, capture_output=True,
                        text=True, timeout=30,
                    )
                    self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                    self.assertIn("fixture check passed", result.stdout)
                    for path in expected:
                        self.assertIn(f"{path} executed", result.stdout)
                    self.assertEqual(result.stdout.count("scripts/test_foo.py executed"), 1)
                    if "--all" not in arguments and "--discover" not in arguments:
                        self.assertNotIn(f"{new_tests[-1]} executed", result.stdout)
                        self.assertIn("skipped locally", result.stdout)
            (root / "scripts/test_foo.py").write_text("raise SystemExit(7)\n")
            failed = subprocess.run(
                ["bash", "scripts/ci/run-gate-commands.sh", "--jobs", "4", "--discover"],
                cwd=root, input="", capture_output=True, text=True, timeout=30,
            )
            self.assertNotEqual(failed.returncode, 0)
            self.assertIn("python3 scripts/test_foo.py", failed.stderr)
            # A failing discovered test does not suppress the other tests.
            self.assertIn("tools/buck2/tests/test_foo.py executed", failed.stdout)

    def test_push_gate_runs_the_gates_whose_self_tests_it_runs(self) -> None:
        push_gate = PUSH_GATE.read_text(encoding="utf-8")
        pre_commit = PRE_COMMIT_CONFIG.read_text(encoding="utf-8")
        self_tests = shell_function_body(push_gate, "run_release_script_tests")

        # A self-test proves the gate works; only the gate proves the tree does.
        # This script ran `test_check_service_gate_pinning.py` while not running
        # its gate, so the CI-blocking failure was invisible until CI.
        self.assertIn("python3 scripts/check_service_gate_pinning.py", push_gate)

        # Locally a gate may live in this script or in the prek hooks; what it
        # may not do is exist only as a self-test. Comment lines are stripped
        # first: the omissions block below names the CI-only gates, and a gate
        # explained there is precisely one that does not run.
        local = "\n".join(
            line
            for line in f"{push_gate}\n{pre_commit}".splitlines()
            if not line.lstrip().startswith("#")
        )
        for name in re.findall(r"scripts/(test_check_[a-z0-9_]+)\.py", self_tests):
            stem = name[len("test_") :]
            candidates = [
                ROOT / "scripts" / f"{spelling}{suffix}"
                for spelling in (stem, stem.replace("_", "-"))
                for suffix in (".py", ".sh")
            ]
            gates = [path for path in candidates if path.exists()]
            with self.subTest(self_test=name):
                self.assertTrue(gates, f"{name} tests no discoverable gate")
                self.assertTrue(
                    any(f"scripts/{path.name}" in local for path in gates),
                    f"{name} runs here but its gate runs only in CI",
                )

    def test_mutation_packages_bounded_leg_rotates_slices(self) -> None:
        """The leg coordinate plus the run index must pick distinct slices."""
        gate = GATE.read_text(encoding="utf-8")
        shard_fn = shell_function_definition(gate, "mutation_packages_shard")
        smoke_fn = shell_function_definition(gate, "run_mutation_smoke")
        full_fn = shell_function_definition(gate, "run_mutation_full")
        harness = f"""\
set -euo pipefail
{shard_fn}
{smoke_fn}
{full_fn}
area_mutation_file_args=()
MUTATION_PACKAGES_SMOKE_MUTANTS=12
MUTATION_EXCLUDED_TEST_NAME='durable_fault_matrix_real_cargo_filters_select_tests'
declare -A MUTATION_PACKAGES_FULL_MUTANTS=([pkg-x]="5")
MUTATION_PACKAGES_FULL_MUTANTS_DEFAULT=4
selected_packages=(pkg-x)
out_dir="$1"
mutation_jobs=2
step() {{ :; }}
require_tool() {{ :; }}
cargo() {{
  if [[ "$*" == *--list* ]]; then seq 1 23; return 0; fi
}}
run_mutants_recorded() {{ printf 'RECORDED %s\\n' "$*"; }}
run_postgres_mutants_recorded() {{ printf 'PG %s\\n' "$*"; }}
"""
        # 23 mutants at budgets 12 (smoke, denom 2) and 5 (full, denom 5).
        # Two legs plus the run index walk consecutive slices of each space.
        # cargo-mutants counts shards from zero: `--shard k/n` needs k < n, and
        # run 35489555344 lost its single-slice protocol-standard leg to
        # "invalid value '1/1' for '--shard <SHARD>'".
        cases = [
            # (run, leg spec) -> (smoke shard, full shard)
            (1, "1/2", "0/2", "0/5"),
            (1, "2/2", "1/2", "1/5"),
            (2, "1/2", "0/2", "2/5"),
            (2, "2/2", "1/2", "3/5"),
            (3, "1/2", "0/2", "4/5"),
            (3, "2/2", "1/2", "0/5"),
        ]
        for run_index, leg, smoke_shard, full_shard in cases:
            with self.subTest(run=run_index, leg=leg):
                env = dict(
                    os.environ,
                    LASH_MUTATION_PACKAGES_BOUNDED="1",
                    LASH_MUTATION_PACKAGES_SHARD=leg,
                    LASH_MUTATION_RUN_INDEX=str(run_index),
                )
                result = subprocess.run(
                    ["bash", "-c", harness + "\nrun_mutation_smoke\nrun_mutation_full", "t", "/tmp/x"],
                    env=env,
                    capture_output=True,
                    text=True,
                )
                self.assertEqual(0, result.returncode, result.stderr)
                self.assertIn(f"--shard {smoke_shard} ", result.stdout)
                self.assertIn(f"--shard {full_shard} ", result.stdout)

        # A package whose whole space fits one slice judges shard 0 of 1.
        env = dict(
            os.environ,
            LASH_MUTATION_PACKAGES_BOUNDED="1",
            LASH_MUTATION_PACKAGES_SHARD="1/1",
            LASH_MUTATION_RUN_INDEX="7",
        )
        result = subprocess.run(
            [
                "bash",
                "-c",
                harness
                + "\nMUTATION_PACKAGES_SMOKE_MUTANTS=64"
                + "\nrun_mutation_smoke",
                "t",
                "/tmp/x",
            ],
            env=env,
            capture_output=True,
            text=True,
        )
        self.assertEqual(0, result.returncode, result.stderr)
        self.assertIn("--shard 0/1 ", result.stdout)

        # Without the bound flag the full pass stays an unsharded sweep and
        # the smoke canary keeps its historical 1/64 slice; explicit shard
        # selectors still win for local reproduction.
        env = dict(os.environ)
        for name in (
            "LASH_MUTATION_PACKAGES_BOUNDED",
            "LASH_MUTATION_PACKAGES_SHARD",
            "LASH_MUTATION_RUN_INDEX",
            "LASH_MUTATION_SMOKE_SHARD",
            "LASH_MUTATION_FULL_SHARD",
        ):
            env.pop(name, None)
        result = subprocess.run(
            ["bash", "-c", harness + "\nrun_mutation_smoke\nrun_mutation_full", "t", "/tmp/x"],
            env=env,
            capture_output=True,
            text=True,
        )
        self.assertEqual(0, result.returncode, result.stderr)
        self.assertIn("--shard 1/64 ", result.stdout)
        full_line = next(
            line for line in result.stdout.splitlines() if "full mutation" in line
        )
        self.assertNotIn("--shard", full_line)

        env["LASH_MUTATION_PACKAGES_BOUNDED"] = "1"
        env["LASH_MUTATION_FULL_SHARD"] = "3/7"
        result = subprocess.run(
            ["bash", "-c", harness + "\nrun_mutation_full", "t", "/tmp/x"],
            env=env,
            capture_output=True,
            text=True,
        )
        self.assertEqual(0, result.returncode, result.stderr)
        self.assertIn("--shard 3/7 ", result.stdout)

    def test_targeted_mutation_sweeps_select_live_functions(self) -> None:
        """A targeted sweep must still have mutants to judge.

        cargo-mutants exits zero when `--file` and `--re` select nothing, so a
        sweep whose source file moved passes while judging no mutant at all:
        the direct-request and runtime-completion sweeps did after their files
        left `crates/lash-core/src/direct.rs` and `crates/lash-sim/src/runner.rs`.
        """
        gate = GATE.read_text(encoding="utf-8")
        sweeps = [
            command
            for command in shell_logical_commands(gate)
            if "cargo mutants" in command and "--re " in command
        ]
        self.assertGreaterEqual(len(sweeps), 5)
        for command in sweeps:
            name = re.search(r'run_mutants_recorded "([^"]+)"', command).group(1)
            with self.subTest(sweep=name):
                package = re.search(r" -p (\S+)", command).group(1)
                files = re.findall(r"--file (\S+)", command)
                self.assertTrue(files, "a targeted sweep names its files")
                functions: set[str] = set()
                for file in files:
                    path = ROOT / file
                    self.assertTrue(path.is_file(), f"{file} is not a source file")
                    manifest = next(
                        parent / "Cargo.toml"
                        for parent in path.parents
                        if (parent / "Cargo.toml").is_file()
                    )
                    self.assertEqual(
                        package,
                        tomllib.loads(manifest.read_text(encoding="utf-8"))["package"][
                            "name"
                        ],
                        f"{file} is not in the mutated package",
                    )
                    functions.update(
                        re.findall(
                            r"\bfn ([a-z_][a-z0-9_]*)", path.read_text(encoding="utf-8")
                        )
                    )
                pattern = re.search(r"--re '([^']+)'", command).group(1)
                for alternative in pattern.split("|"):
                    function = alternative.rsplit("::", 1)[-1]
                    self.assertTrue(
                        any(function in candidate for candidate in functions),
                        f"{alternative} names no function in {files}",
                    )

    def test_area_mutation_selectors_match_mutated_packages(self) -> None:
        """An area or replay/commit selector must still have mutants to judge.

        cargo-mutants exits zero when its `--file` filters select nothing, so
        a selector that names a file that moved out of the mutated package —
        or out of the tree — passes while judging no mutants. The kernel
        split left every area sweep and the replay/commit lane pointing at
        `crates/lash-core/src/...` paths that no longer exist.
        """
        package_of: dict[pathlib.Path, str] = {}

        def owning_package(path: pathlib.Path) -> str:
            for parent in path.parents:
                manifest = parent / "Cargo.toml"
                if manifest.is_file():
                    if manifest not in package_of:
                        package_of[manifest] = tomllib.loads(
                            manifest.read_text(encoding="utf-8")
                        )["package"]["name"]
                    return package_of[manifest]
            raise AssertionError(f"{path} is not inside a package")

        workspace_packages = {
            owning_package(path)
            for path in ROOT.glob("crates/*/Cargo.toml")
        }
        lanes = {
            "full+area:store",
            "full+area:process",
            "full+area:effect-host",
            "full+area:protocol",
            "full+area:provider",
            "full+area:sim",
            "mutation",
        }
        for selector in sorted(lanes):
            with self.subTest(selector=selector):
                plan = subprocess.run(
                    ["bash", str(GATE), "--dry-run", selector],
                    cwd=ROOT,
                    capture_output=True,
                    text=True,
                    check=False,
                )
                self.assertEqual(0, plan.returncode, plan.stderr)
                if selector == "mutation":
                    package_line = re.search(
                        r"replay/commit mutations for ([^:]*):",
                        plan.stdout,
                    )
                else:
                    package_line = re.search(
                        r"area:\S+ targeted shard for: (.*)",
                        plan.stdout,
                    )
                self.assertIsNotNone(
                    package_line,
                    f"{selector} dry run does not name the packages it mutates",
                )
                packages = package_line.group(1).split()
                self.assertTrue(packages, f"{selector} mutates no package")
                if selector == "mutation":
                    # cargo-mutants counts shards from zero and refuses k/n
                    # unless k < n, so the whole-space shard is 0/1.
                    self.assertIn(
                        "LASH_AREA_MUTATION_SHARD=0/1",
                        GATE.read_text(encoding="utf-8"),
                    )
                for package in packages:
                    self.assertIn(
                        package,
                        workspace_packages,
                        f"{selector} mutates {package}, which is not a workspace package",
                    )
                filters = re.findall(r"--file (\S+)", plan.stdout)
                matched_packages: set[str] = set()
                for pattern in filters:
                    matches = [
                        path
                        for path in ROOT.glob(pattern)
                        if path.is_file()
                    ]
                    self.assertTrue(
                        matches, f"{selector} --file {pattern} matches no source file"
                    )
                    for match in matches:
                        owner = owning_package(match)
                        self.assertIn(
                            owner,
                            packages,
                            f"{selector} --file {pattern} matches {match} in "
                            f"{owner}, which the lane does not mutate",
                        )
                        matched_packages.add(owner)
                for package in packages:
                    if filters:
                        self.assertIn(
                            package,
                            matched_packages,
                            f"{selector} mutates {package} but no --file "
                            "selector names its source",
                        )


    def test_mutation_package_loops_skip_the_real_cargo_fault_matrix_probes(self) -> None:
        """The batched fault-matrix probe builds a real Cargo test catalogue and alone
        exceed the baseline's per-test timeout; every package-scoped
        cargo-mutants run must forward the same skip the Buck2 targets use."""
        gate = GATE.read_text(encoding="utf-8")
        self.assertIn(
            "MUTATION_EXCLUDED_TEST_NAME='durable_fault_matrix_real_cargo_filters_select_tests'",
            gate,
        )
        for function in (
            "run_mutation_smoke",
            "run_mutation_full",
            "run_area_targeted_mutation_evidence",
        ):
            body = shell_function_body(gate, function)
            invocations = body.count("cargo mutants")
            self.assertGreater(invocations, 0, function)
            self.assertEqual(
                invocations,
                body.count('-- -- --skip="${MUTATION_EXCLUDED_TEST_NAME}"'),
                f"{function}: every cargo-mutants call must skip the real-Cargo "
                "fault-matrix probes",
            )

        smoke_fn = shell_function_definition(gate, "run_mutation_smoke")
        harness = f"""\
set -euo pipefail
{smoke_fn}
MUTATION_PACKAGES_SMOKE_MUTANTS=4
MUTATION_EXCLUDED_TEST_NAME='durable_fault_matrix_real_cargo_filters_select_tests'
selected_packages=(pkg-x)
area_mutation_file_args=()
out_dir="$1"
mutation_jobs=2
step() {{ :; }}
require_tool() {{ :; }}
cargo() {{ :; }}
run_mutants_recorded() {{ printf 'RECORDED %s\\n' "$*"; }}
"""
        env = dict(os.environ, LASH_MUTATION_SMOKE_SHARD="1/2")
        result = subprocess.run(
            ["bash", "-c", harness + "\nrun_mutation_smoke", "t", "/tmp/x"],
            env=env,
            capture_output=True,
            text=True,
        )
        self.assertEqual(0, result.returncode, result.stderr)
        self.assertIn(
            "-- -- --skip=durable_fault_matrix_real_cargo_filters_select_tests",
            result.stdout,
        )

    def test_panic_gate_scans_ordinary_logs_but_not_mutants_out(self) -> None:
        """A caught mutant's own log legitimately contains `panicked at`; the
        gate must fail on panic markers only outside `mutants.out/`."""
        gate = GATE.read_text(encoding="utf-8")
        function = shell_function_definition(gate, "assert_no_panics_in_artifacts")
        harness = f"{function}\nout_dir=\"$1\"\nassert_no_panics_in_artifacts"
        with tempfile.TemporaryDirectory() as tmp:
            out_dir = pathlib.Path(tmp) / "out"
            (out_dir / "mutants.out" / "log").mkdir(parents=True)
            (out_dir / "mutants.out" / "log" / "mutant.log").write_text(
                "thread '<unnamed>' panicked at crates/pkg/src/lib.rs:1:1\n"
            )
            clean = subprocess.run(
                ["bash", "-c", harness, "t", str(out_dir)],
                capture_output=True,
                text=True,
            )
            self.assertEqual(0, clean.returncode, clean.stderr)
            self.assertIn("panic gate: clean", clean.stdout)

            (out_dir / "sim-evidence.log").write_text(
                "panicked at crates/lash-sim/src/runner.rs:9:3\n"
            )
            dirty = subprocess.run(
                ["bash", "-c", harness, "t", str(out_dir)],
                capture_output=True,
                text=True,
            )
            self.assertEqual(1, dirty.returncode, dirty.stderr)
            self.assertIn("panic gate: FAILED", dirty.stderr)

    def test_runtime_release_publishes_sdk_without_host_assets(self) -> None:
        workflow = RELEASE_WORKFLOW.read_text(encoding="utf-8")

        publish = workflow_job_block(workflow, "publish")
        publish_crates = workflow_job_block(workflow, "publish-crates")
        worker_artifacts = workflow_job_block(workflow, "worker-artifacts")
        validate_release = workflow_job_block(workflow, "validate-release-ref")

        self.assertNotIn("build-release-assets", workflow)
        self.assertNotIn("install_lash.sh", workflow)
        self.assertIn("needs: [prepare-release, publish-crates, worker-artifacts]", publish)
        worker_artifacts = workflow_job_block(workflow, "worker-artifacts")
        self.assertIn("ref: ${{ needs.prepare-release.outputs.release_sha }}", worker_artifacts)
        self.assertIn('release_version.py stamp "${RELEASE_TAG#v}"', worker_artifacts)
        self.assertIn(
            "kiln build --config=optimized --materializations=final",
            worker_artifacts,
        )
        self.assertEqual("ubuntu-24.04", yaml.safe_load(worker_artifacts)["worker-artifacts"]["runs-on"])
        self.assertIn("name: sdk-worker-linux", worker_artifacts)
        self.assertIn("python3 scripts/package_vm_worker.py", worker_artifacts)
        self.assertIn("pattern: sdk-worker-*", publish)
        release_assets = yaml.safe_load(workflow)["jobs"]["publish"]["steps"][-1]["with"]
        self.assertEqual(
            ["worker-artifacts/*.tar.gz", "worker-artifacts/*.sha256"],
            release_assets["files"].splitlines(),
        )
        self.assertIn(
            "needs: [prepare-release, validate-release-preconditions, validate-release-ref, release-postgres, package-crates, latency-gate]",
            publish_crates,
        )
        self.assertIn("runs-on: ubuntu-24.04", validate_release)
        self.assertIn(
            "ref: ${{ needs.prepare-release.outputs.release_sha }}", validate_release
        )
        self.assertIn(
            "ref: ${{ needs.prepare-release.outputs.release_sha }}", publish_crates
        )
        self.assertIn("ref: ${{ needs.prepare-release.outputs.release_sha }}", publish)
        self.assertIn('head_sha="$(git rev-parse HEAD)"', publish_crates)
        self.assertIn('head_sha="$(git rev-parse HEAD)"', publish)
        self.assertIn(
            "profile_runtime.py --profile full --release --scenario all "
            "--enforce-budgets",
            validate_release,
        )

    def test_full_perf_is_release_gated_and_only_manually_dispatchable(self) -> None:
        release_cache_workflow = RELEASE_CACHE_WORKFLOW.read_text(encoding="utf-8")
        perf = PERF_WORKFLOW.read_text(encoding="utf-8")
        release = RELEASE_WORKFLOW.read_text(encoding="utf-8")
        release_cache = workflow_job_block(release_cache_workflow, "linux-release-cache")

        self.assertIn("workflow_dispatch:", perf)
        # The full profile is dispatched manually before release work; it
        # never runs on a schedule, push or pull_request.
        self.assertNotIn("schedule:", perf)
        self.assertNotIn("pull_request:", perf)
        self.assertNotIn("push:", perf)
        self.assertIn("runs-on: ubuntu-24.04", perf)
        self.assertIn(
            "uses: Swatinem/rust-cache@"
            "6323deb102c322ba6fcbdcafc7e3dddab59af2b6 # v2.9.2",
            release_cache,
        )
        self.assertIn("cargo build --locked --release --workspace", release_cache)
        self.assertNotIn("--target x86_64-unknown-linux-gnu", release_cache)
        command = "profile_runtime.py --profile full --release --scenario all --enforce-budgets"
        self.assertIn(command, perf)
        self.assertIn(command, release)

        self.assertIn("image: postgres:18-alpine", perf)
        self.assertRegex(
            release,
            r"image: postgres@sha256:[0-9a-f]{64} # postgres:18-alpine",
        )
        for workflow_with_postgres in (perf, release):
            self.assertIn("LASH_POSTGRES_DATABASE_URL:", workflow_with_postgres)

        # These two legs provision PostgreSQL for one
        # consumer, the store-hardening perf scenario, which refuses to run
        # without the URL instead of degrading to a SQLite-only measurement.
        # Pin both halves — a deleted scenario would leave the service
        # decorative, and a softened refusal would turn a missing database into
        # a green perf report with silently fewer phases. The refusal lives in
        # the scenario dispatch (phase_probe.rs), which errors unconditionally
        # for store-hardening when no URL is configured.
        perf_scenarios = PERF_SCENARIOS_RS.read_text(encoding="utf-8")
        perf_phase_probe = PERF_PHASE_PROBE_RS.read_text(encoding="utf-8")
        self.assertIn('"store_hardening_hot_paths"', perf_scenarios)
        self.assertIn(
            "requires LASH_POSTGRES_DATABASE_URL or DATABASE_URL "
            "(the full perf workflows provide it)",
            perf_phase_probe,
        )
        self.assertIn(
            "RuntimePerfScenario::StoreHardeningHotPaths => {\n"
            "            let postgres_database_url = "
            "configured_postgres_database_url().ok_or_else(|| {",
            perf_phase_probe,
        )

    def test_secret_bearing_workflows_pin_external_actions_by_sha(self) -> None:
        for path in (RELEASE_WORKFLOW,):
            workflow = path.read_text(encoding="utf-8")
            actions = re.findall(
                r"^\s+uses: ([A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+)@([^\s#]+)"
                r"(?:\s+#\s*(\S.*))?$",
                workflow,
                flags=re.MULTILINE,
            )
            self.assertGreater(len(actions), 0, path.name)
            for action, ref, version_comment in actions:
                with self.subTest(workflow=path.name, action=action):
                    self.assertRegex(ref, r"^[0-9a-f]{40}$")
                    self.assertRegex(
                        version_comment or "",
                        r"^(?:v[0-9]+(?:\.[0-9]+){0,2}|stable)$",
                    )


    def test_confidence_selector_vocabulary_and_area_plans_are_executable(self) -> None:
        gate = GATE.read_text(encoding="utf-8")

        for snippet in (
            "fast+area:store",
            "full+area:effect-host",
            "fast:fault-matrix+area:process",
            "sim-search:<i>/<n>",
            "store, process, effect-host, protocol, provider, sim",
        ):
            self.assertIn(snippet, gate)

        with tempfile.TemporaryDirectory() as directory:
            env = {"LASH_CONFIDENCE_OUT_DIR": directory}
            store_plan = subprocess.run(
                ["bash", str(GATE), "--dry-run", "fast+area:store"],
                cwd=ROOT,
                env={**os.environ, **env},
                check=False,
                capture_output=True,
                text=True,
            )
            self.assertEqual(store_plan.returncode, 0, store_plan.stderr)
            self.assertIn("Area: store", store_plan.stdout)
            self.assertIn("store contracts", store_plan.stdout)
            self.assertNotIn("runtime persistence", store_plan.stdout)
            self.assertEqual(list(pathlib.Path(directory).iterdir()), [])

            shard_plan = subprocess.run(
                [
                    "bash",
                    str(GATE),
                    "sim-search:3/9+area:sim",
                    "--dry-run",
                ],
                cwd=ROOT,
                env={**os.environ, **env},
                check=False,
                capture_output=True,
                text=True,
            )
            self.assertEqual(shard_plan.returncode, 0, shard_plan.stderr)
            self.assertIn("search shard 3/9", shard_plan.stdout)

            for selector in (
                "default+area:trigger",
                "broad+area:trigger",
                "full+area:trigger",
                "fast+area:trigger",
                "fast:fault-matrix+area:trigger",
                "fast+area:unknown",
            ):
                with self.subTest(selector=selector):
                    invalid = subprocess.run(
                        ["bash", str(GATE), "--dry-run", selector],
                        cwd=ROOT,
                        env={**os.environ, **env},
                        check=False,
                        capture_output=True,
                        text=True,
                    )
                    self.assertEqual(invalid.returncode, 2)
                    self.assertIn(
                        f"Unknown confidence area '{selector.split('area:')[1]}'.",
                        invalid.stderr,
                    )
                    self.assertIn(
                        "Areas: store, process, effect-host, protocol, provider, sim",
                        invalid.stderr,
                    )
                    self.assertEqual(invalid.stdout, "")
            self.assertEqual(list(pathlib.Path(directory).iterdir()), [])

    def test_model_replay_empty_corpus_writes_failed_verdict_and_exits_nonzero(
        self,
    ) -> None:
        gate = GATE.read_text(encoding="utf-8")
        replay_suite = shell_function_body(gate, "run_model_replay_suite")
        harness = f"""\
set -euo pipefail
out_dir="$1"
step() {{ :; }}
run_model_replay_suite() {{
{replay_suite}
run_model_replay_suite
"""
        with tempfile.TemporaryDirectory() as directory:
            completed = subprocess.run(
                ["bash", "-c", harness, "model-replay-contract", directory],
                check=False,
                capture_output=True,
                text=True,
            )
            summary_path = (
                pathlib.Path(directory) / "sim" / "model-replay" / "summary.json"
            )
            self.assertEqual(
                completed.returncode,
                1,
                f"empty replay corpus did not fail:\n{completed.stdout}\n{completed.stderr}",
            )
            summary = json.loads(summary_path.read_text(encoding="utf-8"))
            self.assertEqual(summary["status"], "failed")
            self.assertEqual(summary["row_count"], 0)

    def test_seeded_mutation_failure_writes_failed_verdict_and_exits_nonzero(
        self,
    ) -> None:
        gate = GATE.read_text(encoding="utf-8")
        recorded = shell_function_body(gate, "run_mutants_recorded")
        finalize = shell_function_body(gate, "finalize_mutation_gate")
        harness = f"""\
set -euo pipefail
out_dir="$1"
mutation_scope="smoke"
mutation_failures=0
write_mutation_evidence_summary() {{ :; }}
write_confidence_summary() {{ printf '%s\\n' "$1" >"${{out_dir}}/summary-verdict"; }}
run_mutants_recorded() {{
{recorded}
finalize_mutation_gate() {{
{finalize}
seeded_mutation_failure() {{
  if [ "${{CARGO_TARGET_DIR:-}}" != "target" ]; then
    return 3
  fi
  return 2
}}
export CARGO_TARGET_DIR="deliberately-shared-target"
run_mutants_recorded \
  "seeded survivor" \
  "${{out_dir}}/seeded-mutant" \
  seeded_mutation_failure
finalize_mutation_gate
"""
        with tempfile.TemporaryDirectory() as directory:
            completed = subprocess.run(
                ["bash", "-c", harness, "mutation-contract", directory],
                check=False,
                capture_output=True,
                text=True,
            )
            artifact = pathlib.Path(directory) / "seeded-mutant"
            command_status = json.loads(
                (artifact / "confidence-status.json").read_text(encoding="utf-8")
            )
            self.assertEqual(command_status["status"], "failed")
            self.assertEqual(command_status["exit_code"], 2)
            self.assertEqual(
                completed.returncode,
                1,
                f"seeded mutation failure did not fail:\n{completed.stdout}\n{completed.stderr}",
            )
            self.assertEqual(
                (pathlib.Path(directory) / "summary-verdict").read_text(
                    encoding="utf-8"
                ),
                "failed\n",
            )

    def test_durable_stores_are_critical_coverage_and_mutation_packages(self) -> None:
        workflow = WORKFLOW.read_text(encoding="utf-8")
        gate = GATE.read_text(encoding="utf-8")

        critical_packages = gate.split("critical_packages=(", 1)[1].split(")", 1)[0]
        self.assertIn("lash-internal-sqlite-store", critical_packages)
        self.assertIn("lash-internal-postgres-store", critical_packages)
        for function_name in ("run_mutation_smoke", "run_mutation_full"):
            body = shell_function_body(gate, function_name)
            loop_headers = re.findall(
                r"^\s*for\s+package\s+in\s+(.+);\s*do\s*$", body, re.MULTILINE
            )
            self.assertEqual(['"${selected_packages[@]}"'], loop_headers)
            self.assertIn(
                'if [ "$package" = "lash-internal-postgres-store" ]; then', body
            )
            self.assertIn("run_postgres_mutants_recorded", body)

        postgres_mutation = shell_function_body(
            gate, "run_postgres_mutants_recorded"
        )
        self.assertIn('start_mutation_postgres "$artifact"', postgres_mutation)
        self.assertIn(
            'LASH_POSTGRES_DATABASE_URL="$mutation_postgres_database_url"',
            postgres_mutation,
        )
        self.assertIn("LASH_POSTGRES_DATABASE_URL", postgres_mutation)
        self.assertIn('"$@" --jobs "$mutation_jobs"', postgres_mutation)

        derive_jobs = shell_function_body(gate, "derive_mutation_jobs")
        harness = f"""\
set -euo pipefail
out_dir="$(mktemp -d)"
derive_mutation_jobs() {{
{derive_jobs}
[ "$(derive_mutation_jobs 1)" = 1 ]
[ "$(derive_mutation_jobs 4)" = 2 ]
[ "$(derive_mutation_jobs 8)" = 4 ]
[ "$(derive_mutation_jobs 32)" = 4 ]
"""
        completed = subprocess.run(
            ["bash", "-c", harness],
            check=False,
            capture_output=True,
            text=True,
        )
        self.assertEqual(
            completed.returncode,
            0,
            f"machine-derived mutation job contract failed:\n"
            f"{completed.stdout}\n{completed.stderr}",
        )
        self.assertEqual(gate.count('local jobs="${LASH_MUTATION_JOBS:-2}"'), 0)

        coverage_body = shell_function_body(gate, "run_coverage_blind_spots")
        coverage_loops = re.findall(
            r"^\s*for\s+package\s+in\s+(.+);\s*do\s*$",
            coverage_body,
            re.MULTILINE,
        )
        self.assertEqual(['"${selected_packages[@]}"'], coverage_loops)
        self.assertIn('coverage_package_args+=(-p "$package")', coverage_body)
        self.assertIn(
            '''critical_package_regex="$(IFS='|'; printf '%s' "${selected_packages[*]}")"''',
            coverage_body,
        )
        self.assertIn(
            'awk -v critical_package_regex="$critical_package_regex"',
            coverage_body,
        )
        self.assertIn(
            'file ~ ("/crates/(" critical_package_regex ")/")',
            coverage_body,
        )
        self.assertIn('if [ "$lane" = "full" ]; then', gate)
        self.assertNotIn("scripts/confidence-gate.sh default", workflow)

    def test_every_gate_postgres_container_preloads_pg_stat_statements(self) -> None:
        """The statement-count tests measure through the extension.

        `pg_stat_statements` only exists when it is preloaded at server start,
        so the flag belongs to the container. One shared starter means a later
        container cannot be added without it.
        """
        gate = GATE.read_text(encoding="utf-8")

        definition = shell_function_definition(gate, "start_gate_postgres")
        self.assertIn("-c shared_preload_libraries=pg_stat_statements", definition)
        self.assertIn('bash scripts/docker-pull-with-retry.sh "$gate_postgres_image"', definition)
        self.assertIn('gate_postgres_image="postgres:18-alpine"', gate)

        # `start_gate_postgres` is the only thing in the gate that starts one.
        docker_runs = [
            command
            for command in shell_logical_commands(gate)
            if command.startswith("docker run")
        ]
        self.assertEqual(len(docker_runs), 1)
        self.assertIn('"$gate_postgres_image"', docker_runs[0])

        starts = [
            line.strip()
            for line in gate.splitlines()
            if line.strip().startswith("start_gate_postgres ")
        ]
        self.assertEqual(len(starts), 4, starts)

        # The wrapper CI uses starts the same image the same way.
        wrapper = (ROOT / "scripts" / "ci" / "with-service.sh").read_text(encoding="utf-8")
        self.assertIn("shared_preload_libraries=pg_stat_statements", wrapper)

    def test_buck2_store_runtime_flags_and_reports_are_forwarded(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            temporary = pathlib.Path(directory)
            root, calls = isolated_store_fixture(temporary)
            runner_temp = temporary / "runner-temp"
            environment = {
                "LASH_POSTGRES_DATABASE_URL": "postgres://fixture/db",
                "LASH_CROSS_BACKEND_CASES": "4",
            }
            first, = run_isolated_store_suite(
                root, calls, "pg-cross-backend", runner_temp, environment
            )
            second, = run_isolated_store_suite(
                root, calls, "pg-cross-backend", runner_temp, environment
            )
            s3_environment = {
                "LASH_REQUIRE_S3": "1",
                "LASH_S3_ENDPOINT": "http://127.0.0.1:9000",
                "LASH_S3_REGION": "fixture-region",
                "LASH_S3_BUCKET": "fixture-bucket",
                "LASH_S3_ACCESS_KEY": "fixture-access",
                "LASH_S3_SECRET_KEY": "fixture-secret",
            }
            s3, = run_isolated_store_suite(
                root, calls, "s3-attachment-differential", runner_temp,
                s3_environment,
            )

        argv = first["argv"]
        forwarded = {
            argv[index + 1]
            for index, argument in enumerate(argv[:-1])
            if argument == "--test_env"
        }
        self.assertGreaterEqual(
            forwarded,
            {
                "LASH_POSTGRES_DATABASE_URL",
                "LASH_CROSS_BACKEND_CASES",
            },
        )
        self.assertEqual(first["environment"], environment)
        self.assertIn("--test_output=all", argv)
        s3_argv = s3["argv"]
        s3_forwarded = {
            s3_argv[index + 1]
            for index, argument in enumerate(s3_argv[:-1])
            if argument == "--test_env"
        }
        self.assertGreaterEqual(s3_forwarded, set(s3_environment))

        def report_directories(call: dict) -> set[pathlib.Path]:
            arguments = call["argv"]
            return {
                pathlib.Path(arguments[arguments.index(option) + 1]).parent
                for option in (
                    "--build-report",
                    "--test-report",
                    "--test-output-dir",
                    "--event-log",
                )
            }

        first_directories = report_directories(first)
        second_directories = report_directories(second)
        self.assertEqual(len(first_directories), 1)
        self.assertEqual(len(second_directories), 1)
        first_directory, = first_directories
        second_directory, = second_directories
        self.assertNotEqual(first_directory, second_directory)
        self.assertEqual(
            first_directory.parent,
            runner_temp / "store-test-results",
        )
        self.assertEqual(
            second_directory.parent,
            runner_temp / "store-test-results",
        )
        workflow = yaml.safe_load(WORKFLOW.read_text(encoding="utf-8"))
        for job_name in ("postgres-store", "postgres-store-synthetic-next", "s3-store"):
            uploads = [
                step
                for step in workflow["jobs"][job_name]["steps"]
                if "upload-artifact@" in step.get("uses", "")
                and step["with"]["name"].startswith("buck2-reports-")
            ]
            self.assertEqual(len(uploads), 1, job_name)
            self.assertEqual(
                uploads[0]["with"]["path"],
                "${{ runner.temp }}/store-test-results/",
            )

    def test_store_suites_state_one_selection_in_both_dialects(self) -> None:
        """Each suite's test selection is written once, rendered twice.

        Nine `case` arms used to encode the selection twice, with the flag
        translation (`--run-ignored all` is `--include-ignored`, `-j1` is
        `--test-threads=1`) written as a comment. Parity was hand-asserted for
        three arms and "both halves non-empty" for the rest.

        Every workflow suite participates, including newly registered suites.
        Four keep explicit arms because their shape varies (`pg-store`,
        `pg-store-synthetic-next` and `s3-store` take generated inventory
        labels rather than one label, and the release gate's `pg-release`
        runs several selections and labels).
        """
        script = STORE_TESTS.read_text(encoding="utf-8")
        workflow = WORKFLOW.read_text(encoding="utf-8") + RELEASE_WORKFLOW.read_text(
            encoding="utf-8"
        )

        table_body = script.split("declare -A uniform_store_suites=(\n", 1)[1]
        table_body = table_body.split("\n)\n", 1)[0]
        uniform = dict(
            re.findall(r'^\s*\[([^\]]+)\]="([^"]*)"$', table_body, re.MULTILINE)
        )
        shaped = set(re.findall(r"^  ([a-z0-9-]+)\)$", script, re.MULTILINE))
        suites = set(uniform) | shaped
        dispatched = set(
            re.findall(r"bash scripts/ci/store-tests\.sh ([a-z0-9-]+)", workflow)
        )

        self.assertTrue(uniform)
        self.assertEqual(
            {
                "pg-release",
                "pg-store",
                "pg-store-synthetic-next",
                "s3-store",
            },
            shaped,
        )
        self.assertEqual(suites, dispatched)
        # A suite cannot be in both halves, or the table would be shadowed.
        self.assertEqual(set(), set(uniform) & shaped)

        flag_dialects = {
            "include-ignored": ("--test_arg=--include-ignored", ("--run-ignored all", "--include-ignored")),
            "ignored-only": ("--test_arg=--ignored", ("--run-ignored ignored-only", " --ignored")),
            "single-threaded": ("--test_arg=--test-threads=1", ("-j1", "--test-threads=1")),
            "nocapture": ("--test_arg=--nocapture", ("--no-capture", "--nocapture")),
        }

        # Parity for every suite, uniform or shaped: both halves render a real
        # command. The stubs make this the command the script would have run.
        for suite in sorted(suites):
            with self.subTest(suite=suite):
                buck2, cargo = store_suite_branches(suite)
                commands = buck2.splitlines()
                self.assertTrue(commands, buck2)
                for command in commands:
                    self.assertRegex(command, r"^buck2 test ")
                self.assertIn("cargo ", cargo)

        for suite, row in sorted(uniform.items()):
            with self.subTest(suite=suite):
                labels, test_filter, package, target, runner, flags = row.split("|")
                buck2, cargo = store_suite_branches(suite)
                invocation_count = len(test_filter.split(","))
                # Buck2 runs every label per filter; Cargo's `target` selects
                # their union, so it runs each filter once.
                self.assertEqual(
                    invocation_count * len(labels.split(",")),
                    len(buck2.splitlines()),
                )
                self.assertEqual(invocation_count, len(cargo.splitlines()))
                for label in labels.split(","):
                    self.assertIn(label, buck2)
                self.assertIn(f"-p {package}", cargo)
                if target:
                    self.assertIn(target, cargo)
                self.assertIn(
                    {
                        "nextest": "cargo nextest run -p",
                        "nextest-ci": "cargo nextest run --profile ci -p",
                        "cargo-test": "cargo test -p",
                    }[runner],
                    cargo,
                )
                for selected_filter in filter(None, test_filter.split(",")):
                    # The one selection reaches both dialects. A libtest filter
                    # that matches nothing exits 0, so a name present on one
                    # side only is a silently retired leg.
                    self.assertIn(f"--test_arg={selected_filter}", buck2)
                    self.assertIn(
                        selected_filter if runner == "cargo-test" else f"test({selected_filter})",
                        cargo,
                    )
                for flag in filter(None, flags.split(",")):
                    if flag.startswith("skip="):
                        skipped_filter = flag.removeprefix("skip=")
                        self.assertTrue(skipped_filter)
                        self.assertIn(
                            f"--test_arg=--skip --test_arg={skipped_filter}",
                            buck2,
                        )
                        self.assertEqual("cargo-test", runner)
                        self.assertIn(f"--skip {skipped_filter}", cargo)
                        continue
                    buck2_spelling, cargo_spellings = flag_dialects[flag]
                    self.assertIn(buck2_spelling, buck2, flag)
                    self.assertTrue(
                        any(spelling in cargo for spelling in cargo_spellings),
                        (flag, cargo),
                    )
                # And a flag the row does not ask for is in neither half.
                for flag, (buck2_spelling, _) in flag_dialects.items():
                    if flag not in flags.split(","):
                        self.assertNotIn(buck2_spelling, buck2, (suite, flag))

        # The rendered arms are gone from the `case`, so there is no second
        # place a selection could be written.
        case_body = script.split("\ncase \"${suite}\" in\n", 1)[1]
        for suite in uniform:
            self.assertNotIn(f"\n  {suite})\n", case_body, suite)

    def test_s3_ci_lane_requires_storage_configuration(self) -> None:
        workflow = WORKFLOW.read_text(encoding="utf-8")
        s3_store_job = workflow_job_block(workflow, "s3-store")

        self.assertNotIn("LASH_S3_ENDPOINT:", s3_store_job)
        self.assertIn('LASH_REQUIRE_S3: "1"', s3_store_job)

        # The require flag is supplied once at job level, so an unavailable
        # service fails instead of skipping for every suite in the job and no
        # step can lose it on its own. A Buck2 test spawn inherits nothing from
        # the client environment, so `buck2_test` forwards it by name.
        s3_job_env = yaml.safe_load(workflow)["jobs"]["s3-store"]["env"]
        self.assertEqual("1", str(s3_job_env["LASH_REQUIRE_S3"]))
        self.assertNotIn("LASH_S3_ENDPOINT", s3_job_env)
        # The endpoint follows the port `with-service.sh` chose (the shared
        # S3 service renders it), and every suite in this job runs inside
        # that wrapper.
        wrapper = (ROOT / "scripts" / "ci" / "with-service.sh").read_text(
            encoding="utf-8"
        )
        self.assertIn('lash_s3_test_env "$port"', wrapper)
        s3_service = (ROOT / "scripts" / "ci" / "s3-service.sh").read_text(encoding="utf-8")
        self.assertIn('"LASH_S3_ENDPOINT=http://127.0.0.1:$1"', s3_service)
        for step_name in (
            "Test S3 store conformance",
            "Test attachment blob-store differential",
        ):
            with self.subTest(step=step_name):
                self.assertIn(
                    "bash scripts/ci/with-service.sh s3 --",
                    workflow_step_block(s3_store_job, step_name),
                )
        conformance_buck2, conformance_cargo = store_suite_branches(
            store_suite_for_step(
                workflow_step_block(s3_store_job, "Test S3 store conformance")
            )
        )
        self.assertIn("cargo test -p lash-internal-s3-store --locked", conformance_cargo)
        # The Buck2 half runs the generated label set, so a new S3-gated
        # binary joins this job without a hand edit. The generated file is what
        # has to name the crate, and the rendered command is what has to carry
        # every label in it.
        s3_labels = json.loads(LANE_TABLE.read_text(encoding="utf-8"))[
            "service_test_targets"
        ]["s3"]
        self.assertTrue(
            any(label.startswith("//crates/lash-s3-store:") for label in s3_labels),
            s3_labels,
        )
        for label in s3_labels:
            self.assertIn(label, conformance_buck2, label)

        differential_buck2, differential_cargo = store_suite_branches(
            store_suite_for_step(
                workflow_step_block(
                    s3_store_job, "Test attachment blob-store differential"
                )
            )
        )
        for branch in (differential_buck2, differential_cargo):
            self.assertIn("attachment_blob_store_differential_agrees", branch)

    def test_every_declared_lane_reaches_the_pool_graph(self) -> None:
        """Each coverage lane is compiled by the one Buck2 feature job.

        The fourteen `Package feature check` legs and the four
        `Runtime feature boundary` legs were a matrix of Cargo commands; they
        are now one job building `//:feature_lane_compile`. The lane names are still
        the contract, so they are spelled out here rather than read out of the
        table under test. The facade-production lane checks the host library
        graph without development feature unification.
        """
        lanes = feature_lane_table()
        self.assertEqual(
            {
                "sansio-schema-validation",
                "dialects",
                "otel-feature-chain",
                "core-internal-features",
                "figments-facade",
                "facade-production",
                "language-testing-features",
                "llm-transport-features",
                "store-features",
                "runtime-features",
                "protocol-rlm-testing",
                "tool-lash-vm-proxies",
                "provider-testing-features",
                "perf-dhat-heap",
                "upgrade-harness-synthetic-next",
                "lashctl-synthetic-next",
                "loadtest-worker-synthetic-next",
                "regress-stable-features",
                "host-features",
            },
            set(lanes),
        )
        for lane, labels in sorted(lanes.items()):
            with self.subTest(lane=lane):
                self.assertTrue(labels, f"lane {lane} compiles nothing")

        worker_labels = set(lanes["loadtest-worker-synthetic-next"])
        inventory = json.loads(LANE_TABLE.read_text(encoding="utf-8"))
        worker_features = {
            frozenset(unit["features"])
            for unit in inventory["feature_lane_units"]
            if unit["label"] in worker_labels
            and unit["package"] == "lash-internal-vm-worker"
            and unit["kind"] == "bin"
            and ":lash-vm-worker__bin" in unit["label"]
        }
        self.assertEqual(
            {
                frozenset({"dialect-python", "dialect-typescript", "testing"}),
                frozenset({"dialect-python", "dialect-typescript", "synthetic-next", "testing"}),
            },
            worker_features,
            "the rolling-deploy lane must compile the predecessor and successor workers",
        )

        job = workflow_job_block(WORKFLOW.read_text(encoding="utf-8"), "feature-lanes")
        self.assertIn("//:feature_lane_compile", job)
        self.assertIn("//:feature_lane_tests", job)
        self.assertIn(
            "python3 scripts/ci/check_feature_lane_test_floors.py", job
        )

    def test_lash_runtime_default_build_still_runs_and_counts_its_tests(self) -> None:
        """The `default-off-tests` leg survived the move onto the pool.

        It carried two claims: the default build's own suite runs, and its case
        count does not fall. The suite is a lane command in
        `scripts/feature-coverage.toml` (so it is a pool test target); the floor
        is `FEATURE_LANE_TEST_FLOORS`, held by a step of the feature job.
        """
        plan = tomllib.loads(FEATURE_COVERAGE.read_text(encoding="utf-8"))
        runtime = next(
            lane for lane in plan["lane"] if lane["name"] == "runtime-features"
        )
        self.assertIn(
            ["cargo", "test", "-p", "lash-runtime", "--no-default-features", "--locked"],
            runtime["commands"],
        )

        inventory = json.loads(LANE_TABLE.read_text(encoding="utf-8"))
        floors = inventory["feature_lane_test_floors"]
        runtime_floors = {
            label: floor for label, floor in floors.items() if "crates/lash:" in label
        }
        self.assertEqual([130], list(runtime_floors.values()))
        self.assertNotIn(next(iter(runtime_floors)), inventory["feature_lane_test_args"])

    def test_filtered_conformance_selections_hold_nonzero_floors(self) -> None:
        """FIG-4470: provider and transport lanes compile every test target
        but execute conformance only in the library harness, and the floor
        counts that filtered selection, so it cannot silently select nothing."""
        inventory = json.loads(LANE_TABLE.read_text(encoding="utf-8"))
        floors = inventory["feature_lane_test_floors"]
        arguments = inventory["feature_lane_test_args"]
        filtered = {
            label for label, args in arguments.items() if args == ["conformance"]
        }
        self.assertEqual(
            {"lash-llm-transport", "lash-provider-anthropic",
             "lash-provider-google", "lash-provider-openai"},
            {label.split(":")[0].removeprefix("//crates/") for label in filtered},
        )
        for label in filtered:
            self.assertIn("__unit_test__fv_", label)
            self.assertEqual(1, floors.get(label), label)

    def test_required_checks_are_delivered_to_merge_queue_entries(self) -> None:
        workflow = WORKFLOW.read_text(encoding="utf-8")

        # A merge-queue entry runs from its own gh-readonly-queue ref and gets no
        # checks at all unless the workflow subscribes to `merge_group`; without
        # this the queue stalls on checks that never report.
        self.assertIn("  merge_group:\n", workflow)

        # Release cache filling is not a queue gate: a cache written from a queue ref is
        # scoped to that ref and discarded with it, so warming it there would
        # only add the longest job on the board to every queue entry.
        release_cache_workflow = RELEASE_CACHE_WORKFLOW.read_text(encoding="utf-8")
        self.assertNotIn("  linux-release-cache:\n", workflow)
        # The release warmer is manual; it never runs automatically on a
        # push to main.
        self.assertNotIn("  push:\n", release_cache_workflow)
        self.assertIn("  workflow_dispatch:\n", release_cache_workflow)
        self.assertIn("group: linux-release-cache-main", release_cache_workflow)
        self.assertIn("cancel-in-progress: false", release_cache_workflow)
        self.assertIn("if: github.ref == 'refs/heads/main'", release_cache_workflow)

    def test_manual_dispatch_explains_skipped_base_relative_gates(self) -> None:
        workflow = WORKFLOW.read_text(encoding="utf-8")
        for trigger in (
            "  workflow_dispatch:\n",
            "  pull_request:\n",
            "  merge_group:\n",
        ):
            self.assertIn(trigger, workflow)
        self.assertNotIn("\n  push:\n", workflow)

        lint = workflow_job_block(workflow, "lint")
        ratchet = workflow_step_block(lint, "Check the Test262 outcome ratchet")
        self.assertIn("if: github.event_name != 'workflow_dispatch'", ratchet)
        manual = workflow_step_block(lint, "Explain skipped base-relative gates")
        self.assertIn("if: github.event_name == 'workflow_dispatch'", manual)
        self.assertIn("bash scripts/ci/report-manual-dispatch-skips.sh", manual)

        script = ROOT / "scripts" / "ci" / "report-manual-dispatch-skips.sh"
        with tempfile.TemporaryDirectory() as directory:
            summary = pathlib.Path(directory) / "summary.md"
            result = subprocess.run(
                ["bash", str(script)],
                env=dict(os.environ, GITHUB_STEP_SUMMARY=str(summary)),
                capture_output=True,
                text=True,
            )
            self.assertEqual(0, result.returncode, result.stderr)
            self.assertIn("::warning", result.stdout)
            self.assertIn("green manual run does not prove it passed", result.stdout)
            self.assertIn("Check the Test262 outcome ratchet", result.stdout)
            summary_text = summary.read_text(encoding="utf-8")
            self.assertIn("Manual CI run is incomplete", summary_text)
            self.assertIn("green manual run does not prove that gate passed", summary_text)
            self.assertIn("Check the Test262 outcome ratchet", summary_text)

    def test_every_shared_debug_cache_reader_resolves_the_writer_rustflags(
        self,
    ) -> None:
        workflow = yaml.safe_load(WORKFLOW.read_text(encoding="utf-8"))
        # RUSTFLAGS is part of cargo's per-unit fingerprint. When the flag was
        # appended by a step, the five jobs that restore `linux-debug` without
        # that step matched the cache key, logged "cache restored", and then
        # cold-built the entire graph. The flag is a property of the workflow
        # now, so nothing about a job's step list can move it.
        self.assertEqual((workflow.get("env") or {}).get("RUSTFLAGS"), MOLD_RUSTFLAGS)

        for job_id, job in workflow["jobs"].items():
            steps = job.get("steps") or []
            if not any(
                (step.get("with") or {}).get("shared-key") == "linux-debug"
                for step in steps
            ):
                continue
            with self.subTest(job=job_id):
                # No job-scoped or step-scoped override may reintroduce the
                # split this fixed.
                self.assertNotIn("RUSTFLAGS", job.get("env") or {})
                for step in steps:
                    self.assertNotIn("RUSTFLAGS", step.get("env") or {})
                # The flag names a linker that has to exist: a job with the
                # fingerprint but without the binary cannot link at all.
                self.assertTrue(
                    any(
                        "setup-mold" in (step.get("uses") or "")
                        for step in steps
                    ),
                    f"{job_id} resolves the mold RUSTFLAGS without installing mold",
                )

    def test_linux_release_cache_stays_mold_free_across_workflows(self) -> None:
        release_cache_workflow = yaml.safe_load(
            RELEASE_CACHE_WORKFLOW.read_text(encoding="utf-8")
        )
        perf = PERF_WORKFLOW.read_text(encoding="utf-8")
        release = RELEASE_WORKFLOW.read_text(encoding="utf-8")

        # perf.yml and release.yml restore `linux-release` and link with the
        # default linker. The one job that writes that cache has to opt out of
        # the workflow-level flag, or those two workflows rebuild everything
        # they restore. Empty and unset are the same value to cargo.
        self.assertEqual(
            release_cache_workflow["env"]["RUSTFLAGS"], ""
        )
        for name, text in (("perf.yml", perf), ("release.yml", release)):
            with self.subTest(workflow=name):
                self.assertNotIn("mold", text)
                self.assertNotIn("RUSTFLAGS", text)


class ReleaseDryRunTests(unittest.TestCase):
    def test_dry_run_runs_validation_without_publication(self):
        workflow = yaml.safe_load(RELEASE_WORKFLOW.read_text())
        inputs = workflow[True]["workflow_dispatch"]["inputs"]
        self.assertEqual({"description": "Run release validation without publishing", "required": False,
                          "type": "boolean", "default": False}, inputs["dry_run"])
        jobs = workflow["jobs"]
        validators = {"validate-release-preconditions", "validate-release-ref",
                      "release-postgres", "latency-gate", "package-crates", "worker-artifacts"}
        self.assertEqual(validators | {"prepare-release", "publish-crates", "publish"}, set(jobs))
        for name in validators:
            with self.subTest(job=name):
                self.assertEqual("prepare-release", jobs[name]["needs"])
                self.assertNotIn("if", jobs[name])
                self.assertNotIn("continue-on-error", jobs[name])
        for name in ("publish-crates", "publish"):
            self.assertEqual("${{ !inputs.dry_run }}", jobs[name]["if"])
        self.assertIn("validate-release-preconditions", jobs["publish-crates"]["needs"])
        upload = next(step for step in jobs["package-crates"]["steps"] if "upload-artifact@" in step.get("uses", ""))
        self.assertEqual("${{ always() && !inputs.dry_run }}", upload["if"])
        resolve = jobs["prepare-release"]["steps"][-1]["run"]
        self.assertIn('git merge-base --is-ancestor "${sha}" origin/main', resolve)
        self.assertNotIn("gh run list", resolve)
        preconditions = jobs["validate-release-preconditions"]["steps"][-1]["run"]
        self.assertIn('run.get("event") == "workflow_dispatch"', preconditions)
        self.assertNotIn("dry_run", preconditions)



if __name__ == "__main__":
    unittest.main()

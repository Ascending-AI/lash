#!/usr/bin/env python3
"""Regression laws for Confidence's build transfer and stage boundaries."""
import json
import os
from pathlib import Path
import pathlib
import yaml
import re
import subprocess
import shutil
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
GATE = ROOT / 'scripts/confidence-gate.sh'
SHARED = ROOT / 'scripts/ci/confidence-shared-build.sh'


def definition(name):
    match = re.search(rf'^{name}\(\) \{{\n.*?^\}}\n', GATE.read_text(), re.M | re.S)
    if match is None:
        raise AssertionError(f'missing function {name}')
    return match.group()


class ConfidenceInfrastructureTests(unittest.TestCase):
    def setUp(self):
        scratch = Path(os.environ.get('LASH_CONFIDENCE_TEST_TMP_DIR', ROOT / '.kiln/confidence-infrastructure'))
        scratch.mkdir(parents=True, exist_ok=True)
        self.directory = tempfile.TemporaryDirectory(dir=scratch)
        self.addCleanup(self.directory.cleanup)
        self.tmp = Path(self.directory.name)

    def shell(self, body, **environment):
        return subprocess.run(['bash', '-euc', body], cwd=ROOT,
                              env=os.environ | environment, text=True, capture_output=True)

    def test_mutation_sim_refuses_a_red_baseline_before_judging_mutants(self):
        # Model cargo-mutants' baseline contract at the real invocation seam.
        # Skipping it launches the entire sweep even when the unmutated suite
        # hangs, as in run 37304222736.
        body = definition('run_lash_sim_runtime_completion_mutation_evidence')
        body += """step() { :; }
require_tool() { :; }
mutation_jobs=2
mutation_failures=0
out_dir=fixture
run_mutants_recorded() {
    shift 2
    local baseline=skip bounded=0 arg
    while (($#)); do
        arg=$1; shift
        case "$arg" in
          --baseline) baseline=$1; shift ;;
          --build-timeout) bounded=$1; shift ;;
        esac
    done
    [ "$baseline" = run ] || { echo 'mutants launched on a red baseline' >&2; exit 91; }
    [ "$bounded" -gt 0 ] || { echo 'unbounded mutant compile' >&2; exit 92; }
    mutation_failures=$((mutation_failures + 1))
    echo baseline-refused
}
run_lash_sim_runtime_completion_mutation_evidence
"""
        result = self.shell(body)
        self.assertEqual(0, result.returncode, result.stderr)
        self.assertEqual(['baseline-refused'], result.stdout.splitlines())

    def test_mutation_sim_shards_judge_the_complete_disjoint_space(self):
        workflow = yaml.safe_load((ROOT / '.github/workflows/confidence.yml').read_text())
        job = workflow['jobs']['confidence-mutation-sim']
        covered = set()
        counts = {'scheduler': 7, 'oracles': 51, 'readiness': 28}
        for row in job['strategy']['matrix']['include']:
            body = definition('run_lash_sim_runtime_completion_mutation_evidence')
            body += """step() { :; }
require_tool() { :; }
mutation_jobs=2
mutation_failures=0
out_dir=fixture
run_mutants_recorded() {
    local name=$1; shift 2
    local shard arg
    while (($#)); do
        arg=$1; shift
        if [ "$arg" = --shard ]; then shard=$1; shift; fi
    done
    printf '%s|%s\\n' "$name" "$shard"
}
run_lash_sim_runtime_completion_mutation_evidence
"""
            result = self.shell(body, LASH_MUTATION_SIM_GROUP=row['group'],
                                LASH_MUTATION_SIM_SHARD=row['shard'])
            self.assertEqual(0, result.returncode, result.stderr)
            calls = result.stdout.splitlines()
            self.assertEqual(1, len(calls))
            expected_names = {
                'scheduler': 'lash-sim scheduler runtime completion queue',
                'oracles': 'lash-sim scheduler-owned and mini-oracles',
                'readiness': 'lash-sim runtime completion readiness',
            }
            name, shard = calls[0].split('|')
            self.assertEqual(expected_names[row['group']], name)
            self.assertEqual(row['shard'], shard)
            index, total = map(int, shard.split('/'))
            selected = {(row['group'], i) for i in range(counts[row['group']]) if i % total == index}
            self.assertFalse(covered & selected, 'mutant judged twice')
            covered |= selected
            # Preserve the full space while bounding the slowest leg from
            # measured warm cost + compile/baseline caps + setup and margin.
            self.assertLessEqual(15 + 3 + len(selected) * 2.03 * 1.3 + 5,
                                 job['timeout-minutes'])
        self.assertEqual({(group, i) for group, count in counts.items() for i in range(count)}, covered)

    def test_postgres_mutation_jobs_belong_to_mutants_not_libtest(self):
        body = '''mutation_jobs=2
mutation_postgres_database_url=postgres://fixture/database
start_mutation_postgres() { :; }
cleanup_mutation_postgres() { :; }
run_mutants_recorded() {
    shift 2
    local after_separator=0 arg
    for arg in "$@"; do
        if [ "$arg" = -- ]; then after_separator=1; fi
        if [ "$arg" = --jobs ] && [ "$after_separator" = 1 ]; then
            echo "error: Unrecognized option: jobs" >&2
            return 101
        fi
    done
    [ "$LASH_POSTGRES_DATABASE_URL" = postgres://fixture/database ]
}
'''
        body += definition('run_postgres_mutants_recorded')
        # This is the actual smoke stage's call, including its libtest delimiter.
        body += definition('run_mutation_smoke')
        body += '''step() { :; }
require_tool() { :; }
selected_packages=(lash-internal-postgres-store)
area_mutation_file_args=()
out_dir=fixture
MUTATION_EXCLUDED_TEST_NAME=fixture
LASH_MUTATION_SMOKE_SHARD=0/1
run_mutation_smoke
'''
        result = self.shell(body)
        self.assertEqual(0, result.returncode, result.stderr)

    def test_harness_postgres_fixture_gets_an_isolated_service(self):
        calls = self.tmp / 'calls'
        body = '''lane=full
repo="$CONFIDENCE_ROOT"
area_selected() { [ "$1" = store ]; }
step() { :; }
ci_features=""
cargo() {
    if [[ "$*" == *postgres_seed* ]]; then
        echo 'PostgreSQL test requires a non-empty LASH_POSTGRES_DATABASE_URL' >&2
        return 101
    fi
}
bash() {
    [ "$1" = "$repo/scripts/ci/with-service.sh" ]
    [ "$2" = pg16 ]
    [ "$3" = -- ]
    printf '%s\\n' "$*" >> "$CONFIDENCE_CALLS"
}
'''
        body += definition('run_cargo_tests')
        body += definition('run_scenario_harnesses')
        body += 'out_dir=fixture\nrun_scenario_harnesses\n'
        result = self.shell(body, CONFIDENCE_ROOT=str(ROOT), CONFIDENCE_CALLS=str(calls),
                            LASH_POSTGRES_DATABASE_URL='')
        self.assertEqual(0, result.returncode, result.stderr)
        self.assertIn('postgres_seed_round_trips_through_a_fresh_store_when_configured',
                      calls.read_text())

    def test_failed_stage_keeps_evidence_at_the_workflow_upload_path(self):
        evidence = self.tmp / 'evidence'
        mocks = self.tmp / 'mocks.sh'
        mocks.write_text('cargo() { return 101; }\ndocker() { return 0; }\n')
        worker = self.tmp / 'lash-vm-worker'
        worker.write_text('#!/bin/sh\nexit 0\n')
        worker.chmod(0o755)
        slug = self.shell('source scripts/worktree-gate-env.sh; echo "$LASH_GATE_WORKTREE_SLUG"').stdout.strip()
        result = subprocess.run(['bash', str(GATE), 'full'], cwd=ROOT,
                                env=os.environ | {
                                    'BASH_ENV': str(mocks),
                                    'LASH_GATE_LOCK_HELD': '1',
                                    'LASH_GATE_LOCK_SLUG': slug,
                                    'LASH_GATE_STATE_ROOT_OVERRIDE': str(self.tmp / 'locks'),
                                    'LASH_CONFIDENCE_STAGE': 'backends',
                                    'LASH_CONFIDENCE_OUT_DIR': str(evidence),
                                    'LASH_VM_WORKER': str(worker),
                                }, text=True, capture_output=True, timeout=10)
        self.assertEqual(101, result.returncode, result.stderr)
        status = json.loads((evidence / 'stage.json').read_text())
        self.assertEqual({'stage': 'backends', 'status': 'failed'}, status)
        self.assertTrue((evidence / 'failure.log').is_file())
        self.assertFalse((evidence / 'full').exists())

    def test_stage_supplies_absolute_worker_path_outside_test_binary_tree(self):
        repo = self.tmp / 'repo'
        scripts = repo / 'scripts'
        (scripts / 'ci').mkdir(parents=True)
        for name in ('confidence-gate.sh', 'worktree-gate-env.sh', 'ci/pg-service.sh',
                     'ci/confidence-stage.sh'):
            shutil.copyfile(ROOT / 'scripts' / name, scripts / name)
        worker = repo / 'target/debug/lash-vm-worker'
        worker.parent.mkdir(parents=True)
        worker.write_text('#!/bin/sh\nexit 0\n')
        worker.chmod(0o755)
        mocks = self.tmp / 'worker-mocks.sh'
        mocks.write_text('cargo() { [ "$LASH_VM_WORKER" = "$EXPECTED_WORKER" ]; }\n'
                         'docker() { return 1; }\n')
        slug = self.shell(f'source "{scripts}/worktree-gate-env.sh"; echo "$LASH_GATE_WORKTREE_SLUG"').stdout.strip()
        environment = {key: value for key, value in os.environ.items()
                       if key != 'LASH_VM_WORKER'}
        result = subprocess.run(['bash', str(scripts / 'confidence-gate.sh'), 'full'],
                                cwd=repo, env=environment | {
                                    'BASH_ENV': str(mocks),
                                    'EXPECTED_WORKER': str(worker),
                                    'LASH_CONFIDENCE_STAGE': 'generated',
                                    'LASH_SIM_SHARD': '1/9',
                                    'LASH_GATE_LOCK_HELD': '1',
                                    'LASH_GATE_LOCK_SLUG': slug,
                                    'LASH_CONFIDENCE_OUT_DIR': str(self.tmp / 'output'),
                                }, text=True, capture_output=True, timeout=10)
        self.assertEqual(0, result.returncode, result.stderr)

    def test_shared_build_restores_the_inputs_cargo_fingerprinted(self):
        built, restored = self.tmp / 'built', self.tmp / 'restored'
        tools, build = self.tmp / 'tools', self.tmp / 'build'
        old_ns = 1_700_000_000_123_456_789
        for tree in (built, restored):
            tree.mkdir()
            subprocess.run(['git', 'init', '-q', str(tree)], check=True)
            (tree / 'source.rs').write_text('fn main() {}\n')
            subprocess.run(['git', '-C', str(tree), 'add', 'source.rs'], check=True)
        os.utime(built / 'source.rs', ns=(old_ns, old_ns))
        cargo_home = built / '.cargo-home'
        (cargo_home / 'bin').mkdir(parents=True)
        for tool in ('cargo-mutants', 'cargo-llvm-cov'):
            executable = cargo_home / 'bin' / tool
            executable.write_text('#!/bin/sh\nexit 0\n')
            executable.chmod(0o755)
        dependency = cargo_home / 'registry/src/fixture/lib.rs'
        dependency.parent.mkdir(parents=True)
        dependency.write_text('dependency source\n')
        os.utime(dependency, ns=(old_ns, old_ns))
        (cargo_home / 'git').mkdir()
        worker = built / 'target/debug/lash-vm-worker'
        worker.parent.mkdir(parents=True)
        worker.write_text('#!/bin/sh\nexit 0\n')
        worker.chmod(0o755)
        fingerprint = built / 'target/debug/.fingerprint/fixture/invoked.timestamp'
        fingerprint.parent.mkdir(parents=True)
        fingerprint.touch()
        os.utime(fingerprint, ns=(old_ns + 10_000_000_000, old_ns + 10_000_000_000))

        def transfer(tree, home, *args):
            # The helper resolves Cargo's home from HOME; the fixture supplies
            # CARGO_HOME only to its subprocess, leaving the development shell alone.
            result = subprocess.run(['bash', str(SHARED), *map(str, args)], cwd=tree,
                                    env=os.environ | {'CARGO_HOME': str(home)},
                                    text=True, capture_output=True)
            self.assertEqual(0, result.returncode, result.stderr)

        transfer(built, cargo_home, 'pack', tools, build)
        consumer_home = restored / '.cargo-home'
        transfer(restored, consumer_home, 'restore', tools, build)
        self.assertEqual(old_ns, (restored / 'source.rs').stat().st_mtime_ns)
        self.assertEqual(old_ns, (consumer_home / 'registry/src/fixture/lib.rs').stat().st_mtime_ns)
        self.assertEqual(old_ns + 10_000_000_000,
                         (restored / fingerprint.relative_to(built)).stat().st_mtime_ns)
        self.assertTrue(os.access(restored / 'target/debug/lash-vm-worker', os.X_OK))
        self.assertEqual([], list(build.iterdir()))
        tools_only = self.tmp / 'tools-only'
        tools_only.mkdir()
        transfer(tools_only, tools_only / '.cargo-home', 'restore', tools)
        self.assertTrue(os.access(tools_only / 'target/debug/lash-vm-worker', os.X_OK))
        manifest = self.tmp / 'changed-inputs.json'
        helper = ROOT / 'scripts/ci/confidence-build-inputs.py'
        subprocess.run(['python3', str(helper), 'snapshot', str(manifest)],
                       cwd=built, check=True)
        (restored / 'source.rs').write_text('fn main() { panic!("changed"); }\n')
        modified_ns = (restored / 'source.rs').stat().st_mtime_ns
        refused = subprocess.run(['python3', str(helper), 'restore', str(manifest)],
                                 cwd=restored, text=True, capture_output=True)
        self.assertNotEqual(0, refused.returncode)
        self.assertIn('differs from the producer', refused.stderr)
        self.assertEqual(modified_ns, (restored / 'source.rs').stat().st_mtime_ns)

    def test_shared_build_never_needs_a_second_copy_of_target(self) -> None:
        """Runs 36294879308 and 37258510534 ran the build runner out of disk in
        "Pack shared build", and run 37181695061 ran it out while building.

        The image offered 86 GB free (90001852 KiB), above the 80 GiB reclaim
        floor, so reclaim never ran, and the compressed archive was written
        beside a tree that already filled the disk. Now every runner that holds
        the tree reclaims disk first, pack deletes each file once it is in the
        archive, and restore deletes each chunk once it is extracted, so no
        runner holds the build twice.
        """
        jobs = yaml.safe_load((ROOT / ".github/workflows/confidence.yml").read_text())["jobs"]
        helper = "bash scripts/ci/confidence-shared-build.sh"
        tools_dir, build_dir = "${RUNNER_TEMP}/confidence-tools", "${RUNNER_TEMP}/confidence-build"

        producer = jobs["confidence-build"]["steps"]
        names = [step.get("name") for step in producer]
        self.assertLess(names.index("Reclaim runner disk"), names.index("Build full confidence"))
        reclaim = producer[names.index("Reclaim runner disk")]
        self.assertEqual("bash scripts/ci-reclaim-disk.sh", reclaim["run"])
        self.assertGreater(int(reclaim["env"]["CI_RECLAIM_MIN_FREE_KIB"]), 90001852)
        self.assertEqual(f'{helper} pack "{tools_dir}" "{build_dir}"',
                         producer[names.index("Pack shared build")]["run"])
        self.assertEqual("${{ runner.temp }}/confidence-tools",
                         producer[names.index("Upload confidence tools")]["with"]["path"])
        upload = producer[names.index("Upload shared build")]["with"]
        self.assertEqual("${{ runner.temp }}/confidence-build", upload["path"])
        self.assertEqual(0, upload["compression-level"])

        consumers = [job for job in jobs.values() if job.get("needs") == "confidence-build"]
        self.assertEqual(9, len(consumers))
        for job in consumers:
            steps = job["steps"]
            restores = [step["run"] for step in steps if step.get("name", "").startswith("Restore ")]
            if "confidence-build-" in str(steps):
                self.assertEqual([f'{helper} restore "{tools_dir}" "{build_dir}"'], restores)
                order = [step.get("name") for step in steps]
                self.assertIn(reclaim, steps)
                self.assertLess(order.index("Reclaim runner disk"), order.index("Download shared build"))
            else:
                self.assertEqual([f'{helper} restore "{tools_dir}"'], restores)
        self.assertNotIn("tar -", (ROOT / ".github/workflows/confidence.yml").read_text())

        # The helper itself: a packed tree costs a fraction of the tree and is
        # gone from the producer; a restore reproduces it, installs the tools
        # (artifact transfer drops the executable bit) and leaves no chunks.
        with tempfile.TemporaryDirectory(dir=self.tmp) as raw:
            tmp = pathlib.Path(raw)
            payload = b"lash shared build\n" * (512 * 1024)
            built, restored, tools_only = tmp / "built", tmp / "restored", tmp / "tools-only"
            (built / "target" / "debug" / "deps").mkdir(parents=True)
            (built / "target" / "debug" / "deps" / "liblash.rlib").write_bytes(payload)
            subprocess.run(["git", "init", "-q", str(built)], check=True)
            subprocess.run(["git", "init", "-q", str(restored)], check=True)
            worker = built / "target/debug/lash-vm-worker"
            worker.write_text("#!/bin/sh\nexit 0\n")
            worker.chmod(0o755)
            for home in ("build-home", "restore-home", "tools-home"):
                (tmp / home / "bin").mkdir(parents=True)
            for tool in ("cargo-mutants", "cargo-llvm-cov"):
                binary = tmp / "build-home" / "bin" / tool
                binary.write_text(f"#!/bin/sh\necho {tool}\n", encoding="utf-8")
                binary.chmod(0o755)
            restored.mkdir(exist_ok=True)
            tools_only.mkdir()
            packed_tools, packed_build = tmp / "transfer" / "tools", tmp / "transfer" / "build"

            def run(cwd: pathlib.Path, home: str, *args: pathlib.Path | str) -> None:
                subprocess.run(
                    ["bash", str(ROOT / "scripts" / "ci" / "confidence-shared-build.sh"), *map(str, args)],
                    cwd=cwd,
                    env={**os.environ, "CARGO_HOME": str(tmp / home)},
                    check=True,
                    capture_output=True,
                    text=True,
                )

            run(built, "build-home", "pack", packed_tools, packed_build)
            self.assertFalse((built / "target").exists())
            chunks = list(packed_build.iterdir())
            self.assertTrue(chunks)
            self.assertLess(sum(c.stat().st_size for c in chunks) * 10, len(payload))
            for tool in packed_tools.iterdir():
                tool.chmod(0o644)
            run(tools_only, "tools-home", "restore", packed_tools)
            self.assertTrue(os.access(tools_only / "target/debug/lash-vm-worker", os.X_OK))
            run(restored, "restore-home", "restore", packed_tools, packed_build)
            self.assertEqual(
                payload, (restored / "target" / "debug" / "deps" / "liblash.rlib").read_bytes()
            )
            for home in ("restore-home", "tools-home"):
                for tool in ("cargo-mutants", "cargo-llvm-cov"):
                    self.assertTrue(os.access(tmp / home / "bin" / tool, os.X_OK))
            self.assertEqual([], list(packed_build.iterdir()))

            # A missing tool fails the pack instead of shipping a build the
            # mutation and coverage stages cannot use.
            (restored / "target").rename(built / "target")
            (tmp / "build-home" / "bin" / "cargo-mutants").unlink()
            with self.assertRaises(subprocess.CalledProcessError):
                run(built, "build-home", "pack", tmp / "again" / "tools", tmp / "again" / "build")


if __name__ == '__main__':
    unittest.main()

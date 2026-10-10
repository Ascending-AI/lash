#!/usr/bin/env python3
"""Regression laws for local Confidence scheduling and stage boundaries."""
import json
import os
from pathlib import Path
import yaml
import re
import subprocess
import shutil
import tempfile
import unittest
import tomllib

ROOT = Path(__file__).resolve().parents[1]
GATE = ROOT / 'scripts/confidence-gate.sh'


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

    def test_inherited_confidence_stage_leaves_network_cleanup_to_its_owner(self):
        # Parallel consumers inherit the driver's lock and network. A consumer
        # must not remove the idle network while another is starting a service.
        body = definition('finish_confidence_gate')
        body += """cleanup_mutation_postgres() { :; }
finish_current_step() { :; }
lash_gate_cleanup() { echo removed-owner-network; }
dry_run=0
LASH_GATE_ACQUIRED_HERE=0
finish_confidence_gate
"""
        result = self.shell(body)
        self.assertEqual(0, result.returncode, result.stderr)
        self.assertEqual('', result.stdout)

    def local_fixture(self):
        # Exercise the real scheduler and conclusion with cheap stage processes.
        # The stage scripts are the boundary: this law never builds Rust.
        root = self.tmp / 'runner'
        for name in ('scripts/confidence-local.sh', 'scripts/ci/confidence_local.py',
                     'scripts/worktree-gate-env.sh', 'scripts/confidence-stages.json'):
            target = root / name
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(ROOT / name, target)
        gate_env = root / 'scripts/worktree-gate-env.sh'
        with gate_env.open('a') as spy:
            spy.write(f'\nlash_gate_cleanup() {{ echo cleanup >> "{root}/cleanup-events"; }}\n')
        stage = root / 'stage.py'
        stage.write_text(r'''import fcntl, json, os, pathlib, time
root = pathlib.Path.cwd()
name = pathlib.Path(os.environ['LASH_CONFIDENCE_OUT_DIR']).name
if name != 'build':
    assert (root / 'built').exists(), 'consumer started before shared build'
def event(kind):
    with (root / 'events.jsonl').open('a') as out:
        fcntl.flock(out, fcntl.LOCK_EX)
        out.write(json.dumps({'event': kind, 'stage': name, 'env': dict(os.environ)}) + '\n')
event('start')
time.sleep(0.02)
if name == 'build':
    (root / 'built').touch()
if os.environ.get('FAIL_LOCAL_STAGE') == name:
    destination = pathlib.Path(os.environ['LASH_CONFIDENCE_OUT_DIR'])
    mutants = destination / 'mutants.out'
    mutants.mkdir()
    (mutants / 'missed.txt').write_text('fixture mutant survivor\n')
    (mutants / 'log').mkdir()
    baseline = 'test fixture::broken ... FAILED\nSummary [1.0s] 1 test run: 0 passed, 1 failed\ntest fixture::baseline ... FAILED\ntest result: FAILED. 0 passed; 1 failed; 0 ignored\n'
    (mutants / 'log/baseline.log').write_text(baseline)
    (mutants / 'log/caught.log').write_text('test fixture::caught ... FAILED\ntest result: FAILED. 0 passed; 1 failed; 0 ignored\n')
    # cargo-mutants forwards baseline failures into its process console.
    print(baseline, end='')
    event('end')
    raise SystemExit(1)
event('end')
''')
        for name in ('confidence-gate.sh', 'hermetic-build.sh'):
            (root / 'scripts' / name).write_text('exec python3 stage.py\n')
        (root / 'scripts/ci/with-service.sh').write_text(
            'export PRIVATE_STAGE_PG=1\nshift 2\nexec "$@"\n')
        slug = self.shell(f'source "{root}/scripts/worktree-gate-env.sh"; echo "$LASH_GATE_WORKTREE_SLUG"').stdout.strip()
        environment = os.environ | {'KILN_GATE_ID': 'fixture', 'LASH_GATE_LOCK_HELD': '1',
                                    'LASH_GATE_LOCK_SLUG': slug, 'LASH_SIM_SHARD': '999/999',
                                    'LASH_POSTGRES_DATABASE_URL': 'postgres://unowned/live'}
        return root, environment

    def test_confidence_runs_locally_without_ci_or_release_dependencies(self):
        # FIG-5103: the on-demand runner must work without any CI definitions.
        root, environment = self.local_fixture()
        self.assertFalse((root / '.github').exists())
        result = subprocess.run(['bash', str(root / 'scripts/confidence-local.sh'), '--list'],
                                cwd=root, env=environment, text=True, capture_output=True)
        self.assertEqual(0, result.returncode, result.stderr)
        self.assertEqual(50, len(result.stdout.splitlines()))
        for path in (ROOT / '.github/workflows').glob('*.yml'):
            workflow = yaml.safe_load(path.read_text())
            self.assertNotEqual('Confidence', workflow.get('name'), path.name)
            for job in workflow['jobs'].values():
                for step in job.get('steps', []):
                    self.assertNotRegex(step.get('run', ''),
                                        r'(confidence-(?:local|gate)\.sh|just confidence|--workflow[ =]+Confidence)')
        release = yaml.safe_load((ROOT / '.github/workflows/release.yml').read_text())
        dispatch = release.get('on', release.get(True))['workflow_dispatch']
        self.assertFalse(any('confidence' in key.lower() for key in dispatch['inputs']))

    def test_local_confidence_preserves_stage_matrix_build_order_and_failure_conclusion(self):
        # The strict full policy: one red leg fails its matrix job and the
        # conclusion, while all the other legs still produce evidence.
        root, environment = self.local_fixture()
        environment['FAIL_LOCAL_STAGE'] = 'mutation-sim-readiness-0'
        out = root / '.kiln/evidence'
        result = subprocess.run(['bash', str(root / 'scripts/confidence-local.sh'),
                                 '-j', '3', '--out-dir', str(out)], cwd=root,
                                env=environment, text=True, capture_output=True)
        self.assertEqual(1, result.returncode, result.stderr)
        self.assertFalse((root / 'cleanup-events').exists(), 'inherited launcher cleaned its owner network')
        summary = json.loads((out / 'summary.json').read_text())
        rows = summary['stages']
        self.assertEqual(50, len(rows))
        self.assertEqual(49, sum(row['result'] == 'success' for row in rows))
        self.assertEqual('failure', summary['groups']['mutation-sim']['result'])
        self.assertEqual('success', summary['groups']['sim-search']['result'])
        self.assertEqual('success', summary['groups']['append-vec-miri']['result'])
        self.assertIn('fixture::broken', '\n'.join(summary['failed_tests']))
        self.assertIn('fixture::baseline', '\n'.join(summary['failed_tests']))
        self.assertNotIn('fixture::caught', '\n'.join(summary['failed_tests']))
        self.assertIn('fixture mutant survivor', '\n'.join(summary['missed_mutants']))
        self.assertIn('Confidence conclusion rejected', (out / 'conclusion.log').read_text())
        events = [json.loads(line) for line in (root / 'events.jsonl').read_text().splitlines()]
        self.assertEqual(['build', 'build'], [event['stage'] for event in events[:2]])
        active = set()
        starts = {}
        for event in events:
            name = event['stage']
            if event['event'] == 'start':
                active.add(name)
                starts[name] = event['env']
            else:
                active.remove(name)
            self.assertLessEqual(len(active), 3)
            self.assertLessEqual(sum(name.startswith('mutation-') or name == 'coverage' for name in active), 1)
        self.assertEqual(50, len(starts))
        workers = {env['LASH_VM_WORKER'] for env in starts.values()}
        self.assertEqual(1, len(workers))
        for row in rows:
            actual = starts[row['stage']]
            for key, value in row['environment'].items():
                if key != 'LASH_CONFIDENCE_OUT_DIR':
                    self.assertEqual(value, actual[key], (row['stage'], key))
            self.assertEqual(str(out / row['stage']), actual['LASH_CONFIDENCE_OUT_DIR'])
            self.assertEqual('0', actual['LASH_CONFIDENCE_MUTATION_POSTGRES_PORT'])
            self.assertNotIn('LASH_POSTGRES_DATABASE_URL', actual)
            self.assertNotIn(Path(row['log']), list((out / row['stage']).rglob('*.log')))
        self.assertEqual('', starts['build']['KILN_CARGO_ROUTE'])
        self.assertEqual('1', starts['backends']['PRIVATE_STAGE_PG'])
        self.assertEqual('1', starts['coverage']['PRIVATE_STAGE_PG'])

        self.assertEqual(2, summary['test_counts_from_logs']['failed'])
        # Resume reruns the requested leg, keeps the shared build, and reports
        # current attempts only. Scratch copies are not execution evidence.
        decoy = out / 'build/tmp/copied'
        decoy.mkdir()
        (decoy / 'failure.log').write_text('test scratch::decoy ... FAILED\n')
        environment.pop('FAIL_LOCAL_STAGE')
        resumed = subprocess.run(['bash', str(root / 'scripts/confidence-local.sh'),
                                  '--resume', '--only', 'mutation-sim-readiness-0',
                                  '--out-dir', str(out)], cwd=root, env=environment,
                                 text=True, capture_output=True)
        self.assertEqual(0, resumed.returncode, resumed.stderr)
        fresh = json.loads((out / 'summary.json').read_text())
        self.assertEqual(50, len(fresh['stages']))
        self.assertTrue(all(row['result'] == 'success' for row in fresh['stages']))
        self.assertEqual([], fresh['failed_tests'])
        self.assertEqual([], fresh['missed_mutants'])
        extra = (root / 'events.jsonl').read_text().splitlines()[len(events):]
        self.assertEqual(['mutation-sim-readiness-0'] * 2,
                         [json.loads(line)['stage'] for line in extra])
        # A source change cannot reuse proof from the old source tree.
        (root / 'scripts/confidence-gate.sh').write_text('exit 0\n')
        refused = subprocess.run(['bash', str(root / 'scripts/confidence-local.sh'),
                                  '--resume', '--out-dir', str(out)], cwd=root,
                                 env=environment, text=True, capture_output=True)
        self.assertEqual(2, refused.returncode)
        self.assertIn('source inputs changed', refused.stderr)

    def test_local_confidence_scratch_packages_do_not_join_the_repository_workspace(self):
        # Miri setup creates a standalone package under TMPDIR. Keeping its
        # evidence in the fork must not make that package a workspace member.
        root, environment = self.local_fixture()
        excluded = tomllib.loads((ROOT / 'Cargo.toml').read_text())['workspace']['exclude']
        (root / 'Cargo.toml').write_text('[workspace]\nmembers = []\nexclude = ' + json.dumps(excluded) + '\n')
        (root / 'scripts/hermetic-build.sh').write_text(r'''set -euo pipefail
mkdir -p "$TMPDIR/probe/src"
printf '[package]\nname = "scratch-probe"\nversion = "0.0.0"\nedition = "2024"\n' > "$TMPDIR/probe/Cargo.toml"
echo 'pub fn probe() {}' > "$TMPDIR/probe/src/lib.rs"
cargo metadata --no-deps --offline --format-version 1 --manifest-path "$TMPDIR/probe/Cargo.toml"
''')
        out = root / '.kiln/scratch-evidence'
        result = subprocess.run(['bash', str(root / 'scripts/confidence-local.sh'),
                                 '--only', 'append-vec-miri', '--out-dir', str(out)],
                                cwd=root, env=environment, text=True, capture_output=True)
        summary = json.loads((out / 'summary.json').read_text())
        miri = next(row for row in summary['stages'] if row['stage'] == 'append-vec-miri')
        self.assertEqual('success', miri['result'], Path(miri['log']).read_text())
        self.assertEqual(1, result.returncode)  # Partial evidence still refuses full confidence.

    def test_local_confidence_partial_selection_cannot_claim_full_success(self):
        root, environment = self.local_fixture()
        out = root / '.kiln/partial'
        result = subprocess.run(['bash', str(root / 'scripts/confidence-local.sh'),
                                 '--only', 'generated-?', '--skip-mutation',
                                 '-j', '2', '--out-dir', str(out)], cwd=root,
                                env=environment, text=True, capture_output=True)
        self.assertEqual(1, result.returncode, result.stderr)
        summary = json.loads((out / 'summary.json').read_text())
        executed = [row for row in summary['stages'] if row['result'] == 'success']
        self.assertEqual(10, len(executed))
        self.assertEqual({'build'} | {f'generated-{i}' for i in range(1, 10)},
                         {row['stage'] for row in executed})
        self.assertEqual('success', summary['groups']['generated']['result'])
        self.assertEqual('skipped', summary['groups']['mutation-core']['result'])
        self.assertEqual(1, summary['conclusion_exit_code'])

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

    def test_mutation_sim_shards_judge_the_complete_disjoint_local_plan(self):
        plan = json.loads((ROOT / 'scripts/confidence-stages.json').read_text())
        job = next(stage for stage in plan['stages'] if stage['group'] == 'mutation-sim')
        covered = set()
        counts = {'scheduler': 7, 'oracles': 51, 'readiness': 28}
        for row in job['matrix']['include']:
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
                                 job['timeout_seconds'] / 60)
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
    [ "$2" = pg ]
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

    def test_failed_stage_keeps_evidence_at_the_local_output_path(self):
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


if __name__ == '__main__':
    unittest.main()

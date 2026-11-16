"""Rule-derived rejection witnesses for the cutover gate."""
import ast
import contextlib
import copy
import io
import subprocess
import tempfile
from unittest.mock import patch
from pathlib import Path
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parent))
import perf_land_gate as gate
import perf_land_ceilings as policy


# Main's real exemptions; each law runs without them unless it is about them.
EXEMPT_ROWS = dict(policy.MONTY_EXEMPT_ROWS)


class GateLaws(unittest.TestCase):
    def setUp(self):
        patcher = patch.dict(policy.MONTY_EXEMPT_ROWS, clear=True)
        patcher.start()
        self.addCleanup(patcher.stop)

    def test_switch_gate_refuses_301_even_inside_own_ceiling_and_reports_201(self):
        with self.assertRaises(ValueError):
            gate.check_instruction_ceiling('candidate', 'python/int_arith', 301, 1000, 100)
        self.assertIsNone(gate.check_instruction_ceiling('candidate', 'python/int_arith', 300, 1000, 300))
        self.assertIn('report-only 2.00x',
                      gate.check_instruction_ceiling('candidate', 'python/int_arith', 300, 1000, 100))
        self.assertIn('report-only 2.00x',
                      gate.check_instruction_ceiling('candidate', 'python/int_arith', 201, 1000, 100))
        self.assertIsNone(gate.check_instruction_ceiling('candidate', 'python/int_arith', 200, 1000, 100))
        with self.assertRaises(ValueError):
            gate.check_instruction_ceiling('candidate', 'typescript/int_arith', 1001, 1000)

    def test_rows_cannot_change_without_the_exact_reviewed_predecessor(self):
        prior = copy.deepcopy(policy.ROWS)
        prior.pop('python/int_arith')
        with self.assertRaisesRegex(ValueError, 'tracked rows changed'):
            gate.check_rows(prior, None)

    def test_cold_access_is_held_to_section_6_and_refuses_one_extra_read_or_write(self):
        # Section 6 by hand for the integer fixtures: d = 1, so a fully cold
        # point read or edit reads d + 2 = 3 pages; heap writes are 3; all
        # writes per seal are 3 + s + 2 = 6 with one run segment.
        row = dict(elements=200_000, reads=3, edit_reads=3, writes=3, total_writes=6,
                   path_depth=1, cold_leaf_pages=3125, directory_depth=1)
        for key in ('list', 'map'):
            gate.check_cold_access(key, row)
            for field, planted in (('reads', 4), ('edit_reads', 4), ('writes', 4), ('total_writes', 7)):
                with self.assertRaisesRegex(ValueError, f'cold {field} {planted} exceeds'):
                    gate.check_cold_access(key, dict(row, **{field: planted}))
            with self.assertRaisesRegex(ValueError, 'directory depth 2 differs'):
                gate.check_cold_access(key, dict(row, directory_depth=2))

    def test_unsupported_base_is_a_declared_no_comparison_that_still_holds_the_candidate(self):
        base = [dict(status='unsupported', error='cold point edit: canonical region reached an '
                     'unintegrated boundary at path [5] Commit')] * 5
        candidate = [dict(status='supported', bytes_written=6226, seal_instructions=1_562_000 + ns, seal_ns=ns)
                     for ns in (5, 1, 4, 2, 3)]
        checkpoint, ratios, errors = gate.checkpoint_outcome('list', base, candidate)
        self.assertEqual(checkpoint['outcome'], 'base unsupported: no comparison')
        self.assertEqual(checkpoint['baseline'], dict(bytes_written=6226, seal_instructions=1_562_003, seal_ns=3))
        self.assertEqual((ratios, errors), ({}, []))
        with self.assertRaisesRegex(ValueError, 'unqualified failure'):
            gate.checkpoint_outcome('list', [dict(status='unsupported', error='store fault')] * 5, candidate)


    # The seal cost is its instruction count. Same-binary seals spread about
    # 0.2% across processes (measured 16 Nov); wall time spread up to 60%.
    def seals(self, counts, ns=None):
        return [dict(status='supported', bytes_written=6251, seal_instructions=count, seal_ns=(ns or 300_000))
                for count in counts]

    def test_wall_clock_seal_time_gates_nothing(self):
        base = self.seals([1_562_082] * 5, ns=200_000)
        checkpoint, ratios, errors = gate.checkpoint_outcome('list', base, self.seals([1_562_082] * 5, ns=600_000))
        self.assertEqual(errors, [])
        self.assertEqual(ratios['seal_ns_information'], 3.0)

    def test_a_seal_one_percent_dearer_than_the_base_fails(self):
        base = self.seals([1_562_082] * 5)
        _, _, errors = gate.checkpoint_outcome('list', base, self.seals([round(1_562_082 * 1.01)] * 5))
        self.assertTrue(any('seal_instructions' in error for error in errors), errors)

    def test_seal_within_same_binary_spread_of_the_base_passes(self):
        # 0.18% above, as identical code measured across processes.
        base = self.seals([1_562_082, 1_562_089, 1_562_084, 1_564_813, 1_564_875])
        _, _, errors = gate.checkpoint_outcome('list', base, self.seals([1_564_900] * 5))
        self.assertEqual(errors, [])

    def test_a_noisy_candidate_seal_fails_even_without_a_comparable_base(self):
        base = [dict(status='unsupported', error='cold point edit: canonical region reached an '
                     'unintegrated boundary at path [5] Commit')] * 5
        noisy = self.seals([1_562_000, 1_562_000, 1_562_000, 1_562_000, 1_562_000 + 15_620])
        _, _, errors = gate.checkpoint_outcome('list', base, noisy)
        self.assertTrue(any('repeat noise' in error for error in errors), errors)

    # An instrument-only land is decided by its diff alone.
    PACKAGES = {'crates/lash', 'crates/lash-perf', 'crates/lash-durable'}
    INSTRUMENT = ['crates/lash-perf/src/compare/mod.rs', 'scripts/perf_land_gate.py', 'docs/perf.md']

    def test_one_product_path_makes_a_land_not_instrument_only(self):
        self.assertTrue(gate.instrument_only(self.INSTRUMENT, self.PACKAGES))
        for product in ('crates/lash/src/lib.rs', 'crates/lash-durable/src/page.rs'):
            self.assertFalse(gate.instrument_only([*self.INSTRUMENT, product], self.PACKAGES), product)

    def test_a_product_file_moved_into_the_perf_crate_is_not_instrument_only(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            def run(*args):
                return subprocess.run(["git", "-c", "user.name=t", "-c", "user.email=t@t", *args], cwd=root,
                                      check=True, capture_output=True, text=True).stdout.strip()
            run("init", "-q")
            for package in ("crates/lash-kernel", "crates/lash-perf"):
                (root / package / "src").mkdir(parents=True)
                (root / package / "Cargo.toml").write_text("[package]\nname = 'x'\n")
            (root / "crates/lash-kernel/src/engine.rs").write_text("".join(f"fn f{i}() {{}}\n" for i in range(40)))
            run("add", "-A"); run("commit", "-qm", "base")
            base = run("rev-parse", "HEAD")
            run("mv", "crates/lash-kernel/src/engine.rs", "crates/lash-perf/src/engine.rs")
            run("commit", "-qm", "move")
            with patch.object(gate, "ROOT", root):
                self.assertFalse(gate.instrument_only(*gate.land_paths(base, run("rev-parse", "HEAD"))))

    def test_workload_baseline_ceiling_pin_manifest_or_build_change_is_not_instrument_only(self):
        for fixed in ('crates/lash-perf/src/compare/workloads/int_arith.py',
                      'crates/lash-perf/src/compare/workloads.rs',
                      'crates/lash-perf/src/boundary/kernel_cold_workload.rs',
                      'crates/lash-perf/fixtures/heap-baseline.txt', 'scripts/perf_land_ceilings.py',
                      'scripts/build-compare-references.py', 'crates/lash-perf/Cargo.toml', 'Cargo.toml',
                      'Cargo.lock', 'rust-toolchain.toml', 'tools/buck2/lash_rust.bzl', '.buckconfig',
                      '.cargo/config.toml', 'crates/lash-perf/BUCK'):
            self.assertFalse(gate.instrument_only([*self.INSTRUMENT, fixed], self.PACKAGES), fixed)

    # Every row at main's median under its ceiling (ceiling / factor) and
    # 1.9x Monty, five equal repeats a side; `monty` sets one side's
    # reference ratio of a row: {(side, row): ratio}.
    @staticmethod
    def population(sides, plant=None, monty=None):
        samples = []
        for side in sides:
            for key, row in policy.ROWS.items():
                for repeat in range(5):
                    secant = row['ceiling'] / policy.CEILING_FACTOR
                    reference = secant / (monty or {}).get((side, key), 1.9)
                    if plant:
                        secant = plant(side, key, repeat, secant)
                    samples.append(dict(side=side, row=key, repeat=repeat, secant=secant, results=[1, 2],
                                        reference_secants={'monty': reference} if key.startswith('python/') else {}))
        return samples

    def test_instrument_only_land_still_fails_a_noisy_candidate_row(self):
        noisy = self.population(('base', 'candidate'), lambda side, key, repeat, secant:
                                secant * (1.02 if (side, key, repeat) == ('candidate', 'typescript/int_arith', 3) else 1))
        _, errors, _, _ = gate.instrument_verdict(noisy, self.population(('old',)))
        self.assertTrue(any(e.startswith('candidate/typescript/int_arith: repeat noise') for e in errors), errors)

    def test_instrument_only_land_still_fails_an_over_ceiling_row(self):
        over = self.population(('base', 'candidate'), lambda side, key, repeat, secant:
                               policy.ROWS[key]['ceiling'] * 1.01 if key == 'typescript/int_arith' else secant)
        _, errors, _, _ = gate.instrument_verdict(over, self.population(('old',)))
        self.assertTrue(any(e.startswith('candidate/typescript/int_arith: median') and 'exceeds ceiling' in e
                            for e in errors), errors)

    def test_old_instrument_noise_is_recorded_and_does_not_fail_the_land(self):
        # The land 2 base repeats of python/string_build1 differ by 2.9%.
        old = self.population(('old',), lambda side, key, repeat, secant:
                              secant * (1, 1.0291, 1, .9999, 1)[repeat] if key == 'python/string_build1' else secant)
        _, errors, _, record = gate.instrument_verdict(self.population(('base', 'candidate')), old)
        self.assertEqual(errors, [])
        self.assertGreater(record['rows']['python/string_build1']['old_noise'], .01)

    def test_instrument_only_mode_leaves_the_boundary_rows_on_the_checkpoint_base(self):
        # The pinned checkpoint base cannot do the cold point edit, so today's
        # boundary rows make no comparison; the instrument-only mode must not
        # switch one on.
        rows = {'checkpoint': [dict(status='unsupported', error='cold point edit: canonical region reached an '
                                    'unintegrated boundary at path [5] Commit')] * 5,
                'candidate': [dict(status='supported', bytes_written=6251, seal_instructions=1_562_000, seal_ns=ns)
                              for ns in (5, 1, 4, 2, 3)]}
        outcomes = [gate.checkpoint_outcome('list', *(rows[side] for side in
                                                      gate.boundary_pair('checkpoint', 'candidate', instrument)))
                    for instrument in (False, True)]
        self.assertEqual(outcomes[0], outcomes[1])
        self.assertEqual(outcomes[1][0]['outcome'], 'base unsupported: no comparison')

    # Job D: the candidate against the base per row, a reviewed way past it,
    # and ceilings that move down by themselves and up only under review.
    ROW = 'typescript/int_arith'

    def regressed(self, factor):
        return self.population(('base', 'candidate'), lambda side, key, repeat, secant:
                               secant * factor if (side, key) == ('candidate', self.ROW) else secant)

    def replacement(self, **changes):
        return {**dict(previous_sha256='0' * 64, reviewer='r60 lead', added_rows=[], replaced_ceilings=[],
                       changed_fields=[]), **changes}

    def test_a_one_and_a_half_percent_regression_on_one_row_is_refused(self):
        # Inside its ceiling (main x 1.03), so only the base comparison sees it.
        _, errors, _ = gate.judge(self.regressed(1.015), False)
        self.assertEqual([e.split(':')[0] for e in errors], [f'candidate/{self.ROW}'], errors)
        self.assertIn('base', errors[0])

    def test_the_same_regression_with_a_reviewed_ceiling_replacement_passes_and_is_printed(self):
        replacement = self.replacement(replaced_ceilings=[self.ROW])
        prior = copy.deepcopy(policy.ROWS)
        prior[self.ROW]['ceiling'] /= 1.05
        gate.check_rows(prior, replacement)
        with contextlib.redirect_stdout(io.StringIO()) as out:
            _, errors, notes = gate.judge(self.regressed(1.015), False, exempt=gate.exemptions(replacement))
        self.assertEqual(errors, [])
        self.assertTrue(any(self.ROW in line and 'r60 lead' in line and 'exempt' in line
                            for line in out.getvalue().splitlines()), out.getvalue())

    def test_an_unreviewed_raised_ceiling_is_refused(self):
        prior = copy.deepcopy(policy.ROWS)
        prior[self.ROW]['ceiling'] /= 1.05
        with self.assertRaisesRegex(ValueError, f'{self.ROW}: ceiling'):
            gate.check_rows(prior, None)
        # A replacement that does not name the row does not cover it.
        with self.assertRaisesRegex(ValueError, f'{self.ROW}: ceiling'):
            gate.check_rows(prior, self.replacement(replaced_ceilings=['python/int_arith']))

    def test_a_hand_lowered_ceiling_is_refused(self):
        prior = copy.deepcopy(policy.ROWS)
        prior[self.ROW]['ceiling'] *= 1.05
        with self.assertRaisesRegex(ValueError, f'{self.ROW}: ceiling'):
            gate.check_rows(prior, None)

    def ratcheted(self, factor):
        rows, errors, _ = gate.judge(self.regressed(factor), False)
        self.assertEqual(errors, [])
        source = (gate.ROOT / 'scripts/perf_land_ceilings.py').read_text()
        lowered = gate.apply_ratchet(source, gate.ratchet(rows))
        return rows, {n.targets[0].id: ast.literal_eval(n.value) for n in ast.parse(lowered).body
                      if isinstance(n, ast.Assign)}

    def test_a_ten_percent_improvement_lowers_the_ceiling_in_the_land(self):
        rows, lowered = self.ratcheted(.9)
        expected = copy.deepcopy(policy.ROWS)
        expected[self.ROW]['ceiling'] = rows[f'candidate/{self.ROW}']['median'] * 1.03
        self.assertEqual(lowered['ROWS'], expected)

    def test_a_one_percent_improvement_does_not_move_the_ceiling(self):
        _, lowered = self.ratcheted(.99)
        self.assertEqual(lowered['ROWS'], policy.ROWS)


    # Job E: the binding 3x rule holds every PY row unless a reviewed
    # replacement lists it as exempt, and a stale exemption is refused.
    EXEMPT = 'python/records'

    def test_a_formerly_unbound_row_over_three_times_the_reference_is_refused(self):
        policy.MONTY_EXEMPT_ROWS.update(EXEMPT_ROWS)
        with self.assertRaisesRegex(ValueError, 'exceeds 3.00x'):
            gate.check_instruction_ceiling('candidate', 'python/object_sort', 301, 1000, 100)

    def test_a_python_row_added_later_is_bound_without_being_listed(self):
        policy.MONTY_EXEMPT_ROWS.update(EXEMPT_ROWS)
        with self.assertRaisesRegex(ValueError, 'exceeds 3.00x'):
            gate.check_instruction_ceiling('candidate', 'python/added_later', 301, 1000, 100)

    def test_the_exempt_row_over_three_times_passes_and_is_printed(self):
        policy.MONTY_EXEMPT_ROWS.update(EXEMPT_ROWS)
        main = {(side, self.EXEMPT): 5.29 for side in ('base', 'candidate')}
        with contextlib.redirect_stdout(io.StringIO()) as out:
            _, errors, _ = gate.judge(self.population(('base', 'candidate'), monty=main), False)
        self.assertEqual(errors, [])
        lines = [line for line in out.getvalue().splitlines() if f'candidate/{self.EXEMPT}' in line]
        self.assertTrue(any('exempt' in line and 'report-only 2.00x' in line for line in lines), lines)

    # Job F2: a land is held to what its candidate produces; main's base
    # side stays at 5.29x in the land that fixes the row.
    FIXED = {('base', 'python/records'): 5.29, ('candidate', 'python/records'): 2.9}

    def test_an_exemption_whose_candidate_is_at_or_under_three_times_is_stale_whatever_the_base(self):
        policy.MONTY_EXEMPT_ROWS.update(EXEMPT_ROWS)
        _, errors, _ = gate.judge(self.population(('base', 'candidate'), monty=self.FIXED), False)
        self.assertEqual(len(errors), 1, errors)
        self.assertTrue(errors[0].startswith(f'{self.EXEMPT}: exemption no longer needed: remove it'), errors)

    def test_the_land_removing_an_exemption_passes_on_its_candidate_whatever_the_base(self):
        # The exempt list is empty here, as the removing land leaves it.
        with contextlib.redirect_stdout(io.StringIO()) as out:
            _, errors, _ = gate.judge(self.population(('base', 'candidate'), monty=self.FIXED), False)
        self.assertEqual(errors, [])
        self.assertIn(f'perf base/{self.EXEMPT}: 5.290000x Monty, information only', out.getvalue())


class NoSecondCache(unittest.TestCase):
    """The gate runs only against the fork's store: without the fork
    environment it refuses, naming the variable and the fix, and builds
    nothing in a second cache (FIG-7694, job G1)."""

    def refusal(self, store):
        with tempfile.TemporaryDirectory() as home:
            env = {k: v for k, v in __import__('os').environ.items() if k != 'LASH_BUCK2_STORE'}
            env['HOME'] = home
            if store is not None:
                env['LASH_BUCK2_STORE'] = store
            argv = ['perf_land_gate.py', '--out', f'{home}/out',
                    '--base-artifact', f'{home}/base.json', '--candidate-artifact', f'{home}/candidate.json']
            with patch.dict('os.environ', env, clear=True), patch.object(sys, 'argv', argv), \
                    contextlib.redirect_stdout(io.StringIO()), self.assertRaises(SystemExit) as refused:
                gate.main()
            self.assertFalse(Path(home, '.cache').exists())
        return str(refused.exception)

    def test_the_gate_refuses_without_the_fork_store(self):
        message = self.refusal(None)
        self.assertIn('LASH_BUCK2_STORE is unset', message)
        self.assertIn('. ./env.sh', message)

    def test_the_gate_refuses_a_store_that_is_not_an_absolute_path(self):
        message = self.refusal('off')
        self.assertIn("LASH_BUCK2_STORE is 'off'", message)
        self.assertIn('. ./env.sh', message)

if __name__ == '__main__':
    unittest.main()

"""Regression laws at kernel_perf's actual process boundary."""
import csv
import copy
import hashlib
import json
import os
from pathlib import Path
import subprocess
import unittest

from summarize import summarize, validate
import embedded


class Comparison(unittest.TestCase):
    def receipt(self):
        return dict(host='test-host', recorded_at='2026-10-13T00:00:00Z', rows=[dict(
            program='baseline', artifact_sha256='a' * 64, workload_sha256='b' * 64,
            result_sha256='c' * 64, observe='nothing', compiler='test-compiler',
            build_profile='test-profile', debug_assertions=False, allocator='system+stats_alloc',
            split=False, fixed_iterations=True, cap=1, iterations=1, warmups=1, rounds=1,
            samples=[dict(iteration=0, round=1, ns_run=10, ns_start_and_run=20,
                allocations_run=1, allocations_start_and_run=2, reallocations_run=0,
                reallocations_start_and_run=0, charged=3)])])

    def test_comparison_refuses_mismatched_result_digest(self):
        baseline = self.receipt()
        current = copy.deepcopy(baseline)
        current['rows'][0]['result_sha256'] = 'd' * 64
        with self.assertRaisesRegex(ValueError, 'result_sha256 identity mismatch'):
            summarize(baseline, current)

    def test_comparison_refuses_zero_samples(self):
        baseline = self.receipt()
        current = copy.deepcopy(baseline)
        current['rows'][0]['samples'] = []
        with self.assertRaisesRegex(ValueError, 'incomplete iteration population'):
            summarize(baseline, current)

    def test_comparison_refuses_missing_or_observed_identity(self):
        for field, value in [('observe', None), ('observe', 'work'),
                             ('observe', 'context'), ('result_sha256', None),
                             ('result_sha256', 'not-a-digest')]:
            with self.subTest(field=field, value=value):
                baseline = self.receipt()
                current = copy.deepcopy(baseline)
                if value is None:
                    del current['rows'][0][field]
                else:
                    current['rows'][0][field] = value
                with self.assertRaises(ValueError):
                    summarize(baseline, current)


class EmbeddedMemoryPolicy(unittest.TestCase):
    def test_memory_refusal_cannot_enter_a_performance_average(self):
        # FIG-6731: a complete population containing a memory refusal is a
        # failed comparison, even if its measurements and peers are present.
        rows = [dict(program='allocation_churn', engine=engine, pair=pair)
                for engine in ['lash-ts', 'quickjs', 'lash-py', 'monty', 'node', 'cpython']
                for pair in [1, 2]]
        for row in rows:
            row.update(result=1, result_sha256=embedded.digest(1), instructions=1,
                       wall_ns=1, cold_first_line_ns=1, peak_rss_kib=1)
        rows[0]['failure'] = 'BoundExceeded { bound: Memory, limit: 1073741824 }'
        receipt = dict(programs=['allocation_churn'], samples=rows)
        with self.assertRaisesRegex(ValueError, 'allocation_churn/lash-ts: .*Memory'):
            embedded.validate(receipt)


class Selection(unittest.TestCase):
    def test_canonical_result_and_observe_identity(self):
        text = 'user=sam;attempt=3;active=alpha:1,gamma:3;total=6;count=3;seen=3;indexes=4'
        expected = hashlib.sha256(json.dumps({'text': text}, sort_keys=True,
            separators=(',', ':'), ensure_ascii=False).encode()).hexdigest()
        receipts = []
        for observe in ['', '', 'work', 'context']:
            with self.subTest(observe=observe):
                result = subprocess.run([os.environ['KERNEL_PERF_BINARY'], '1', 'baseline'],
                    env=dict(os.environ, KERNEL_PERF_SPLIT='0', KERNEL_PERF_FIXED='1',
                             KERNEL_PERF_OBSERVE=observe),
                    capture_output=True, text=True, check=True)
                row = json.loads(list(csv.DictReader(result.stdout.splitlines(), delimiter='\t'))[0]['receipt'])
                self.assertEqual(row['observe'], observe or 'nothing')
                self.assertEqual(row['result_sha256'], expected)
                if observe:
                    with self.assertRaisesRegex(ValueError, 'unobserved identity'):
                        validate(row)
                else:
                    validate(row)
                receipts.append(row)
        Path('.kiln/FIG-6135/kernel-aa-identity.json').write_text(json.dumps(dict(
            expected_result_sha256=expected, receipts=receipts), indent=2) + '\n')

    def test_zero_accepted_guest_population_is_insufficient(self):
        directory = Path('.kiln/FIG-6135/empty-guest')
        result = subprocess.run([os.environ['KERNEL_PERF_BINARY'], 'guest', str(directory), 'baseline'],
            env=dict(os.environ, KERNEL_PERF_GUEST_SECONDS='0', KERNEL_PERF_GUEST_SAMPLES='0',
                     KERNEL_PERF_OBSERVE=''), capture_output=True, text=True, check=False)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('zero accepted samples', result.stderr)
        with (directory / 'guest-completeness.tsv').open() as source:
            row = next(csv.DictReader(source, delimiter='\t'))
        self.assertEqual(row['accepted'], '0')
        self.assertEqual(row['adds_up'], 'true')
        self.assertEqual(row['population'], 'insufficient')
        self.assertEqual((directory / 'guest-symbols.tsv').read_text().count('\n'), 1)

    def test_empty_selection_exits_nonzero(self):
        result = subprocess.run(
            [os.environ['KERNEL_PERF_BINARY'], '3', 'no-such-kernel-program'],
            capture_output=True, text=True, check=False,
        )
        self.assertNotEqual(result.returncode, 0, result.stdout)

    def test_refused_selection_exits_nonzero(self):
        source = Path('.kiln/FIG-5879/refused.ts')
        source.write_text('import unsupported from "nowhere";')
        result = subprocess.run(
            [os.environ['KERNEL_PERF_BINARY'], '3', str(source)],
            capture_output=True, text=True, check=False,
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('refused', result.stderr)

    def test_partial_selection_exits_nonzero(self):
        result = subprocess.run(
            [os.environ['KERNEL_PERF_BINARY'], '3', 'baseline', 'no-such-kernel-program'],
            capture_output=True, text=True, check=False,
        )
        self.assertNotEqual(result.returncode, 0)

    def test_receipt_retains_every_timed_iteration(self):
        binary = os.environ['KERNEL_PERF_BINARY']
        result = subprocess.run([binary, '3', 'baseline'], env=dict(os.environ,
            KERNEL_PERF_SPLIT='0', KERNEL_PERF_FIXED='1'),
            capture_output=True, text=True, check=True)
        rows = list(csv.DictReader(result.stdout.splitlines(), delimiter='\t'))
        self.assertEqual(len(rows), 1)
        raw = json.loads(rows[0]['receipt'])
        self.assertEqual(raw['artifact_sha256'], hashlib.sha256(Path(binary).read_bytes()).hexdigest())
        self.assertEqual([sample['iteration'] for sample in raw['samples']], [0, 1, 2])
        for sample in raw['samples']:
            self.assertEqual(sample['charged'], int(rows[0]['charged']))
            self.assertGreater(sample['ns_run'], 0)
            self.assertGreaterEqual(sample['ns_start_and_run'], sample['ns_run'])
            self.assertGreater(sample['allocations_run'], 0)

    def test_comparison_refuses_empty_baseline(self):
        baseline = Path('.kiln/FIG-5879/empty-baseline.json')
        baseline.write_text('{}')
        result = subprocess.run([
            'python3', 'scripts/perf/kernel/measure.py', '--kernel',
            os.environ['KERNEL_PERF_BINARY'], '--baseline', str(baseline),
            '--out', '.kiln/FIG-5879/empty-baseline-comparison',
            '--iterations', '3', '--rounds', '1', 'baseline',
        ], capture_output=True, text=True, check=False)
        self.assertNotEqual(result.returncode, 0, result.stdout)


if __name__ == '__main__':
    unittest.main()

"""Regression laws at kernel_perf's actual process boundary."""
import csv
import hashlib
import json
import os
from pathlib import Path
import subprocess
import unittest


class Selection(unittest.TestCase):
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

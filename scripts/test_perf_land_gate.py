"""Rule-derived rejection witnesses for the cutover gate."""
import copy
from pathlib import Path
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parent))
import perf_land_gate as gate
import perf_land_ceilings as policy


class GateLaws(unittest.TestCase):
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
            gate.check_rows(prior, 'unreviewed policy', False)

    def test_cold_access_is_held_to_section_6_and_refuses_one_extra_read_or_write(self):
        # Section 6 by hand for the integer fixtures: d = 1, so a fully cold
        # point read or edit reads d + 2 = 3 pages; heap writes are 3; all
        # writes per seal are 3 + s + 2 = 6 with one run segment.
        row = dict(elements=200_000, reads=3, edit_reads=3, writes=3, total_writes=6,
                   path_depth=1, cold_leaf_pages=3125)
        for key in ('list', 'map'):
            gate.check_cold_access(key, row)
            for field, planted in (('reads', 4), ('edit_reads', 4), ('writes', 4), ('total_writes', 7)):
                with self.assertRaisesRegex(ValueError, f'cold {field} {planted} exceeds'):
                    gate.check_cold_access(key, dict(row, **{field: planted}))

    def test_unsupported_base_is_a_declared_no_comparison_that_still_holds_the_candidate(self):
        base = [dict(status='unsupported', error='cold point edit: canonical region reached an '
                     'unintegrated boundary at path [5] Commit')] * 5
        candidate = [dict(status='supported', bytes_written=6226, seal_ns=ns) for ns in (5, 1, 4, 2, 3)]
        checkpoint, ratios, errors = gate.checkpoint_outcome('list', base, candidate)
        self.assertEqual(checkpoint['outcome'], 'base unsupported: no comparison')
        self.assertEqual(checkpoint['baseline'], dict(bytes_written=6226, seal_ns=3))
        self.assertEqual((ratios, errors), ({}, []))
        with self.assertRaisesRegex(ValueError, 'unqualified failure'):
            gate.checkpoint_outcome('list', [dict(status='unsupported', error='store fault')] * 5, candidate)


if __name__ == '__main__':
    unittest.main()

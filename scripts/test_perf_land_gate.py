"""Rule-derived rejection witnesses for the cutover gate."""
import copy
from pathlib import Path
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parent))
import perf_land_gate as gate
import perf_land_ceilings as policy


class GateLaws(unittest.TestCase):
    def test_hard_monty_refuses_201_even_inside_own_ceiling(self):
        with self.assertRaises(ValueError):
            gate.check_instruction_ceiling('candidate', 'python/int_arith', 201, 1000, 100)
        gate.check_instruction_ceiling('candidate', 'python/int_arith', 200, 1000, 100)

    def test_rows_cannot_change_without_the_exact_reviewed_predecessor(self):
        prior = copy.deepcopy(policy.ROWS)
        prior.pop('python/int_arith')
        with self.assertRaisesRegex(ValueError, 'tracked rows changed'):
            gate.check_rows(prior, 'unreviewed policy', False)

    def test_cold_access_refuses_fourth_page(self):
        row = dict(elements=200_000, reads=4, edit_reads=3, writes=3, path_depth=2, cold_leaf_pages=4)
        with self.assertRaises(ValueError):
            gate.check_cold_access('list', row)
        gate.check_cold_access('list', dict(row, reads=3))

    def test_point_edit_is_bounded_by_its_tree_path_and_one_leaf(self):
        row = dict(elements=200_000, reads=3, edit_reads=4, writes=4,
                   path_depth=3, cold_leaf_pages=8)
        gate.check_cold_access('list', row)
        with self.assertRaises(ValueError):
            gate.check_cold_access('list', dict(row, writes=5))


if __name__ == '__main__':
    unittest.main()

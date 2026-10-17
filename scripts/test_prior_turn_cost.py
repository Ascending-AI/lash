"""Laws for the profilesmash observation-unit and identity rules (FIG-6623)."""
import copy
import json
import tempfile
import unittest
from pathlib import Path

import prior_turn_cost as reader


class PriorTurnCostLaws(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        (Path(__file__).resolve().parents[1] / '.kiln/FIG-6623').mkdir(parents=True, exist_ok=True)

    def test_insufficient_distinct_depths_stay_unknown_and_sweep_uncertainty_is_visible(self):
        # Unknown is absent, never a measured flat slope (audit authoring rule 2).
        self.assertIsNone(reader.slopes([(3, 12), (3, 14)])['least_squares'])
        points = [(x, 10 + 7*x + (1000 if x == 4 else 0)) for x in range(6)]
        result = reader.slopes(points)
        self.assertEqual(result['n'], 6)
        self.assertEqual(result['distinct_depths'], 6)
        self.assertEqual(result['theil_sen'], 7)
        self.assertNotEqual(result['least_squares'], 7)
        self.assertGreater(result['least_squares_standard_error'], 0)
        self.assertLess(result['pairwise_slope_range'][0], 7)
        self.assertGreater(result['pairwise_slope_range'][1], 7)

    def captures(self, root):
        paths = []
        for depth in (1, 3, 5):
            capture = root / str(depth)
            capture.mkdir()
            configuration = dict(observer='off+DHAT', backend='sqlite-file', worker='worker-sha',
                                 window=[depth+1, depth+1], useful_work='same-script', executables='parent-sha',
                                 terminal='all scripted replies consumed; all turns answered; clean shutdown',
                                 script_digest='script-sha', producer={'build': 'same'})
            rows = [dict(turn=t, history_depth=t-1, frame_depth=t-1, sessions=1,
                         metrics={'allocated_bytes': 10+7*(t-1)}) for t in range(1, depth+2)]
            compactions = [dict(turn=t, history_depth=t, frame_depth=2, sessions=1,
                                metrics={'allocated_bytes': 1000000*t}) for t in range(2, depth+1, 2)]
            observations = []
            for mode in ('off', 'dhat'):
                directory = capture / mode
                directory.mkdir()
                payload = dict(identity={'build': 'same'}, workloads=[dict(
                    backend='sqlite-file', script_digest='script-sha', rounds=rows,
                    compactions=compactions, units={'allocated_bytes': 'bytes'})])
                (directory / 'replay.json').write_text(json.dumps(payload))
                observations.append(dict(mode=mode, exit_code=0, identity=payload['identity'], instructions_u=123))
            profile = dict(ftbl=['source:10'], pps=[dict(fs=[0], tb=100+depth, eb=10, gb=20)])
            (capture / 'dhat/parent.dhat.json').write_text(json.dumps(profile))
            receipt = dict(kind='lash.replay-heap', observations=observations, turns=depth+1,
                           profile_turn=depth+1, profile_through_turn=depth+1,
                           window_receipt=dict(completed=True, role='replay-host', process_id=123,
                                               first_turn=depth+1, last_turn=depth+1))
            receipt['experiment'] = reader.experiment_identity(configuration, observations, receipt['window_receipt'])
            (capture / 'receipt.json').write_text(json.dumps(receipt))
            paths.append(capture)
        return paths

    def test_compactions_are_a_separate_population_and_window_live_is_not_retained_history(self):
        # Audit rule 2: distinct observation units cannot pollute each other.
        with tempfile.TemporaryDirectory(dir=Path(__file__).resolve().parents[1] / '.kiln/FIG-6623') as temp:
            paths = self.captures(Path(temp))
            prior, compaction = reader.read_series(paths)
            self.assertEqual(prior['metrics']['allocated_bytes']['history']['theil_sen'], 7)
            self.assertEqual(compaction['metrics']['allocated_bytes']['history']['theil_sen'], 1000000)
            self.assertEqual(len(compaction['samples']), 2)  # one history, without weighting early depths by extra prefix runs
            self.assertIsNone(prior['retained_history_live_bytes'])
            self.assertEqual(prior['source_sites'][0]['quantities']['allocated_traffic_bytes']['theil_sen'], 1)
            self.assertEqual(prior['source_sites'][0]['quantities']['window_end_live_bytes']['theil_sen'], 0)

    def test_missing_or_mixed_observer_worker_window_and_terminal_refuse_comparison(self):
        # Audit rule 1: identity survives through the final reader.
        with tempfile.TemporaryDirectory(dir=Path(__file__).resolve().parents[1] / '.kiln/FIG-6623') as temp:
            paths = self.captures(Path(temp))
            with self.assertRaisesRegex(ValueError, 'duplicate'):
                reader.read_series([paths[0], paths[0]])
            path = paths[-1] / 'receipt.json'
            original = json.loads(path.read_text())
            for field in ('observer', 'worker', 'window', 'terminal', 'backend'):
                for value in (None, 'other'):
                    altered = copy.deepcopy(original)
                    configuration = altered['experiment']['configuration']
                    configuration[field] = value
                    altered['experiment'] = reader.experiment_identity(
                        configuration, altered['observations'], altered['window_receipt'])
                    path.write_text(json.dumps(altered))
                    with self.subTest(field=field, value=value), self.assertRaises(ValueError):
                        reader.read_series(paths)
            path.write_text(json.dumps(original))


if __name__ == '__main__':
    unittest.main()

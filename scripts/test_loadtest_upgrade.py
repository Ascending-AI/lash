#!/usr/bin/env python3
"""The rolling-upgrade campaign's decisions (FIG-3805 phase B), over the
`lashctl --json` envelopes and refusal texts the two builds print."""
import json
import unittest

import loadtest_faults as faults
import loadtest_upgrade as upgrade


def envelope(command, result=None, error=None):
    return json.dumps({'schema_version': 1, 'command': command, 'result': result, 'error': error})


class LashctlTest(unittest.TestCase):
    def test_an_envelope_must_answer_its_verb(self):
        body = upgrade.lashctl_answer('preflight', 0, envelope('preflight', {'outcome': 'ready'}))
        self.assertEqual(body['result'], {'outcome': 'ready'})
        with self.assertRaises(faults.FaultFailed):
            upgrade.lashctl_answer('finalize', 0, envelope('preflight'))
        with self.assertRaises(faults.FaultFailed):
            upgrade.lashctl_answer('drain', 1, 'lashctl: connection refused')

    def test_the_typed_refusal_is_read_from_the_error(self):
        refused = json.loads(envelope('finalize', error={
            'message': 'N is registered', 'refusal': {'refusal': 'deployments_retained'}}))
        self.assertEqual(upgrade.refusal(refused), 'deployments_retained')
        self.assertIsNone(upgrade.refusal(json.loads(envelope('finalize', {}))))


class FinalizeTest(unittest.TestCase):
    def test_finalize_moves_f_with_every_backfill_applied(self):
        flip = {'outcome': 'finalized', 'from': 1, 'to': 2}
        self.assertTrue(upgrade.finalized({'flip': flip, 'backfills': [{'state': 'applied'}]}))
        self.assertFalse(upgrade.finalized({'flip': flip, 'backfills': []}))
        self.assertFalse(upgrade.finalized({'flip': flip, 'backfills': [{'state': 'running'}]}))
        self.assertFalse(upgrade.finalized({'flip': {'outcome': 'already_finalized', 'from': 2, 'to': 2},
                                            'backfills': [{'state': 'applied'}]}))


class DrainTest(unittest.TestCase):
    STATUS = {'drained': True, 'stalled': [], 'stalled_obligations': {}}

    def test_drained_needs_the_store_the_engine_and_no_stalled_obligation(self):
        self.assertTrue(upgrade.drained(self.STATUS, 0))
        self.assertFalse(upgrade.drained(self.STATUS, 1))
        self.assertFalse(upgrade.drained({**self.STATUS, 'drained': False}, 0))
        self.assertFalse(upgrade.drained({**self.STATUS, 'stalled_obligations': {'ingress': 1}}, 0))
        self.assertFalse(upgrade.drained({**self.STATUS, 'stalled': [{'kind': 'ingress'}]}, 0))


class SessionsTest(unittest.TestCase):
    def test_every_session_must_answer_after_a_step(self):
        self.assertEqual(upgrade.sessions_through({'0', '1', '2'}, {'0', '1', '2', '3'}), [])
        self.assertEqual(upgrade.sessions_through({'0', '1', '2'}, {'1'}), ['0', '2'])


class ObjectsTest(unittest.TestCase):
    def test_the_ledger_keeps_counts_not_every_object_key(self):
        preflight = {'upgraded': False, 'families': [{
            'service': 'LashDurableWaitIndex', 'component': 'restate-durable-wait-registry', 'newest': 2, 'objects': 55,
            'pending': [{'key': f'k{index}', 'format': 1} for index in range(40)]}]}
        self.assertEqual(upgrade.objects_summary(preflight), {'upgraded': False, 'families': [
            {'service': 'LashDurableWaitIndex', 'newest': 2, 'objects': 55, 'pending': 40}]})
        sweep = {'swept': [{'key': f'k{index}'} for index in range(300)], 'remaining': []}
        self.assertEqual(upgrade.objects_summary(sweep), {'swept': 300, 'remaining': [], 'remaining_total': 0})


class FenceTest(unittest.TestCase):
    def test_the_writer_fence_is_told_apart_from_other_failures(self):
        self.assertTrue(upgrade.fenced(
            'writer fenced: the fleet epoch is 2, outside this build\'s writable range [1, 1]; '
            'this deployment takes no more work.'))
        self.assertFalse(upgrade.fenced('connection reset by peer'))
        self.assertFalse(upgrade.fenced('{"marked":true}'))

    def test_a_refused_store_names_the_reader_floor_or_the_fleet_epoch(self):
        self.assertTrue(upgrade.refused_store(
            'Error: connect Postgres storage\n\nCaused by:\n    lash_core_store is at version 3 with reader '
            'floor 2, above the newest this build reads ([1, 1]): a newer release contracted it.'))
        self.assertTrue(upgrade.refused_store(
            'store records fleet epoch 2, outside this build\'s writable range [1, 1]'))
        self.assertFalse(upgrade.refused_store('listening on 0.0.0.0:18200'))


class ChoreographyTest(unittest.TestCase):
    def test_every_generation_is_a_build_and_rollback_returns_n(self):
        self.assertEqual(upgrade.STEPS, ('half-roll', 'rollback', 'roll', 'finalize', 'fence'))
        self.assertEqual(upgrade.GENERATIONS['initial'], upgrade.GENERATIONS['rollback'])
        self.assertEqual(upgrade.GENERATIONS['next'], upgrade.GENERATIONS['final'])
        self.assertNotEqual(upgrade.GENERATIONS['initial'], upgrade.GENERATIONS['next'])
        # Every generation name fits the chart's 12-character limit.
        self.assertTrue(all(len(name) <= 12 for name in upgrade.GENERATIONS))


if __name__ == '__main__':
    unittest.main()

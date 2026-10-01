#!/usr/bin/env python3
"""The fault controller's decisions (FIG-4169), over recorded shapes of the
pinned Restate 1.7.12 cluster tables and Kubernetes pod status."""
import unittest

import loadtest_faults as faults


def partition(partition_id, node, epoch, lsn, mode='Leader'):
    return {'partition_id': partition_id, 'plain_node_id': node, 'gen_node_id': f'{node}:2',
            'effective_mode': mode, 'leader_epoch': epoch, 'applied_log_lsn': lsn}


NODES = [
    {'plain_node_id': 'N1', 'gen_node_id': 'N1:2', 'name': 'lash-loadtest-restate-0', 'state': 'alive'},
    {'plain_node_id': 'N2', 'gen_node_id': 'N2:2', 'name': 'lash-loadtest-restate-2', 'state': 'alive'},
    {'plain_node_id': 'N3', 'gen_node_id': 'N3:1', 'name': 'lash-loadtest-restate-1', 'state': 'alive'},
]


def pod(uid, restarts, ready=True, exit_code=None):
    status = {'name': 'worker', 'restartCount': restarts, 'ready': ready, 'lastState': {}}
    if exit_code is not None:
        status['lastState'] = {'terminated': {'exitCode': exit_code}}
    return {'metadata': {'uid': uid, 'name': 'lash-loadtest-worker-0-initial-abc'},
            'status': {'podIP': '10.0.0.7', 'containerStatuses': [status]}}


class RestatectlTest(unittest.TestCase):
    def test_rows_follow_the_row_count_line(self):
        output = '2 rows. Query took 5ms\n[{"partition_id":0},{"partition_id":1}]'
        self.assertEqual(faults.restatectl_rows(output), [{'partition_id': 0}, {'partition_id': 1}])
        with self.assertRaises(ValueError):
            faults.restatectl_rows('0 rows. Query took 5ms')

    def test_a_partition_with_two_leaders_is_refused(self):
        with self.assertRaises(ValueError):
            faults.leaders([partition(0, 'N1', 2, 5), partition(0, 'N2', 3, 5)])


class RestateTargetTest(unittest.TestCase):
    def test_the_leader_with_advancing_partitions_is_chosen(self):
        before = [partition(0, 'N1', 2, 10), partition(1, 'N2', 2, 10), partition(2, 'N2', 2, 10),
                  partition(0, 'N2', 2, 10, 'Follower')]
        after = [partition(0, 'N1', 2, 10), partition(1, 'N2', 2, 12), partition(2, 'N2', 2, 10),
                 partition(0, 'N2', 2, 10, 'Follower')]
        target = faults.choose_restate_target(before, after, NODES)
        self.assertEqual((target.node, target.pod, target.generation), ('N2', 'lash-loadtest-restate-2', 'N2:2'))
        self.assertEqual(target.led, (1, 2))
        self.assertEqual(target.advancing, (1,))
        self.assertEqual(target.epochs, {1: 2, 2: 2})

    def test_idle_leaders_or_moved_leadership_are_not_busy(self):
        idle = [partition(0, 'N1', 2, 10), partition(1, 'N2', 2, 10)]
        self.assertIsNone(faults.choose_restate_target(idle, idle, NODES))
        moved = [partition(0, 'N2', 3, 11)]
        self.assertIsNone(faults.choose_restate_target([partition(0, 'N1', 2, 10)], moved, NODES))

    def test_recovery_needs_the_same_identity_a_new_generation_and_reelected_partitions(self):
        target = faults.RestateTarget('N2', 'N2:2', 'lash-loadtest-restate-2', (1, 2), (1,), {1: 2, 2: 2})
        restarted = [dict(NODES[0]), {**NODES[1], 'gen_node_id': 'N2:3'}, dict(NODES[2])]
        led = [partition(0, 'N1', 2, 20), partition(1, 'N1', 3, 20), partition(2, 'N3', 3, 20)]
        evidence = faults.restate_recovery(target, 3, led, restarted)
        self.assertEqual(evidence['generation_after'], 'N2:3')
        self.assertEqual((evidence['leaders'], evidence['partitions'], evidence['epochs_bumped']), (3, 3, 2))
        self.assertEqual(evidence['leaders_on_restarted'], 0)
        # Not yet restarted, a partition without a leader, a stale epoch, a
        # node down, or a different pod under the node ID: not recovered.
        self.assertIsNone(faults.restate_recovery(target, 3, led, NODES))
        self.assertIsNone(faults.restate_recovery(target, 3, led[:2], restarted))
        stale = [partition(0, 'N1', 2, 20), partition(1, 'N1', 2, 20), partition(2, 'N3', 3, 20)]
        self.assertIsNone(faults.restate_recovery(target, 3, stale, restarted))
        down = [dict(restarted[0]), restarted[1], {**restarted[2], 'state': 'dead'}]
        self.assertIsNone(faults.restate_recovery(target, 3, led, down))
        renamed = [dict(restarted[0]), {**restarted[1], 'name': 'other'}, dict(restarted[2])]
        self.assertIsNone(faults.restate_recovery(target, 3, led, renamed))


class WorkerTest(unittest.TestCase):
    def test_the_busiest_worker_is_chosen(self):
        activities = [{'worker_id': 'worker-0', 'active': ['a']},
                      {'worker_id': 'worker-1', 'active': ['b', 'c']}]
        self.assertEqual(faults.choose_worker(activities)['worker_id'], 'worker-1')
        self.assertIsNone(faults.choose_worker([{'worker_id': 'worker-0', 'active': []}]))

    def test_a_restart_is_the_same_pod_ready_after_the_expected_signal(self):
        before = pod('uid-1', 0)
        self.assertIsNone(faults.pod_restart(before, pod('uid-1', 0), 'worker', (137,)))
        self.assertIsNone(faults.pod_restart(before, pod('uid-1', 1, ready=False, exit_code=137), 'worker', (137,)))
        evidence = faults.pod_restart(before, pod('uid-1', 1, exit_code=137), 'worker', (137,))
        self.assertEqual((evidence['same_pod'], evidence['restarts_after'], evidence['exit_code']), (True, 1, 137))
        self.assertEqual(faults.pod_restart(before, pod('uid-2', 0), 'worker', (137,)), {'same_pod': False})
        with self.assertRaises(faults.FaultFailed):
            faults.pod_restart(before, pod('uid-1', 1, exit_code=1), 'worker', (137,))

    def test_a_restart_whose_exit_the_kubelet_did_not_retain_is_still_the_same_pod_restarted(self):
        # The kubelet reports `lastState` only while the runtime still holds
        # the stopped container. FIG-4264's campaign lost it: restartCount 1,
        # ready, `lastState: {}`, for the whole watchdog.
        before = pod('uid-1', 0)
        self.assertIsNone(faults.pod_restart(before, pod('uid-1', 1, ready=False), 'worker', (137,)))
        evidence = faults.pod_restart(before, pod('uid-1', 1), 'worker', (137,))
        self.assertEqual((evidence['same_pod'], evidence['restarts_before'], evidence['restarts_after'],
                          evidence['exit_code']), (True, 0, 1, None))
        # No restart is still no restart, and a reported exit is still judged.
        self.assertIsNone(faults.pod_restart(before, pod('uid-1', 0), 'worker', (137,)))
        with self.assertRaises(faults.FaultFailed):
            faults.pod_restart(before, pod('uid-1', 1, exit_code=101), 'worker', (137,))


class RecoveryTest(unittest.TestCase):
    STATE = {'in_flight_unfinished': 0, 'turn': True, 'queued': True, 'cron': True, 'moved': True,
             'backlog': 3, 'pre_fault_max': 3, 'target': {'same_pod': True}}

    def test_every_condition_is_required(self):
        self.assertTrue(faults.recovered(self.STATE))
        for key, value in [('in_flight_unfinished', 1), ('turn', False), ('queued', False), ('cron', False),
                           ('moved', False), ('backlog', 4), ('target', None)]:
            with self.subTest(key=key):
                self.assertFalse(faults.recovered({**self.STATE, key: value}))

    def test_the_pre_fault_range_needs_a_healthy_sample(self):
        backlog = faults.Backlog()
        with self.assertRaises(faults.FaultFailed):
            backlog.pre_fault_max()
        for value in [1, 4, 2]:
            backlog.sample(value)
        self.assertEqual(backlog.pre_fault_max(), 4)
        backlog.sample(3)
        self.assertEqual(backlog.pre_fault_max(), 4)


if __name__ == '__main__':
    unittest.main()

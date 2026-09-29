#!/usr/bin/env python3
import json
import subprocess
from pathlib import Path
import unittest
import yaml

import check_loadtest_cluster as proof

ROOT = Path(__file__).resolve().parents[1]


class ClusterEvidenceTests(unittest.TestCase):
    def setUp(self):
        self.nodes = {
            'cluster_fingerprint': 'test-cluster',
            'nodes': [[index, {'Node': {'address': f'http://node-{index}:5122',
                       'metadata_server_config': {'metadata_server_state': 'member'}}}]
                      for index in range(3)],
        }
        self.partitions = {'partitions': [[index, {}] for index in range(24)], 'replication': {'limit': 2}}
        self.logs = {'logs': [[index, {'chain': [[1, {'kind': 'replicated', 'params': json.dumps({
            'replication': {'node': 2}, 'nodeset': [0, 1, 2], 'sequencer': f'{index % 3}:1',
        })}]]}] for index in range(24)]}

    def test_complete_replication_evidence(self):
        self.assertIn('replicated_logs=24', proof.replication(self.nodes, self.logs, self.partitions))

    def test_incomplete_cluster_fails(self):
        self.nodes['nodes'].pop()
        with self.assertRaises(ValueError):
            proof.replication(self.nodes, self.logs, self.partitions)

    def test_standby_metadata_does_not_prove_quorum(self):
        self.nodes['nodes'][1][1]['Node']['metadata_server_config']['metadata_server_state'] = 'standby'
        with self.assertRaises(ValueError):
            proof.replication(self.nodes, self.logs, self.partitions)

    def test_local_log_does_not_prove_replication(self):
        self.logs['logs'][0][1]['chain'][0][1]['kind'] = 'local'
        with self.assertRaises(ValueError):
            proof.replication(self.nodes, self.logs, self.partitions)

    def test_duplicate_storage_node_does_not_prove_replication(self):
        self.logs['logs'][0][1]['chain'][0][1]['params'] = json.dumps({
            'replication': {'node': 2}, 'nodeset': [0, 0], 'sequencer': '0:1',
        })
        with self.assertRaises(ValueError):
            proof.replication(self.nodes, self.logs, self.partitions)

    def test_partial_partition_evidence_fails(self):
        self.partitions['partitions'].pop()
        with self.assertRaises(ValueError):
            proof.replication(self.nodes, self.logs, self.partitions)

    def test_committed_partition_replicas_and_leaders(self):
        epochs = [{'current': {'replication': '{node: 2}', 'replica_set': [0, 1]},
                   'leader_metadata': {'node_id': index % 3}} for index in range(24)]
        self.assertIn('leaders=3', proof.placement(epochs))
        epochs[0]['current']['replica_set'] = [0]
        with self.assertRaises(ValueError):
            proof.placement(epochs)

    def test_unassigned_partition_does_not_prove_leadership(self):
        epochs = [{'current': {'replication': '{node: 2}', 'replica_set': [0, 1]},
                   'leader_metadata': None} for _ in range(24)]
        with self.assertRaises(ValueError):
            proof.placement(epochs)

    def recovery_fixture(self):
        for index, entry in self.nodes['nodes']:
            entry['Node'].update(name=f'node-{index}', current_generation=[index, 1])
        return 'N0:1 node-0 1m Member 8 16\nN1:1 node-1 1m Member 8 16\nN2:2 node-2 3s Member 8 16\n'

    def test_recovery_preserves_node_identity(self):
        status = self.recovery_fixture()
        self.assertIn('stable_node_ids=3', proof.recovery(self.nodes, status, 'node-2'))

    def test_live_scrape_does_not_prove_node_recovery(self):
        status = self.recovery_fixture().replace('N2:2 node-2 3s Member 8 16', 'N2 node-2 offline')
        with self.assertRaises(ValueError):
            proof.recovery(self.nodes, status, 'node-2')

    def test_recovery_rejects_replaced_identity_and_wrong_restart(self):
        status = self.recovery_fixture()
        for changed in [status.replace('N2:2', 'N9:2'), status.replace('N2:2', 'N2:1'),
                        status.replace('N1:1', 'N1:2')]:
            with self.subTest(status=changed), self.assertRaises(ValueError):
                proof.recovery(self.nodes, changed, 'node-2')

    def availability_fixture(self):
        self.recovery_fixture()
        return 'N0:1 node-0 1m Member 12 12 24 12 worker\nN1:1 node-1 1m Member 12 12 24 12 worker\nN2 node-2 offline worker\n'

    def test_survivor_placement_proves_ready_quorum(self):
        status = self.availability_fixture()
        self.assertIn('leaders=24', proof.availability(self.nodes, status, 'node-2'))

    def test_survivors_with_missing_leader_are_not_ready(self):
        status = self.availability_fixture().replace('Member 12 12', 'Member 11 12', 1)
        with self.assertRaises(ValueError):
            proof.availability(self.nodes, status, 'node-2')

    def test_survivor_check_rejects_wrong_fault_or_identity(self):
        status = self.availability_fixture()
        for changed in [status.replace('N0:1', 'N9:1'),
                        status.replace('N2 node-2 offline worker', 'N2:1 node-2 1m Member 0 0 24 0 worker')]:
            with self.subTest(status=changed), self.assertRaises(ValueError):
                proof.availability(self.nodes, changed, 'node-2')

    def test_collector_rejects_failed_scrape(self):
        document = {'data': {'activeTargets': [{'labels': {'job': 'restate'}, 'health': 'up'} for _ in range(3)]}}
        self.assertIn('healthy=3', proof.metrics(document))
        document['data']['activeTargets'][0]['health'] = 'down'
        with self.assertRaises(ValueError):
            proof.metrics(document)


class ChartTests(unittest.TestCase):
    def documents(self, profile):
        return [document for document in yaml.safe_load_all(
            (ROOT / 'target/loadtest-tools' / profile).read_text()) if document]

    def test_local_topology_and_persistence(self):
        documents = self.documents('values-local.yaml.rendered.yaml')
        restate = next(item for item in documents if item['kind'] == 'StatefulSet' and item['metadata']['name'].endswith('-restate'))
        self.assertEqual(restate['spec']['replicas'], 3)
        self.assertEqual(restate['spec']['podManagementPolicy'], 'Parallel')
        self.assertEqual(restate['spec']['volumeClaimTemplates'][0]['spec']['accessModes'], ['ReadWriteOnce'])
        config = next(item['data']['restate.toml'] for item in documents if item['kind'] == 'ConfigMap' and item['metadata']['name'].endswith('-restate'))
        self.assertIn('auto-provision = false', config)
        self.assertIn('type = "replicated"', config)
        self.assertEqual(config.count('.lash-loadtest-peers:5122'), 3)
        self.assertIn('snapshots', config)
        schema = next(item for item in documents if item['kind'] == 'Job' and item['metadata']['name'].endswith('-schema'))
        self.assertEqual(schema['metadata']['annotations']['helm.sh/hook'], 'post-install')
        workers = [item for item in documents if item['kind'] == 'Deployment' and '-worker-' in item['metadata']['name']]
        self.assertEqual(len(workers), 2)
        for worker in workers:
            init = worker['spec']['template']['spec']['initContainers'][0]
            self.assertIn('NET_ADMIN', init['securityContext']['capabilities']['add'])
            self.assertEqual(init['command'].count('netem'), 1)

    def test_scaleway_storage_and_placement(self):
        documents = self.documents('values-scaleway.yaml.rendered.yaml')
        restate = next(item for item in documents if item['kind'] == 'StatefulSet' and item['metadata']['name'].endswith('-restate'))
        self.assertEqual(restate['spec']['volumeClaimTemplates'][0]['spec']['storageClassName'], 'sbs-5k')
        spec = restate['spec']['template']['spec']
        self.assertEqual(spec['nodeSelector'], {'lash-loadtest-pool': 'restate'})
        self.assertIn('requiredDuringSchedulingIgnoredDuringExecution', spec['affinity']['podAntiAffinity'])

    def test_external_s3_has_no_garage_volume(self):
        documents = self.documents('scaleway-object-storage.yaml')
        self.assertFalse(any(item['kind'] == 'StatefulSet' and item['metadata']['name'].endswith('-s3') for item in documents))
        config = next(item['data']['restate.toml'] for item in documents if item['kind'] == 'ConfigMap' and item['metadata']['name'].endswith('-restate'))
        self.assertIn('https://s3.fr-par.scw.cloud', config)
        self.assertIn('aws-allow-http = false', config)

    def test_retained_generation_keeps_its_image_and_endpoint(self):
        documents = self.documents('retained-generations.yaml')
        workers = [item for item in documents if item['kind'] == 'Deployment' and '-worker-' in item['metadata']['name']]
        self.assertEqual(len(workers), 4)
        smoke = next(item for item in documents if item['kind'] == 'Job' and item['metadata']['name'].endswith('-smoke'))
        self.assertEqual(smoke['metadata']['annotations']['helm.sh/hook'], 'post-install,post-upgrade')
        self.assertEqual(smoke['metadata']['annotations']['helm.sh/hook-delete-policy'], 'before-hook-creation')
        for worker in workers:
            expected_tag = 'old' if worker['metadata']['name'].endswith('-initial') else 'new'
            spec = worker['spec']['template']['spec']
            self.assertTrue(spec['containers'][0]['image'].endswith(':' + expected_tag))
            self.assertTrue(spec['initContainers'][0]['image'].endswith(':' + expected_tag))
        services = {item['metadata']['name'] for item in documents if item['kind'] == 'Service'}
        self.assertIn('lash-loadtest-workers-initial', services)
        self.assertIn('lash-loadtest-workers-next', services)

    def test_invalid_topologies_fail_at_render(self):
        for overrides in [
            ['--set', 'restate.replicas=1'],
            ['--set', 'restate.replication=4'],
            ['--set', 's3.snapshotPrefix=attachments'],
            ['--set', 'workers.retainedGenerations[0]=initial'],
            ['--set', 'faults.workerKill.index=2'],
            ['--set', 'faults.restateRestart.index=3'],
            ['--set', 'nameOverride=' + 'a' * 40, '--set', 'workers.generation=' + 'b' * 32],
        ]:
            with self.subTest(overrides=overrides):
                result = subprocess.run([
                    str(ROOT / 'target/loadtest-tools/helm'), 'template', 'topology',
                    str(ROOT / 'deploy/helm/lash-loadtest'), *overrides,
                ], capture_output=True, text=True)
                self.assertNotEqual(result.returncode, 0)

    def test_credentials_are_operator_secrets(self):
        documents = self.documents('values-local.yaml.rendered.yaml')
        self.assertFalse(any(item['kind'] == 'Secret' for item in documents))
        for item in documents:
            if item['kind'] not in {'Deployment', 'StatefulSet', 'Job'}:
                continue
            spec = item['spec']['template']['spec']
            for container in spec['containers']:
                for entry in container.get('env', []):
                    if entry['name'] in {'DATABASE_URL', 'WITNESS_DATABASE_URL', 'S3_ACCESS_KEY', 'S3_SECRET_KEY', 'POSTGRES_PASSWORD'}:
                        self.assertIn('secretKeyRef', entry['valueFrom'])


if __name__ == '__main__':
    unittest.main()

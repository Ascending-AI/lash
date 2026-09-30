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

    def test_measurement_collectors_and_explicit_journal_retention(self):
        documents = self.documents('load-enabled.yaml')
        load = next(row for row in documents if row['kind'] == 'Job' and row['metadata']['name'].endswith('-load'))
        env = {row['name']: row.get('value') for row in load['spec']['template']['spec']['containers'][0]['env']}
        self.assertEqual(env['LASH_LOAD_JOURNAL_RETENTION'], '5m')
        self.assertEqual(env['LASH_LOAD_MEASUREMENTS_PATH'], '/tmp/load-measurements.jsonl')
        self.assertEqual(len(env['RESTATE_METRICS_URLS'].split(',')), 3)
        self.assertEqual(env['WORKER_CONTROL_URLS'],
                         'http://lash-loadtest-worker-0-control:18101,http://lash-loadtest-worker-1-control:18101')
        restate = next(row for row in documents if row['kind'] == 'StatefulSet' and row['metadata']['name'].endswith('-restate'))
        spec = restate['spec']['template']['spec']
        self.assertTrue(spec['shareProcessNamespace'])
        self.assertIn({'name': 'fault', 'emptyDir': {}}, spec['volumes'])
        collector = next(row for row in restate['spec']['template']['spec']['containers'] if row['name'] == 'physical-collector')
        self.assertTrue(collector['volumeMounts'][0]['readOnly'])
        self.assertEqual(collector['readinessProbe']['httpGet']['path'], '/health')

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
        # The replacement build expands the store with its own operator
        # binary before its workers start.
        migrate = proof.job(documents, 'migrate-next')
        self.assertEqual(migrate['metadata']['annotations']['helm.sh/hook'], 'pre-upgrade')
        container = migrate['spec']['template']['spec']['containers'][0]
        self.assertTrue(container['image'].endswith(':new'))
        self.assertEqual(container['command'], ['lashctl', 'migrate', '--json'])
        self.assertIn('secretKeyRef', container['env'][0]['valueFrom'])
        # Control ports span both generations under one name per index.
        control = next(item for item in documents if item['kind'] == 'Service'
                       and item['metadata']['name'] == 'lash-loadtest-worker-0-control')
        self.assertEqual(control['spec']['selector'], {'app': 'lash-loadtest-worker-0'})
        ids = {entry['value'] for worker in workers
               for entry in worker['spec']['template']['spec']['containers'][0]['env']
               if entry['name'] == 'WORKER_INSTANCE_ID'}
        self.assertEqual(ids, {'worker-0-initial', 'worker-1-initial', 'worker-0-next', 'worker-1-next'})
        generations = {entry['value'] for worker in workers
                       for entry in worker['spec']['template']['spec']['containers'][0]['env']
                       if entry['name'] == 'WORKER_GENERATION'}
        self.assertEqual(generations, {'initial', 'next'})

    def test_fault_targets_restart_in_place_after_a_hold(self):
        documents = self.documents('values-local.yaml.rendered.yaml')
        self.assertFalse(any(item['kind'] == 'Job' and '-migrate-' in item['metadata']['name'] for item in documents))
        pods = [item for item in documents if item['kind'] in {'Deployment', 'StatefulSet'}
                and ('-worker-' in item['metadata']['name'] or item['metadata']['name'].endswith('-restate'))]
        self.assertEqual(len(pods), 3)
        for item in pods:
            spec = item['spec']['template']['spec']
            self.assertTrue(spec['shareProcessNamespace'])
            container = spec['containers'][0]
            script = container['args'][0]
            self.assertIn('/fault/restart-hold', script)
            self.assertIn('/fault/held-$fault', script)
            self.assertRegex(script.strip().splitlines()[-1], r'^exec (lash-e2e-worker|restate-server --config-file /config/restate.toml)$')
            self.assertIn({'name': 'fault', 'mountPath': '/fault'}, container['volumeMounts'])
            self.assertIn({'name': 'fault', 'emptyDir': {}}, spec['volumes'])
        targets = json.loads(next(item['data']['targets.json'] for item in documents
                                  if item['kind'] == 'ConfigMap' and item['metadata']['name'].endswith('-faults')))
        self.assertEqual(targets['restatePods'], [f'lash-loadtest-restate-{index}' for index in range(3)])
        self.assertEqual((targets['generation'], targets['rollingGeneration']), ('initial', 'next'))
        self.assertEqual((targets['workerCount'], targets['partitions']), (2, 24))
        probe = next(item for item in documents if item['kind'] == 'Deployment'
                     and item['metadata']['name'] == targets['probe'])
        self.assertEqual(probe['spec']['template']['spec']['containers'][0]['command'], ['sleep', 'infinity'])

    def test_invalid_topologies_fail_at_render(self):
        for overrides in [
            ['--set', 'restate.replicas=1'],
            ['--set', 'restate.replication=4'],
            ['--set', 's3.snapshotPrefix=attachments'],
            ['--set', 'workers.retainedGenerations[0]=initial'],
            ['--set', 'faults.rollingGeneration=Next'],
            ['--set', 'load.run=Not_A_Run'],
            ['--set', 'nameOverride=' + 'a' * 40, '--set', 'workers.generation=' + 'b' * 32],
            ['--set', 'load.workload=figments-v2'],
            ['--set', 'load.turnsPerSession=0'],
            ['--set', 'load.journalRetention=0s'],
        ]:
            with self.subTest(overrides=overrides):
                result = subprocess.run([
                    str(ROOT / 'target/loadtest-tools/helm'), 'template', 'topology',
                    str(ROOT / 'deploy/helm/lash-loadtest'), *overrides,
                ], capture_output=True, text=True)
                self.assertNotEqual(result.returncode, 0)

    def test_durable_workload_job_is_opt_in_and_names_its_workload(self):
        default = self.documents('values-local.yaml.rendered.yaml')
        self.assertFalse(any(item['kind'] == 'Job' and item['metadata']['name'].endswith('-load') for item in default))
        documents = self.documents('load-enabled.yaml')
        load = proof.job(documents, 'load')
        self.assertNotIn('annotations', load['metadata'])
        self.assertEqual(load['spec']['backoffLimit'], 0)
        container = load['spec']['template']['spec']['containers'][0]
        self.assertEqual(container['command'], ['lash-loadtest-driver'])
        env = {entry['name']: entry.get('value') for entry in container['env']}
        self.assertEqual(env['LASH_LOAD_WORKLOAD'], 'smoke-v1')
        self.assertEqual(env['LASH_LOAD_SESSIONS'], '4')
        self.assertEqual(env['LASH_LOAD_TURNS_PER_SESSION'], '6')
        self.assertEqual(env['WORKER_CONTROL_URLS'].split(','), [
            'http://lash-loadtest-worker-0-control:18101', 'http://lash-loadtest-worker-1-control:18101'])
        self.assertEqual(env['LASH_LOAD_FAULT_CAMPAIGN'], '0')
        self.assertNotIn('LASH_LOAD_RUN', env)
        campaign = proof.job(self.documents('load-campaign.yaml'), 'load')
        env = {entry['name']: entry.get('value') for entry in campaign['spec']['template']['spec']['containers'][0]['env']}
        self.assertEqual((env['LASH_LOAD_FAULT_CAMPAIGN'], env['LASH_LOAD_RUN']), ('1', 'smoke-v1-fault'))
        for item in documents:
            if item['kind'] == 'Deployment' and ('-worker-' in item['metadata']['name'] or item['metadata']['name'].endswith('-provider')):
                names = {entry['name']: entry.get('value') for entry in item['spec']['template']['spec']['containers'][0]['env']}
                self.assertEqual(names['LASH_LOAD_WORKLOAD'], 'smoke-v1')
        with self.assertRaises(ValueError):
            proof.job(default, 'load')

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

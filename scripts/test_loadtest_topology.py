#!/usr/bin/env python3
import json
import subprocess
import tempfile
from pathlib import Path
import unittest
import yaml

import check_loadtest_cluster as proof
from loadtest_connection_budget import peak_connections

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

    def peer_views(self, changes=None):
        """Each Restate node's own failure-detector view after node-2 restarted."""
        self.recovery_fixture()
        view = [{'plain_node_id': f'N{index}', 'gen_node_id': f'N{index}:{2 if index == 2 else 1}',
                 'name': f'node-{index}', 'state': 'alive'} for index in range(3)]
        views = [[dict(row) for row in view] for _ in range(3)]
        for (viewer, name), update in (changes or {}).items():
            views[viewer][int(name[-1])].update(update)
        return views

    def test_recovery_waits_until_every_peer_sees_the_restarted_node_alive(self):
        self.assertEqual(proof.peers(self.nodes, self.peer_views(), 'node-2'),
                         'peer_views=3 alive=3 restarted_node=node-2 generation=2')
        # The first census query ran through a node its peers still suspected.
        for views in [self.peer_views({(0, 'node-2'): {'state': 'suspect'}}),
                      self.peer_views({(1, 'node-2'): {'gen_node_id': 'N2:1'}}),
                      self.peer_views({(2, 'node-0'): {'gen_node_id': 'N0:2'}}),
                      self.peer_views({(0, 'node-1'): {'plain_node_id': 'N7'}}),
                      self.peer_views()[:2],
                      [view[:2] for view in self.peer_views()]]:
            with self.subTest(views=views), self.assertRaises(ValueError):
                proof.peers(self.nodes, views, 'node-2')

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


class WorkerHelperTests(unittest.TestCase):
    """Each image generation ships the VM helper its worker handshakes with."""

    CLIENT = '''lash_rust_library(name = "lash-vm-client", crate_features = ["testing"])
lash_rust_feature_library(name = "lash-vm-client__fv_plain", crate_features = [])
'''
    HELPER = '''lash_rust_library(name = "lash-vm-worker", crate_features = ["testing"])
lash_rust_binary(name = "lash-vm-worker__bin", crate_name = "lash_vm_worker", crate_root = "src/main.rs",
    crate_features = ["testing"], library = ":lash-vm-worker")
lash_rust_feature_library(name = "lash-vm-worker__fv_plain", crate_features = [])
lash_rust_feature_binary(name = "lash-vm-worker__bin__fv_plain", crate_name = "lash_vm_worker",
    crate_root = "src/main.rs", crate_features = [], library = "//crates/lash-vm-worker:lash-vm-worker__fv_plain")
lash_rust_feature_library(name = "lash-vm-worker__fv_next", crate_features = ["synthetic-next", "testing"])
lash_rust_binary(name = "lash-vm-worker-fixture__bin", crate_name = "lash_vm_worker_fixture",
    crate_root = "src/bin/fixture.rs", crate_features = ["testing"], library = ":lash-vm-worker")
'''
    NEXT_HELPER = '''lash_rust_feature_library(name = "lash-vm-worker__fv_alone", crate_features = ["synthetic-next", "testing"])
lash_rust_feature_binary(name = "lash-vm-worker__bin__fv_alone", crate_name = "lash_vm_worker",
    crate_root = "src/main.rs", crate_features = ["synthetic-next"], library = "//crates/lash-vm-worker:lash-vm-worker__fv_alone")
'''
    WORKER = '''lash_rust_binary(name = "worker__bin", crate_features = [], library = ":lib")
lash_rust_feature_binary(name = "worker__bin__fv_plain", crate_features = [],
    variant_deps = {"//crates/lash-vm-client:lash-vm-client": "//crates/lash-vm-client:lash-vm-client__fv_plain",
                    "//crates/lash-vm-worker:lash-vm-worker": "//crates/lash-vm-worker:lash-vm-worker__fv_plain"})
lash_rust_feature_binary(name = "worker__bin__fv_next", crate_features = ["synthetic-next"],
    variant_deps = {"//crates/lash-vm-worker:lash-vm-worker": "//crates/lash-vm-worker:lash-vm-worker__fv_next"})
lash_rust_feature_binary(name = "worker__bin__fv_split", crate_features = [],
    variant_deps = {"//crates/lash-vm-client:lash-vm-client": "//crates/lash-vm-client:lash-vm-client__fv_plain"})
'''

    def workspace(self, client=CLIENT, helper=HELPER):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        root = Path(directory.name)
        for package, text in [('crates/lash-vm-client', client), ('crates/lash-vm-worker', helper),
                              ('runbooks/e2e', self.WORKER)]:
            (root / package).mkdir(parents=True)
            (root / package / 'BUCK').write_text(text)
        return root

    def test_a_worker_runs_the_helper_built_with_its_helper_features(self):
        root = self.workspace()
        self.assertEqual(proof.vm_helper(root, '//runbooks/e2e:worker__bin'),
                         ('//crates/lash-vm-worker:lash-vm-worker__bin', True))
        self.assertEqual(proof.vm_helper(root, '//runbooks/e2e:worker__bin__fv_plain'),
                         ('//crates/lash-vm-worker:lash-vm-worker__bin__fv_plain', False))
        paired = self.workspace(helper=self.HELPER + self.NEXT_HELPER)
        self.assertEqual(proof.vm_helper(paired, '//runbooks/e2e:worker__bin__fv_next'),
                         ('//crates/lash-vm-worker:lash-vm-worker__bin__fv_alone', True))

    def test_a_helper_without_the_workers_features_does_not_pair(self):
        # The synthetic N+1's helper differs in protocol, not in build
        # identity: the base helper passes its handshake and then writes
        # artifacts the N+1 worker refuses.
        with self.assertRaisesRegex(ValueError, r"no lash-vm-worker helper binary with features \['synthetic-next', 'testing'\]"):
            proof.vm_helper(self.workspace(), '//runbooks/e2e:worker__bin__fv_next')

    def test_a_helper_library_whose_features_disagree_with_the_client_fails(self):
        with self.assertRaisesRegex(ValueError, 'do not pair on testing'):
            proof.vm_helper(self.workspace(), '//runbooks/e2e:worker__bin__fv_split')

    def test_an_ambiguous_helper_fails(self):
        ambiguous = self.workspace(helper=self.HELPER + '''lash_rust_binary(name = "lash-vm-worker__bin__twin",
    crate_name = "lash_vm_worker", crate_root = "src/main.rs", crate_features = ["testing"], library = ":lash-vm-worker")
''')
        with self.assertRaisesRegex(ValueError, 'expected one lash-vm-worker helper binary'):
            proof.vm_helper(ambiguous, '//runbooks/e2e:worker__bin')

    def image(self, info):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        bin_dir = Path(directory.name)
        (bin_dir / 'lash-e2e-worker').write_text('')
        if info is not None:
            helper = bin_dir / 'lash-vm-worker'
            helper.write_text(f'#!/bin/sh\n[ "$1" = --version ] && echo \'{json.dumps(info)}\'\n')
            helper.chmod(0o755)
        return bin_dir

    def test_an_image_without_its_paired_helper_fails(self):
        with self.assertRaisesRegex(ValueError, 'no executable lash-vm-worker'):
            proof.image_helper(self.image(None), True)
        with self.assertRaisesRegex(ValueError, 'does not pair'):
            proof.image_helper(self.image({'protocol_version': 1, 'testing': False}), True)
        self.assertEqual(proof.image_helper(self.image({'protocol_version': 1, 'testing': True}), True),
                         {'protocol_version': 1, 'testing': True})


class ChartTests(unittest.TestCase):
    def documents(self, profile):
        return [document for document in yaml.safe_load_all(
            (ROOT / 'target/loadtest-tools' / profile).read_text()) if document]

    def test_rolling_connection_budget_is_enforced_before_upgrade(self):
        documents = self.documents('retained-generations.yaml')
        job = proof.job(documents, 'connection-preflight-next')
        self.assertEqual(job['metadata']['annotations']['helm.sh/hook'], 'pre-upgrade')
        self.assertEqual(job['metadata']['annotations']['helm.sh/hook-weight'], '-10')
        container = job['spec']['template']['spec']['containers'][0]
        command = container['command']
        self.assertEqual(command[:2], ['lashctl', 'preflight'])
        self.assertEqual(command[command.index('--processes-per-generation') + 1], '2')
        self.assertEqual(command[command.index('--pool-max') + 1], '18')
        self.assertEqual(command[command.index('--generations') + 1], '2')
        self.assertIn('secretKeyRef', container['env'][0]['valueFrom'])
        with self.assertRaises(subprocess.CalledProcessError):
            subprocess.run([
                str(ROOT / 'target/loadtest-tools/helm'), 'template', 'budget',
                str(ROOT / 'deploy/helm/lash-loadtest'), '--set', 'workers.count=100',
            ], check=True, capture_output=True)

    def test_sizing_counts_all_pools_and_three_rollback_generations(self):
        values = yaml.safe_load((ROOT / 'deploy/helm/lash-loadtest/values.yaml').read_text())
        self.assertEqual(peak_connections(values), 94)
        values['postgres']['maxGenerations'] = 3
        self.assertEqual(peak_connections(values), 130)
        values['workers']['witnessConnections'] = 8
        with self.assertRaises(ValueError):
            peak_connections(values)
        values['postgres']['otherWorkers'] = 30
        self.assertEqual(peak_connections(values), 184)
        values['workers']['pgConnections'] = 0
        with self.assertRaises(ValueError):
            peak_connections(values)

    def test_local_topology_and_persistence(self):
        documents = self.documents('values-local.yaml.rendered.yaml')
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
        self.assertEqual(env['WORKER_CONTROL_URLS'],
                         'http://lash-loadtest-worker-0-control:18101,http://lash-loadtest-worker-1-control:18101')

    def test_external_s3_has_no_garage_volume(self):
        documents = self.documents('scaleway-object-storage.yaml')
        self.assertFalse(any(item['kind'] == 'StatefulSet' and item['metadata']['name'].endswith('-s3') for item in documents))

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

    def test_a_rollback_generation_returns_n_without_its_migrate(self):
        documents = self.documents('rollback-generations.yaml')
        workers = [item for item in documents if item['kind'] == 'Deployment' and '-worker-' in item['metadata']['name']]
        self.assertEqual(len(workers), 6)
        for worker in workers:
            expected_tag = 'new' if worker['metadata']['name'].endswith('-next') else 'old'
            self.assertTrue(worker['spec']['template']['spec']['containers'][0]['image'].endswith(':' + expected_tag))
        services = {item['metadata']['name'] for item in documents if item['kind'] == 'Service'}
        self.assertTrue({'lash-loadtest-workers-initial', 'lash-loadtest-workers-next',
                         'lash-loadtest-workers-rollback'} <= services)
        # N's operator never migrates over the store N+1 expanded.
        self.assertFalse(any(item['kind'] == 'Job' and '-migrate-' in item['metadata']['name'] for item in documents))

    def test_fault_targets_restart_in_place_after_a_hold(self):
        documents = self.documents('values-local.yaml.rendered.yaml')
        self.assertFalse(any(item['kind'] == 'Job' and '-migrate-' in item['metadata']['name'] for item in documents))
        pods = [item for item in documents if item['kind'] in {'Deployment', 'StatefulSet'}
                and '-worker-' in item['metadata']['name']]
        self.assertEqual(len(pods), 2)
        for item in pods:
            spec = item['spec']['template']['spec']
            self.assertTrue(spec['shareProcessNamespace'])
            container = spec['containers'][0]
            script = container['args'][0]
            self.assertIn('/fault/restart-hold', script)
            self.assertIn('/fault/held-$fault', script)
            self.assertRegex(script.strip().splitlines()[-1], r'^exec lash-e2e-worker$')
            self.assertIn({'name': 'fault', 'mountPath': '/fault'}, container['volumeMounts'])
            self.assertIn({'name': 'fault', 'emptyDir': {}}, spec['volumes'])
        targets = json.loads(next(item['data']['targets.json'] for item in documents
                                  if item['kind'] == 'ConfigMap' and item['metadata']['name'].endswith('-faults')))
        self.assertEqual((targets['generation'], targets['rollingGeneration']), ('initial', 'next'))
        self.assertEqual(targets['workerCount'], 2)
        probe = next(item for item in documents if item['kind'] == 'Deployment'
                     and item['metadata']['name'] == targets['probe'])
        self.assertEqual(probe['spec']['template']['spec']['containers'][0]['command'], ['sleep', 'infinity'])

    def test_invalid_topologies_fail_at_render(self):
        for overrides in [
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

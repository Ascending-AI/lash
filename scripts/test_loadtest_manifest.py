#!/usr/bin/env python3
import hashlib
import copy
import json
import subprocess
import re
import tarfile
import tempfile
import unittest
from pathlib import Path

import yaml
import check_loadtest_cluster as proof
import loadtest_manifest as m

ROOT = Path(__file__).resolve().parents[1]

WORKLOAD = {
    'format_version': 1, 'seed': 3790, 'generator': {'algorithm': 'ChaCha20', 'version': 1},
    'warmup_s': 60, 'history_prefill_turns': [[0, 0.5]], 'rotate_after_turns': 4,
    'compact_every_turns': 50,
    'inventory': {'lash_sha': 'a' * 40, 'figments_sha': 'b' * 40},
}

RUN_VALUES = """nameOverride: lash-loadtest
network: {enabled: true, delayMs: 1, jitterMs: 0.25, bandwidthMbps: 1000}
"""

COLLECTION = {
    'schema_version': 1, 'record': 'run', 'run': 'smoke-v1-1', 'mode': 'smoke', 'workload': 'smoke-v1',
    'seed': 3790, 'sessions': 4, 'turns_per_session': 6, 'workers': ['w0', 'w1'], 'fault_campaign': False,
    'journal_settings': {'journal_retention': '5m'},
    'scope': 'pipeline smoke; no baseline or budgets', 'source_log_sha256': 'c' * 64,
}

SUMMARY = {'schema_version': 1, 'run': 'smoke-v1-1', 'mode': 'smoke',
           'qualification': {'status': 'PASSED'}, 'baseline': None}


def fixture(root):
    """A minimal but complete run directory and reconciled results tree."""
    run_dir = root / 'run'
    results = root / 'results' / 'fig-3790' / 'smoke-v1-1'
    evidence = {
        'run-values.yaml': RUN_VALUES, 'topology.yaml': 'apiVersion: v1\n',
        'build-settings.json': json.dumps({'name': 'lash-loadtest'}),
        'kind.yaml': 'kind: Cluster\n',
        'sources.json': json.dumps({'lash_sha': 'd' * 40, 'lash_dirty': False}),
        'image-digests.json': json.dumps({
            'runtime': {'reference': 'lash-loadtest:tag', 'id': 'sha256:' + 'e' * 64},
            'next_runtime': {'reference': 'lash-loadtest:tag-next', 'id': 'sha256:' + 'f' * 64},
            'restate': 'restatedev/restate:1.7.12@sha256:' + '0' * 64,
            'runtime_base': 'ubuntu:24.04', 'node_image': 'kindest/node:v1.33.1'}),
        'hardware.json': json.dumps({'arch': 'x86_64', 'logical_cpus': 32, 'memory_kib': 131804228,
                                     'kernel': '6.8.0-137-generic', 'workspace_capacity_bytes': 3779301580800}),
        'placement.json': json.dumps({'target': 'local', 'cluster': 'kind:x', 'namespace': 'x',
                                      'nodes': ['x-control-plane', 'x-worker', 'x-worker2'],
                                      'pods': {'pod-a': 'x-worker'}}),
        'node-ids.txt': 'N0\nN1\nN2\n', 'replication.txt': 'replicated_logs=24\n',
        'placement.txt': 'assigned_partitions=24\n', 'cluster-status.txt': 'ok\n',
        'result.txt': 'topology gates passed\n',
    }
    for name, text in evidence.items():
        run_dir.mkdir(parents=True, exist_ok=True)
        (run_dir / name).write_text(text)
    results.mkdir(parents=True)
    collection = dict(COLLECTION)
    workload = root / 'workloads' / 'smoke-v1.json'
    workload.parent.mkdir()
    workload.write_text(json.dumps(WORKLOAD))
    collection['workload_sha256'] = hashlib.sha256(workload.read_bytes()).hexdigest()
    collection.update(definition_hash=m.definition_hash(yaml.safe_load(RUN_VALUES), WORKLOAD), target='local')
    for name in m.RESULT_JSONL:
        (results / name).write_text(json.dumps({'schema_version': 1, 'run': 'smoke-v1-1'}) + '\n')
    for name, document in [('witness.json', {'schema_version': 1, 'record': 'witness', 'run': 'smoke-v1-1'}),
                           ('collection.json', collection), ('summary.json', SUMMARY),
                           ('histograms.json', {'schema_version': 1, 'histograms': []})]:
        (results / name).write_text(json.dumps(document))
    return run_dir, root / 'results', workload


class ManifestTests(unittest.TestCase):
    def test_workload_has_no_second_topology_or_unused_model_pool(self):
        for name in ('figments-v1', 'smoke-v1'):
            workload = json.loads((ROOT / 'crates/lash-perf/workloads' / f'{name}.json').read_text())
            self.assertNotIn('topology', workload)
            self.assertNotIn('model_code_pool', workload)
            self.assertFalse(any(path.startswith(('/topology', '/model_code_pool'))
                                 for path in workload['provenance']['fields']))

    def build(self, root, target='local'):
        run_dir, results, workload = fixture(root)
        return m.build(run_dir, results, 'smoke-v1-1', target, workload), results

    def test_manifest_records_provenance_and_baseline_is_none(self):
        with tempfile.TemporaryDirectory() as tmp:
            result, results = self.build(Path(tmp))
            output = results / 'fig-3790' / 'smoke-v1-1'
            manifest = json.loads((output / 'manifest.json').read_text())
            self.assertEqual(manifest['schema_version'], 1)
            self.assertEqual(manifest['target'], 'local')
            self.assertEqual(manifest['definition_hash'],
                             m.definition_hash(yaml.safe_load(RUN_VALUES), WORKLOAD))
            self.assertEqual(manifest['sources']['lash_sha'], 'd' * 40)
            self.assertEqual(manifest['sources']['inventory_figments_sha'], 'b' * 40)
            self.assertFalse(manifest['sources']['lash_dirty'])
            self.assertEqual(manifest['generator'], {'algorithm': 'ChaCha20', 'version': 1, 'seed': 3790})
            self.assertEqual(manifest['links'], {'enabled': True, 'one_way_delay_ms': 1,
                                                 'jitter_ms': 0.25, 'bandwidth_mbps': 1000})
            self.assertEqual(manifest['phases']['journal_retention'], '5m')
            self.assertIsNone(manifest['baseline_id'])
            self.assertEqual(manifest['qualification'], 'PASSED')
            self.assertEqual(result['files'], len(manifest['files']) + 1)

    def test_archive_bundle_and_checksums_cover_every_file(self):
        with tempfile.TemporaryDirectory() as tmp:
            result, results = self.build(Path(tmp))
            output = results / 'fig-3790' / 'smoke-v1-1'
            sums = {name: digest for digest, name in
                    (line.split('  ') for line in (output / 'SHA256SUMS').read_text().splitlines())}
            present = {str(path.relative_to(output)) for path in output.rglob('*') if path.is_file()}
            self.assertEqual(set(sums) | {'SHA256SUMS'}, present)
            for name, digest in sums.items():
                self.assertEqual(digest, hashlib.sha256((output / name).read_bytes()).hexdigest())
            with tarfile.open(result['bundle']) as tar:
                members = {entry.name for entry in tar.getmembers() if entry.isreg()}
            self.assertIn('smoke-v1-1/manifest.json', members)
            self.assertIn('smoke-v1-1/evidence/run-values.yaml', members)
            self.assertEqual(len(sums) + 1, len(members))
            checksum = (output.parent / 'smoke-v1-1.tar.gz.sha256').read_text().split()[0]
            self.assertEqual(checksum, hashlib.sha256(result['bundle'].read_bytes()).hexdigest())

    def test_evidence_is_copied_alongside_results(self):
        with tempfile.TemporaryDirectory() as tmp:
            _, results = self.build(Path(tmp))
            evidence = results / 'fig-3790' / 'smoke-v1-1' / 'evidence'
            self.assertIn('run-values.yaml', {path.name for path in evidence.iterdir()})
            manifest = json.loads((evidence.parent / 'manifest.json').read_text())
            self.assertTrue(all(name.startswith('evidence/') for name in manifest['evidence']))
            self.assertIn('evidence/sources.json', manifest['evidence'])

    def test_missing_results_fail(self):
        with tempfile.TemporaryDirectory() as tmp:
            run_dir, results, workload = fixture(Path(tmp))
            with self.assertRaisesRegex(ValueError, 'missing reconciled results'):
                m.build(run_dir, results, 'smoke-v1-9', 'local', workload)

    def test_run_identity_mismatch_fails(self):
        with tempfile.TemporaryDirectory() as tmp:
            run_dir, results, workload = fixture(Path(tmp))
            output = results / 'fig-3790' / 'smoke-v1-1'
            collection = json.loads((output / 'collection.json').read_text())
            collection['run'] = 'smoke-v1-2'
            (output / 'collection.json').write_text(json.dumps(collection))
            with self.assertRaisesRegex(ValueError, 'different run'):
                m.build(run_dir, results, 'smoke-v1-1', 'local', workload)

    def test_unversioned_result_row_fails(self):
        with tempfile.TemporaryDirectory() as tmp:
            run_dir, results, workload = fixture(Path(tmp))
            output = results / 'fig-3790' / 'smoke-v1-1'
            (output / 'operations.jsonl').write_text(json.dumps({'schema_version': 2}) + '\n')
            with self.assertRaisesRegex(ValueError, 'schema_version'):
                m.build(run_dir, results, 'smoke-v1-1', 'local', workload)

    def test_workload_mismatch_fails(self):
        with tempfile.TemporaryDirectory() as tmp:
            run_dir, results, workload = fixture(Path(tmp))
            workload.write_text(json.dumps({**WORKLOAD, 'seed': 1}))
            with self.assertRaisesRegex(ValueError, 'workload file differs'):
                m.build(run_dir, results, 'smoke-v1-1', 'local', workload)

    def test_missing_required_evidence_fails(self):
        with tempfile.TemporaryDirectory() as tmp:
            run_dir, results, workload = fixture(Path(tmp))
            (run_dir / 'sources.json').unlink()
            with self.assertRaisesRegex(ValueError, 'required run evidence'):
                m.build(run_dir, results, 'smoke-v1-1', 'local', workload)

    def test_placement_target_mismatch_fails(self):
        with tempfile.TemporaryDirectory() as tmp:
            run_dir, results, workload = fixture(Path(tmp))
            (run_dir / 'placement.json').write_text(json.dumps(
                {'target': 'scaleway', 'cluster': 'kapsule', 'namespace': 'x',
                 'nodes': ['n'], 'pods': {'p': 'n'}}))
            with self.assertRaisesRegex(ValueError, 'placement target'):
                m.build(run_dir, results, 'smoke-v1-1', 'local', workload)

    def test_unknown_target_fails(self):
        with tempfile.TemporaryDirectory() as tmp:
            run_dir, results, workload = fixture(Path(tmp))
            with self.assertRaisesRegex(ValueError, 'unknown load target'):
                m.build(run_dir, results, 'smoke-v1-1', 'minikube', workload)

    def test_failed_analysis_still_gets_a_manifest(self):
        with tempfile.TemporaryDirectory() as tmp:
            run_dir, results, workload = fixture(Path(tmp))
            output = results / 'fig-3790' / 'smoke-v1-1'
            (output / 'summary.json').write_text(json.dumps(
                {'schema_version': 1, 'verdict': 'failed', 'error': 'durability witness failed'}))
            result = m.build(run_dir, results, 'smoke-v1-1', 'local', workload)
            self.assertEqual(result['qualification'], 'failed')
            manifest = json.loads((output / 'manifest.json').read_text())
            self.assertEqual(manifest['qualification'], 'failed')

    def test_incomplete_provenance_fails(self):
        with tempfile.TemporaryDirectory() as tmp:
            run_dir, results, workload = fixture(Path(tmp))
            (run_dir / 'hardware.json').write_text(json.dumps({'arch': 'x86_64'}))
            with self.assertRaisesRegex(ValueError, 'hardware field'):
                m.build(run_dir, results, 'smoke-v1-1', 'local', workload)


class DefinitionTests(unittest.TestCase):
    def values(self):
        return yaml.safe_load((ROOT / 'deploy/helm/lash-loadtest/values.yaml').read_text())

    def test_controller_preserves_the_shared_postgres_capacity(self):
        script = (ROOT / 'scripts/multi-node-load.sh').read_text()
        prepare = re.search(r"<<'PYVALUES'\n(.*?)\nPYVALUES", script, re.S).group(1)
        with tempfile.TemporaryDirectory() as tmp:
            subprocess.run(['python3', '-', str(ROOT / 'deploy/helm/lash-loadtest/values.yaml'),
                            str(ROOT / 'deploy/helm/lash-loadtest/values-local.yaml'),
                            tmp, 'smoke', 'faults'], input=prepare, text=True, cwd=ROOT, check=True)
            actual = yaml.safe_load((Path(tmp) / 'run-values.yaml').read_text())
            self.assertEqual(actual['postgres']['maxConnections'], self.values()['postgres']['maxConnections'])

    def test_target_file_cannot_override_resources(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / 'values-target.yaml'
            path.write_text('workers:\n  resources:\n    requests: {cpu: 250m}\n')
            result = subprocess.run(['bash', str(ROOT / 'scripts/check-loadtest-chart.sh'), str(path)],
                                    capture_output=True, text=True)
            self.assertNotEqual(result.returncode, 0, result.stdout)
            self.assertIn('workers.resources', result.stderr)

    def test_target_validation_is_an_exact_allowlist(self):
        proof.target_values({'credentialsSecret': 'private', 'image': {'repository': 'registry/lash'},
                             'restate': {'storageClass': 'disk', 'nodeSelector': {'custom': 'node'},
                                         'antiAffinity': 'required', 'tolerations': [{'key': 'pool'}]},
                             's3': {'mode': 'external', 'externalEndpoint': 'https://s3', 'region': 'fr-par'}})
        for override in [{'load': {'sessions': 20}}, {'restate': {'replication': 3}},
                         {'network': {'delayMs': 20}}, {'faults': {'rollingGeneration': 'other'}},
                         {'image': {'tag': 'other'}}, {'workers': {'nodeSelectorr': {}}}]:
            with self.subTest(override=override), self.assertRaises(ValueError):
                proof.target_values(override)

    def test_rendered_targets_share_resource_caps(self):
        def resources(target):
            text = subprocess.check_output([str(ROOT / 'target/loadtest-tools/helm'), 'template',
                                            'topology', str(ROOT / 'deploy/helm/lash-loadtest'),
                                            '-f', str(ROOT / f'deploy/helm/lash-loadtest/values-{target}.yaml')], text=True)
            documents = [row for row in yaml.safe_load_all(text) if row]
            definition = next(row for row in documents if row['metadata']['name'].endswith('-definition'))
            digest = m.definition_hash(json.loads(definition['data']['values.json']), WORKLOAD)
            return digest, {row['metadata']['name']: [container.get('resources') for container in
                    row['spec']['template']['spec']['containers']]
                    for row in documents if row['kind'] in {'Deployment', 'StatefulSet', 'Job'}}
        self.assertEqual(resources('local'), resources('scaleway'))

    def test_definition_identity_ignores_placement_access_and_run_ids(self):
        values = self.values()
        changed = copy.deepcopy(values)
        changed['restate'].update(storageClass='other', nodeSelector={'pool': 'r'}, antiAffinity='required')
        changed['s3'].update(mode='external', externalEndpoint='https://s3', region='fr-par')
        changed['credentialsSecret'] = 'other'
        changed['image'].update(repository='registry/other', tag='fresh-build')
        changed['load']['run'] = 'fresh-run'
        self.assertEqual(m.definition_hash(values, WORKLOAD), m.definition_hash(changed, WORKLOAD))
        reordered = dict(reversed(list(changed.items())))
        self.assertEqual(m.definition_hash(values, WORKLOAD), m.definition_hash(reordered, WORKLOAD))

    def test_definition_identity_changes_with_each_definition_dimension(self):
        values = self.values()
        digest = m.definition_hash(values, WORKLOAD)
        for section, key, value in [('workers', 'count', 3), ('restate', 'replication', 3),
                                    ('workers', 'resources', {'requests': {'cpu': '3'}}),
                                    ('load', 'sessions', 2), ('load', 'faultCampaign', True),
                                    ('network', 'delayMs', 2), ('faults', 'rollingGeneration', 'third')]:
            changed = copy.deepcopy(values)
            changed[section][key] = value
            with self.subTest(section=section, key=key):
                self.assertNotEqual(digest, m.definition_hash(changed, WORKLOAD))
        self.assertNotEqual(digest, m.definition_hash(values, {**WORKLOAD, 'seed': 0}))

    def manifests(self):
        return ({'definition_hash': 'a' * 64, 'target': 'local', 'qualification': 'PASSED'},
                {'definition_hash': 'a' * 64, 'target': 'scaleway', 'qualification': 'PASSED'})

    def budgets(self):
        return {'schema_version': 1, 'definitions': {'a' * 64: {'deployment_memory_peak_bytes': {'max': 100}}}}

    def test_release_comparison_labels_both_targets_for_one_definition(self):
        baseline, candidate = self.manifests()
        result = m.compare_release(baseline, candidate, {'deployment_memory_peak_bytes': 90}, self.budgets())
        self.assertEqual(result['targets'], {'baseline': 'local', 'candidate': 'scaleway'})
        self.assertEqual(result['definition_hash'], 'a' * 64)
        self.assertEqual(result['verdict'], 'PASSED')
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            for name, manifest in [('baseline', baseline), ('candidate', candidate)]:
                (root / name).mkdir()
                (root / name / 'manifest.json').write_text(json.dumps(manifest))
            (root / 'candidate/summary.json').write_text(json.dumps({'deployment_memory_peak_bytes': 90}))
            (root / 'budgets.json').write_text(json.dumps(self.budgets()))
            command = ['python3', str(ROOT / 'scripts/loadtest_manifest.py'), '--compare',
                       str(root / 'baseline'), str(root / 'candidate'), '--budgets', str(root / 'budgets.json')]
            compared = subprocess.run(command, capture_output=True, text=True)
            self.assertEqual(compared.returncode, 0, compared.stderr)
            self.assertEqual(json.loads(compared.stdout)['targets'], result['targets'])
            candidate['definition_hash'] = 'b' * 64
            (root / 'candidate/manifest.json').write_text(json.dumps(candidate))
            refused = subprocess.run(command, capture_output=True, text=True)
            self.assertNotEqual(refused.returncode, 0)
            self.assertIn('different or missing definition', refused.stderr)

    def test_release_comparison_refuses_different_or_missing_definitions(self):
        baseline, candidate = self.manifests()
        for digest in ['b' * 64, None, 'invalid']:
            with self.subTest(digest=digest), self.assertRaisesRegex(ValueError, 'definition'):
                m.compare_release(baseline, {**candidate, 'definition_hash': digest}, {}, self.budgets())

    def test_release_budgets_are_selected_by_definition_and_fail_closed(self):
        baseline, candidate = self.manifests()
        with self.assertRaisesRegex(ValueError, 'budget'):
            m.compare_release(baseline, candidate, {}, {'schema_version': 1, 'definitions': {'b' * 64: {}}})
        result = m.compare_release(baseline, candidate, {'deployment_memory_peak_bytes': 101}, self.budgets())
        self.assertEqual(result['verdict'], 'FAILED')
        self.assertEqual(result['violations'][0]['metric'], 'deployment_memory_peak_bytes')
        with self.assertRaisesRegex(ValueError, 'measurement'):
            m.compare_release(baseline, candidate, {}, self.budgets())
        with self.assertRaisesRegex(ValueError, 'qualification'):
            m.compare_release(baseline, {**candidate, 'qualification': 'INCOMPLETE'}, {}, self.budgets())


if __name__ == '__main__':
    unittest.main()

#!/usr/bin/env python3
import hashlib
import json
import tarfile
import tempfile
import unittest
from pathlib import Path

import loadtest_manifest as m

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
    for name in m.RESULT_JSONL:
        (results / name).write_text(json.dumps({'schema_version': 1, 'run': 'smoke-v1-1'}) + '\n')
    for name, document in [('witness.json', {'schema_version': 1, 'record': 'witness', 'run': 'smoke-v1-1'}),
                           ('collection.json', collection), ('summary.json', SUMMARY),
                           ('histograms.json', {'schema_version': 1, 'histograms': []})]:
        (results / name).write_text(json.dumps(document))
    return run_dir, root / 'results', workload


class ManifestTests(unittest.TestCase):
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


if __name__ == '__main__':
    unittest.main()

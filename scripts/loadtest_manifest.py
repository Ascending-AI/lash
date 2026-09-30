#!/usr/bin/env python3
"""Assemble the FIG-3790 results manifest and durable archive (L6)."""
import argparse
import hashlib
import json
from pathlib import Path
import shutil
import tarfile

import yaml

TARGETS = {'local', 'scaleway'}
RESULT_JSONL = ('operations.jsonl', 'samples.jsonl', 'metrics.jsonl', 'faults.jsonl',
                'sample_errors.jsonl', 'query_retries.jsonl', 'witness_evidence.jsonl')
RESULT_JSON = ('witness.json', 'collection.json', 'summary.json', 'histograms.json')
# Topology, build and placement evidence copied beside the measurement files so
# the archive is self-contained. Files absent from a run are skipped; the
# required set must always exist.
EVIDENCE = ('run-values.yaml', 'driver-values.yaml', 'load-values.yaml', 'rolling-values.yaml',
            'retired-values.yaml', 'topology.yaml', 'build-settings.json', 'kind.yaml',
            'sources.json', 'image-digests.json', 'hardware.json', 'placement.json',
            'node-ids.txt', 'nodes.json', 'logs.json', 'partitions.json', 'replication.txt',
            'placement.txt', 'metadata-quorum.txt', 'cluster-status.txt', 'provision.log',
            'availability.txt', 'recovery.txt', 'metrics.txt', 'snapshots.txt', 'result.txt',
            'faults.jsonl', 'helm-lint.log')
REQUIRED_EVIDENCE = {'run-values.yaml', 'topology.yaml', 'build-settings.json', 'kind.yaml',
                     'sources.json', 'image-digests.json', 'hardware.json', 'placement.json',
                     'node-ids.txt', 'replication.txt', 'placement.txt', 'cluster-status.txt'}


def require(condition, message):
    if not condition:
        raise ValueError(message)


def sha256(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def versioned_files(directory):
    """Every result file declares schema_version 1."""
    for name in RESULT_JSONL:
        path = directory / name
        require(path.is_file(), f'missing result file {name}')
        for line in path.read_text().splitlines():
            require(json.loads(line)['schema_version'] == 1, f'{name} row is not schema_version 1')
    for name in RESULT_JSON:
        path = directory / name
        require(path.is_file(), f'missing result file {name}')
        require(json.loads(path.read_text())['schema_version'] == 1, f'{name} is not schema_version 1')


def copy_evidence(run_dir, output):
    evidence = output / 'evidence'
    evidence.mkdir(exist_ok=True)
    copied = []
    for name in EVIDENCE:
        source = run_dir / name
        if source.is_file():
            shutil.copyfile(source, evidence / name)
            copied.append(name)
    missing = REQUIRED_EVIDENCE - set(copied)
    require(not missing, f'missing required run evidence: {sorted(missing)}')
    return copied


def build(run_dir, results_root, run_id, target, workload_path):
    require(target in TARGETS, f'unknown load target {target!r}; expected local or scaleway')
    output = Path(results_root) / 'fig-3790' / run_id
    run_dir = Path(run_dir)
    require(output.is_dir(), f'missing reconciled results for run {run_id}')
    versioned_files(output)
    collection = json.loads((output / 'collection.json').read_text())
    summary = json.loads((output / 'summary.json').read_text())
    require(collection['run'] == run_id, 'result files name a different run')
    # A failed analysis keeps its raw rows and a verdict-only summary.
    if summary.get('run') is not None:
        require(summary['run'] == run_id, 'result files name a different run')
    spec = json.loads(Path(workload_path).read_text())
    digest = sha256(Path(workload_path))
    require(spec['format_version'] == 1, 'unsupported workload format version')
    require(collection['workload'] == Path(workload_path).stem, 'workload name differs from the run record')
    require(collection['workload_sha256'] == digest, 'workload file differs from the run record')
    require(collection['seed'] == spec['seed'], 'run seed differs from the workload')
    copied = copy_evidence(run_dir, output)
    sources = json.loads((output / 'evidence/sources.json').read_text())
    require(len(sources['lash_sha']) == 40 and isinstance(sources['lash_dirty'], bool),
            'incomplete source provenance')
    images = json.loads((output / 'evidence/image-digests.json').read_text())
    for name in ('runtime', 'next_runtime'):
        require(images[name]['reference'] and images[name]['id'], f'missing {name} image digest')
    hardware = json.loads((output / 'evidence/hardware.json').read_text())
    for name in ('arch', 'logical_cpus', 'memory_kib', 'kernel', 'workspace_capacity_bytes'):
        require(hardware.get(name), f'missing hardware field {name}')
    placement = json.loads((output / 'evidence/placement.json').read_text())
    require(placement['target'] == target, 'recorded placement target differs from the recipe target')
    require(placement['nodes'] and placement['pods'], 'incomplete pod placement')
    values = yaml.safe_load((output / 'evidence/run-values.yaml').read_text())
    network = values['network']
    inventory = spec.get('inventory', {})
    files = {str(path.relative_to(output)): sha256(path)
             for path in sorted(output.rglob('*')) if path.is_file()}
    manifest = {
        'schema_version': 1,
        'run': run_id,
        'target': target,
        'scope': collection['scope'],
        'mode': summary.get('mode'),
        'qualification': summary.get('qualification', {}).get('status', summary.get('verdict')),
        'sources': {
            'lash_sha': sources['lash_sha'],
            'lash_dirty': sources['lash_dirty'],
            'inventory_lash_sha': inventory.get('lash_sha'),
            'inventory_figments_sha': inventory.get('figments_sha'),
        },
        'workload': {
            'name': collection['workload'],
            'file': str(workload_path),
            'sha256': digest,
            'format_version': spec['format_version'],
        },
        'generator': {**spec['generator'], 'seed': spec['seed']},
        'images': images,
        'hardware': hardware,
        'placement': placement,
        'links': {
            'enabled': network['enabled'],
            'one_way_delay_ms': network['delayMs'],
            'jitter_ms': network['jitterMs'],
            'bandwidth_mbps': network['bandwidthMbps'],
        },
        'settings': {
            'chart': 'deploy/helm/lash-loadtest',
            'run_values': 'evidence/run-values.yaml',
            'run_values_sha256': sha256(output / 'evidence/run-values.yaml'),
        },
        'phases': {
            'warmup_s': spec['warmup_s'],
            'history_prefill_turns': spec['history_prefill_turns'],
            'rotate_after_turns': spec['rotate_after_turns'],
            'compact_every_turns': spec['compact_every_turns'],
            'sessions': collection['sessions'],
            'turns_per_session': collection['turns_per_session'],
            'fault_campaign': collection['fault_campaign'],
            'journal_retention': collection['journal_settings']['journal_retention'],
        },
        'baseline_id': None,
        'evidence': ['evidence/' + name for name in copied],
        'files': files,
    }
    (output / 'manifest.json').write_text(json.dumps(manifest, indent=1) + '\n')
    files = {str(path.relative_to(output)): sha256(path)
             for path in sorted(output.rglob('*')) if path.is_file()}
    (output / 'SHA256SUMS').write_text(''.join(f'{digest}  {name}\n' for name, digest in files.items()))
    bundle = output.parent / f'{run_id}.tar.gz'
    with tarfile.open(bundle, 'w:gz') as tar:
        tar.add(output, arcname=run_id)
    (bundle.parent / (bundle.name + '.sha256')).write_text(f'{sha256(bundle)}  {bundle.name}\n')
    return {'manifest': output / 'manifest.json', 'bundle': bundle,
            'files': len(files), 'qualification': manifest['qualification']}


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument('--run-dir', type=Path, required=True, help='the recipe evidence directory')
    parser.add_argument('--results', type=Path, required=True, help='the reconciled results root')
    parser.add_argument('--run-id', required=True)
    parser.add_argument('--target', required=True, choices=sorted(TARGETS))
    parser.add_argument('--workload', type=Path, required=True, help='the checked-in workload JSON file')
    args = parser.parse_args()
    result = build(args.run_dir, args.results, args.run_id, args.target, args.workload)
    print(f"load manifest run={args.run_id} target={args.target} qualification={result['qualification']} "
          f"files={result['files']} bundle={result['bundle']}")


if __name__ == '__main__':
    main()

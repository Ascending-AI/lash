#!/usr/bin/env python3
"""Fail closed on the committed Restate metadata and Prometheus evidence."""
import json
from pathlib import Path
import re
import sys


def nodes(document):
    entries = [(key, value['Node']) for key, value in document['nodes'] if 'Node' in value]
    if len(entries) != 3:
        raise ValueError(f'expected three nodes, found {len(entries)}')
    if len({value['address'] for _, value in entries}) != 3:
        raise ValueError('node addresses are not independent')
    if not document.get('cluster_fingerprint'):
        raise ValueError('missing shared cluster identity')
    return entries


def replication(node_document, log_document, partition_document):
    entries = nodes(node_document)
    if any(value['metadata_server_config']['metadata_server_state'] != 'member' for _, value in entries):
        raise ValueError('all three nodes must be committed metadata members')
    partitions = partition_document['partitions']
    if len(partitions) != 24:
        raise ValueError(f'expected 24 partitions, found {len(partitions)}')
    if partition_document['replication'] != {'limit': 2}:
        raise ValueError(f"partition replication is {partition_document['replication']!r}")
    logs = log_document['logs']
    if len(logs) != 24:
        raise ValueError(f'expected 24 logs, found {len(logs)}')
    sequencers = set()
    for _, log in logs:
        tail = log['chain'][-1][1]
        if tail['kind'] != 'replicated':
            raise ValueError('a log uses a non-replicated provider')
        params = json.loads(tail['params'])
        if params['replication'] != {'node': 2} or len(set(params['nodeset'])) < 2:
            raise ValueError('a log has insufficient replication or distinct storage nodes')
        sequencers.add(params['sequencer'])
    if len(sequencers) < 2:
        raise ValueError('log sequencing is not distributed')
    return f'cluster_nodes=3 metadata_members=3 partitions=24 replicated_logs=24 replication=2 sequencers={len(sequencers)}'


def placement(documents):
    if len(documents) != 24:
        raise ValueError('placement proof requires every partition')
    leaders = set()
    for document in documents:
        current = document['current']
        if current['replication'] != '{node: 2}' or len(set(current['replica_set'])) != 2:
            raise ValueError('a partition does not have two distinct assigned replicas')
        leader = document.get('leader_metadata')
        if not leader:
            raise ValueError('a partition has no committed leader')
        leaders.add(json.dumps(leader['node_id'], sort_keys=True))
    if len(leaders) < 2:
        raise ValueError('partition leadership is not distributed')
    return f'assigned_partitions=24 replicas_per_partition=2 leaders={len(leaders)}'


def availability(node_document, status, unavailable_name):
    expected = {value['name']: (key, value['current_generation'][1])
                for key, value in nodes(node_document)}
    rows = re.findall(r'^\s*N(\d+):(\d+)\s+(\S+)\s+[^\n]*?\sMember\s+(\d+)\s+(\d+)\s+(\d+)\s+(\d+)\s+', status, re.MULTILINE)
    if len(rows) != 2 or {row[2] for row in rows} != set(expected) - {unavailable_name}:
        raise ValueError('both surviving metadata members must be alive')
    unavailable_id, _ = expected[unavailable_name]
    if not re.search(rf'^\s*N{unavailable_id}\s+{re.escape(unavailable_name)}\s+offline\s', status, re.MULTILINE):
        raise ValueError('the selected node must remain unavailable')
    placement = []
    for node_id, generation, name, leaders, followers, _, sequencers in rows:
        if (int(node_id), int(generation)) != expected[name]:
            raise ValueError('a surviving node changed identity or restarted')
        placement.append(f'{name}:{leaders}/{followers}/{sequencers}')
    if any(sum(int(row[column]) for row in rows) != 24 for column in [3, 4, 6]):
        raise ValueError('all 24 leaders, followers and sequencers must be on the survivors')
    return 'quorum_nodes=2 unavailable_nodes=1 leaders=24 followers=24 sequencers=24 placement=' + ','.join(sorted(placement))


def recovery(node_document, status, restarted_name):
    expected = {value['name']: (key, value['current_generation'][1])
                for key, value in nodes(node_document)}
    rows = re.findall(r'^\s*N(\d+):(\d+)\s+(\S+)\s+([^\n]+)$', status, re.MULTILINE)
    if len(rows) != 3 or {row[2] for row in rows} != set(expected):
        raise ValueError('recovery requires all three original nodes to be alive')
    restarted_generation = None
    for node_id, generation, name, detail in rows:
        original_id, original_generation = expected[name]
        if int(node_id) != original_id or 'Member' not in detail.split():
            raise ValueError('recovery changed a node identity or lost metadata membership')
        if name == restarted_name:
            if int(generation) <= original_generation:
                raise ValueError('the selected node did not restart')
            restarted_generation = int(generation)
        elif int(generation) != original_generation:
            raise ValueError('an unselected node restarted')
    if restarted_generation is None:
        raise ValueError('the selected restarted node is missing')
    return f'recovered_nodes=3 metadata_members=3 stable_node_ids=3 restarted_node={restarted_name} generation={restarted_generation}'


def metrics(document):
    targets = [target for target in document['data']['activeTargets'] if target['labels']['job'] == 'restate']
    if len(targets) != 3 or any(target['health'] != 'up' for target in targets):
        raise ValueError('all three Restate scrapes must be up')
    return 'restate_scrapes=3 healthy=3'


def main():
    mode, *paths = sys.argv[1:]
    if mode == 'smoke-job':
        import yaml
        documents = [document for document in yaml.safe_load_all(sys.stdin) if document]
        jobs = [document for document in documents if document['kind'] == 'Job' and document['metadata']['name'].endswith('-smoke')]
        if len(jobs) != 1:
            raise ValueError('expected one smoke Job')
        sys.stdout.write(json.dumps(jobs[0]))
        return
    if mode in {'availability', 'recovery'}:
        node_path, status_path, restarted_name = paths
        checker = availability if mode == 'availability' else recovery
        print(checker(json.loads(Path(node_path).read_text()), Path(status_path).read_text(), restarted_name))
        return
    documents = [json.loads(Path(path).read_text()) for path in paths]
    if mode == 'nodes':
        for key, _ in nodes(documents[0]):
            print(key)
    elif mode == 'replication':
        print(replication(*documents))
    elif mode == 'placement':
        print(placement(documents))
    elif mode == 'metrics':
        print(metrics(documents[0]))
    else:
        raise ValueError(f'unknown proof {mode}')


if __name__ == '__main__':
    main()

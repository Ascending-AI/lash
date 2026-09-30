#!/usr/bin/env python3
"""Fail closed on the committed Restate metadata and Prometheus evidence."""
import ast
import json
import os
from pathlib import Path
import re
import subprocess
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


def peers(node_document, views, restarted_name):
    """Every node's own failure detector sees all three nodes alive, the
    restarted one at its new generation. `ctl status` lists a restarted node
    before its peers stop suspecting it, and a distributed query it
    coordinates then loses its scanners."""
    expected = {value['name']: (key, value['current_generation'][1])
                for key, value in nodes(node_document)}
    if len(views) != len(expected):
        raise ValueError(f'expected {len(expected)} peer views, found {len(views)}')
    generations = set()
    for view in views:
        if sorted(row['name'] for row in view) != sorted(expected):
            raise ValueError('a peer view does not list all three original nodes')
        for row in view:
            node_id, generation = expected[row['name']]
            if row['plain_node_id'] != f'N{node_id}' or row['state'] != 'alive':
                raise ValueError(f"{row['name']} is {row['state']} as N{node_id} in a peer view")
            seen = int(row['gen_node_id'].split(':')[1])
            if row['name'] == restarted_name:
                if seen <= generation:
                    raise ValueError(f'a peer still sees {restarted_name} at its old generation')
                generations.add(seen)
            elif seen != generation:
                raise ValueError(f"a peer sees {row['name']} restarted")
    if len(generations) != 1:
        raise ValueError(f'peers disagree on the generation of {restarted_name}')
    return f'peer_views={len(views)} alive={len(expected)} restarted_node={restarted_name} generation={generations.pop()}'


def metrics(document):
    targets = [target for target in document['data']['activeTargets'] if target['labels']['job'] == 'restate']
    if len(targets) != 3 or any(target['health'] != 'up' for target in targets):
        raise ValueError('all three Restate scrapes must be up')
    return 'restate_scrapes=3 healthy=3'


def job(documents, suffix):
    jobs = [document for document in documents
            if document['kind'] == 'Job' and document['metadata']['name'].endswith('-' + suffix)]
    if len(jobs) != 1:
        raise ValueError(f'expected one {suffix} Job')
    return jobs[0]


def build_rules(path):
    """The literal attributes of each rule in a generated BUILD file, by name."""
    rules = {}
    for statement in ast.parse(Path(path).read_text()).body:
        call = getattr(statement, 'value', None)
        if not isinstance(call, ast.Call) or not isinstance(call.func, ast.Name):
            continue
        rule = {'kind': call.func.id}
        for keyword in call.keywords:
            try:
                rule[keyword.arg] = ast.literal_eval(keyword.value)
            except ValueError:
                pass
        if 'name' in rule:
            rules[rule['name']] = rule
    return rules


def resolve(root, label):
    package, name = label.removeprefix('//').split(':')
    return build_rules(Path(root) / package / 'BUILD.bazel')[name]


def vm_helper(root, worker):
    """The helper binary that speaks the protocol of the worker's linked helper library.

    The handshake identity covers only the source fingerprint, platform,
    debug and `testing`; a helper built without the worker's lash-vm-worker
    features (the synthetic N+1's `synthetic-next`) passes it and then breaks
    the protocol. The helper is the binary on the worker's own helper library
    or, as Cargo ships it, the one standalone binary whose lash-vm-worker
    features equal that library's, read from the generated feature variants.
    Its `testing` must match the linked lash-vm-client's."""
    deps = resolve(root, worker).get('variant_deps', {})
    client = deps.get('//crates/lash-vm-client', '//crates/lash-vm-client:lash-vm-client')
    library = deps.get('//crates/lash-vm-worker', '//crates/lash-vm-worker:lash-vm-worker')
    testing = 'testing' in resolve(root, client)['crate_features']
    features = set(resolve(root, library)['crate_features'])
    if ('testing' in features) != testing:
        raise ValueError(f'{worker} links {library} and {client}, which do not pair on testing')
    helpers = {}
    for name, rule in build_rules(Path(root) / 'crates/lash-vm-worker/BUILD.bazel').items():
        if rule.get('crate_name') == 'lash_vm_worker' and rule.get('crate_root') == 'src/main.rs':
            helpers[name] = rule['library'] if rule['library'].startswith('//') else '//crates/lash-vm-worker' + rule['library']
    exact = [name for name, own in helpers.items() if own == library]
    equal = [name for name, own in helpers.items() if set(resolve(root, own)['crate_features']) == features]
    matches = exact or equal
    if not matches:
        raise ValueError(f'no lash-vm-worker helper binary with features {sorted(features)} of {library}, which {worker} links')
    if len(matches) != 1:
        raise ValueError(f'expected one lash-vm-worker helper binary for {library}, found {matches}')
    return f'//crates/lash-vm-worker:{matches[0]}', testing


def image_helper(bin_dir, testing):
    """One image generation ships an executable helper that pairs with its worker."""
    helper = Path(bin_dir) / 'lash-vm-worker'
    if not helper.is_file() or not os.access(helper, os.X_OK):
        raise ValueError(f'{bin_dir} has no executable lash-vm-worker beside lash-e2e-worker')
    identity = subprocess.run([str(helper), '--build-identity'], capture_output=True, text=True,
                              check=True, timeout=30).stdout.strip()
    if not identity.endswith(f'/testing-{str(testing).lower()}'):
        raise ValueError(f'{helper} identity {identity!r} does not pair with a testing={testing} worker')
    return identity


def main():
    mode, *paths = sys.argv[1:]
    if mode == 'helper':
        root, worker = paths
        label, testing = vm_helper(root, worker)
        print(label, str(testing).lower())
        return
    if mode == 'image':
        bin_dir, testing = paths
        print(image_helper(bin_dir, testing == 'true'))
        return
    if mode == 'job':
        import yaml
        (suffix,) = paths
        documents = [document for document in yaml.safe_load_all(sys.stdin) if document]
        sys.stdout.write(json.dumps(job(documents, suffix)))
        return
    if mode == 'peers':
        node_path, restarted_name, *view_paths = paths
        views = []
        for path in view_paths:
            text = Path(path).read_text()
            views.append(json.loads(text[text.index('['):]))  # restatectl may print a row count first
        print(peers(json.loads(Path(node_path).read_text()), views, restarted_name))
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

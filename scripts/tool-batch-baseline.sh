#!/usr/bin/env bash
# Archive the FIG-4868 controlled structural baseline. Double latency is diagnostic.
# Usage: scripts/tool-batch-baseline.sh --archive-root DIR [--receipt-log LOG]
# Otherwise pass additional tool_batch_baseline arguments after --.
set -euo pipefail
repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"
. ./env.sh
archive_root=""
receipt_log=""
while (($#)); do
  case "$1" in
    --archive-root) archive_root="$2"; shift 2 ;;
    --receipt-log) receipt_log="$2"; shift 2 ;;
    --) shift; break ;;
    *) echo "unknown option: $1" >&2; exit 2 ;;
  esac
done
[[ -n "$archive_root" ]] || { echo "--archive-root is required" >&2; exit 2; }
sha="$(git rev-parse HEAD)"
destination="$archive_root/$sha"
mkdir -p "$destination"
if [[ -n "$receipt_log" ]]; then
  python3 - "$receipt_log" "$destination/receipts.jsonl.gz" <<'PYTHON'
import gzip
import base64
import json
import sys
from pathlib import Path
count = 0
with Path(sys.argv[1]).open() as source, gzip.open(sys.argv[2], 'wt') as out:
    for line in source:
        prefix, found, payload = line.partition('COST_RECEIPT_GZIP_BASE64 ')
        if found:
            data = gzip.decompress(base64.b64decode(payload.strip(), validate=True))
            json.loads(data)
            out.write(data.decode() + '\n')
            count += 1
if not count:
    raise SystemExit('no executed COST_RECEIPT_GZIP_BASE64 records in the supplied log')
PYTHON
else
  kiln run //crates/lash-perf:tool_batch_baseline__bin -- \
    --source-sha "$sha" --out "$destination/receipts.jsonl" "$@"
  python3 - "$destination/receipts.jsonl" <<'PYTHON'
import gzip
import sys
from pathlib import Path
path = Path(sys.argv[1])
with gzip.open(str(path) + '.gz', 'wb') as out:
    out.write(path.read_bytes())
path.unlink()
PYTHON
fi
python3 - "$destination" "$sha" <<'PYTHON'
import gzip
import json
import subprocess
import hashlib
import tomllib
from collections import Counter
import sys
from pathlib import Path
sys.path.insert(0, str(Path('scripts').resolve()))
from loadtest_measurements import tool_cost_census
root, sha = Path(sys.argv[1]), sys.argv[2]
summary, revisions, seen = [], set(), set()
with gzip.open(root / 'receipts.jsonl.gz', 'rt') as source:
    for line in source:
        receipt = json.loads(line)
        counts = tool_cost_census(receipt)
        key = (receipt['fixture']['branch'], receipt['fixture']['width'], receipt['fixture']['payload_bytes'])
        if key in seen:
            raise SystemExit(f'duplicate fixture receipt: {key}')
        seen.add(key)
        revisions.add(receipt['source_sha'])
        trace = counts.pop('sql_trace')
        intervals = counts['sdk_waits'].pop('intervals')
        counts['sdk_waits'].update(total_intervals=len(intervals),
            censored_intervals=sum(row['censored'] for row in intervals),
            parallel_sum_ns=sum(row['elapsed_ns'] or 0 for row in intervals))
        counts['sql'] = dict(statements=receipt['sql']['statements'],
            by_verb=receipt['sql']['by_verb'], expanded_statement_bytes=receipt['sql']['expanded_statement_bytes'],
            transaction_table_roles=dict(Counter(role for row in trace['transactions'] for role in row['table_roles'])),
            attribution=trace['attribution'])
        counts['rpc'] = dict(http_requests=len(receipt['rpc']['http_requests']),
            **{key:value for key,value in receipt['rpc'].items() if key != 'http_requests'})
        summary.append(dict(receipt['fixture'], **counts, source_by_kind=receipt['source']['by_kind'],
            raw_by_kind=receipt['engine']['by_kind'], bytes=receipt['bytes'], latency=receipt['latency'], waits=receipt['waits']))
(root / 'summary.json').write_text(json.dumps(summary, indent=2) + '\n')
(root / 'manifest.json').write_text(json.dumps(dict(archive_head=sha,
    runtime_source_revisions=sorted(revisions),
    samples=len(summary), tier='in-process Restate server double',
    unavailable_predecessor_routes={'explicit_isolated_process_start':'A01/D04 declaration absent',
                                    'operation_run_transfer':'O01/O02 operation Run absent'},
    build_profile='debug', test_runtime_threads=4,
    rust_toolchain=tomllib.loads(Path('rust-toolchain.toml').read_text())['toolchain']['channel'],
    dependency_versions={row['name']:row['version'] for row in tomllib.loads(Path('Cargo.lock').read_text())['package']
                         if row['name'] in {'restate-sdk','rusqlite','tokio'}},
    cargo_lock_sha256=hashlib.sha256(Path('Cargo.lock').read_bytes()).hexdigest(),
    instrumentation_files_sha256={str(path): hashlib.sha256(path.read_bytes()).hexdigest()
        for path in [Path('crates/lash-perf/src/bin/tool_batch_baseline.rs'),
                     Path('crates/lash-perf/src/bin/tool_batch_baseline/mod.rs'),
                     Path('crates/lash-core-ids/src/perf_witness.rs'),
                     Path('crates/lash-sqlite-store/src/conn.rs'),
                     Path('crates/lash-restate-test/src/backend.rs'),
                     *sorted(Path('crates/lash-restate-test/src/server').glob('*.rs')),
                     Path('scripts/loadtest_measurements.py'),
                     Path('scripts/tool-batch-baseline.sh')]},
    raw_receipts_sha256=hashlib.sha256((root / 'receipts.jsonl.gz').read_bytes()).hexdigest(),
    latency_claim='diagnostic only; live paired comparison and quiet-host release are external',
    tree_status=subprocess.check_output(['git', 'status', '--porcelain'], text=True)), indent=2) + '\n')
print(f'Archived and reconciled {len(summary)} controlled samples at {root}')
PYTHON

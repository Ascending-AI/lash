#!/usr/bin/env bash
set -euo pipefail

prototype_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$prototype_root"
if [[ -z "${KILN_GATE_ID:-}" ]]; then
    exec kiln gate lash "$(basename "$prototype_root")" -- bash "$prototype_root/examples/workflow-graph-roundtrip/run-authoring-prototype.sh"
fi
. ./env.sh
prototype_port="$(python3 -c 'import os,zlib; print(20000 + zlib.crc32(os.environ["KILN_GATE_ID"].encode()) % 10000)')"
export WORKFLOW_GRAPH_ADDR="${WORKFLOW_GRAPH_ADDR:-127.0.0.1:$prototype_port}"
if [[ -z "${WORKFLOW_GRAPH_SQLITE_PATH:-}" ]]; then
    prototype_state="$(mktemp -d "/tmp/${KILN_GATE_ID}-authoring.XXXXXX")"
    export WORKFLOW_GRAPH_SQLITE_PATH="$prototype_state/prototype.sqlite"
fi
printf 'Authoring prototype: http://%s/authoring-prototype.html\nScratch database: %s\n' "$WORKFLOW_GRAPH_ADDR" "$WORKFLOW_GRAPH_SQLITE_PATH"
exec kiln run //examples/workflow-graph-roundtrip:workflow-graph-roundtrip__bin

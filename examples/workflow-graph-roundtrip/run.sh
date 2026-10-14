#!/usr/bin/env bash
set -euo pipefail

workflow_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$workflow_root"
if [[ -z "${KILN_GATE_ID:-}" ]]; then
    exec kiln gate lash "$(basename "$workflow_root")" -- bash "$workflow_root/examples/workflow-graph-roundtrip/run.sh"
fi
. ./env.sh
workflow_port="$(python3 -c 'import os,zlib; print(20000 + zlib.crc32(os.environ["KILN_GATE_ID"].encode()) % 10000)')"
export WORKFLOW_GRAPH_ADDR="${WORKFLOW_GRAPH_ADDR:-127.0.0.1:$workflow_port}"
if [[ -z "${WORKFLOW_GRAPH_SQLITE_PATH:-}" ]]; then
    workflow_state="$(mktemp -d "/tmp/${KILN_GATE_ID}-workflow.XXXXXX")"
    export WORKFLOW_GRAPH_SQLITE_PATH="$workflow_state/workflow.sqlite"
fi
printf 'Workflow editor: http://%s/\nScratch database: %s\n' "$WORKFLOW_GRAPH_ADDR" "$WORKFLOW_GRAPH_SQLITE_PATH"
workflow_outputs="$(kiln build --materializations final --show-full-output //examples/workflow-graph-roundtrip:workflow-graph-roundtrip__bin //crates/lash-vm-worker:lash-vm-worker__bin)"
workflow_binary=""
while read -r workflow_target workflow_output; do
    case "$workflow_target" in
        root//examples/workflow-graph-roundtrip:workflow-graph-roundtrip__bin)
            workflow_binary="$workflow_output" ;;
        root//crates/lash-vm-worker:lash-vm-worker__bin)
            export LASH_VM_WORKER="$workflow_output" ;;
    esac
done <<< "$workflow_outputs"
if [[ -z "$workflow_binary" || -z "${LASH_VM_WORKER:-}" ]]; then
    printf 'Build did not return the workflow editor and worker executables.\n%s\n' "$workflow_outputs" >&2
    exit 1
fi
exec "$workflow_binary"

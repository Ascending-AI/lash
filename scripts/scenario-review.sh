#!/usr/bin/env bash
# Intent-to-add the scenario harness sources so `git diff` shows untracked
# cases without staging them (ADR 0007). With no arguments every harness
# root is marked; pass paths to mark a narrower set.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

if [[ $# -eq 0 ]]; then
    set -- \
        crates/lash-core/tests/runtime/tests/runtime_scenarios.rs \
        crates/lash-core/tests/runtime/tests/runtime_scenarios \
        crates/lash-protocol-standard/tests/protocol_scenarios.rs \
        crates/lash-protocol-rlm/tests/protocol_drivers.rs \
        crates/lash-protocol-rlm/tests/protocol_drivers \
        crates/lash/src/tests/agent_scenarios \
        docs/adr/0007-four-layer-scenario-harnesses.md
fi

exec git add --intent-to-add -- "$@"

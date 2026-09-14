# RLM Smoke: File Edit Bugfix

This scripted deterministic scenario is governed by
[`../../../RULES.md`](../../../RULES.md) and is run by `just rlm-smoke-e2e`.

The agent should inspect a tiny shell project, observe `sh test.sh` failing,
edit `calc.sh`, rerun the test, and terminate only after the oracle passes.

The oracle rejects edits to `test.sh`.

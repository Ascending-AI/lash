# RLM Smoke: Config Contract Edit

This scripted deterministic scenario is governed by
[`../../../RULES.md`](../../../RULES.md) and is run by `just rlm-smoke-e2e` in the
TypeScript dialect.

Multi-file inspection scenario. The behavior is controlled by a sourced config
file. The agent should inspect the scripts, run the test, update the config,
rerun the test, and stop after it passes.

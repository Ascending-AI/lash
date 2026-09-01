# RLM Smoke: Missing Helper File

This scripted deterministic scenario is governed by
[`../../../RULES.md`](../../../RULES.md) and is run by `just rlm-smoke-e2e` in both RLM
dialects.

File creation scenario. The main script sources a helper that is missing from
the fixture. The agent should create the helper, rerun the test, and preserve
the test file.

# RLM smoke runbooks

These three live-model scenarios belong to the scripted deterministic layer governed by
[`../RULES.md`](../RULES.md). A real OpenRouter-backed RLM turn drives each fresh fixture,
then the checked-in shell oracle alone decides pass or fail. They are not browser journeys
and receive no agent judgement.

Run the six-row gate with:

```sh
just rlm-smoke-e2e
```

The runner executes every scenario once with `LASH_RUNBOOK_DIALECT=lashlang` and once with
`LASH_RUNBOOK_DIALECT=typescript`. Every row gets a fresh workspace copy, durable data
directory, session id, reserved port, trace offset, and artifact directory. The configured
driver model is recorded separately from provider-reported served-model evidence.

## Tool jail

The scripted host exposes only `files.list`, `files.read`, `files.write`, and `exec.run`.
File paths are resolved beneath the row's temporary workspace and reject absolute paths,
parent traversal, and symlink escapes. `exec.run` accepts only `sh test.sh` and executes it
inside a networkless Docker container with a read-only root filesystem and only that copied
workspace mounted writable. No repository or other host path is exposed to the session.

The general prohibition on host-affecting tools governs agent-judged runbooks and examples.
These scenarios are gate machinery in the scripted layer, but the tools are still jailed as
above so a paid model turn cannot affect the host outside its fixture copy.

`OPENROUTER_API_KEY` is required. Missing credentials stop the runner after its host build;
they never produce skipped or synthetic verdict rows. `RLM_SMOKE_SANDBOX_IMAGE` may pin a
different compatible image, and `LASH_RLM_SMOKE_ARTIFACT_DIR` may select the artifact root.

The runner remains a local/manual paid gate. It is intentionally not part of per-PR CI.

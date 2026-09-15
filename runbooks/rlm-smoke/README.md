# RLM smoke runbooks

These three live-model scenarios belong to the scripted deterministic layer governed by
[`../RULES.md`](../RULES.md). A real OpenRouter-backed RLM turn drives each fresh fixture,
then the checked-in shell oracle alone decides pass or fail. They are not browser journeys
and receive no agent judgement.

Run the three-row gate with:

```sh
just rlm-smoke-e2e
```

The runner executes every scenario once: TypeScript is the sole RLM language
([ADR 0096](../../docs/adr/0096-typescript-is-the-sole-rlm-dialect.md)). Every row gets a fresh workspace copy, durable data
directory, session id, reserved port, trace offset, and artifact directory. Ports come from
the gate's own `LASH_E2E_PORT_BASE` allocation, not from a band a drive pins for its rows.

## What the row's trace proves

`host-evidence.json` is the host's summary; the row's own `trace.jsonl` is the record a
judge reads. Each row fails unless its trace carries both readings:

- Every workspace tool call carries a typed payload on `tool_call_completed`, under
  `output.outcome.payload`: `{path, entries}` for `workspace_list`, `{path, content}` for
  `workspace_read`, `{path, bytes_written}` for `workspace_write`, and
  `{command, exit_code, stdout, stderr}` for `workspace_exec`. The path or command is read
  off the record, never decoded out of `graph_node_id`.
- The configured driver model is recorded separately from the provider-reported served
  model: the request slug is `llm_call_completed.response.request_model`, and the served
  slug the provider actually reported is
  `llm_call_completed.attempts[].execution_evidence.served_model`. The row fails when a
  completed attempt reports no served model, and when the served models in the trace
  disagree with the ones the host wrote to `host-evidence.json`.

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

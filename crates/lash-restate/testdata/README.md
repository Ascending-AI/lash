# Lash Restate test data

`replay-corpus/<scenario>/journal.json` contains generated, deterministic
journals. Do not edit fixture contents by hand.

- The controller scenarios hold `RecordedRuntimeEffect` journals.
- `service-<Service>` holds one lash Restate service's handler journals,
  recorded from the real handlers on the in-process server double over SQLite
  memory (FIG-4805). One workload runs a session turn with a tool call and a
  handler-driven process start. Per handler the fixture lists each distinct
  ordered command sequence its invocations wrote, with minted ids elided as
  `#`. A `RunCommand` step whose run settled a Run record is annotated with
  that record's families (`RunCommand <name> [event-tag, …]`); a step whose
  completion failed reads `[failed]`, and a step that settled no record keeps
  its bare name.
- `run-<name>` holds one Run-behaviour scenario's whole deployment — every
  lash service's handler journals in one fixture (FIG-4902). Each scenario
  drives real handlers on its own double and asserts its semantic law from
  the tool bodies' delivery evidence: `run-partial-result` (a crash between a
  result's proposal and its ACK replays the body under the same call id),
  `run-retry-backoff`, `run-cancel`, `run-handover` (a journal-budget cut
  continues the run in a new `LashTurn` invocation) and `run-plugin-state`.

The workload initializes the session's durable-wait index through `reinstate`
before starting the turn, keeping the bootstrap commands in its initialization
journal rather than a later wait-registration handler.
The initialization remains part of the recorded corpus.

The service list is derived:
`every_restate_service_in_the_source_has_a_recorded_scenario` fails when a
`#[restate_sdk::object]` or `#[restate_sdk::workflow]` in `src` has no
scenario. Regenerate the corpus to record one.

The ignored generator writes under `BUILD_WORKSPACE_DIRECTORY` when supplied,
otherwise `CARGO_MANIFEST_DIR`. Every fixture records the corpus core's complete
build generation; the capture manifest owns its Git revision. Regenerate the corpus from the repository root with:

```console
kiln test //crates/lash-restate:lash-restate__unit_test \
  --local-test-execution --no-test-cache --test_env=LASH_REGENERATE=1 \
  --test_env="BUILD_WORKSPACE_DIRECTORY=$PWD" \
  --test_arg=--ignored --test_arg=--exact \
  --test_arg=tests::replay_corpus::regenerate_replay_corpus_fixtures
```

Review the generated diff and run its changed replay laws by full test path,
requiring nonzero executed counts. Z04 captures final handler fixtures; the
1.0 cut captures the tagged corpus after the baseline reset, with read-back
and same-generation replay red/green receipts. See `docs/release/cut-1.0.md`.

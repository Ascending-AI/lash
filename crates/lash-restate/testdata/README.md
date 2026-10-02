# Lash Restate test data

`replay-corpus/<scenario>/journal.json` contains generated, deterministic
journals. Do not edit fixture contents by hand.

- The controller scenarios hold `RecordedRuntimeEffect` journals.
- `service-<Service>` holds one lash Restate service's handler journals,
  recorded from the real handlers on the in-process server double over SQLite
  memory (FIG-4805). One workload runs a session turn with a tool call, a
  process, a durable wait and a process attach. Per handler the fixture lists
  each distinct ordered command sequence its invocations wrote, with minted
  ids elided as `#`.

The workload initializes the session's durable-wait index through `reinstate`
before starting the turn. Otherwise `register` and `record_group_child` race to
initialize it, moving the bootstrap commands between their handler journals.
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

Review the resulting fixture diff and run `kiln test
//crates/lash-restate:lash-restate__unit_test --test_arg=replay_corpus` before committing it.

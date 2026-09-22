# Lash Restate test data

`replay-corpus/<scenario>/journal.json` contains generated, deterministic
`RecordedRuntimeEffect` journals. Do not edit fixture contents by hand.

The ignored generator writes under `CARGO_MANIFEST_DIR` and records the checkout's
Git revision. Regenerate the corpus from the repository root with:

```console
LASH_REGENERATE_REPLAY_CORPUS=1 cargo test -p lash-internal-restate \
  tests::replay_corpus::regenerate_replay_corpus_fixtures -- --ignored --exact
```

Review the resulting fixture diff and run `kiln test
//crates/lash-restate:lash-restate__unit_test --test_arg=replay_corpus` before committing it.

# Lash Restate test data

`replay-corpus/<scenario>/journal.json` contains generated, deterministic
`RecordedRuntimeEffect` journals. Do not edit fixture contents by hand.

Regenerate the corpus from the repository root with:

```console
LASH_REGENERATE_REPLAY_CORPUS=1 cargo test -p lash-internal-restate \
  tests::replay_corpus::regenerate_replay_corpus_fixtures -- --ignored --exact
```

Review the resulting fixture diff and run `cargo test -p lash-internal-restate
replay_corpus` before committing it.

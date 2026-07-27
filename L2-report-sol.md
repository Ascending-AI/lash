# L2 post-rebase green-up

## Store module split

Moved `StoreError` from `crates/lash-core/src/store/mod.rs` into
`crates/lash-core/src/store/error.rs` and re-exported it as
`lash_core::store::StoreError`, so the public API and all call sites remain
unchanged.

- `crates/lash-core/src/store/mod.rs`: 1,521 lines, down from 1,617.
- `crates/lash-core/src/store/error.rs`: 99 lines.
- Production-file headroom in `store/mod.rs`: 79 lines below the 1,600-line
  limit.

The PostgreSQL collision lookup was formatted, and the staged cross-backend
differential carry was committed with the extraction in `7d8373b5` (`Restore
post-rebase workspace gates`).

## Integration findings against process PRs #140, #141, and #142

Nothing broke against the three new process commits. In particular, the
process-adjacent lifecycle, scoped-wait, disposition recovery, execution
fencing, stale-completion, failover, and cross-backend durable-state tests all
passed on the rebased stack.

Validation passed:

- `cargo check --workspace --all-targets`
- `cargo test --workspace`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `just push-gate`

The final push gate included 2,531 passing nextest cases, PostgreSQL
conformance, agent-service and agent-workbench Restate E2E suites, and the
28-workflow Restate/PostgreSQL/MinIO workers E2E.

## Spec accuracy

No inaccuracies found. The stated line counts, formatting failure location,
staged differential changes, and process-adjacent risk areas matched the
rebased code.

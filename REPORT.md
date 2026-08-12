# FIG-1300 review-fix report

## Result

All five confirmed review findings were addressed on `samuel-fig-1300` without changing the deferred TypeScript projection work or the pre-existing `worker_capacity.rs` clippy finding.

## Per-fix disposition

1. **Trace documentation — fixed.** The tracing and architecture docs now use `TraceEvent::LanguageExecution` and `TraceLanguageExecutionMap`, document the producer-stamped `language = "lashlang"` field, record the v3→v4 OpenTelemetry key rename and new language attribute, and distinguish generic language execution events from the Lashlang-only graph projection.
2. **Snapshot engine id — pinned.** A regression test asserts both `LashlangDialect::snapshot_engine_id() == "lashlang"` and that a snapshot created through `dialect.create_session()` encodes `engine == "lashlang"`.
3. **Bound-variable render lock scope — restored.** A dialect session now prepares a render by copying the live globals while the execution mutex is held. The expensive render runs afterward through a separate shared render cache, after the execution guard has been dropped. The existing large-globals degradation test remains green.
4. **Empty-prompt fallback — restored.** A missing dialect session now renders the canonical empty bound-variable section instead of an empty string. A direct regression test checks the header and `history` entry.
5. **Busy state panic/reset hazard — fixed.** `state()` and `state_mut()` return the typed `SessionError::Protocol("RLM execution state is busy")`; fallible reset-state construction happens before the live state is taken, so a constructor failure cannot strand the session with `state = None`. A focused typed-error test was added.

## Verification

- PASS — `cargo test -p lash-protocol-rlm` (250 passed, 1 ignored; plus 50 integration tests passed and doc-tests passed).
- PASS — `cargo check --workspace --all-targets --locked`.
- EXPECTED PRE-EXISTING FAILURE — `cargo clippy -p lash-protocol-rlm --all-targets -- -D warnings` stops in `crates/lash-core/src/runtime/worker_capacity.rs:122` on `clippy::derivable_impls`. The batch specification explicitly excludes that existing finding, so the file was left untouched.
- PASS — the same targeted clippy run with only that excluded lint allowed (`-A clippy::derivable_impls`) confirms the changed crate and all targets are otherwise warning-free.
- PASS — `cargo fmt --all --check`.
- PASS — `python3 scripts/lint_docs.py` (46 HTML pages, 42 registry pages).
- PASS — `python3 scripts/check_api_example_coverage.py` (8,002 entries; no registry refresh or diff required).

# FIG-1062 implementation report

## Projection shape

The workbench product-event log remains the rendering authority for submitted
user rows, and those rows are now session-scoped rather than turn-scoped.
`SessionEventRegistry::reconcile_settled` retains every
`workbench-user:<turn_id>` row after settlement and across `continue_as` frame
changes. Failed turns still explicitly retire their optimistic rows through
`retire_turn_rows`, so this does not preserve user content for a turn that never
committed an outcome.

`/api/state` replaces the first committed `MessageOrigin::TurnInput` for a turn
with the matching workbench-owned row in place. This preserves chronological
ordering while using the durable message only as typed correlation provenance;
the projection never reconstructs user text or attachments from the
model-facing graph. A settled workbench user row that belongs to an older frame
has no current-frame committed counterpart, so it is prepended from the
session-scoped product log before the current-frame projection. Running prompt
fallbacks retain their existing behavior.

Assistant ownership remains unchanged: live `workbench-assistant:<turn_id>`
product rows retire when their turn settles, and the existing termination-kind
rule still decides which committed assistant copy survives. Consequently,
old-frame assistant rows collapse at a `continue_as` boundary and the completed
follow-frame reply appears once.

The opening task of a `continue_as` follow frame is excluded from chat rows by
typed state: the current frame must have `AgentFrameReason == "continue_as"`,
and the hidden row must be its first `MessageOrigin::TurnInput`. This does not
inspect runtime id shapes or compare task/seed text. Later injected turn inputs
remain visible. Seed and follow-frame task material therefore stay protocol
state rather than becoming user chat rows.

## Tests

- Updated settlement reconciliation coverage to assert that the user product
  row survives while `Done` and assistant/mirror rows retire without reusing
  product cursors.
- Updated the FIG-972 single-send test to pin the UI-owned row as the settled
  rendering authority instead of accepting durable-graph backfill.
- Updated attachment coverage to prove the workbench-owned attachment row also
  survives settlement.
- Added
  `continue_as_keeps_session_user_rows_collapses_old_assistant_and_survives_reload`.
  It runs a scripted-provider `continue_as` plus follow-frame completion and
  reconciles the three relevant layers:
  - durable graph retains the pre-switch user and assistant records;
  - `/api/state` and transcript retain both submitted user rows, omit the old
    assistant and protocol-only seed/task, and contain one non-empty follow
    assistant row;
  - reopening both the SQLite-backed runtime and persisted product-event log
    reproduces the identical projected row set.
- Updated the production send-path test to assert that settlement leaves only
  the session-scoped user row in the product-event message lane.

## Battery

- `cargo fmt --all -- --check` — passed (also passed through pre-commit).
- `cargo check --workspace --all-targets` — passed.
- `cargo clippy --workspace --all-targets -- -D warnings` — passed.
- `cargo test -p agent-workbench` — passed: 113 passed, 11 environment-gated
  ignored.
- `cargo test -p lash-runtime --features rlm` — passed, including runtime,
  integration, compile-fail UI, and doctest targets.
- `python3 scripts/check_api_example_coverage.py` — passed with exit code 0
  (7,522 entries). The repository-provided `--refresh` mechanically updated
  source line anchors moved by this change.
- `python3 scripts/lint_docs.py` — passed: 46 HTML pages and 42 registry pages.
- `prek run --files <changed files>` — passed all applicable pre-commit hooks.
  An exploratory `--all-files` run touched the unrelated newline-less
  `docs/CNAME`; that baseline-only edit was restored before the scoped passing
  run.
- `git diff --check` — passed.

## Fix round

### Blocking fix

- The `/api/turn` Restate-submission error path now removes the active-turn
  marker and calls `publish_turn_failed` before returning. That retires the
  optimistic `workbench-user:<turn_id>` row and publishes the existing safe
  failure/done sequence even when the turn never starts.
- `/api/state` now derives two typed provenance sets: current-frame
  `TurnInput` turn ids from `read_view.messages()`, and session-wide committed
  `TurnInput` turn ids by walking `read_view.message_tree()` across every frame
  and branch. Settlement keeps a workbench user row only while its turn is
  active or after it has committed somewhere in that graph.
- Historical placement now means exactly “committed somewhere, absent from the
  current frame.” A never-committed row cannot be mistaken for old-frame
  history or prepended above the conversation.

### Projection and documentation

- `project_chat` in `chat_projection.rs` now owns user replacement, historical
  placement, protocol-task hiding, and stable-id deduplication for both the
  `messages` and `transcript` outputs. The route only gathers source data and
  installs the returned pair. `missing_active_user_rows` was renamed to
  `replayed_active_user_rows`, and the committed transcript walk has one
  message arm.
- The continue-as frame walk documents why the last active-path frame-open node
  is current and records the intended lash-core facade follow-up. Its first
  `TurnInput` heuristic now states the required `AgentFrameTask`/schema
  dependency.
- The workbench README records the durable user-transcript growth/reset
  contract, the reference host's per-mutation full-snapshot persistence cost,
  and the continue-as boundary rule: old-frame assistant/trigger input rows
  collapse while user chat rows persist.

### Regressions

- `submit_failure_retires_a_user_row_for_a_turn_that_never_commits` drives the
  production send handler against a failing ephemeral Restate ingress and
  proves the rejected prompt and `workbench-user:` row are absent from
  `messages`, `transcript`, and the settled product-event snapshot.
- `two_continue_as_switches_keep_real_sends_and_hide_each_follow_task` uses the
  production send path, crosses two real `continue_as` switches (three frames),
  performs an ordinary second send inside the final follow frame, and checks
  runtime-stamped `MessageOrigin::TurnInput` provenance. Both user sends and
  current-frame replies render once; both protocol tasks/seeds remain hidden.
- The new multi-frame probe lives in its own included test file so both test
  files remain below the repository's 2,500-line budget.

### Fix-round battery

- `cargo fmt --all -- --check` — passed through the final pre-commit run.
- `cargo check --workspace --all-targets` — passed.
- `cargo clippy --workspace --all-targets -- -D warnings` — passed.
- `cargo test -p agent-workbench` — passed: 115 passed, 11 environment-gated
  ignored. The moved multi-frame test was rerun in its final file and passed.
- `cargo test -p lash-runtime --features rlm` — passed, including integration,
  compile-fail UI, and doctest targets.
- `python3 scripts/check_api_example_coverage.py` — passed with explicit exit
  code 0 (7,522 entries); moved source anchors were refreshed first.
- `python3 scripts/lint_docs.py` — passed: 46 HTML pages and 42 registry pages.
- `prek run --files <changed files>` — passed every applicable pre-commit hook,
  including formatting, production/test file-size budgets, and the core/UI
  boundary check.
- `git diff --check` — passed.

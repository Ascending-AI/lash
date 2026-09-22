## Efficient build and test loop

Use one normal Kiln fork per agent: `F=$(kiln fork lash <name>)`, then
`cd "$F" && . ./env.sh` before **any** Cargo command. Start a short-lived branch
from current `origin/main`. Remove a clean fork after merge with
`kiln rm lash <name>`; never write under `/workspace/kiln/*/golden-*` or remove
a fork with `rm -rf`.

During an edit, run the narrowest command that proves the change:

- `kiln test <label> --test_arg=<case>` for a focused test case;
  `kiln clippy <label>` for a focused Rust lint verdict.
- `kiln build <label>` for compile/link proof. `kiln check` is the **same full
  compile/link proof**, not Cargo's metadata-only check; do not run both for
  the same change. `kiln analyze` validates the generated graph without
  compiling Rust.
- Before calling an implementation done, inspect
  `python3 scripts/dev-test.py --dry-run`, then run `python3 scripts/dev-test.py`
  for the change-scoped developer tests and mapped script checks. It selects
  complete test batches once, excludes deferred/manual service tests, and
  widens for shared or unknown inputs. A docs-only diff needs no Rust build.
  Use `just test-changed` only when reverse-dependent test coverage adds value.

Bare `kiln test` runs `//:dev_tests`; `kiln test //:workspace_tests` runs the
broader PR test partition. `kiln clippy` defaults to `//:workspace_clippy`.
Feature checkpoints are `kiln build //:feature_lanes`,
`kiln test //:feature_lane_tests`, and `kiln clippy //:feature_lane_clippy`.
Run a broad checkpoint for a named risk, not after every edit. `just floor` is
an opt-in broad tooling check on a **committed** head; `just push-gate` and
`just confidence*` are opt-in diagnostics, not routine push prerequisites.
Use `just seal` for public-facade/trybuild changes. Use named `just` service
recipes or `kiln gate lash <name> -- <cmd>` for integration gates; take gate
names and ports from `KILN_GATE_ID`.

Kiln commands detect the fork from any subdirectory, load its environment,
and use the shared NativeLink Bazel pool. An unreachable executor is an error,
not a silent local build; explicit `--local` is diagnostic only. Pass Bazel
labels and options to Kiln, not Cargo flags. `kiln doc` renders
`//:workspace_docs` into bazel-bin; `kiln run <label> -- <args>` compiles on the
pool and starts the program locally. `kiln fmt [-- --check]` runs local Cargo
formatting. `kiln sync` regenerates the graph and lockfile after input changes;
`kiln clean` expunges only this fork's output base and is not routine.

Keep one heavy build request in flight per fork. Check `kiln cgroup status`
before optional broad gates on the shared host; if it is pressured, defer
another full suite rather than stacking builds. Do not raise job budgets,
set `CARGO_*` by hand, or purge shared caches. Cargo on PATH is a cgroup shim,
not NativeLink: reserve it for semantics Bazel does not cover, such as
`just seal`/trybuild, nested-Cargo fault matrices, service tests, publishing,
and nightly fuzzing. Preserve the workspace feature graph with
`--workspace --all-targets` when a local Cargo compile is required.

## Commits, PRs, published text

Never a Claude, Anthropic, Codex, or any AI co-author trailer or AI mention in commits, PR bodies, comments, tickets, or teammate-visible text. Attribution is the user alone. Stage exact paths; never `commit -a`.

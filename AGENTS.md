
## Rust builds

Work in `kiln fork` trees, one per agent;
a tree: `F=$(kiln fork lash <name>)`, then `cd "$F" && . ./env.sh` before ANY
cargo command (skipping env.sh cold-builds). `kiln rm lash <name>` when done.
Never write under `/workspace/kiln/*/golden-*`.

Use `kiln build`, `kiln check`, `kiln test`, `kiln clippy`, `kiln doc` and
`kiln run <label> -- <args>` for Bazel workflows; they load the fork's
environment, detect the repo from any subdirectory, and execute on the shared
NativeLink pool. Shared execution fails closed: an unreachable executor is an
error, never a silent local compile. Explicit `--local` is diagnostic only.
`check` performs the same compile/link proof as `build`; `analyze` checks the
Bazel graph without compiling Rust. `test` runs the cacheable partition
(`//:dev_tests`), not every gate. Pass Bazel labels and options, not Cargo
flags; `--test_arg=<name>` filters test cases. `clippy` defaults to
`//:workspace_clippy`; `doc` renders `//:workspace_docs` into bazel-bin; `run`
compiles on the pool and starts the program locally.

Feature coverage: `kiln build //:feature_lanes`, `kiln test
//:feature_lane_tests`, `kiln clippy //:feature_lane_clippy`.`kiln fmt [-- --check]` is local `cargo fmt`;
`kiln sync` regenerates the graph and lockfile; `kiln clean` expunges only
this workspace's output base. `kiln gate lash <name> -- <cmd>` runs the
integration gates, names and ports from `KILN_GATE_ID`.

Cargo is for what Bazel does not cover: `just seal`/trybuild fixtures,
nested-Cargo fault matrices, service tests, publishing, nightly fuzzing.
Everything else is `kiln check`/`kiln test` — cargo on PATH is a cgroup shim
that does not submit to NativeLink, so every cargo compile is a cold local
build. Never run cargo before `. ./env.sh`; never set CARGO_* by hand.

## Commits, PRs, published text

Never a Claude, Anthropic, Codex, or any AI co-author trailer or AI mention in commits, PR bodies, comments, tickets, or teammate-visible text. Attribution is the user alone. Stage exact paths; never `commit -a`.

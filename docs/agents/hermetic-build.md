# Hermetic Rust builds

Lash uses Bazel 9.1.0 and rules_rs 0.0.110 for checkout-independent local
compilation. Cargo manifests and `Cargo.lock` remain the source of truth. The
generated BUILD files expose one Bazel action for each first-party library,
binary, example, benchmark, unit-test crate, integration-test crate, and
custom build script reachable in Cargo's resolved default workspace graph.
The complete Cargo metadata inventory also records targets whose required
features are outside that graph; their existing named Cargo feature recipes
remain authoritative. Third-party crates are imported from the same lockfile.
These labels are repository build surfaces; they do not expand the supported
SDK package surface described by ADR 0079.

On the shared development box, create one Kiln fork per agent with `kiln fork
lash <name>`. The printed path ends in `/merged`; change to that directory and
source `env.sh` before **any** Cargo command. Never write under `golden-*` or
remove a fork with `rm -rf`. After its change merges, remove it with `kiln rm
lash <name>`.

The implementer loop is `kiln test <label> --test_arg=<name>` while editing
and `python3 scripts/dev-test.py` before calling a change done. Bare `kiln test` runs
`//:dev_tests`, the developer suite; `kiln test //:workspace_tests` runs the
PR partition CI runs. Implementer loops do not run Postgres,
S3, or E2E (`scripts/ci/with-service.sh`, store recipes, Restate workers): CI
owns those, and local live gates fight over ports (`KILN_GATE_ID`) and the
single-box database.

```sh
. ./env.sh
python3 scripts/dev-test.py
```

`python3 scripts/dev-test.py --dry-run` prints the changed paths, exact labels,
base/head revisions and a checkout/config snapshot digest. `--dependents` selects reverse dependencies;
`just test-changed` uses this same planner. Live store environments are refused.
The runner writes its plan and final receipt under Git's `lash-validation/`
directory and serializes concurrent requests in one fork. Every request calls
Bazel, including a request that waited for another run: only Bazel can validate
ignored package data and external toolchain inputs before reusing cached actions.
The snapshot covers Git-visible content, relevant environment variables, env.sh
and .kiln.bazelrc; it is not the complete action-input digest. A changed snapshot
at the end of execution makes the receipt stale. These receipts do not replace
Bazel's cache or CI's required gates.

`just floor` is an explicit broad tooling checkpoint, not the default per-edit
command. It runs the dev/feature/clippy and schema checks together. `just bump-check`
combines both Rust targets in one Bazel invocation while its script checks run
beside it. A fork should have only one build request in flight; reuse its result
or wait before starting another service command.

Launcher shell self-tests require Bubblewrap. Each invocation mounts a private
`/tmp` and `/run`, uses separate PID/network namespaces, and sees the checkout
read-only. Production launchers retain their real global ownership locks;
tests cannot address host PIDs, the host network or the `/run` Docker socket.
Inherited Docker host/context overrides are removed; test commands still use
fixtures and mock Docker. This is not containment for arbitrary hostile code.
Install the `bubblewrap` package on a new host.

The lower-level entry script remains available for graph analysis, sync, local
executor reproduction, and focused Bazel labels.

`analyze` validates generated files and performs Bazel loading and analysis with
`--nobuild`; it does not run rustc and is not a substitute for `cargo check`.
`build` and `check` compile and link the requested Bazel labels (`check` is the
full compile proof, not Cargo's metadata-only mode). `test` without labels
builds and executes `//:dev_tests`, the generated developer suite
(`//:workspace_tests` minus the two dev-deferred binaries); explicit labels
remain available for a focused edit loop. `clippy` builds a clippy aggregate,
`doc` renders the `rust_doc` targets into bazel-bin, `run` compiles a binary on
the pool and starts it locally, and `fmt` is a local `cargo fmt`.

```sh
# Analyze the generated graph and reject metadata or module-lock drift.
scripts/hermetic-build.sh analyze

# Compile the complete Cargo --workspace --all-targets shape.
kiln build

# Run the generated developer suite (the PR partition minus two dev-deferred
# binaries); `kiln test //:workspace_tests` runs the full PR partition.
kiln test

# Narrow //:dev_tests to the changed package directories. A shared input
# (manifest, lockfile, toolchain, tools/, scripts/) runs the whole suite.
# Never starts Postgres, S3, or E2E.
python3 scripts/dev-test.py

# Lint the `--workspace --all-targets` shape (one clippy action per target).
kiln clippy

# Render the workspace API docs into bazel-bin.
kiln doc

# Compile a binary on the pool and start it locally.
kiln run //crates/lash-sim:lash-sim__bin -- --help

# Compile or run focused targets instead.
kiln build //crates/lash-core:lash-core
kiln test \
  //crates/lash-sansio:lash-sansio__unit_test \
  //crates/lash-sqlite-store:integration__test
```

Three `just` recipes compose those commands into the routine gates. `just
floor` is the pre-push floor: the dev and feature-lane test and clippy
partitions, `kiln fmt -- --check`, `git diff --check`, the repository-script
gates CI runs as `Test repository scripts`
(`scripts/ci/repository-gates.sh` extracts the command list from
`.github/workflows/ci.yml` so the local run cannot drift), minus
`scripts/test-agent-workbench-dev-reset.sh`, which the local run skips and
names in its table row because it alone took 170 s — CI still runs it,
`scripts/ci/repository-gates.sh --all` restores it — and the two
version-bump checks, all run concurrently and reported as one PASS/FAIL
table — run it on a committed head because `check_version_bumps.py` reads
committed state. `just bump-check` narrows that to the store-bump gates: both
version-bump checks, `scripts/check-store-sql-ownership.py`, the lash-sim
`schema_congruence__test` target, and the lash-core-store unit target that
holds the runtime-error classification exhaustiveness test. `just
test-changed [base]` diffs against `<base>` (default `origin/main`), maps the
changed files to their Bazel packages, queries the test targets in the
reverse dependencies of those packages within `//...`, and runs them
through `kiln test`, falling back to `//:dev_tests` when the query selects
nothing.

These build and test commands select the shared Kiln execution pool by default.
`scripts/hermetic-build.sh --shared build` makes that choice explicit. It uses
the REAPI instance, endpoint and mutual-TLS client certificate `.kiln.bazelrc`
declares, requires the declared NativeLink runtime identity, and fails when the
pool or the certificate is unavailable — including when `.kiln.bazelrc` is
absent, which it says rather than building with no executor. One scheduler dispatches each action to whichever
pool worker is free: this host or a Hetzner worker. NativeLink executes trusted
builds directly on the chosen worker. Hermeticity here describes pinned
tools and declared inputs, not an OS security boundary. `--local` executes
actions in the checkout for CI, bootstrap, and reproducing an executor-specific
failure:

```sh
scripts/hermetic-build.sh --local build //crates/lash-core:lash-core
```

After changing workspace membership, a manifest, or `Cargo.lock`, regenerate
and review the graph:

```sh
scripts/hermetic-build.sh sync
git diff -- MODULE.bazel.lock tools/bazel '**/BUILD.bazel'
scripts/hermetic-build.sh analyze
```

`sync` is the only workflow that updates `MODULE.bazel.lock`. Normal commands
use lockfile error mode. The lock records module, crate-source, and toolchain
archive integrity; `rust-toolchain.toml`, rules_rs, and CI all select Rust
1.98.1. Published manifests keep `rust-version = "1.90"` as their compatibility
floor.

The shared caches live where `.kiln.bazelrc` points them; Bazel action keys use
declared repository-relative source, patch, data, runfiles, build environment,
and rule inputs, so two Kiln forks can reuse the same results. Successful test
results are cacheable (`--cache_test_results=yes`) and an input change produces
a different test action key. Failed tests are never reused as successes. Inherited actions request one CPU and 2 GiB in both local and CI builds.
Generated targets retain the measured resource requests in
`tools/bazel/action-sizes.json`, including the four-CPU test floor. Local
clients submit at most 16 jobs; CI submits 32. These are in-flight action
limits, not compiler thread counts. The scheduler admits work against each
worker's advertised capacity. Keep a fork's Bazel server alive to preserve
its analysis cache.

The Kiln golden is maintained outside agent forks. Its refresh prewarms the
shared action cache with the equivalent of:

```sh
scripts/hermetic-build.sh --shared build
scripts/hermetic-build.sh --shared clippy
```

New agents use `kiln build`, `kiln test`, or focused labels. A golden refresh
skips the full Cargo warmup. Cargo target directories remain lazy and scoped to
the gates below that need Cargo semantics; those gates use direct rustc rather
than a separate shared compiler-cache daemon. The fork's generated `env.sh`
selects its private Cargo target and applies the shared build budget; do not
override it or create a cold target by hand.

`kiln rm lash <name>` runs the repository cleanup hook before unmounting and
deleting the fork. Do not call the hook directly or manually remove the
checkout.

Bazel maps each checkout to a hashed directory below the `--output_user_root`
`.kiln.bazelrc` declares; `clean --expunge` removes only the calling checkout's
output base. It preserves the shared repository and disk caches that file names
alongside it. Kiln invokes this cleanup before
deleting a fork.

The generated graph follows Cargo's resolved default workspace feature graph.
Of its 114 executable test binaries, `//:workspace_tests` owns 97 deterministic
binaries; `//:dev_tests` drops the two dev-deferred ones to 95 for the
developer loop. The other 17 labels are deferred or Cargo-owned: 16 carry
`manual`, a reason tag, and a durable
`cargo_only` explanation in `tools/bazel/target-inventory.json`; this keeps both
the aggregate and `bazel test //...` from treating an unconfigured service,
trybuild fixture cache, or frontend asset workflow as proof. The partition is generated from
Cargo metadata and checked by `scripts/test_bazel_test_contract.py` so new or
reclassified targets cannot disappear into a hand-maintained list.

Two binaries that once carried `manual` are in the partition: sharing a binary
with a Cargo-owned live-service suite is not by itself a Bazel blocker.

- `lash-restate__unit_test`'s live-Restate cases are `#[ignore]`d and stay
  ignored in Bazel exactly as in an ordinary Cargo run; `just
  effect-group-conformance-e2e` and the workers E2E jobs remain their gate.
- `lash__unit_test`'s one PostgreSQL-gated Agent Scenario self-skips without a
  database here, exactly as it did in the Cargo workspace job, and the
  `Test Postgres store` job below executes it against a real database.

The four binaries that `tools/bazel/cargo_owned_nextest_filter.txt` once
selected are in the partition as of 2026-09-14, which retires the
`Test Cargo workspace partition` Rust run on trusted events entirely — the job
now runs only the agent-workbench cases that shell out to `node --test`, and
only when the diff touches the workbench binary's first-party dependency
closure, which `scripts/ci_plan.py` derives from the workspace manifests. The
rest of the workbench unit binary is
partition-owned and runs on the pool; see "The workbench split" below. Each
blocker was fixed as a test defect rather than exempted:

- `//crates/lash-core:integration_boundary__test` shelled out to `cargo
  metadata` at the workspace root to prove that core declares no dependency on
  an integration protocol crate. The generator now records each package's
  declared dependency names in `tools/bazel/target-inventory.json`, and the
  test reads that checked-in fact. Every assertion is unchanged, including the
  one that each forbidden library target still resolves to a workspace package,
  so a renamed crate fails the test rather than vacuously passing.
  `generate_build_files.py --check` and
  `scripts/test_bazel_test_contract.py::test_inventory_carries_every_package_dependency_set`
  keep the fact current.
- `//crates/lash-core:lash-core__unit_test` ran the confidence-gate routing
  probes against a `cargo` recorder written to a temporary directory, then
  executed `scripts/confidence-gate.sh` with the repository root as its working
  directory. Under Bazel `CARGO_MANIFEST_DIR` is package-relative, so the
  derived root was the empty path and the spawn failed with `NotFound`. The
  recorder is now a declared test input
  (`crates/lash-core/tests/fixtures/fake-cargo.sh`), the gate script and the
  helpers it sources ride in runfiles as `//:confidence_gate_scripts`, the
  probe prepends the recorder directory to `PATH` as well as setting `HOME`,
  and the root resolver maps the empty path to the working directory.
  `turn_cancel_modes::native_takeover_settles_unresolved_cancel_authorization_before_fresh_work`
  passes in the single libtest process; the earlier `StoreCommitContended`
  report did not reproduce on the pool.
- `//crates/lash-sim:lash-sim__unit_test` walked up from `CARGO_MANIFEST_DIR`
  to read repository-root docs and gate files. `//:confidence_gate_corpus`
  declares those four files and the walk now recognises the runfiles root. The
  `generated_sim_search_mode_keeps_summary_lean_and_labels_shards` case passed
  under Bazel's scheduling on every run measured for this change; no
  determinism fix was needed and none was faked.
- `//crates/lash-typescript:integration__test` already passed, because its
  Test262 and WPT trees ride in runfiles. It stayed on Cargo only because
  moving it alone left the Cargo compile in place; with the other three moved,
  that reason is gone. It carries `no-remote-exec`: its no-abort guarantee
  forks a dozen children that each parse deliberately deep sources right up to
  the stack bound, and their combined footprint exceeds any single-action
  memory budget the pool grants. Re-measured against the raised per-action
  floor, `the_abort_corpus_survives_without_the_preflight` and
  `fuzzed_sources_survive_without_the_preflight` still die of
  `signal: 9 (SIGKILL)`; the same label passes locally in 43 s. The tag pins
  placement rather than softening what the test proves.

`//crates/lash-core:lash-core__unit_test` needed the same pin when the pool
capped every action at 4 GiB: rustc for the workspace's largest test binary
peaks just above that and was killed without a diagnostic. Every pool worker
now grants an action at least 5 GiB, the compile executes remotely again, and
the pin is gone. Its two effect source lints still resolve module paths through
symlinks, which is correct in a runfiles tree and in a Cargo checkout alike.

`//crates/lash-sim:lash-sim__unit_test` declares `timeout = "long"`. It carries
the generated-simulation and minimizer fixture replays and ran 227-300 s on the
pool, which straddles Bazel's default `medium` 300 s bound.

Five tests do not move, and are excluded from the Bazel label by name rather
than silently: `durable_fault_matrix_real_cargo_filters_chunk_0..4` each fork a
real `cargo test -p … -- --list` against the workspace to prove the confidence
gate's name filters still select tests. That is a claim about Cargo's own test
selection, which a hermetic action without Cargo cannot make.
`tools/bazel/generate_build_files.py` passes libtest `--skip` for them on
`//crates/lash-core:lash-core__unit_test`, and the trunk-only `Test heavy
suites` job (`profile.ci-heavy`) is where they run — it now contains nothing
else. The `lash-sim` runner and minimizer fixture cases that used to share that
job are deterministic compute and run in the Bazel partition as cached actions;
they are no longer excluded from `profile.ci` either, so the untrusted Cargo
path runs them too.

Bazel bakes `CARGO_MANIFEST_DIR` as a path relative to the execution root,
where Cargo bakes an absolute one. A Bazel-built test binary is therefore only
path-correct when it runs with the repository root as its working directory,
which is neither a Bazel test action's runfiles root nor nextest's per-crate
working directory. That rules out feeding pool-built binaries to nextest
through `--binaries-metadata`.

The 15 Cargo-owned executable labels are eight PostgreSQL targets, the S3 unit
binary, the two `lash-sim` cross-backend binaries, the `lash-runtime` trybuild
binary, and three workflow-graph frontend binaries. Use the existing Cargo
recipes for these correctness contracts:

- `scripts/check_feature_coverage.py` still owns feature-combination coverage,
  but its lane *commands* no longer run as Cargo legs on a runner. This
  paragraph used to rule the opposite way, and the ruling was wrong. It rested
  on the claim that Bazel cannot express a per-command feature resolution; it
  can. `all_crate_deps()` returns first-party labels, `aliases()` maps them to
  extern crate names, and a variant target may substitute both, so
  `tools/bazel/generate_build_files.py` emits one target per distinct
  `(package, resolved feature set, Cargo target kind)` unit of every lane
  command, with first-party variants depending on first-party variants.
  `tools/bazel/feature_variants.py` reimplements Cargo's resolver v2 over
  `cargo metadata --locked` to compute those sets. Two reconciliations keep it
  honest: `generate_build_files.py --check` compares the unit SET against
  Cargo's own target tables, and `--verify-resolution` re-derives every
  distinct lane request with `cargo tree` and diffs the feature sets
  (`cargo check --unit-graph` would answer both at once, but it is nightly-only
  and this repo builds on stable). `scripts/check_feature_coverage.py check`
  additionally refuses a lane with no Bazel target, so the coverage plan and
  the lane graph cannot drift apart.

  The general feature-lane limitation is third-party: `crate.from_cargo` in
  `MODULE.bazel` pins `@crates` from one `//:Cargo.toml` + `//:Cargo.lock`
  resolution, so a variant sets `crate_features` on first-party targets but
  links third-party crates at the workspace feature union. The union is a
  superset, so a variant compiles against at least the third-party API Cargo
  would offer it; what a variant cannot catch is first-party code that only
  compiles because a third-party feature the workspace enables elsewhere is on.
  Optional third-party crates a variant's features turn on and the workspace
  never enables are named outright from `Cargo.lock` (`extra_deps`), because
  `all_crate_deps()` reports only the workspace resolution. Untrusted pull
  requests keep the Cargo matrix exactly as it was, which is where a real
  third-party feature divergence would still surface.

  The unconditional runtime OFF witness uses a separate `//:runtime_off`
  graph. `runtime_off_workspace.py` flattens Cargo workspace inheritance into
  generated manifests and links their source directories to the checkout.
  Its single root depends on `lash-runtime` with default features disabled.
  `runtime-off.Cargo.lock` records this resolution; `kiln sync` regenerates it
  from the current workspace lock. The `runtime_off_crates` hub selects
  `prune_unreachable` in the local rules_rs patch, so platform-inactive lock
  packages cannot enable features on reachable dependencies. The ordinary
  hub retains dormant dependencies needed by the feature-lane variants.

  Trusted lint CI builds this target alongside Clippy and schemas, then
  `check_runtime_off_graph.py` compares its actual Rust compiler features
  and dependency edges with the package-only Cargo tree. The existing native
  AWS-LC annotation replaces only `aws-lc-sys`'s Cargo build-script dependency
  subtree. All remaining Rust units and edges must match. Untrusted CI keeps
  `cargo check -p lash-runtime --lib --no-default-features --locked`.

  Cargo-required targets omitted from the resolved default graph are still
  recorded with `cargo-feature-gate` in `tools/bazel/target-inventory.json`.
  The `dependency-boundary` check (21 s of `cargo tree`, no compilation) moved
  to `repo-gates`; the `>= 130` default-off test-count floor is
  `FEATURE_LANE_TEST_FLOORS`, held by `scripts/ci/check_feature_lane_test_floors.py`
  against the pool-built test binary.
- The `lash-runtime` `ui` target owns trybuild compile-fail fixtures and their
  nested Cargo target cache.
- nextest profiles own workspace filtering, retries, and scheduling; the
  fault-matrix and simulation fixtures invoke nested Cargo builds.
- PostgreSQL, MinIO/S3, Restate, browser, Test262, judged-runbook, and other
  named live recipes own their services, environment, ignored-test selection,
  and runtime assets.
- fuzzing keeps its nightly `cargo fuzz` toolchain and corpus workflow.
- `cargo package`, the layered publisher, exact-SHA release validation, the
  release profile (`thin` LTO and stripping), and the `judged` profile remain
  Cargo-owned publication and artifact contracts.

Ignored live, regeneration, soak, and measurement tests remain ignored in the
ordinary Bazel binaries exactly as they are in Cargo's ordinary workspace run;
their named `--ignored` or live recipes remain authoritative. The one target
whose required feature is outside the default graph remains recorded as a
Cargo feature-gate target without a Bazel label.

## Doctests

There are none. Doctests were removed from the repository by ruling on
2026-09-13: there is no `//:workspace_doctests` partition, no `rust_doc_test`
wrapper, and every workspace library manifest sets `[lib] doctest = false`, so
`cargo test` never compiles a doc snippet either. The doc comments and their
fenced examples remain as prose; nothing compiles or executes them.
`scripts/test_bazel_test_contract.py` refuses a doc-test label, a
`rust_doc_test` load, or a library manifest that drops `doctest = false`.

## Clippy

`//:workspace_clippy` is the `cargo clippy --workspace --all-targets` shape as
one cached Bazel action per target: 190 labels, every first-party target of the
resolved default graph except the one label that merely *runs* a build script,
whose exemption is recorded as `clippy_exempt` in
`tools/bazel/target-inventory.json` because running a script compiles nothing
and that label exposes no `CrateInfo` for a clippy aspect to attach to. The
`build.rs` compile behind it is a `rust_binary` of its own,
`//crates/lash-protocol-rlm:build_script_`, and it is in the partition.

`tools/bazel/clippy.bzl` wraps the upstream `rules_rust` clippy action for three
reasons, all about matching Cargo's effective lint set rather than an
approximation of it:

- Clippy resolves its configuration by walking up from each crate's manifest
  directory and stopping at the first `clippy.toml`. This repository has three —
  the workspace file plus `crates/lash-core/clippy.toml` and
  `crates/lash-core-store/clippy.toml`, which carry the `disallowed-methods`
  list that `clippy::disallowed_methods` denies — while the
  upstream aspect binds a single config for the whole build. The aspect here
  selects the nearest declared config per target, so `lash-core` sees its own
  list instead of an empty one.
- The upstream action drops its `-D warnings` default as soon as a target
  carries a `lint_config`, and every rule in `lash_rust.bzl` sets one. `-D warnings`
  is appended after the `[workspace.lints]` flags, the same position Cargo's
  trailing `-- -D warnings` occupies.
- `cargo_build_script` declares the `build.rs` compile as a `rust_binary` of its
  own and forwards only a fixed set of attributes to it, `lint_config` not among
  them, so that one target reaches the action with no lint table. Cargo lints a
  build script against `[workspace.lints]` like any other target, so the aspect
  reads `@crates//:workspace_cargo_lints` itself for a target that carries none.

`slack-clone`'s `e2e` feature is outside the resolved default workspace graph,
so `cargo clippy -p slack-clone --all-targets --features e2e --no-deps` has no
Bazel equivalent and stays a Cargo command on every event.
## Service-backed jobs

The eleven `cargo-service-gate` labels are still *built* by Bazel from the
shared cache; only their execution is Cargo-free. `scripts/ci/store-tests.sh`
owns both paths for every suite in the `Test Postgres store` and `Test S3 store`
jobs and dispatches on `BAZEL_TRUSTED`. Two properties hold on the Bazel path:

- The PostgreSQL major, the connection URL, and the MinIO settings reach the
  binaries only through `--test_env`, which is part of the test spawn and of
  nothing else. Every compile action key is identical across the PG 14/16/18
  matrix legs, so the three jobs reuse one set of compiled outputs from the
  shared cache. `tools/bazel/postgres_test_labels.txt` and
  `tools/bazel/minio_test_labels.txt` are generated, so a new service-gated
  binary reaches its service job without a hand edit.
- A cached green for a test whose verdict depends on a live service is a false
  green, so these invocations pass `--nocache_test_results` and
  `--modify_execution_info=TestRunner=+no-cache,TestRunner=+no-remote-cache`.
  The execution-info filter is scoped to the `TestRunner` mnemonic precisely so
  the compile actions above it stay cacheable. `--local_test_jobs=1` reproduces
  Cargo's one-binary-at-a-time execution, which the suites that share one
  database and one bucket depend on.

### Running them locally: `scripts/ci/with-service.sh`

```sh
scripts/ci/with-service.sh                 # list the services
scripts/ci/with-service.sh pg16 -- bash scripts/ci/store-tests.sh pg-store
scripts/ci/with-service.sh all  -- bash scripts/ci/store-tests.sh s3-store
```

This is the same wrapper CI uses. The `postgres-store` and `s3-store` jobs run
every one of their suites inside it -- the workflow starts no container of its
own and names no image -- so a local run and a CI run are one code path rather
than two descriptions of the same thing that drift.

The wrapper starts the CI image on a free ephemeral port of the loopback
interface (never a fixed 5432 or 9000, so two lanes on one box cannot share a
database), waits for readiness on the same budget CI's health check used,
performs the one-time setup a service needs, exports the connection settings
under the environment variable names `store-tests.sh` forwards to the test
spawn with `--test_env`, and removes the container on success, failure and
Ctrl-C alike. `all` runs the given command against each service in turn.

The service table -- image, container port, container environment, readiness
probe, setup, exported settings -- lives in the wrapper and nowhere else.
`scripts/test_with_service.py` holds it to `ci.yml` (every store suite is
wrapped, every PostgreSQL matrix major is a declared service, the workflow
names no image) and exercises the lifecycle against a fake `docker` on PATH.

Locally the run takes the trusted path, so the binaries are shared-cache hits
and pool actions exactly as `kiln build`'s are; only the `TestRunner` spawn is
pinned local (`--strategy=TestRunner=local`), because the container publishes
its port on this host's loopback and a test action on a pool worker would
reach nothing. Test results are never cached, on either side. Outside GitHub
Actions `store-tests.sh` defaults `BAZEL_SHARED_CACHE_FLAGS` to that
configuration; inside CI both shared-cache variables stay required, so a job
that lost its credentials fails instead of quietly missing the cache.

A local run closes by printing the service-shaped cases it did *not* cover and
the exact recipe for each, so a green `with-service.sh all` is never mistaken
for full service coverage. (Inside GitHub Actions the report is suppressed:
each step wraps one suite, and the coverage question it answers is a local
one.) Those are the three Cargo-owned jobs below, the `slack-clone` `e2e`
feature, and the process-operations E2E driver, which stands up its own MinIO.
No `justfile` recipe was converted or removed: the store suites had none, and
the `*-soak` recipes are separate opt-in property runs that keep their Cargo
commands.

Untrusted events receive no cache credentials, so every step runs exactly the
Cargo command it ran before this cutover, including the Rust toolchain, mold,
nextest and Swatinem cache steps, which are conditioned on the same trust
decision.

The PostgreSQL matrix itself is per-event, resolved by `scripts/ci_plan.py
postgres-matrix` and consumed through the `plan` job's `postgres_matrix`
output:

| Event | PG 14 (compatibility) | PG 16 (primary) | PG 18 (compatibility) |
| --- | --- | --- | --- |
| `pull_request` (rust) | schema diffs only | runs | schema diffs only |
| `merge_group` (rust) | schema diffs only | runs | schema diffs only |
| `push` to `main` | skipped (queue already witnessed the SHA) | skipped | skipped |
| `workflow_dispatch` | runs | runs | runs |

The compatibility lanes only compare the live catalog artifact and a focused
version-stamp gate. They run when the diff touches `lash-postgres-store` or
`lash-sqlite-store`, or on the full-profile dispatch. Weekly confidence
backends remain the compatibility witness for unrelated landings. The lane is
removed from the matrix rather than kept with its steps skipped -- a leg that
ran no tests would be a hollow green.

Three jobs stay entirely Cargo-owned, and not for want of trying:

- `Test heavy suites` runs the nested-Cargo fault-matrix chunks, which fork
  real `cargo test` invocations of their own. Bazel cannot own a suite whose
  work is a Cargo build.
- `Seal-test the API surface` runs the `lash-runtime` trybuild binary. Bazel can
  build that binary from the shared cache, but the 742 s is not the harness
  compile: trybuild spawns its own `cargo` against `CARGO_TARGET_DIR` to build
  each compile-fail fixture, so the runner still needs the full workspace
  dependency graph materialised in a Cargo target directory. Moving the harness
  compile to the pool would leave that untouched, and narrowing the command off
  `--workspace` would change the feature unification the fixtures are sealed
  against.
- `Build worker release artifacts` compiles `--release` binaries. The generated
  graph is the development compilation graph; the release profile (`thin` LTO
  and stripping) stays a Cargo-owned artifact contract.
- `Restate + Postgres + MinIO Workers` executes shell E2E drivers
  (`scripts/restate-postgres-workers-e2e.sh` and the operator-flow scripts)
  over those release binaries rather than any Cargo or Bazel test label, so
  there is nothing to convert.

The `lash-runtime --features rlm` Agent Scenario is not a feature-gate
exception: `rlm` is inside the resolved default workspace graph, so the label
`//crates/lash:lash__unit_test` carries it. The remaining feature-gated target
is recorded with `cargo-feature-gate` in `tools/bazel/target-inventory.json`
and keeps its Cargo recipe.

The main CI workflow makes this a single authoritative partition. Trusted
same-repository pull requests and merge-queue groups run `//:workspace_tests`
with the authenticated shared cache. `main` pushes skip that core board (the
queue already witnessed the SHA) and keep breadth jobs. A rust PR runs no Cargo
workspace job at all on a trusted event: `tools/bazel/cargo_owned_nextest_filter.txt`
is `none()`, and the job's trusted branch refuses to run without a workbench
diff rather than launder an empty selection into a green. What it does run is
the workbench's Node-gated cases, selected by
`tools/bazel/workbench_nextest_filter.txt`, and its build is scoped to
`--package agent-workbench --tests` rather than `--workspace`: compiling and
linking every workspace crate on a two-core runner to execute one test made
this job the CI tail (836 s against the Bazel partition's 145 s on queue run
34804868851). An untrusted (fork or Dependabot) pull request receives no cache
credentials and therefore no Bazel partition, so it keeps the full Cargo
workspace run — the `sqlite-await-event-helper` example included, which the
store conformance tests spawn.

### The workbench split

`examples/agent-workbench`'s unit binary is partition-owned apart from the
cases that shell out to `node --test` to drive
`examples/agent-workbench/tests/browser_projection.mjs`, which need a Node
toolchain the Bazel action does not install and a Cargo-relative asset path a
runfiles tree does not reproduce. Rather than fork the binary, the split is by
name out of one binary, exactly as `lash-internal-core`'s fault-matrix chunks
are split: `tools/bazel/generate_build_files.py` holds the case names in
`NODE_GATED_WORKBENCH_TESTS` and derives both halves from that one list — the
Bazel label's `args = ["--skip=…"]`, so the label is honest about what it
executed, and `tools/bazel/workbench_nextest_filter.txt`'s `test(=…)` terms, so
the Cargo job selects exactly the skipped set. The generated inventory records
the same names under `bazel_skipped`, and
`scripts/test_bazel_test_contract.py` refuses a hand edit that leaves a gap or
an overlap between the two halves. Adding a Node-dependent workbench case means
adding its name to that one tuple and regenerating.
The `Lint` job builds
`//:workspace_clippy` in place of the workspace `cargo clippy`, and the
`Check workspace` job builds `//:workspace_compile` in place of
`cargo check --workspace --all-targets`, with
`--remote_download_outputs=minimal`: nothing on that runner consumes the
outputs, and a compile or link error still fails the build.
`//:workspace_compile` compiles *and links*
every label of the resolved default graph, including the
unit- and integration-test crates that carry the `cfg(test)` shape and the
members that are not `default-members`; it is generated from the same
`cargo metadata --locked` resolution the Cargo command uses, so feature
unification is identical, and the only Cargo target outside it,
`slack-clone-live-e2e`, is one `cargo check --workspace --all-targets` skips for
the same required-feature reason. Formatting, the Python and shell gates,
actionlint and the versioned-surface bump check stay as they were. The remaining trybuild, heavy, service,
feature, fuzz, packaging, and release jobs keep their own Cargo commands and
schedules. `CI conclusion` requires the Bazel job to succeed on every trusted
event.

Fork and Dependabot pull requests never receive cache credentials: their Bazel
job is intentionally skipped, their ordinary nextest job omits the generated
filter, the `Lint` and `Check workspace` jobs run exactly the
Cargo clippy and check commands that predate this cutover, and the
service jobs take the Cargo branch of `scripts/ci/store-tests.sh`, preserving
the full workspace fallback. `CI conclusion` accepts that
skip only when the shared trust decision classifies the event as untrusted.

Every CI job that talks to the shared pool configures it through the
`.github/actions/bazel-shared-cache` composite action, which pins Bazelisk,
resolves the executor runtime fingerprint, materializes the client certificate,
and exports `BAZEL_SHARED_CACHE_FLAGS` and `BAZEL_OUTPUT_USER_ROOT`. Pair it
with an `if: always()` step that removes `"$RUNNER_TEMP/build-cache"`.

CI does not compile. Trusted events submit their actions to the same execution
pool a local fork uses, so the two-core GitHub runner uploads inputs, waits, and
downloads results. The runner and a fork advertise one `kiln_executor_runtime`
and share one action cache namespace: a label a fork already built is a cache
hit in CI and the reverse.

## Where each deployment fact lives

The pool endpoint, the REAPI instance, the executor runtime fingerprint, the
client certificate and this host's cache directories are deployment facts. They
move when the pool is redeployed or the executor is repinned, and they differ
between a development host and a CI runner, so none of them is committed:

| Fact | Locally | In CI |
| --- | --- | --- |
| Endpoint, instance, runtime fingerprint | `.kiln.bazelrc` | `CACHE_ENDPOINT`, `CACHE_INSTANCE`, `KILN_EXECUTOR_RUNTIME` |
| Client certificate | `.kiln.bazelrc` (paths under `~/.config`) | `CACHE_CA`, `CACHE_CERT`, `CACHE_KEY` |
| Output base and caches | `.kiln.bazelrc` | `$RUNNER_TEMP` |

kiln generates `.kiln.bazelrc` from the installed executor manifest on every
fork and golden refresh; it is gitignored and `.bazelrc` `try-import`s it. The
CI values are `build-cache` environment secrets, masked on read and validated
by `.github/actions/bazel-shared-cache`, which fails with the name of any empty
one. The build-infra repository writes both sides; nothing here is edited by
hand. `.bazelrc` keeps only what the pool is asked *for* — the common one-CPU,
2 GiB fallback and explicit per-target resource requests, `--remote_local_fallback=false`, and the download and
upload policy — and `scripts/test_bazel_test_contract.py` refuses an IP
address, an instance name, a fingerprint, a certificate path or a home
directory in `.bazelrc`, under `.github/`, or in `scripts/ci_plan.py`.

The local `--jobs=16` and CI `--jobs=32` limits count in-flight remote actions,
not local cores. Resource defaults live in the unconditional `build` section
of `.bazelrc`, so forks and CI use identical action keys for inherited requests.
Sized targets keep their existing higher floors. Aligning CI's previous
4 CPU/4 GiB fallback with the local 1 CPU/2 GiB fallback changes the keys of
unannotated CI actions once; those actions can then reuse local results.

`--remote_local_fallback=false`
makes an unreachable pool a red job rather than a silent two-core compile,
which is the intended trust posture. Service-backed tests are the one spawn
that stays on the runner: `scripts/ci/store-tests.sh` adds `no-remote-exec` to
the `TestRunner` mnemonic, because the database or bucket the job stood up
listens on the runner's loopback and exists nowhere else. Their compile actions
still run on the pool.

The Bazel default is the development compilation graph. Timing comparisons
must use Rust 1.98.1, the resolved default workspace features, equivalent
optimization and debug flags, the same target set, and separately reported
cold repository, cold action-cache, warm, representative-edit, and second-fork
runs. Compare focused Bazel edit builds with Cargo's existing `cargo check`
path as separate operations: Bazel `build` produces linkable
artifacts, while Cargo `check` normally stops at metadata. Cargo release or
judged timings are not comparable to this graph.


## Schema checks in portable functional E2E

The workflow-graph functional E2E job remains a Cargo-owned full-profile gate
without pool credentials. Its integration recipe explicitly checks the example
schemas through the portable generator before checking generated TypeScript,
running Vitest, and building with Vite. This route requires a GitHub workflow
dispatch; it does not turn a missing local Kiln installation into a Cargo fallback.
Ordinary forks use the shared schema actions, and untrusted Lint retains its
separate portable path for both host and example schemas.

## Remote action diagnostics

Use an explicit bundle and baseline when a compile unexpectedly repeats or a
remote action is slow. The command submits through Kiln with the normal jobs
and resource policy; it adds a JSON profile, compact execution log, gRPC log
and invocation/source manifest. Bundles are private local artifacts and may
contain command environment values: keep them outside the checkout and do not
upload raw logs to CI artifacts.

```sh
python3 scripts/bazel-diagnose.py capture --output /tmp/lash-before test //crates/lash-sansio:lash-sansio__unit_test
# Make the representative edit, then capture a separate bundle.
python3 scripts/bazel-diagnose.py capture --output /tmp/lash-after --baseline /tmp/lash-before test //crates/lash-sansio:lash-sansio__unit_test
python3 scripts/bazel-diagnose.py report /tmp/lash-after
python3 scripts/bazel-diagnose.py compare /tmp/lash-before /tmp/lash-after
```

The first invocation downloads the checksum-pinned BuildBuddy CLI into the
user cache. Its `print` and `explain` commands decode local files without a
BuildBuddy service or credentials. It does not replace the Bazel executable.
`build.log` receives live build output; `manifest.json` records exit status,
interruptions, source-content identity and capture/analysis durations.
`summary.json` joins action digests to real executed-action worker metadata;
`explain.txt` identifies source, argument, environment and property changes.
A cached result's worker is historical. Missing metadata, ambiguous retries,
truncated captures and negative clock-skewed queue timestamps remain explicit.
A successful build with incomplete diagnostics keeps its successful exit status
and marks the diagnostics incomplete in the manifest.

The source digest covers tracked and non-ignored untracked files, including
executable bits and symlink destinations; ignored/generated inputs are described
by the captured action log instead. The diagnostic manifest is an observation,
not a reusable validation receipt. Capture is opt-in: remote logs add I/O and
result decoding has its own measured duration. Compare like target/features,
cache state and host load, and keep upload/queue time separate from execution.

### Compile source ownership

`tools/bazel/source-ownership.json` records reviewed source boundaries that the
Cargo target list cannot express. A test entry names its crate root and all
module/include source patterns it compiles, including shared helpers. Its
patterns apply to both the default target and every feature-lane variant.
Unlisted targets retain conservative package source inputs. Python interpreter
bytecode caches and node_modules dependency trees are excluded from package input
and runfiles globs so script execution or npm installation cannot invalidate Rust
actions. Generated frontend assets remain declared where they are consumed. The generator
rejects missing roots, stale patterns, unknown targets and paths outside the
package; `kiln sync` writes the declarations into BUILD files.

`library_test_sources` names individual external modules that are reachable
only under `cfg(test)`. Normal and feature-variant libraries exclude these
files from compilation inputs; the unit-test crate retains them. Do not put
`cfg(feature = "testing")` fixtures here: libraries compile those fixtures.
If a module becomes production-reachable, remove its test-only declaration in
the same change. Rustc must still find every real compile input in a hermetic
build, so a missing declaration fails compilation instead of silently hiding
the dependency. Runtime source-scanning tests continue to declare their files
through `extra_data`; compile ownership does not remove those witnesses.

When narrowing a boundary, compare action inputs and run a controlled sibling
edit and shared-helper edit. The sibling edit should compile only its owner;
the shared edit should compile both. Count Rustc executions independently of
test executions, since package source runfiles can still re-run source-reading
tests without recompiling them.

`unit_test_sources` can narrow a library's unit-test source patterns separately
from its integration roots. Core-execution uses it to keep public model
contracts out of the large unit-test compile. `tests/process_model.rs` owns
58 lease-wire and registry-transition tests; `tests/effect_model.rs` owns
35 effect-group, retained tool-child and journal-outcome tests. Their module
trees retain the original unit-test names and assertions and call existing
public APIs. Private runtime tests remain in the unit binary.

Both Cargo discovery and generated Bazel partitions include these integration
binaries. The `core-internal-features` lane also runs them with no default
features, including no `testing` feature, against the production library.
The lease-preimage and retained-tool-child mutation recipes select their
integration targets as well as the unit tests.

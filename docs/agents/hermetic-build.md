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

Use `kiln build` for the warm shared-cache compilation path and `kiln test` for
the generated cacheable test partition. Implementer loops do not run Postgres,
S3, or E2E (`kiln test --service`, store recipes, Restate workers): CI owns
those, and local live gates fight over ports (`KILN_GATE_ID`) and the
single-box database.

```sh
. ./env.sh
kiln build
kiln test
```

The lower-level entry script remains available for graph analysis, sync, local
executor reproduction, and focused Bazel labels.

`analyze` validates generated files and performs Bazel loading and analysis with
`--nobuild`; it does not run rustc and is not a substitute for `cargo check`.
`build` compiles and links the requested Bazel labels. `test` without labels
builds and executes the generated deterministic/default-feature workspace
suite; explicit labels remain available for a focused edit loop.

```sh
# Analyze the generated graph and reject metadata or module-lock drift.
scripts/hermetic-build.sh analyze

# Compile the complete Cargo --workspace --all-targets shape.
kiln build

# Run the generated cacheable test suite (PR Bazel partition).
kiln test

# Path-plan like CI. Docs-only skips compile; workbench-only does not
# compile lash-core or Postgres. Never starts Postgres, S3, or E2E.
scripts/dev-test.sh

# Lint the `--workspace --all-targets` shape (170 clippy actions).
kiln build //:workspace_clippy

# Compile or run focused targets instead.
kiln build //crates/lash-core:lash-core
kiln test \
  //crates/lash-sansio:lash-sansio__unit_test \
  //crates/lash-sqlite-store:integration__test
```

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
a different test action key. Failed tests are never reused as successes. The
executor tracks up to eight actions. Each action declares four CPUs and 4 GiB by
default; its 16-CPU scheduling capacity admits up to four such actions at once.
Bazel queues at most eight jobs, repository loading uses four threads,
and each checkout's Bazel server has a 4 GiB heap ceiling. The Bazel server
remains in the caller's cgroup; remote compilation runs inside the executor's
`kiln-heavy.slice` budget. Keeping a Bazel server alive preserves its analysis
cache.

The Kiln golden is maintained outside agent forks. Its refresh prewarms the
shared action cache with the equivalent of:

```sh
scripts/hermetic-build.sh --shared build
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
Of its 109 executable test binaries, `//:workspace_tests` owns 89 deterministic
binaries. The remaining 20 labels carry `manual`, a reason tag, and a durable
`cargo_only` explanation in `tools/bazel/target-inventory.json`; this keeps both
the aggregate and `bazel test //...` from treating an unconfigured service,
special scheduler, or path fixture as proof. The partition is generated from
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

`lash-core__unit_test` and `lash-sim__unit_test` were tried in the partition
and moved back: the reasons were real, not bookkeeping. `lash-core`'s
`durable_fault_matrix_fast_gate_executes_all_nonblocked_evidence` executes
`scripts/confidence-gate.sh` against a fake Cargo on `PATH`; `lash-sim`'s
`postgres_effect_history_native_claim_is_consistent_across_reviews_docs_and_gate`
walks up from `CARGO_MANIFEST_DIR` to read repository-root docs and gate files,
and its `generated_sim_search_mode_keeps_summary_lean_and_labels_shards` case
reported simulator nondeterminism under Bazel's scheduling. Their reasons in
`tools/bazel/target-inventory.json` now name those cases.

The 20 Cargo-owned executable labels are eight PostgreSQL targets, the S3 unit
binary, the two `lash-sim` cross-backend binaries, the `lash-sim` unit binary,
the `lash-runtime` trybuild binary, the `lash-core` unit and nested-metadata
binaries, the TypeScript integration binary, the agent-workbench unit binary
that includes a Node.js browser projection gate, and three workflow-graph
frontend binaries. Use the existing Cargo recipes for these correctness
contracts:

- `scripts/check_feature_coverage.py` and the explicit no-default-feature
  commands own feature-combination coverage. Cargo-required targets omitted
  from the resolved default graph are recorded with `cargo-feature-gate` in
  `tools/bazel/target-inventory.json`.
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
one cached Bazel action per target: 170 labels, every first-party target of the
resolved default graph except the one `cargo_build_script` label,
whose exemption is recorded as `clippy_exempt` in
`tools/bazel/target-inventory.json` because `cargo_build_script` exposes no
`CrateInfo` for a clippy aspect to attach to.

`tools/bazel/clippy.bzl` wraps the upstream `rules_rust` clippy action for two
reasons, both about matching Cargo's effective lint set rather than an
approximation of it:

- Clippy resolves its configuration by walking up from each crate's manifest
  directory and stopping at the first `clippy.toml`. This repository has two —
  the workspace file and `crates/lash-core/clippy.toml`, which carries the
  `disallowed-methods` list that `clippy::disallowed_methods` denies — while the
  upstream aspect binds a single config for the whole build. The aspect here
  selects the nearest declared config per target, so `lash-core` sees its own
  list instead of an empty one.
- The upstream action drops its `-D warnings` default as soon as a target
  carries a `lint_config`, and every generated Lash target does. `-D warnings`
  is appended after the `[workspace.lints]` flags, the same position Cargo's
  trailing `-- -D warnings` occupies.

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

### Running them locally: `kiln test --service`

`kiln test --service <pg14|pg16|pg18|s3|all>` runs those same suites on this
box. It starts the CI image on a free ephemeral port, waits for readiness,
runs `scripts/ci/store-tests.sh` for each suite the matching CI job runs, in
the same order and with the same environment variable names, and removes the
container on success, failure and Ctrl-C alike. It is the same script, not a
second copy of the test selection: `tools/kiln/services.json` names only the
image, the readiness probe, the environment and the suite list, and
`scripts/test_kiln_service_manifest.py` fails if it drifts from `ci.yml`.

Locally the run takes the trusted path, so the binaries are shared-cache hits
and pool actions exactly as `kiln build`'s are; only the `TestRunner` spawn is
pinned local (`--strategy=TestRunner=local`), because the container publishes
its port on this host's loopback and a test action on a pool worker would
reach nothing. Test results are never cached, on either side. Outside GitHub
Actions `store-tests.sh` defaults `BAZEL_SHARED_CACHE_FLAGS` to that
configuration; inside CI both shared-cache variables stay required, so a job
that lost its credentials fails instead of quietly missing the cache.

Every run closes by printing the service-shaped cases it did *not* cover and
the exact recipe for each, so a green `kiln test --service all` is never
mistaken for full service coverage. Those are the three Cargo-owned jobs
below, the `slack-clone` `e2e` feature, and the process-operations E2E driver,
which stands up its own MinIO. No `justfile` recipe was converted or removed:
the store suites had none, and the `*-soak` recipes are separate opt-in
property runs that keep their Cargo commands.

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
  real `cargo test` invocations of their own, and the generated simulation and
  minimizer fixtures scheduled by `profile.ci-heavy`. Bazel cannot own a suite
  whose work is a Cargo build.
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
queue already witnessed the SHA) and keep breadth jobs. On rust PRs the
ordinary nextest job reads the generated
`tools/bazel/cargo_owned_nextest_filter.txt` (service-gated, trybuild, and
workbench binaries excluded so they are not compiled just to self-skip).
Workbench unit tests run only when `examples/agent-workbench/**` changed.
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
hand. `.bazelrc` keeps only what the pool is asked *for* — the four-CPU,
4 GiB action shape, `--remote_local_fallback=false`, and the download and
upload policy — and `scripts/test_bazel_test_contract.py` refuses an IP
address, an instance name, a fingerprint, a certificate path or a home
directory in `.bazelrc`, under `.github/`, or in `scripts/ci_plan.py`.

`--jobs=32` counts in-flight remote actions rather than local cores, against
the eight concurrent slots the pool advertises. `--remote_local_fallback=false`
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

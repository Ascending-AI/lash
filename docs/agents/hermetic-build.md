# Hermetic Rust builds

Lash uses Bazel 9.1.0 and rules_rs 0.0.110 for checkout-independent local
compilation. Cargo manifests and `Cargo.lock` remain the source of truth. The
generated BUILD files expose one Bazel action for each first-party library,
binary, example, benchmark, unit-test crate, integration-test crate, doctest,
and custom build script reachable in Cargo's resolved default workspace graph.
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
the generated cacheable test partition:

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

# Run the generated cacheable test suite (87 test binaries).
kiln test

# Run the doctest partition (35 rustdoc binaries).
kiln test //:workspace_doctests

# Lint the `--workspace --all-targets` shape (170 clippy actions).
kiln build //:workspace_clippy

# Compile or run focused targets instead.
kiln build //crates/lash-core:lash-core
kiln test \
  //crates/lash-sansio:lash-sansio__unit_test \
  //crates/lash-sqlite-store:integration__test
```

These build and test commands select the host shared executor by default.
`scripts/hermetic-build.sh --shared build` makes that choice explicit. It uses REAPI
instance `kiln` at `grpc://127.0.0.1:45191`, requires the declared NativeLink
runtime identity, and fails when the executor is unavailable. NativeLink
executes trusted builds directly on the host. Hermeticity here describes pinned
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

The shared caches live under `/home/sam/.cache/lash-bazel`; Bazel action keys use
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

Bazel maps each checkout to a hashed directory below
`/home/sam/.cache/lash-bazel/user-root`; `clean --expunge` removes only the
calling checkout's output base. It preserves the shared
`/home/sam/.cache/lash-bazel/repository` and
`/home/sam/.cache/lash-bazel/disk` caches. Kiln invokes this cleanup before
deleting a fork.

The generated graph follows Cargo's resolved default workspace feature graph.
Of its 109 executable test binaries, `//:workspace_tests` owns 87 deterministic
binaries. The remaining 22 labels carry `manual`, a reason tag, and a durable
`cargo_only` explanation in `tools/bazel/target-inventory.json`; this keeps both
the aggregate and `bazel test //...` from treating an unconfigured service,
special scheduler, or path fixture as proof. The partition is generated from
Cargo metadata and checked by `scripts/test_bazel_test_contract.py` so new or
reclassified targets cannot disappear into a hand-maintained list.

The 22 Cargo-owned executable labels are eight PostgreSQL targets, the S3 and
Restate unit binaries, the `lash-runtime` unit and trybuild binaries, the
`lash-core` unit and nested-metadata binaries, three `lash-sim` heavy/backend
binaries, the TypeScript integration binary, the agent-workbench unit binary
that includes a Node.js browser projection gate, and three workflow-graph
frontend binaries. Use the existing Cargo recipes for these correctness contracts:

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

`//:workspace_doctests` executes the 35 `rust_doc_test` labels — one per
first-party library whose manifest leaves `doctest` enabled, which is exactly
the set Cargo builds. rustdoc runs them against the pinned 1.98.1 toolchain and
the crate's declared dependency graph; none reaches a service, the network, or a
Cargo-relative asset, and none depends on the working directory, so their
results are deterministic and cacheable under `--cache_test_results=yes` like
any other Bazel test action. An input change produces a different action key,
and a failed doctest is never reused as a success. On this tree the partition
runs 25 cases and skips 4 ignored ones, the same counts `cargo test --doc
--workspace --locked` reports across the same 35 rustdoc binaries.
`scripts/test_bazel_test_contract.py` refuses any doctest label that
reacquires `manual` or a `cargo_only` reason, so a label cannot leave the
partition silently.

## Clippy

`//:workspace_clippy` is the `cargo clippy --workspace --all-targets` shape as
one cached Bazel action per target: 170 labels, every first-party target of the
resolved default graph except doctests and the one `cargo_build_script` label,
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

The main CI workflow makes this a single authoritative partition. Trusted
same-repository pull requests, merge-queue groups, `main` pushes, and manual CI
dispatches run `//:workspace_tests` with the authenticated shared cache. On the
same events the ordinary nextest job reads the generated
`tools/bazel/cargo_owned_nextest_filter.txt`, so its `profile.ci` run executes
only ordinary cases from the 22 Cargo-owned binaries. The `Lint` job builds
`//:workspace_clippy` in place of the workspace `cargo clippy`, and the
`Check workspace + doctests` job builds `//:workspace_compile` and runs
`//:workspace_doctests` in place of `cargo check --workspace --all-targets` and
`cargo test --doc --workspace`. `//:workspace_compile` compiles *and links*
every label of the resolved default graph except doctests, including the
unit- and integration-test crates that carry the `cfg(test)` shape and the
members that are not `default-members`; it is generated from the same
`cargo metadata --locked` resolution the Cargo command uses, so feature
unification is identical, and the only Cargo target outside it,
`slack-clone-live-e2e`, is one `cargo check --workspace --all-targets` skips for
the same required-feature reason. Formatting, the Python and shell gates,
actionlint, the versioned-surface bump check and the trunk-only perf smoke are
cheap and stay exactly as they were. The remaining trybuild, heavy, service,
feature, fuzz, packaging, and release jobs keep their own Cargo commands and
schedules. `CI conclusion` requires the Bazel job to succeed on every trusted
event.

Fork and Dependabot pull requests never receive cache credentials: their Bazel
job is intentionally skipped, their ordinary nextest job omits the generated
filter, and the `Lint` and `Check workspace + doctests` jobs run exactly the
Cargo clippy, check and doctest commands that predate this cutover, preserving
the full workspace fallback. `CI conclusion` accepts that
skip only when the shared trust decision classifies the event as untrusted.

GitHub-hosted actions execute locally, not in the shared executor's pinned
runtime image. Their remote-cache platform property is therefore derived from
GitHub's `runner.os`, `runner.arch`, `ImageOS`, and `ImageVersion` values. This
gives each concrete GitHub runner image a deterministic action identity
distinct from `kiln_executor_runtime`; the cache service and instance remain
shared, but actions cannot cross the runtime boundary under the same key.

The Bazel default is the development compilation graph. Timing comparisons
must use Rust 1.98.1, the resolved default workspace features, equivalent
optimization and debug flags, the same target set, and separately reported
cold repository, cold action-cache, warm, representative-edit, and second-fork
runs. Compare focused Bazel edit builds with Cargo's existing `cargo check`
path as separate operations: Bazel `build` produces linkable
artifacts, while Cargo `check` normally stops at metadata. Cargo release or
judged timings are not comparable to this graph.

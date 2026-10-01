# Hermetic Rust builds

Lash uses Buck2 with the existing Kiln NativeLink execution pool. Cargo
manifests and `Cargo.lock` remain canonical. Generated BUCK files expose the
libraries, binaries, examples, benchmarks, tests and build scripts in Cargo's
resolved workspace graph. These labels do not expand the published SDK surface
described by ADR 0079. Cargo consumers, including Figments, build these sources
independently. Figments retains its existing build backend.

## Developer loop

Use one isolated Kiln fork per agent. Change to the printed path and source its
environment before any Cargo command:

```sh
fork="$(kiln fork lash my-change)"
cd "$fork"
. ./env.sh
kiln test //crates/lash-core:lash-core__unit_test --test_arg=<filter>
python3 scripts/dev-test.py
```

`kiln test` runs `//:dev_tests`, the deterministic developer partition.
`//:workspace_tests` adds developer-deferred binaries and is the complete PR
partition. Neither is every correctness gate. PostgreSQL, S3, Restate,
nested-Cargo tests, release artifacts and named recipes retain their contracts.

| Command | Behavior |
| --- | --- |
| `kiln analyze` | Checks generated-file drift and analyzes the workspace graph without running compilers. |
| `kiln build` | Compiles and links the resolved workspace targets. |
| `kiln check` | Checks the workspace's libraries, binaries and tests using Rust metadata outputs, without unnecessary code generation or linking. |
| `kiln test` | Runs `//:dev_tests`; explicit labels select focused tests. |
| `kiln clippy` | Lints `//:workspace_clippy`, or requested Rust labels, with the effective Cargo lint policy and `-D warnings`. |
| `kiln doc` | Renders workspace API documentation. |
| `kiln run //path:binary -- <args>` | Compiles on the pool, materializes the executable and runs it locally. |
| `kiln fmt -- --check` | Runs local Cargo formatting. |
| `kiln sync` | Regenerates dependency and first-party rules from canonical Cargo inputs. |
| `kiln clean` | Cleans this checkout's Buck2 state. |

Package patterns work as in stock Buck2. `kiln check //crates/lash-store-sql/...`
selects every generated Cargo target beneath the package; `//pkg:all`, `//pkg:`
and `//...` also work. `build`, `check` and `clippy` map each target to the
output its label would select and omit feature-lane variants. As under Bazel, a
pattern in `build`, `check`, `test`, `clippy`, `doc` or `analyze` skips every
`manual` target and prints the skipped labels on one line; a manual target is
selected only by its own label. An explicit `kiln test` of a `cargo-trybuild`
label fails at once, naming `//crates/lash:ui_fixtures` and `just seal`. A
`check`, `clippy` or `doc` pattern must match a generated target.
A `build` pattern that matches none passes to Buck2 unchanged. `kiln analyze`
accepts labels and patterns as its dependency universe.

Use the owning target while editing. `python3 scripts/dev-test.py --dry-run`
prints the diff, labels, revisions and commands. `--dependents` includes reverse
dependencies within the developer partition. Complete batches are substituted
once; individual members, manual service targets and deferred tests are not
silently added by package wildcards. Shared build inputs and unknown tooling
widen selection. A docs-only diff does not run Rust tests. Known Python test
edits run the exact repository-gate command CI uses.

The planner refuses live PostgreSQL/S3 settings. It writes a plan and receipt
under Git's `lash-validation/` directory and serializes requests within a fork.
Every Rust request still calls Buck2, including a request that waited for another
run. A receipt observes source/configuration state; it does not replace action
validation. Changes during a run invalidate it. The build engine validates
ignored generated inputs and toolchain files beyond the planner's Git snapshot.

`just floor` remains an explicit broad tooling checkpoint. It combines developer
and feature tests, Clippy, formatting, schema checks and the CI repository-script
inventory. The locally excluded launcher-reset gate is named in its result table;
`scripts/ci/repository-gates.sh --all` includes it. `just schema-check` selects
store-schema gates. Use one build request at a time per fork and independent
forks for concurrent agents.

Never write under `golden-*`, delete forks manually, share an active
implementation/review checkout or stop another agent's builds. Remove your fork
with `kiln rm lash <name>` after merge. Cargo on PATH is Kiln's shim. Do not set
`CARGO_*` manually or change budgets without measurements. Local Cargo validation
must retain `--workspace --all-targets --locked`; a package-only check changes
feature unification and rebuilds dependencies.

## Reproducible graph and tools

`tools/buck2/pins.json` selects a checksum-verified official Buck2 release and
matching prelude. Bootstrap installs a regular executable in ignored
`.buck2/bin`; it does not compile Buck2 or modify installed tools.
`tools/buck2/toolchain-lock.json` pins the Rust 1.98.1 archives, including rustc,
rustdoc, Clippy and the Linux standard library. Toolchain files are declared
action inputs. `tools/buck2/reindeer-lock.json` pins Reindeer, and
`native-tools-lock.json` pins the LLVM and Node archives. CI restores the
materialized tools and dependency sources only under an exact key covering
the pins, bootstraps, rule overlay, Reindeer inputs and Python ABI. The trusted
main warmer is the sole writer; credentials, daemons and build outputs are
excluded from that tool cache.

Outside CI, checkouts share these pinned inputs through one store, by default
`$XDG_CACHE_HOME/lash-buck2` (`~/.cache/lash-buck2`); `LASH_BUCK2_STORE` names
another directory, or `off`. Each entry is named by its pin, built once under a
lock, sealed by a manifest and replaced if it no longer matches. A checkout
gets the Buck2 executable, prelude, Rust toolchain, native tools and Reindeer
as ordinary read-only files: reflinks or hardlinks when the store is on the
checkout's filesystem, copies otherwise. Buck2 never reads an input through a
link out of the project, so action digests equal those of a private install.
Only `vendor`, which Buck2 ignores, is a symlink into the store. A generated
graph receipt is shared the same way and re-verified by the checkout that
adopts it. Replace a pinned file instead of editing it in place. With no usable
store, bootstrap installs everything into the checkout as before. Bound the
store with `python3 tools/buck2/bootstrap.py --prune-store`; see
[the tooling notes](../../tools/buck2/README.md#shared-bootstrap-store).

After changing dependencies, workspace membership or target policy:

```sh
kiln sync
git diff -- tools/buck2 third-party/rust '**/BUCK'
kiln analyze
```

`tools/buck2/sync.py --check` rejects stale generated files. The target inventory
is `tools/buck2/target-inventory.json`. Third-party rules live in
`third-party/rust/BUCK`, with checksum-pinned crate archive downloads as Buck2
inputs. Bootstrap also prepares a Cargo vendor cache, but the generated Buck2
graph currently consumes the archive directories. Do not hand-edit generated
BUCK files. Normal builds reject drift; they do not silently update lockfiles
or Cargo features.

Reindeer fixups under `tools/buck2/fixups/` describe AWS-LC, SQLite, ring and
other native dependencies. Build scripts use declared compiler/linker, headers
and runtime inputs. Reindeer has no global fixup default. `sync.py` therefore
forwards `rustc-link-lib` and `rustc-link-search` output from every third-party
build script, as Cargo does. To opt a crate out, set `rustc_link_lib = false`
or `rustc_link_search = false` in its fixups, with a reason comment on the line
above. The graph contracts reject missing forwarding, stale opt-outs and
redundant `true` fixups. `rustc-link-arg*` needs no forwarding: Cargo applies
it only to the emitting package's own binaries, tests and cdylibs. The
contracts require third-party packages to contribute libraries only. Proc macros and build scripts remain host tools. The
first-party RLM schema and VM-worker fingerprint generation retain their
build-script outputs and environment. Browser tests declare the pinned Node
interpreter and script inputs, including feature variants; Cargo retains its
existing PATH fallback.

`tools/buck2/package-policy.toml` contains behavior Cargo metadata cannot
express: partitions and reasons, extra inputs, serial execution, libtest
arguments, helper binaries, timeouts, shards, feature-only inputs, service
membership and resource exceptions. Rules apply in file order. Unknown packages,
target kinds, targets or helper binaries fail generation.

## Sources, assets and profiles

`tools/buck2/source-ownership.json` records reviewed compile boundaries for
integration and unit targets. Shared helper patterns apply to default and feature
variants. Unlisted targets retain conservative source inputs. The generator
rejects missing crate roots, stale patterns, unknown labels and paths outside
the owning package.

`library_test_sources` excludes files reachable only under `cfg(test)` from
production libraries while retaining them in unit tests. Do not put
`cfg(feature = "testing")` fixtures there. Remove a declaration when its module
becomes production-reachable. Runtime source-scanning tests retain their inputs.

Non-Rust compile inputs are explicit. Embedded SQL, schema-shape files,
regression JSON, provider scripts and UI HTML belong in the owning package's
`compile_data`; cross-package inputs remain additive. `test_data` assigns a
fixture to its owning test. Other runtime inputs remain conservative where tests
read package sources. Bytecode caches and `node_modules` do not become Rust
compile inputs. Add an embedded asset and its declaration together.

A test runs from the project root. `CARGO_MANIFEST_DIR` and
`CARGO_BIN_EXE_<name>` are project-relative at compile and run time, so they
resolve on any runner. Generated filegroups are symlink trees and runtime files
are exported by reference: a declared file is visible at its checkout path,
whichever package owns it. Each cross-package input keeps a name derived from
its label, so two packages' `:rust_sources` never displace each other. The
wrapper and batch runner pass the Rust test's own environment; they do not use
the prelude's env injector, whose file records the writing host's paths.

For a narrowed boundary, compare sibling-only and shared-helper edits. The first
should recompile only its owner; the second should recompile both. Count
compilation independently from tests that scan source at runtime.

The default preserves the previous compilation geometry: target optimization,
debug information, assertions, overflow checks, lints, features and metadata/full
artifact behavior are part of the contract. Per-package optimization derives
from `Cargo.toml`. Host proc macros and build tools retain separate optimization.
`--config=judged` changes target configuration without changing host tools.
Cargo release and publishing profiles remain authoritative for their artifacts.
Published `rust-version` retains its existing compatibility floor.

## Feature coverage and linting

`scripts/feature-coverage.toml` remains the canonical lane board. The generator
emits first-party variants for each distinct package, resolved feature set and
Cargo target kind, with dependencies pointing to the correct variants. A
variant links only the dependencies its resolution activates, and a test
variant runs under its ordinary label's shard count and timeout.
`tools/buck2/feature_variants.py` computes the feature closures from locked
metadata for the workspace's Cargo resolver 3. `sync.py --verify-resolution` reconciles them with Cargo's resolution.
`scripts/check_feature_coverage.py check` rejects missing lane units.

`//:feature_lanes` provides metadata-only checks matching the lane commands.
`//:feature_lane_clippy` lints them; `//:feature_lane_tests` links and executes the
required test units. Runtime default-off and Restate release-feature witnesses
remain explicit regressions. Required-feature targets outside the default graph
remain recorded with a Cargo feature-gate reason in the inventory.

The existing third-party feature limitation remains explicit. External
dependencies use the workspace feature union; first-party variants select their
exact lane closure. A trusted lane therefore cannot detect first-party code
that accidentally relies on an external feature enabled elsewhere. Untrusted CI
retains the actual Cargo feature matrix. Optional external crates used only by a
lane remain explicit locked dependencies. This migration does not claim a
stronger third-party feature proof.

Clippy covers the resolved workspace shape, including first-party build-script
compilation. It selects the nearest declared `clippy.toml`, applies workspace and
package lints, and appends `-D warnings` last. Unsupported labels, an empty
selection or missing lint outputs fail. Feature linting retains each lane's
configuration. Every workspace library keeps `doctest = false`; documentation
examples remain prose and there is no doctest aggregate.

## Execution and resource accounting

Shared execution is the default:

```sh
scripts/hermetic-build.sh --shared build //crates/lash-core:lash-core
```

The driver fails when pool configuration, credentials or the runtime identity
are missing. It does not silently fall back to a local compiler. `--local`
explicitly selects reproduction/bootstrap execution:

```sh
scripts/hermetic-build.sh --local build //crates/lash-core:lash-core
```

Platforms preserve `cpu_count`, `memory_kb`, `cpu_arch`, `OSFamily` and
`kiln_executor_runtime`. Compile requests derive from
`tools/buck2/action-sizes.json`; test runs use the separate
`tools/buck2/test-run-sizes.json`. Generated `exec_sizes.bzl` resolves measured rows,
inherited feature rows, policy floors and pinned exceptions. Inherited compiles
request 1 CPU and 1.5 GiB. Unmeasured tests retain their existing policy requests,
including large-suite and timing-sensitive floors. Batches reserve a measured
row or the two largest member requests side by side and run at most two members.

The pool books worker slots by these requests, so an oversized request is lost
capacity and an undersized one is a throttled or killed action. The rule, in
`tools/buck2/action_sizes_from_log.py`:

- A compile asks for one CPU while its p95 is at most 1.6 cores, because every
  worker runs a one-CPU request under its slot share (1.67 cores at the least),
  not under a one-core quota. Above that, and for every test run, the request
  is `ceil(p95 - 0.2)`, capped at 8: there the cgroup's `cpu.max` is the request.
- Memory is the p99 peak x 1.25, rounded up to 256 MiB. A run that stayed
  inside the request it ran under needs no more than that request. A compile
  row never drops below its largest recorded peak or the 1.5 GiB default; a
  test row never drops below 1 GiB.
- Each remote action category takes its request from the target that runs it;
  `ACTION_CATEGORY_SIZES` in `tools/buck2/generate_model.py` names the source
  for every category. A target that names no budget resolves to the first
  platform in `POOL_BUDGETS`, 1 CPU and 1 GiB.

Refresh the sizes from the workers' usage logs
(`/workspace/kiln-executor/usage/actions.log*` on each pool box) with one
command, then commit the three files it rewrites:

```sh
python3 tools/buck2/action_sizes_from_log.py --refresh --since <unix seconds> usage-*.log
python3 tools/buck2/action_sizes_from_log.py --report --since <unix seconds> usage-*.log
```

`--refresh` rewrites `action-sizes.json` and `test-run-sizes.json` and runs
`sync.py`, which regenerates `exec_sizes.bzl`, including the execution
platforms. `--report` prints reserved against used CPU and memory per action
category. The usage log cannot tell a target's metadata, Clippy, codegen and
link actions apart; `tools/buck2/action_categories_from_events.py` joins it
with `buck2 log show` output to measure each. A changed request changes the
action key, so the first build after a refresh recompiles the resized crates. The graph contracts fail when a
category has no entry, when a compile row is below its recorded peak, and when
a test row is below its p99 peak.

These are scheduler reservations, not compiler-thread counts. The worker
supervisor uses the requests to size cgroups within unchanged floors and
ceilings. Stock Buck2 puts properties in REAPI `Command.platform`. The pool
scheduler accepts them; its current worker's property-to-environment path reads
`Action.platform`. Lash therefore also puts the same canonical CPU/memory request
in the outer action environment through the documented
[`ctx.actions.run(env=...)`](https://buck2.build/docs/api/build/AnalysisActions/)
API. The worker already accepts this before launching the supervisor. The checksum-verified Starlark overlay in `tools/buck2/prelude_overlay.py`
installs that rule integration into the private prelude and covers compiler,
lint, documentation and helper actions. Build-script and test environments
carry their own requests.
Rust's inner `rustc_env` alone would occur too late to size the cgroup.

Every compiler, lint, documentation, build-script and helper action runs on the
pool; the shared execution platforms are remote-only, so an action's
`prefer_local` is ignored and `local_only` is refused. Do not make them hybrid:
the stock C++ toolchain prefers local binary links and archives, which would
then run on the developer host. The one step that leaves the pool is the
prelude's `failure_filter`, which follows each metadata compile whose
diagnostics must not fail dependents. The overlay decides it in the daemon from
the compile's build status: a passing compile's output is re-exposed by a
declared copy, with no second remote action and nothing materialized, and a
failing compile runs the stock remote `failure_filter`, so the error names the
same action and diagnostics. Source trees, argument files and that copy are
daemon-internal actions. The transitive-dependency symlink tree (`deps`) remains
a remote action; it reruns only when a target's dependency set changes.

Buck2's `notify` watcher follows directory symlinks when the daemon starts, and
the pinned release has no setting that stops it (`buck2.file_watcher` selects
only `notify`, `watchman` or the whole-tree `fs_hash_crawler`). `.buckconfig`
ignores root `bazel-*` entries so listings never descend into a leftover Bazel
link, but an ignore does not stop the watcher walking it: the driver's admission
still removes those links and restarts the daemon, and refuses any other root
link leading back into the project.

The executable remains the official release. Pinned Starlark rule changes are
repository build logic with explicit source/version checks and a coverage audit.
A prelude update requires reviewing every action-registration site. This cutover
changes no NativeLink endpoints, scheduler, workers, storage, authentication,
image or resource limits.

Kiln uses the fork-local isolation directory `kiln`. Each worktree owns its
daemon and outputs; remote action results can be reused without sharing local
source or daemon state. `kiln rm` stops only the departing fork's daemon under
its lifecycle lease. Do not use broad shutdown or cache deletion for benchmarks.

## Reports and materialized outputs

Tests use a separate repository runner through Buck2's documented
[`test.v2_test_executor`](https://buck2.build/docs/rule_authors/test_execution/)
interface. It asks Buck2 to execute actual test actions with declared outputs.
Passing cache hits retain their verdict and reports; failures remain failed
tests. Batches preserve one XML suite per member and a case per libtest result.
A result is read even when the test's own output separates it from its name,
as under `--nocapture`. The cases must add up to libtest's `test result:`
summary; a report that does not is an error in its suite and fails the test.
A test's child process that prints libtest output into the same stream, such as
the test binary re-running one of its own tests, adds no case: only the
binary's own block and summary are counted, and a child's record never stands
in for a missing one of the binary's.
Output that names no test, as under `--format terse`, is not checked.
Arguments reach every member; a filter matching no member fails. Use a direct
member label for a narrow filter.

```sh
kiln test //crates/lash-sansio:lash-sansio__unit_test \
  --test-report /tmp/lash-test-report.json \
  --test-output-dir /tmp/lash-test-results
```

The default stable paths are `.buck2/test-report.json` and
`.buck2/test-results`. Each target has a
`<cell>/<package>/<target>` directory containing `test.xml`, `test.log` and
an undeclared-output directory for law receipts and
similar witnesses. Consumers resolve these through the structured test report
instead of guessing configuration hashes under `buck-out`.

An invocation that names neither path writes under its own
`.buck2/test-invocations/<id>/`. When it ends, the two default paths become
links to what it wrote, and it prints both link targets. The default paths
therefore show the invocation that finished last, whole. When invocations
overlap, read the printed paths or name your own. A finished invocation's
directory is kept for an hour, and for as long as a default path points to it.

### Concurrent invocations

Concurrent `kiln` commands in one fork are safe, including two `kiln test`
commands on the same test binary with different filters.

- **Selection.** Buck2 names a test's declared-output directory after its
  target and the stage variant, not its command. The runner sets the variant
  to a digest of the test arguments, runtime environment, timeout and
  execution mode, so two selections of one binary never share a `test.xml`.
  The digest depends on the selection alone, which keeps cached verdicts
  shared between checkouts. Invocations that run the same selection of the
  same target take turns on a lease under `.buck2/test-leases`.
- **Reports.** The report, the per-target directories and the `run-<k>`
  repetition directories are private to each invocation, as above.
- **Mismatch guard.** A report must name the cases its own execution printed
  and no case outside the requested filters or behind a requested `--skip`.
  Otherwise the test is an `INFRA_FAILURE` whose message starts
  `Test report does not match its selection`. It never passes.
- **`--jobs`.** A command whose `--jobs` differs from the running daemon's
  waits for every running command to finish before it restarts the daemon. It
  never restarts the daemon under another command. Overlapping loops with
  different `--jobs` restart the daemon at every handoff, so give them one
  value.
- **Build and test together.** Both hold a shared lease on the daemon and run
  at once. Buck2 itself orders commands whose configuration differs.

Artifact consumers request final materialization and use the native build report:

```sh
kiln build //crates/lash-sim:lash-sim__bin \
  --materializations final --build-report /tmp/lash-build-report.json
python3 tools/buck2/outputs.py --report /tmp/lash-build-report.json \
  --label //crates/lash-sim:lash-sim__bin --single
```

`--single` requires exactly one materialized artifact and returns its absolute
path. Missing or ambiguous outputs fail. Full libraries select `[static]`;
Buck2's ordinary library output alone is a metadata check. The driver preserves
full compilation for build aggregates; `check` selects `[check]` metadata
outputs. Lint/compile-only CI can use
`--materializations none`; executable, documentation and schema consumers need
final outputs.

## Repeating a test

Prove a concurrency, timing, crash or replay law by executing one exact case
repeatedly:

```sh
kiln test //crates/lash-store-sql:lash-store-sql__unit_test \
  --test_arg=--exact \
  --test_arg=render::tests::a_vocabulary_token_expands_once_for_both_backends \
  --test_sharding_strategy=disabled --runs_per_test=20
```

`--runs_per_test=N` runs `buck2 test` N times with `--no-test-cache`, so every
run executes instead of reusing a cached verdict. Run `k` writes its report,
`test.xml` and `test.log` under `<test-output-dir>/run-<k>/`. Repetitions of
different cases may [run at once](#concurrent-invocations). The driver prints
each run's passed and failed case counts. The invocation fails if any run fails,
reuses a cached verdict or executes zero cases. A zero-case run stops the
repetition: a bare name under `--exact` matches nothing. Use the full module
path.

The driver accepts these Bazel spellings. `--test_filter=<f>` is
`--test_arg=<f>`. `--nocache_test_results` is `--no-test-cache`.
`--test_sharding_strategy=disabled` is a no-op: only `package-policy.toml`
shards, and a filter selects across the shards' union. Other Bazel flags fail
and name the replacement where one exists.

## Service and Cargo-owned gates

`scripts/ci/with-service.sh` starts the same private PostgreSQL/Garage containers
used by CI, publishes an ephemeral loopback port, waits for readiness and removes
the container on success, failure or interruption:

```sh
scripts/ci/with-service.sh
scripts/ci/with-service.sh pg16 -- bash scripts/ci/store-tests.sh pg-store
scripts/ci/with-service.sh all -- bash scripts/ci/store-tests.sh s3-store
```

Trusted store jobs compile through the pool and execute locally against the
private service. Service settings are runtime inputs only; PG 14/16/18 reuse
compiled artifacts. Those tests are never cached. Driver controls are
`--local-test-execution`, `--no-test-cache` and repeated `--test_env KEY=VALUE`.
Untrusted jobs keep their Cargo commands and receive no pool credentials.
Local runs list service-shaped contracts they did not exercise, with recipes.

PG 16 is the pull-request primary. Merge groups add PG 14/18 compatibility
witnesses for schema diffs; full dispatch runs all three. Compatibility compares
live catalog artifacts and version stamps. Use `kiln gate lash <fork> -- <cmd>`
for other live gates, with identities and ports derived from `KILN_GATE_ID`.

The ordinary partition retains ignored-test selection and exclusions. Live
Restate tests remain ignored. The five
`durable_fault_matrix_real_cargo_filters_chunk_0..4` cases remain in the named
nested-Cargo heavy gate. Deep TypeScript child-process tests retain their measured
resource requests and tail partition; their previous local-only exception has
already been removed.
Manual service, trybuild, frontend and required-feature labels have explicit
inventory reasons.

Cargo remains for nested builds and trybuild's fixture cache, fuzzing, Miri,
nextest profiles, named live recipes, judged/release artifacts, packaging and
publication. The trusted facade seal stays `//crates/lash:ui_fixtures`, comparing
the same `.stderr` pins. Beside it, `//crates/lash:facade_completeness` runs
`scripts/facade_completeness.py` over the `doc-json` subtarget (rustdoc JSON)
of the facade and every first-party library in its closure; `[facade]` in
`tools/buck2/package-policy.toml` names the package. Runtime trybuild, workflow-graph frontend gates, Restate
workers and Git-consumer checks retain their supported recipes. Workbench
projection remains a declared pool test with pinned Node; full browser/service
E2E remains separate.

Launcher self-tests use Bubblewrap with private `/tmp`, `/run`, PID/network
namespaces and a read-only checkout. They retain production ownership locks and
mock Docker. Install Bubblewrap on a new development host.

## CI and deployment configuration

Trusted CI and local forks share runtime, tools, graph and budgets. Pull requests
select the affected developer/core partition and feature compile/lint when Rust
inputs change. Merge groups validate the combined tree's complete core/tail
partition through `buck2-tests` and `buck2-tests-tail`. The planner exposes the
shared-cache trust decision as `buck2_trusted`; feature tests retain their path
gate. `CI conclusion` remains the required aggregate and rejects failed,
cancelled, missing or incorrectly skipped jobs. Heavy, worker, fuzz, deferred Unicode, consumer and release witnesses keep
their schedules. Release and seal cache workflows remain Cargo-owned.

The `.github/actions/buck2-shared-cache` action writes private deployment
configuration and certificate state.
Fork/Dependabot events receive no credentials and retain Cargo lint, workspace
tests, feature checks and service branches. Always clean the job's private
certificate directory. Pool cache warming keeps one trusted main writer; cold
caches affect timing rather than correctness.

| Fact | Local owner | CI owner |
| --- | --- | --- |
| Release, prelude, Rust and dependency pins | Committed lock/config files | Same files |
| Endpoint, instance and runtime | Executor metadata projected into `.buckconfig.local` | Existing build-cache secrets and metadata |
| CA and client identity | Private Kiln identity; combined PEM under `.kiln/` | Private runner-temporary certificate files |
| Daemon and outputs | This worktree's Buck2 isolation | This job's checkout |
| Budgets and required properties | Committed rule tables | Same tables |

Kiln detects Lash's `.buckconfig` per checkout and generates ignored local
configuration. Older Lash worktrees and Figments keep their existing backend.
Credentials, addresses, instance names and host paths never belong in Git.
Configuration uses TLS with the existing client identity and SHA256 digests.
No permanent parallel build system is retained in Lash.

Deployment order, rollback, evidence and coordinator measurements are recorded
in [the migration report](buck2-migration.md).

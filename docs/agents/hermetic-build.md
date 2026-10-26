# Hermetic Rust builds

Lash uses Buck2 through Kiln's NativeLink pool. Cargo manifests and `Cargo.lock`
are canonical. Start in an isolated fork as described in [AGENTS.md](../../AGENTS.md).

## Developer loop

| Command | Behavior |
| --- | --- |
| `kiln analyze` | Checks generated-file drift and analyzes the graph without compiling. |
| `kiln check` | Checks Rust metadata without linking. |
| `kiln build` | Compiles full libraries and executables. |
| `kiln test` | Runs `//:dev_tests`; explicit labels select focused tests. |
| `kiln clippy` | Lints `//:workspace_clippy` with Cargo's lint policy and `-D warnings`. |
| `kiln doc` | Renders workspace API documentation. |
| `kiln run //path:binary -- <args>` | Builds on the pool, materializes the executable and runs it locally. |
| `kiln fmt -- --check` | Checks local Cargo formatting. |
| `kiln sync` | Regenerates dependency and first-party rules. |
| `kiln clean` | Cleans this checkout's Buck2 state. |

While editing, select the owning test by its full path:

```sh
kiln test //crates/lash-core:lash-core__unit_test --test_arg=<full-test-path> --test_arg=--exact
```

A filter executing zero cases fails. Use a direct member label rather than a
batch when selecting one case. State what executed; focused evidence does not
claim a full-suite run. [AGENTS.md](../../AGENTS.md) defines required proof.

Package patterns such as `//crates/lash-store-sql/...`, `//pkg:all` and `//...`
work across build commands. Patterns omit feature-lane variants and `manual`
targets and print skipped labels. Select a manual target by its explicit label.
A `check`, `clippy` or `doc` pattern must match a generated Rust target.

The partitions are:

- `//:dev_tests`: cacheable developer tests.
- `//:workspace_tests`: adds developer-deferred binaries.
- `//:main_tests`: hourly full hermetic coverage, including manual and
  PR-deferred tests, feature-check binaries, UI fixtures, facade completeness
  and VM source inventories. External services and credentials remain separate.

`tools/buck2/target-inventory.json` records membership and exclusions.
For service laws and backend selection, use [test-backends.md](test-backends.md).

`python3 scripts/dev-test.py --dry-run` prints its affected-tests plan.
`--dependents` queries reverse dependencies; `--include-deferred` restores
selected developer-deferred tests. Shared or unknown inputs widen selection.
Docs-only changes run no Rust tests. The planner refuses live PostgreSQL/S3
settings and records its plan and receipts under Git's `lash-validation/`.
This is optional iteration tooling, not an additional landing gate.

`just floor` is an opt-in broad tooling checkpoint. It runs developer and
feature tests, Clippy, schema checks, formatting and repository gates.
`scripts/ci/repository-gates.sh --all` includes the locally deferred launcher
reset check. CI and the local runner discover self-tests through
`scripts/ci/repository_gate_commands.py`; the CI heredoc supplies checks with
other filename conventions. `just schema-check` selects store-schema gates.

## Reproducible graph and tools

After changing dependencies, workspace membership or target policy:

```sh
kiln sync
git diff -- tools/buck2 third-party/rust '**/BUCK'
kiln analyze
```

Normal builds reject generated drift. Do not hand-edit BUCK files or inventories.
`tools/buck2/package-policy.toml` supplies partitions, inputs, shards, timeouts,
service membership and resource exceptions that Cargo metadata cannot express.
Unknown packages, targets and helper binaries fail generation.

Tool and dependency pins live under `tools/buck2/`. The verified bootstrap
store shares immutable tool inputs across checkouts; replace a pinned file
instead of editing it in place. See the [tooling notes](../../tools/buck2/README.md#shared-bootstrap-store)
for store verification, pruning, native fixups and deployment configuration.

## Sources, assets and profiles

`tools/buck2/source-ownership.json` declares narrowed test compile boundaries.
For `derive_tests = true`, `kiln sync` derives module and literal-include
closures, including conditional modules. Unsupported computed paths retain
conservative inputs and appear in `test_input_fallbacks`.

Declare non-Rust embedded assets in `compile_data`, runtime fixtures in
`test_data`, and source-inspection inputs with their owning test. Add the asset
and its declaration together. `library_test_sources` excludes test-only modules
from production libraries; it must not exclude `cfg(feature = "testing")`
fixtures or production-reachable modules.

Tests run from the project root. `CARGO_MANIFEST_DIR` and
`CARGO_BIN_EXE_<name>` are project-relative at compile and run time. Declared
cross-package inputs remain visible at their checkout paths.

The default profile follows Cargo's optimization, assertions, overflow checks,
debug information and lints. `--config=optimized` selects optimized artifacts;
`--config=judged` changes target configuration without changing host tools.
Cargo release and publishing profiles own their artifacts.

### VM candidate iteration

The [interpreter comparison driver](compare-bench.md) can compile its downstream
host crates without LLVM optimization while keeping optimization level 3 for
the VM, worker adapter, dialects, native crates and third-party code:

```sh
kiln build //crates/lash-perf:lash-perf__bin \
  --target-platforms //tools/buck2:profiling -c kiln.rust_profile=optimized \
  -c kiln.perf_iteration=true --materializations final \
  --build-report .kiln/compare-iteration.json
python3 tools/buck2/outputs.py --report .kiln/compare-iteration.json \
  --label //crates/lash-perf:lash-perf__bin --single
```

Iterate with this build and use the full optimized build for final numbers if
its results differ. Omit `-c kiln.perf_iteration=true` for that reference build.
The mode is for `lash-perf compare` VM candidates. Runtime, durable, startup
and throughput measurements require the full optimized build.

Only `lash-perf`, `lash-runtime`, `lash-internal-vm-runtime`,
`lash-internal-protocol-rlm` and `lash-internal-plugin-mcp` use optimization
level 0 in this mode. The comparison executable uses level 1 to optimize its
global allocator. Both retain disabled debug assertions and overflow checks.
The VM explicitly retains rustc's optimized default of 16 codegen units.
The switch applies only to the optimized and profiling configurations. Host
build tools, features, executor properties and compile/test budgets do not
change. The existing build-profile identity separates iteration history from
full-build history. Allocation-heavy absolute instruction counts can differ;
compare paired candidate improvements and retain the full-build numbers.
The [measured build and ranking report](../perf/build-iteration.md) records
the action breakdown, pool load and qualification limits.

## Feature coverage and linting

`scripts/feature-coverage.toml` owns the feature lane board. First-party variants
use each lane's resolved feature closure. `//:feature_lanes` checks metadata;
`//:feature_lane_tests` links and executes its test units.
`scripts/check_feature_coverage.py check` rejects missing units.

Third-party dependencies use the workspace feature union. First-party lane
checks cannot detect accidental reliance on an external feature enabled
elsewhere. The Cargo feature matrix remains the isolated feature proof.

Clippy includes first-party build scripts and uses the nearest declared
`clippy.toml`, Cargo package/workspace lints and `-D warnings`. Unsupported
labels, empty selections and missing lint outputs fail. Workspace libraries
have `doctest = false`; documentation examples remain prose.

## Execution and resource accounting

Shared execution is the default. Missing pool configuration, credentials or
runtime identity fails without a local compiler fallback. To reproduce locally:

```sh
scripts/hermetic-build.sh --local build //crates/lash-core:lash-core
```

Compile, Clippy, metadata and test requests have separate sizing tables under
`tools/buck2/`. Platforms preserve `cpu_count`, `memory_kb`, `cpu_arch`,
`OSFamily` and `kiln_executor_runtime`. The same CPU/memory request reaches the
worker supervisor through the outer action environment. Admission requests
and enforced cgroup limits are separate.

Refresh measured sizes from captured worker logs and matching Buck2 events:

```sh
python3 tools/buck2/action_sizes_from_log.py --refresh --since <unix-seconds> \
  --worker-metadata <per-log-worker-metadata.json> \
  --events <checkout>/buck-out/kiln/log/*_events.pb.zst -- usage-*.log
python3 tools/buck2/action_sizes_from_log.py --report --since <unix-seconds> usage-*.log
```

Commit the rewritten size/evidence tables and generated rules. The worker
metadata sidecar is keyed by exact log path and records that worker's
`worker_cpu_count` and `worker_max_inflight` at capture time. Records carrying
these fields need no sidecar. The tool owns sizing formulas and evidence
requirements. A resized request changes action keys and re-executes its targets.

If a measured compile is OOM-killed and cannot wait for a refresh, scale its
first-party compile requests for one invocation:

```sh
kiln build -c kiln.memory_scale=2 //crates/lash-core:lash-core
```

The multiplier is a positive integer. It also works for `check`, `test` and
`clippy`; third-party compiles and test runs retain their requests. Refresh the
tables afterward. Scaled compiles use different cache entries.

Compiler, lint, documentation and helper actions stay remote. Crate archive
unpacking is the sole local helper. Daemon-internal dependency trees and
successful diagnostic filters create no extra remote action. See the
[tooling implementation notes](../../tools/buck2/README.md#action-resources-and-results).

The driver removes leftover root `bazel-*` symlinks before daemon startup and
refuses other root links back into the project. Buck2's watcher follows directory
symlinks even when directory listings ignore them. Each fork owns its daemon
and outputs; remove it through `kiln rm`.

## Reports and materialized outputs

```sh
kiln test //crates/lash-sansio:lash-sansio__unit_test \
  --test-report /tmp/lash-test-report.json \
  --test-output-dir /tmp/lash-test-results
```

Each target writes `<cell>/<package>/<target>/test.xml`, `test.log` and an
`undeclared` receipt directory. Passing cache hits retain verdicts and reports;
failed actions retain failure evidence. Malformed case counts or a report that
does not match its selection fail as infrastructure errors.

Without explicit paths, each invocation writes to
`.buck2/test-invocations/<id>/`. On completion, `.buck2/test-report.json` and
`.buck2/test-results` link to that invocation. Use the printed paths or explicit
paths when collecting results. Old invocation directories survive for an hour
and while a default link refers to them.

Artifact consumers request final materialization and resolve the native report:

```sh
kiln build //crates/lash-sim:lash-sim__bin \
  --materializations final --build-report /tmp/lash-build-report.json
python3 tools/buck2/outputs.py --report /tmp/lash-build-report.json \
  --label //crates/lash-sim:lash-sim__bin --single
```

`--single` requires one materialized artifact and returns its absolute path.
Do not guess configuration hashes under `buck-out`. Library builds select
`[static]`; checks select `[check]`. Compile-only CI may use
`--materializations none`; executable, documentation and schema consumers need
final outputs.

### Concurrent invocations

Run one build request at a time per lane. The runner isolates reports and
selection outputs and leases identical selections. Driver jobs default to 32,
clamped by Kiln's `REMOTE_JOBS_MAX`. Changing the effective remote limit waits
for running commands before restarting the daemon. Configuration differences
can serialize commands within a fork.

Kiln's admission wraps remote clients before daemon transition locks. Service
and explicitly local tests run in a separate local phase after remote tests.
Analysis, clean and local-only operations acquire no remote phase slot.

### Queue priority

Interactive, agent and required CI work uses priority `100`; scheduled full
campaigns explicitly use `0`. The driver accepts `KILN_RE_PRIORITY` from -1000
to 1000. Higher priorities precede older queued requests; running actions are
never preempted. Priority changes reconfigure the graph, so use a separate fork
for a campaign with another priority.

## Test shards

`shards = N` in `tools/buck2/package-policy.toml` produces
`<label>__shard_1..N`. Each shard derives disjoint membership from the binary's
listing and committed case weights. Filters preserve assignment; unmeasured
cases use round-robin placement. Shard a long binary rather than hiding hangs
with a longer timeout.

Refresh measured weights, never hand-edit them:

```sh
kiln test --test_env RUSTC_BOOTSTRAP=1 \
  --test_arg=-Z --test_arg=unstable-options --test_arg=--report-time \
  --runs_per_test=3 --test-output-dir /tmp/shards <label>...
python3 tools/buck2/shard_weights.py --refresh /tmp/shards/run-*/test-report.json
python3 tools/buck2/shard_weights.py --plan <label> <shards>
```

Refresh replaces measured rows and retains others. For cases contending on one
service, add `--jobs 1 --test_arg=--test-threads=1`. Changing weights reruns
sharded tests; stale case names affect balance, not coverage.

## Repeating a test

```sh
kiln test //crates/lash-store-sql:lash-store-sql__unit_test \
  --test_arg=--exact \
  --test_arg=render::tests::a_vocabulary_token_expands_once_for_both_backends \
  --runs_per_test=10
```

Each repetition is uncached and writes `<test-output-dir>/run-<k>/`. Any failure,
cached verdict or zero-case selection fails the invocation. Use the full module
path with `--exact`.

Compatibility spellings are `--test_filter=<f>` for `--test_arg=<f>` and
`--nocache_test_results` for `--no-test-cache`.
`--test_sharding_strategy=disabled` is a no-op; shard membership comes from
package policy. Other unsupported flags fail and name their replacement.

## Hermetic PostgreSQL tests

`hermetic-postgres` tests start pinned PostgreSQL 18 inside their action and
stay remote and cacheable. This covers the PostgreSQL store package and its
feature variants:

```sh
kiln test //crates/lash-postgres-store:conformance__test
```

`tools/buck2/postgres_action_runner.py` creates the private cluster, applies
`crates/lash-postgres-store/schema.sql`, exports the URL and cleans up server
processes afterward. Each shard has its own server. Supplying an external
`LASH_POSTGRES_DATABASE_URL` instead selects local uncached execution.
To convert another package, declare `hermetic-postgres` and its `service_floor`
in package policy, then run `kiln sync`. Tests needing another service or a
shared server retain the service wrapper.

## Service and Cargo-owned gates

`kiln check` includes `//:facade_production_check`, which checks the facade's
baseline and all-host-extension library graphs without development dependencies
or `testing`. The hourly Cargo gate additionally checks each production feature
alone, catching external feature unification hidden by the Buck2 workspace:

```sh
kiln gate lash <fork> -- just facade-production
kiln gate lash <fork> -- scripts/ci/cargo-parity.sh
```

Cargo parity also checks the workspace and isolated lash_vm/integrator graphs.
Source `env.sh` first; Kiln's Cargo shim supplies paths, flags and admission.

For external services, use a private gate:

```sh
kiln gate lash <fork> -- scripts/ci/with-service.sh pg -- scripts/ci/store-tests.sh pg-store
kiln gate lash <fork> -- scripts/ci/with-service.sh all -- scripts/ci/store-tests.sh s3-store
```

The wrapper starts private containers on ephemeral loopback ports and cleans
them up. Service compilation remains remote; execution is local and uncached.
Use `KILN_GATE_ID` for private service identities and ports. PostgreSQL 18 is
primary; 17 runs in the release gate as `with-service.sh pg17 --
scripts/ci/store-tests.sh pg-release`. See [test-backends.md](test-backends.md)
for required service flags and durable PostgreSQL settings.

Cargo owns nested builds, trybuild's cache, fuzzing, Miri, named live recipes,
release artifacts, packaging and publication. The trusted facade checks are
`//crates/lash:ui_fixtures` and `//crates/lash:facade_completeness`; `just seal`
retains the Cargo trybuild recipe. Launcher self-tests require Bubblewrap.
Browser/service E2E remains separate from the developer partition.

The package-layering law runs with
`kiln gate lash <fork> -- python3 scripts/check-package-seams.py`.

## CI and deployment configuration

CI selects tests and feature lanes through `scripts/ci_plan.py`; full campaigns
run hourly on main. Use the workflow and generated inventory for exact current
selection. A permitted policy skip is not execution evidence.

Trusted CI and local forks share pins, runtime and budgets. Private
`.buckconfig.local` and certificate files hold endpoints, instance and identity.
Do not commit them. Untrusted jobs receive no pool credentials and retain
Cargo-owned paths. The trusted main warmer writes shared action-cache entries;
agent forks reuse them.

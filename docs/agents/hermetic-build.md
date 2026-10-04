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
partition. Neither is every correctness gate. The PostgreSQL store package's
tests are in both: each starts [its own server](#hermetic-postgresql-tests).
The other PostgreSQL suites, S3, Restate, nested-Cargo tests, release artifacts
and named recipes retain their contracts.

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
prints the diff, labels, revisions and commands. Complete batches are substituted
once; individual members, manual service targets and deferred tests are not
silently added by package wildcards. Shared build inputs and unknown tooling
widen selection. A docs-only diff does not run Rust tests. Known Python test
edits run the exact repository-gate command CI uses.

The proof a change needs is minimal and fast; main's hourly full run covers
the rest and reds are fixed forward:

- the tests the change adds or changes, run once on the cheapest tier
  (SQLite stores, the in-process Restate server double), by full test path;
- for a bug, a law that fails once on the unfixed code;
- one `kiln clippy`;
- only when they apply: `//crates/lash:ui_fixtures` and
  `//crates/lash:facade_completeness` when exports change, and
  `kiln build //:schema_checks` when a serialized shape changes.

`python3 scripts/dev-test.py --dependents` is an optional local tool for a
wider affected-tests selection, not a required gate:

```sh
python3 scripts/dev-test.py --dependents
```

It diffs the merge base with `origin/main` against the commits, the working
tree and untracked files together. A Buck2 `rdeps` query selects every
dev-suite test that depends on what the diff changed, read file by file:

| Changed path | Selects (plus reverse dependencies) |
|---|---|
| A package source, `BUCK` or `Cargo.toml` | That package's targets. |
| Root `BUCK`, `third-party/rust/BUCK` | The targets whose rule call changed. Repository gates run. |
| `Cargo.lock` | Changed workspace members' packages and the third-party targets of changed crates. |
| Root `Cargo.toml` | New members and changed `[workspace.dependencies]` rows. Any other section selects the suite. |
| `schemas/`, `fixtures/`, `fuzz/`, a runtime doc input, shared example sources | The targets that declare the file as an input, plus `//:schema_checks`. |
| `justfile` | The script self-tests that read it. |
| `scripts/`, `.github/` | The repository gates, plus the targets that declare the file as an input. |

The whole suite, `//:dev_tests`, still runs for the inputs in
`DEV_TEST_GLOBAL_INPUTS` (`scripts/ci_plan.py`): the toolchain pin,
`.buckconfig`, everything under `tools/`, Cargo and lint configuration and
`ci.yml`. It also runs for a path nothing classifies, a deleted data file, a
data file no target declares, and a failed query. Without `--dependents`
nothing is queried, so a shared input or a package manifest selects the suite.

The repository gates run at the same time as the Buck2 build and tests. Each
command keeps its own log, and one summary and one exit code cover both
halves. A gate listed in `REPOSITORY_GATE_INPUTS` is skipped when the diff
touches none of its inputs: `test_check_version_bumps.py` runs only for a
package, a root manifest or its own modules. CI runs every gate.

The gate names the affected `dev-deferred` labels it skips, which run hourly on
main. Add `--include-deferred` to restore their selection, including all deferred
labels on a broad plan. CI's PR selection still includes the tail labels.
The tests run through `kiln test` on the
pool at the default `--jobs`, never serially, and cached verdicts bound the
cost, so there is no sampling. Every planned command runs, also after one
fails. The closing lines give the target count and name
every `FAIL`, `TIMEOUT` and `INFRA_FAILURE`, then the affected tests it
never runs (`manual` service and Cargo-owned labels, `pr-deferred` suites). The
exit code is non-zero for any of the first three and for a build, script or
infrastructure error.

The planner refuses live PostgreSQL/S3 settings. It writes a plan and receipt
under Git's `lash-validation/` directory and serializes requests within a fork.
Every Rust request still calls Buck2, including a request that waited for another
run. A receipt observes source/configuration state; it does not replace action
validation. Changes during a run invalidate it. The build engine validates
ignored generated inputs and toolchain files beyond the planner's Git snapshot.

`just floor` remains an explicit broad tooling checkpoint. It combines developer
and feature tests, Clippy, formatting, schema checks and the CI repository-script
inventory. The locally excluded launcher-reset gate is named in its result table;
`scripts/ci/repository-gates.sh --all` includes it. CI and this local entrypoint
discover `test_*.py` and `test-*.sh` in `scripts`, `tools/buck2` and
`tools/buck2/tests`. `scripts/ci/repository_gate_commands.py` names production
modules and live-service scripts excluded from discovery, with a reason for
each. The workflow heredoc retains checks with other filename conventions.
It also names the feature-coverage self-test because that checker requires a
literal witness; both runners deduplicate it against discovery.
New self-tests need no workflow edit; tooling edits run this discovered suite.
The static CI classifier can conservatively widen a discovered self-test diff
when no consumer names the file literally. `just schema-check` selects
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
`native-tools-lock.json` pins the LLVM, Node and PostgreSQL archives. CI restores the
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
inputs for registry packages. Git packages retain their full resolved revision
in the synthetic manifest. Bootstrap copies their Cargo-vendored files into
the ignored, fork-local `third-party/rust/.git-sources/` tree, verifies Cargo's
file checksums, and exposes them through generated filegroups. The generated
`git-sources.json` records repository subdirectories, including nested packages;
fresh forks recreate these inputs before checking the graph receipt. Do not
hand-edit generated BUCK files. Normal builds reject drift; they do not
silently update lockfiles or Cargo features.

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
`tools/buck2/action-sizes.json`, one row per crate, and
`tools/buck2/target-kind-sizes.json`, which sizes a library apart from the
unit-test binary built from the same crate; test runs use the separate
`tools/buck2/test-run-sizes.json`. Generated `exec_sizes.bzl` resolves measured rows,
inherited feature rows, policy floors and pinned exceptions. Unmeasured compiles
and Clippy twins request 1 CPU and 512 MiB. Unmeasured tests request 512 MiB,
with explicit large-suite and timing-sensitive policy exceptions. Batches reserve a measured
row or the two largest member requests side by side and run at most two members.

The pool books worker slots by these requests, so an oversized request is lost
capacity and an undersized one is a throttled or killed action. The rule, in
`tools/buck2/action_sizes_from_log.py`:

- A compile asks for one CPU while its p95 is at most 1.6 cores, because every
  worker runs a one-CPU request under its slot share (1.67 cores at the least),
  not under a one-core quota. Above that, and for every test run, the request
  is `ceil(p95 - 0.2)`, capped at 8: there the cgroup's `cpu.max` is the request.
- Compile memory is p99 anonymous peak x 1.25 plus 512 MiB, rounded up to
  256 MiB, with the existing 1.5 GiB measured-row floor and per-crate floors. The anonymous
  measurement is sampled every 250 ms and is a lower bound. The fixed allowance
  covers sampling error and a working set of file-backed pages. Older records
  without anonymous measurements use the previous cgroup-peak formula, including
  its bound at a request the run fit inside and its largest-peak floor.
- Test runs with anonymous measurements reserve p99 anonymous peak x 1.25,
  rounded up to 256 MiB with a 256 MiB floor. Every refresh applies that
  request without compile hysteresis. Legacy records without anonymous peaks
  retain their conservative cgroup sizing. Killed runs contribute their total
  peak x 1.25 as a minimum, separately from successful p99 samples. Explicit
  floors keep `lash__unit_test` at 3.75 GiB and `lash-sim__unit_test` at 3.5 GiB.
- Admission and the cgroup limit are separate. The executor's action budget
  sets `memory.max` to at least 2 GiB or the worker's slot share, whichever is
  larger, then raises it for a larger request. Lowering a request below 2 GiB
  changes admission without reducing that minimum. Unsized memory contracts
  check the enforced cgroup floor; measured compile rows keep their existing
  headroom contracts and requests.
- Buck2 resolves one execution platform per target, so a request covers every
  category the target runs: a library's metadata, rlib and Rustdoc actions
  share one, as do a test binary's check and link. The split that is possible
  is between targets. Clippy takes it: every generated Rust target has a
  Clippy twin, `<label>__clippy`, the same rule and attributes on a platform
  of its own, and `kiln clippy` builds the twin's `[clippy.txt]`. Its request
  is the crate's row in `tools/buck2/clippy-sizes.json` (the anonymous compile rule
  over Clippy's own records, with a 512 MiB floor), else 512 MiB.
  First-party build-script runs and the schema actions are helper targets
  and request 512 MiB (`HELPER_ACTION_BUDGET`); a third-party build-script
  run also requests 512 MiB. Changing its request rekeys that run and can
  relink consumers when the build-script output is not reproducible.
  Lash's XML writer runs inside the test action. `generate-xml.sh` and
  `test-setup.sh` in shared usage logs belong to other repositories' Bazel
  runners; Lash declares no separate remote action for either.
  A library and its unit-test binary share a crate name and so a crate row, but the test binary's link peaks
  several times higher than anything the library runs; where Buck2's event
  logs told the two apart at least 20 times, each has its own row in
  `target-kind-sizes.json`, and a kind without one keeps the crate row.
- `--config=optimized` and the host configuration compile with
  `-Copt-level=3`, where LLVM holds more than a dev compile does.
  `tools/buck2/optimized-sizes.json` carries what such a compile asks for
  where that is more than the dev request, and the Rust macros select it on
  the profile constraint. A row only raises; one optimized sample is evidence;
  and a dev request that a refresh lowers leaves the optimized request where
  it was until 20 optimized samples say otherwise. A dev build takes the
  select's default branch, so its action keys do not depend on this table.
- A request is a platform property and so part of the action key. A refresh
  keeps a compile row in force unless the request moves by a whole CPU or at
  least 512 MiB. Legacy rows also move for unsafe peaks; anonymous test rows
  apply every measured change.
  A smaller correction is not worth
  re-executing the row's targets on a cold cache.
- Each remote action category takes its request from the target that runs it;
  `ACTION_CATEGORY_SIZES` in `tools/buck2/generate_model.py` names the source
  for every category. A target that names no budget resolves to the first
  platform in `POOL_BUDGETS`, 1 CPU and 512 MiB.

Refresh the sizes from the workers' usage logs
(`/workspace/kiln-executor/usage/actions.log*` on each pool box) with one
command, then commit the size and evidence files it rewrites:

```sh
python3 tools/buck2/action_sizes_from_log.py --refresh --since <unix seconds> \
  --events <checkout>/buck-out/kiln/log/*_events.pb.zst -- usage-*.log
python3 tools/buck2/action_sizes_from_log.py --report --since <unix seconds> usage-*.log
```

`--refresh` rewrites `action-sizes.json`, `target-kind-sizes.json`,
`optimized-sizes.json`, `clippy-sizes.json`, `category-sizes.json`,
`test-run-sizes.json` and `compile-memory-evidence.json`, then runs `sync.py`, which
regenerates `exec_sizes.bzl`, including the execution platforms. `--report`
prints reserved against used CPU and memory per Buck2 category. The usage log
names an action's category and crate but not its target or what it emitted,
so it cannot tell a library's rlib from its test binary's link; `--events`
names the Buck2 event logs of any checkouts that built on the pool in the
same period, and the join (`tools/buck2/action_categories_from_events.py`,
which also prints it per category, emit and target kind) labels the records
those builds executed. Without `--events` the target-kind rows stay as they
are. A changed request changes the action key, so the first build after a
refresh re-executes the resized targets. The graph contracts fail when a
category in the rules or in `category-sizes.json` has no entry, when an action
no row sizes peaked above its category's smallest request, when compile p99
anonymous memory reaches 80% of its request, when any compile was OOM-killed in
the window, and when test p99 reaches 90% of its request. Compile cgroup peaks
may exceed the request because page cache is reclaimable. Legacy records retain
their cgroup-peak floor when no anonymous measurement exists. Evidence includes
failed compiles and default rows. Joined records check each target kind and
profile; unjoined records use the largest resolved request for their crate.
Clippy records are kept apart and checked against their own resolved request.
Usage logs currently omit an OOM counter, so SIGKILL exits are conservatively
counted as OOMs, including the Rust wrapper's exit 247 for signal 9. Explicit
`oom_kill` or `oom_kills` fields are accepted when available.

When a compile is OOM-killed at its request (`rustc` exits on signal 9 with
no output) and the build cannot wait for a refresh, raise every sized
first-party compile request for that one invocation:

```sh
kiln build -c kiln.memory_scale=2 //crates/lash-core:lash-core
```

The value is a positive integer multiplier on `memory_kb`; `kiln check`,
`kiln test` and `kiln clippy` take it the same way, alone or with
`--config=optimized`. Unset, it is 1 and every request is exactly its table's.
Scaled requests are different action keys, so the resized compiles re-execute
and nothing they produce is shared with an unscaled build's cache entries;
third-party compiles and test runs are not scaled and stay cached. Then
refresh the tables so the next build does not need the flag. A Buck2
configuration modifier (`-m`) is not the mechanism: this repository registers
no modifier constructor, and a modifier would reconfigure the whole dependency
closure, third-party crates included, into a cold cache.

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
then run on the developer host. Unpacking a crate archive (`http_archive`) is
the one action that runs on the invoking host: about 0.09 s of `tar` over a
file the daemon has just downloaded, which on the pool waited for a slot and a
worker's set-up for longer than it ran. `third_party_http_archive` names
`LOCAL_HELPER_CONSTRAINT`, the value of the budget setting carried by a
separate local-only platform registered last, so no target reaches that
platform by omission and the graph contracts allow no other user. The unpacked
tree is the same, so the compiles that read it keep their action digests. The
host needs `/bin/sh` and `tar`. The one step that leaves the pool without
running anywhere else is the
prelude's `failure_filter`, which follows each metadata compile whose
diagnostics must not fail dependents. The overlay decides it in the daemon from
the compile's build status: a passing compile's output is re-exposed by a
declared copy, with no second remote action and nothing materialized, and a
failing compile runs the stock remote `failure_filter`, so the error names the
same action and diagnostics. Source trees, argument files and that copy are
daemon-internal actions. So is each compile's transitive-dependency directory
(the prelude's remote `deps` action, a Python tool that only symlinks files):
the overlay builds the same tree with `ctx.actions.assembled_dir`, the same
`<n>/<file>` relative symlinks and the same `dirs` flag file, so a consumer's
input tree, action digest and cache entry are unchanged. It needs no
`crate_dynamic`, which no Lash crate uses, and the overlay refuses one.

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

### Queue priority

The driver sends `KILN_RE_PRIORITY` as the priority of every Execute request,
as `-c kiln.re_priority=<n>`: `0` when the variable is unset, and an error
before Buck2 starts when it is not an integer from -1000 to 1000. Kiln exports
`100` for `kiln build|check|test|clippy|doc|run` and `0` for `kiln gate` and
the commands a gate's script runs; CI and direct `scripts/hermetic-build.sh`
calls leave it unset. The pool queue serves the higher priority first, then
the older request, so `kiln` commands outrank gates and CI.

- **No preemption.** Priority orders queued actions only. A running action is
  never interrupted, and a request for an action already queued keeps that
  action's priority.
- **One fork, two priorities.** The priority is part of the graph's
  configuration, so a gate and an agent command in one fork serialize, and the
  first command after a change of priority re-analyses the graph. Run a gate in
  its own fork.

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

## Test shards

A `shards = N` rule in `tools/buck2/package-policy.toml` splits one libtest
binary into `<label>__shard_1..N`, each its own test action. Shard, do not
raise a timeout: a longer bound hides a real hang. Every shard runs
`tools/buck2/test_shard.py`, which lists the whole binary and derives the same
assignment in each process from the listed names and the committed weights
alone, so the shards are disjoint and their union is the listing.

- **Balance.** `tools/buck2/test-shard-weights.json` holds each sharded test's
  per-case milliseconds. Measured cases go longest first, each to the shard
  with the least load so far. Cases the table does not name go round-robin in
  name order, starting from the lightest shard; a test with no row is split
  round-robin entirely. A name the binary no longer lists is ignored, so a
  stale table costs balance, never coverage. A feature-lane variant uses its
  ordinary label's row.
- **Exclusion.** A shard runs the binary under `--exact` with one `--skip` per
  listed case that is not its own. libtest applies `--exact` to skips, so a
  case whose name contains another's is assigned on its own.
- **Filters.** libtest has one `--exact` for filters and skips alike, so the
  wrapper applies the caller's positional filters and `--skip` patterns to the
  listing itself, by libtest's rule (a substring, or equality under the
  caller's `--exact`), drops them from the command and skips every case
  outside the selection by name. A case runs on the same shard whatever the
  filter. Other arguments, `--ignored` and `--include-ignored` among them,
  reach libtest unchanged.

Refresh the weights from the JUnit reports of a run that asked libtest for
case times, then commit the table:

```sh
kiln test --test_env RUSTC_BOOTSTRAP=1 \
  --test_arg=-Z --test_arg=unstable-options --test_arg=--report-time \
  --runs_per_test=3 --test-output-dir /tmp/shards <label>...
python3 tools/buck2/shard_weights.py --refresh /tmp/shards/run-*/test-report.json
python3 tools/buck2/shard_weights.py --plan <label> <shards>
```

`--refresh` replaces the row of each sharded test the reports measured with
the least of every case's `time` over the reports and keeps the other rows; a
loaded worker only inflates a time, so measure with `--runs_per_test=3`, which
writes `<test-output-dir>/run-<k>/test-report.json`. A case's time includes
whatever it waits on, so measure a suite whose cases contend for one service,
such as the PostgreSQL conformance tests, one shard and one case at a time:
add `--jobs 1 --test_arg=--test-threads=1`. `--plan` prints
the cases and milliseconds the table puts on each shard, for choosing a shard
count. The table is an input of every sharded test, so their next run
re-executes; unsharded tests keep their cached verdicts. The graph contracts
fail when a row names no sharded test or holds anything but positive
milliseconds.

## Repeating a test

To diagnose a flake, execute one exact case repeatedly:

```sh
kiln test //crates/lash-store-sql:lash-store-sql__unit_test \
  --test_arg=--exact \
  --test_arg=render::tests::a_vocabulary_token_expands_once_for_both_backends \
  --test_sharding_strategy=disabled --runs_per_test=10
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

## Hermetic PostgreSQL tests

A test tagged `hermetic-postgres` in `tools/buck2/package-policy.toml` runs
against a PostgreSQL 16 that its own action starts, so it executes on the pool
and its verdict is cached like any other test's: an unchanged test is a cache
hit in every fork and in CI. Today that is every test of
`lash-internal-postgres-store` and its feature-lane variants.

```sh
kiln test //crates/lash-postgres-store:conformance__test
```

- **Server.** `native-tools-lock.json` pins a self-contained PostgreSQL 16
  build (`native//:postgres`: server, `initdb`, ICU and `pg_stat_statements`)
  and `libnss_wrapper.so` (`native//:nss_wrapper`). Both are declared inputs
  of the test; the pool image has no PostgreSQL.
- **Runner.** `tools/buck2/postgres_action_runner.py` is prefixed to the test
  command inside the launcher's watchdog. It runs `initdb` into the action's
  temporary directory (ICU `en-US`, trust authentication), starts the server
  on a free loopback port with the settings of `with-service.sh pg16`
  (`fsync=off`, `pg_stat_statements` preloaded), creates the `lash` database,
  applies `crates/lash-postgres-store/schema.sql` and exports
  `LASH_POSTGRES_DATABASE_URL` to the test. It stops the server and deletes
  the cluster however the test ends; the server is a child in the test's
  process group, so a timeout's group kill takes it too. Setup costs about
  one second.
- **Sandbox.** A pool action has loopback only, a private `/tmp` and its own
  PID namespace, which is all the server needs. `initdb` looks its user up in
  the password database and the action's user is not in the image's;
  `libnss_wrapper.so` answers that lookup.
- **Size.** The server shares the test's request. CPU requests keep each
  label's measured size or existing default. `[test_runs] service_floor`
  retains the existing 1 GiB memory request. The executor binds its private
  action temporary directory over `/tmp`; the cluster lives on the work disk.
- **Sharding.** Each shard is an action with a server of its own, so the
  shards of `conformance` and `integration` need no database slots.
- **An external server.** A run that is handed `LASH_POSTGRES_DATABASE_URL`
  (`--test_env`) starts nothing and uses that server, locally and uncached as
  for any service input. The PostgreSQL 14/18 compatibility lanes run these
  labels that way.

To convert another package, tag its tests `hermetic-postgres`, add it to
`service_floor` and run `kiln sync`. A test that needs a second service, a
PostgreSQL major other than 16, or a server shared with another process stays
on `with-service.sh`.

## Tool-run proof selection

The tool contract in [ADR 0099](../adr/0099-tool-children-of-effect-groups-are-live-closing-settled.md)
uses owning Run/source laws. Select a full test path from the current source and
verify the executed-case count in the printed `kiln test` report. A successful
zero-case filter proves nothing. Use the Restate server double and SQLite for
the developer proof; registered live or synthetic-next gates retain their own
release jobs. Do not select retired child/group-service test families or revive
removed targets. Generated target membership changes through `kiln sync` in the
owning definition closure.

## Service and Cargo-owned gates

Registered Restate suites without a `ci_driver` run as cacheable remote
actions. The registry in `scripts/restate-suites.toml` generates a target
named `restate_<suite>_<leg>` beside its Rust test binary, with hyphens in the
suite name replaced by underscores. The existing entrypoint selects it:

```sh
kiln gate lash <fork> -- python3 scripts/ci/restate_suite.py suite server-double --leg live
kiln test //crates/lash-restate-test:restate_server_double_live \
  --test_arg=--exact \
  --test_arg=live_restate_routes_new_invocations_to_the_newest_deployment_and_keeps_pins
```

Each shard declares the checksum-pinned Restate 1.7.13 release binary
(`native//:restate`), its suite runner, registry and divergence files. It
starts one private server, runs each selected law in its own process with
the existing progress bound and strict replay-divergence checks, then stops
and reaps the server. A readiness failure also stops it before removing its
data. The action's loopback, PID namespace and temporary directory isolate
its services. Law logs, server logs, the suite summary and JUnit cases are
returned as test outputs. Shards partition the selected registered laws by
name, so each law executes once.

The same action starts the pinned PostgreSQL 16 and applies the published
schema, preserving the SQL coverage the former local suite gate supplied.
New suite actions use the canonical unmeasured test-run policy until they
have measurements; ordinary compile and test requests are unchanged.
`target-inventory.json` lists the suite/leg labels under
`restate_suite_targets`. The workbench's registered custom driver keeps its
local fixture ownership. `serve` and explicit `--binary` recipes keep their
caller-owned service lifecycle.

`scripts/ci/with-service.sh` starts the same private PostgreSQL/Garage containers
used by CI, publishes an ephemeral loopback port, waits for readiness and removes
the container on success, failure or interruption:

```sh
scripts/ci/with-service.sh
scripts/ci/with-service.sh pg16 -- bash scripts/ci/store-tests.sh pg-store
scripts/ci/with-service.sh all -- bash scripts/ci/store-tests.sh s3-store
```

Trusted store jobs compile through the pool and execute locally against the
private service; the package-wide `pg-store` suites are the exception and run
their [hermetic](#hermetic-postgresql-tests) labels on the pool. Service
settings are runtime inputs only; PG 14/16/18 reuse compiled artifacts. Tests
run against a service are never cached. Driver controls are
`--local-test-execution`, `--no-test-cache` and repeated `--test_env KEY=VALUE`.
Untrusted jobs keep their Cargo commands and receive no pool credentials.
Local runs list service-shaped contracts they did not exercise, with recipes.

PG 16 is the pull-request primary. Merge groups add PG 14/18 compatibility
witnesses for schema diffs; full dispatch runs all three. Compatibility compares
live catalog artifacts and version stamps. Use `kiln gate lash <fork> -- <cmd>`
for other live gates, with identities and ports derived from `KILN_GATE_ID`.

Main's hourly full-profile dispatch derives its Restate suite/leg matrix from
`scripts/restate-suites.toml`. Registering a suite adds live and replay jobs.
`python3 scripts/ci/restate_matrix.py check` verifies the producer, matrix,
runner and conclusion wiring. Jobs run at most sixteen at once. Registered
suite actions use private PostgreSQL and Restate servers on the pool. The original three-job cap
had no shared service constraint. With 109–134 job-minutes across 48 legs,
sixteen slots imply about 6.8–8.4 minutes at even load instead of 36–45 minutes.
The existing 26–29 minute jobs should then set the full-run critical path.
A registry `ci_driver` retains specialized fixture cleanup.
Run the same entrypoint locally through a gate:

```sh
kiln gate lash <fork> -- python3 scripts/ci/restate_matrix.py run <suite> --leg replay
```

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
caches affect timing rather than correctness. The push warm covers the clippy,
`[check]` (workspace and `__fv_` lane-variant) and test-binary graphs, so an
agent's first `kiln check` or `kiln test` after a push reads the action cache
rather than re-executing. A separate `static-checks` job in the same workflow
runs the schema checks, `facade_completeness`, the repository-gate contracts
and a feature-lane `[check]` on every push and fails the run visibly; it does
not gate the warm.

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

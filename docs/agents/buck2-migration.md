# Buck2 migration evidence

Implementation and validation are in progress. This report records verified
results and outstanding acceptance gates; it does not claim a completed cutover
or a performance improvement. The final migration PR must replace outstanding
items with evidence or identify a concrete blocker.

## Scope and deployment

The Lash change replaces its development build/check/test/lint graph and callers
with Buck2. Cargo manifests, package identities, feature declarations, source APIs,
publication and supported Cargo recipes remain intact. Figments continues using
its existing backend and consuming Lash as pinned Git dependencies.

[Kiln PR #41](https://github.com/Ascending-AI/kiln/pull/41) selects a backend
from each checkout's marker and generates private deployment configuration. It preserves the agent-facing
`build|check|test|analyze|clippy|doc|run|fmt|sync|clean` dispatch path. Old Lash
worktrees and Figments continue to dispatch to their existing driver revision.
No installed tooling or NativeLink infrastructure has been changed during
validation.

Deployment order is Kiln first, then Lash. Deploy the reviewed Kiln revision on
development/executor hosts before creating forks of the migrated Lash revision.
The compatibility phase permits old Lash forks to remain active. CI uses the
repository's pinned bootstrap and projects the existing secrets into Buck2
configuration. NativeLink endpoints, authentication, instance, storage,
scheduler, worker runtime and resource limits stay unchanged.

Warm Buck2's new action namespace through the trusted main cache warmer after
cutover. The Rust rules and toolchain action identities differ from the previous
engine, so a populated NativeLink CAS does not imply warm Buck2 action results.
Expect first-build compilation and uploads; do not clear existing pool caches.
Each new worktree also needs its own initial analysis and daemon, while remote
action results are shared. Reuse immutable pinned tool archives where possible.

Rollback reverts the Lash migration as one change, restoring its previous graph,
driver and CI action together. Keep the Kiln backend selector deployed: it detects
the reverted checkout as the previous backend and leaves Figments unaffected.
Create a fresh fork at the reverted revision rather than changing another
agent's running worktree. Reusing old action caches is safe; cache deletion is
unnecessary. Remove a migrated fork with the new Kiln cleanup implementation so
its private Buck2 daemon is stopped correctly.

## Official-client compatibility

The executable is the official Buck2 2026-09-15 Linux musl release, commit
`6507dd157a6f81a810c48583edf1758dd0c337c5`, SHA256
`8d91d8d3654531f137ee1a0f2752cd67446065c8a2c65a02ee2c810b7899a4c9`.
The matching prelude is `4d101dce3482c35b32f9f1e7072b354ae789d256`.
The build path downloads this verified executable and does not compile Buck2
or apply a binary patch. The repository Starlark overlay changes rule action
environments through the documented API.

TLS/client identity, the existing REAPI instance and SHA256 worked against the
actual pool. Dependency-free `lash-internal-store-sql` compiled remotely, and
its 31 unit tests passed. A fresh daemon reused the compile outputs and actual
cached test verdict. Output materialization was verified, rather than inferred
from a successful remote call.

The scheduler accepts Buck2's `Command.platform` properties. The current
NativeLink worker reads `Action.platform` for property-derived environment, which
left the CPU/memory handoff empty in the initial stock-client probe. The
documented action-environment API provides a compatible route through the
existing supervisor:

| Canonical request | Observed `cpu.max` | Observed `memory.max` |
| --- | --- | --- |
| 1 CPU, 1,572,864 KiB | `166666 100000` | `2236960768` bytes |
| 2 CPU, 3,145,728 KiB | `200000 100000` | `3221225472` bytes |

Both were actual remote executions with the unmodified release and all five
platform properties. The smaller request was raised by the worker's existing
minimum share; the larger request received its exact budget. Both reported zero
OOM events. The same canonical request must feed scheduling properties and
`KILN_ACTION_CPU_COUNT`/`KILN_ACTION_MEMORY_KB` in the outer command environment.
Rust's inner `rustc_env` is applied after supervisor startup and cannot replace
that handoff.

A repository external test runner uses the supported `test.v2_test_executor`
and Execute2 APIs. The stock client executed all 31 SQL tests remotely and
materialized all 31 JUnit cases, logs and declared receipt directories. After
cleaning only its private isolation state and starting a fresh daemon, the
verdict was cached and all reports materialized into a new result directory.
A deliberate failing fixture returned a native failed-test result and retained
its case XML, log and nested receipt. Repetition cached the passing fixture
and reran the failing fixture. An internally enforced 1-second timeout reported
a native timeout and retained its outputs; a short outer deadline grace permits
report finalization. No failure is disguised as a successful build action.

The generated Cargo-shaped graph also passed the representative target: the
SQL `[static]` build executed two remote commands and zero local commands, and
the wrapped unit target executed remotely with all 31 JUnit cases passing.
Repeating the test returned `cache=true` for the same action digest,
`bf47aa1e14f661f26221145ec4324fcde0e383d5cfd32044befc0aedd77e37d7:147`.
The generated graph's runtime probes reproduced the same existing worker floor
and exact 2 CPU / 3 GiB limits recorded above.

Full acceptance still requires rule-level CPU/memory coverage for every Rust,
build-script, native child, test and helper action, followed by the workspace
and feature parity runs. The representative target does not establish that
coverage by itself.

Primary contracts are the [action environment API](https://buck2.build/docs/api/build/AnalysisActions/),
the [executor configuration API](https://buck2.build/docs/api/build/CommandExecutorConfig/),
the [external runner interface](https://buck2.build/docs/rule_authors/test_execution/),
and the pinned [NativeLink action environment implementation](https://github.com/TraceMachina/nativelink/blob/0d5f173fd39edbf5b284e550aede94c60e93479a/nativelink-worker/src/running_actions_manager.rs#L1855).

## Coordinator measurements

All baseline runs used source revision
`c5b6ec44cfabb937a1138af45c6170813116fdd1`, Rust 1.98.1 and the existing
NativeLink pool. Compilation used the actual prior engine flags, not an assumed
Cargo profile. The representative build/test target was the SQL library/unit
binary. Workspace analysis covered the whole generated default graph.

| Workload | Wall seconds | Daemon CPU seconds | Peak daemon RSS GiB |
| --- | ---: | ---: | ---: |
| Cold SQL build, action-cache lookup bypassed | 780.092 | 145.07 | 1.099 |
| Warm SQL build | 10.380 | 20.40 | 1.172 |
| First SQL test, existing pool cache allowed | 69.167 | 105.70 | 2.462 |
| Two SQL builds, warm first coordinator/fresh second | 97.635 | 178.35 | 2.860 |
| Two warm SQL builds | 9.690 | 6.54 | 2.725 |
| Small source edit, full check | 11.457 | 2.84 | 1.640 |
| Same edit, unit test | 35.805 | 98.64 | 2.363 |
| Fresh-daemon workspace analysis | 82.584 | 287.45 | 2.288 |
| Warm workspace analysis | 5.517 | 3.62 | 1.395 |
| Two fresh-daemon workspace analyses | 108.153 | 558.18 | 3.981 |

The two fresh workspace analyses took 103.299 and 107.782 seconds; the slowest
was 107.782 seconds. Each analyzed 106,777 configured targets. The cold SQL
build executed 341 remote actions, including previously cold LLVM/toolchain
actions, so its wall time is not a measurement of the Rust leaf alone. The
initial SQL test used a cached 31-test verdict. The edited test executed the
changed compilation and test remotely, then restored the exact source preimage.

RSS includes only the selected worktrees' coordinator daemons. The sampler uses
passive `/proc` reads every 250 ms and discovers daemon processes every two
seconds. CPU is the daemon's user/system counter delta. Buck2 measurements must
include its forkserver as well as its daemon. Compilation on remote workers is
excluded from coordinator RSS/CPU. Client CPU is recorded separately; it must
not be added to daemon CPU without checking process accounting.

Concurrency was limited to two remote actions total, one per worktree for paired
builds. Workspace analysis performed no compilation. Shared host/pool load and
single-sample timing limit conclusions. Tool download/extraction and first
bootstrap must be reported separately. The previous engine's LLVM build and
Buck2's tool bootstrap may differ; an end-to-end difference cannot automatically
be attributed to coordinator efficiency.

[Exact measurements](build-coordinator-benchmarks.json) include byte counts and
separate client CPU. The passive sampler is `tools/buck2/benchmarks/measure.py`;
`pair.py` records individual durations and the slowest concurrent workload.
Neither tool signals processes or clears caches. For example, in an owned fork:

```sh
python3 tools/buck2/benchmarks/measure.py --repo "$PWD" \
  --result /tmp/lash-build-metrics.json --log /tmp/lash-build.log -- \
  bash scripts/hermetic-build.sh build //crates/lash-store-sql:lash-store-sql --jobs 2
```

The first matched workspace-analysis observations used the owning Kiln CLI,
the same source baseline and the complete generated default graph, with tools
already materialized and no compilation:

| Analysis | Bazel wall s | Buck2 wall s | Bazel daemon CPU s | Buck2 daemon CPU s | Bazel peak RSS MiB | Buck2 peak RSS MiB |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Fresh daemon | 82.584 | 14.393 | 287.45 | 4.85 | 2343.36 | 256.39 |
| Warm daemon, initial graph check | 5.517 | 8.279 | 3.62 | 2.28 | 1428.14 | 299.44 |
| Warm daemon, verified graph receipt | 5.517 | 3.495 | 3.62 | 1.40 | 1428.14 | 281.15 |

These analysis observations show lower coordinator memory and CPU. The first
warm observation included dependency generation on every invocation. A private
receipt now validates the generator inputs and checked-in outputs before
reusing the graph. Rust bodies contribute their `CARGO_BIN_EXE_*` references;
new source paths, manifests, dependencies, features, policy and lint
configuration still invalidate generation. Ordinary compiler edits are checked
by Buck2 without regenerating dependencies.

The final small-target and concurrent observations are below. Both engines used
the frozen source revision above. Buck2's first single-target measurements used
candidate `86f1e1ae`; the receipt correction is `1309acacf8`. Compilation,
linking and test execution used NativeLink with zero local execution actions.
Buck2 single-target runs used one action slot alongside the independent one-slot
UI proof; the earlier single-target Bazel runs used two slots. Tools were already
bootstrapped. These are single observations on a shared pool.

| Workload | Bazel wall s | Buck2 wall s | Bazel daemon CPU s | Buck2 daemon CPU s | Bazel peak RSS MiB | Buck2 peak RSS MiB |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Warm SQL build | 10.380 | 2.485 | 20.40 | 0.11 | 1199.88 | 236.98 |
| First SQL test | 69.167 | 43.189 | 105.70 | 1.01 | 2522.68 | 250.73 |
| Warm SQL test | not measured | 3.543 | not measured | 0.19 | not measured | 239.23 |
| Edited SQL check, initial receipt | 11.457 | 17.871 | 2.84 | 0.36 | 1678.89 | 240.77 |
| Edited SQL check, corrected receipt | 11.457 | 10.953 | 2.84 | 1.60 | 1678.89 | 281.01 |
| Edited SQL test, corrected receipt | 35.805 | 22.280 | 98.64 | 1.18 | 2419.42 | 279.32 |
| Two fresh workspace analyses | 108.153 | 7.986 | 558.18 | 6.15 | 4076.48 | 458.63 |
| Two warm workspace analyses | not measured | 4.516 | not measured | 1.36 | not measured | 512.31 |

The first Bazel test reused a cached verdict; Buck2's first test actually ran
all 31 cases remotely. Both are successful developer invocations, but this is
not a controlled comparison of uncached test execution. The warm Buck2 verdict
and all 31 JUnit cases were downloaded into a new report directory.

The initial edited check was slower because it spent about 12 client CPU seconds
regenerating Cargo/Reindeer. After narrowing receipt inputs it spent 4.99 client
CPU seconds and returned in 10.95 seconds. This is close to the earlier Bazel
observation, rather than evidence of a large check speedup. The corrected
check/test pair followed whole-workspace analysis, so its retained coordinator
had higher RSS than the earlier small-target-only run. Both edits were restored
byte-for-byte. A separate temporary test-only `compile_error!` failed native
metadata check and passed after exact source restoration.

The cold Buck2 SQL build took 14.081 seconds, 2.53 daemon CPU seconds and
280.36 MiB RSS, with two remote actions and final output materialization. The
Bazel cold build took 780.092 seconds and executed 341 remote actions including
LLVM/toolchain compilation. Buck2 uses checksum-pinned prebuilt toolchains and
its bootstrap was completed before timing. These cold wall times have different
toolchain preparation and action sets; they do not establish a 55-fold compiler
speedup. Bootstrap/download costs and a genuinely empty remote pool were not
benchmarked together.

Both fresh Buck2 worktrees had their own idle coordinator and output state
cleaned before concurrent analysis. Shared pool caches and other worktrees were
untouched. The slowest Buck2 worktree took 7.91 seconds; the corresponding
Bazel observation was 107.78 seconds. RSS totals include the two daemons and two
forkservers. The strongest current result is lower coordinator overhead for
independent worktrees. Full-workspace build/test latency remains a separate
acceptance measurement, and source revisions after the frozen benchmark are
validated for correctness rather than mixed into the comparison.

## Remaining delivery gates

- Complete Cargo-shaped dependency and target generation, native build scripts,
  source/asset declarations, features and profiles.
- Build/check and Clippy parity, deterministic unit/integration partition,
  generated files, feature lanes, service runners and relevant CI/release paths.
- Cargo consumption from the migration Git revision.
- Concurrent independent worktrees with separate source, outputs and daemons.
- Kiln generation/dispatch for Lash/Buck2 and Figments's existing backend.
- Matched coordinator benchmarks with compilation work reported separately.
- Final stale-command audit, removal of superseded build configuration and rules,
  source review and cross-linked unmerged Lash/Kiln PRs.

The Kiln implementation at `08481af66c9ef822c760c883608bcf6c7ac2d43f` passed
its four required batteries and independent review after correcting orphan
cleanup fencing, failed recreation identity loss and external executable
symlinks. Full Lash integration validation remains separate. Neither migration
PR will be merged as part of this change.

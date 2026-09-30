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

These single observations show lower coordinator memory and CPU for analysis.
The initial warm observation regressed in wall time because the driver recomputed
Cargo/Reindeer and source-ownership reconciliation on every invocation. Profiling that stage found
6.5 seconds under instrumentation, including 3.5 seconds in generated-model
work and 2.6 seconds waiting for subprocesses. A private verified receipt now checks graph inputs and generated output identities
before reusing that result. After the necessary owned-daemon configuration restart,
a retained-daemon repeat took 3.495 seconds with 281.15 MiB peak RSS. Graph changes
still require full generation and drift checking. These are different candidate
stages, recorded in the measurements file; the final candidate still needs a
matched re-measurement. No build or test speedup has been established; matched cold, warm, edit and concurrent runs remain
outstanding. Re-measure the final candidate after those changes.

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
  independent review and cross-linked unmerged Lash/Kiln PRs.

The Kiln implementation at `08481af66c9ef822c760c883608bcf6c7ac2d43f` passed
its four required batteries and independent review after correcting orphan
cleanup fencing, failed recreation identity loss and external executable
symlinks. Full Lash integration validation remains separate. Neither migration
PR will be merged as part of this change.

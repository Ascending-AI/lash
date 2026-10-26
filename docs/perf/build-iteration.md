# VM comparison build iteration (FIG-7411)

On 2026-10-26, a one-line VM-only edit rebuilt the full profiling harness in
**1366.2 seconds (22.8 minutes)** and the iteration harness in
**261.3 seconds (4.35 minutes)**. These are shared-pool observations,
with pool load recorded below. The two production candidate pairs retain their
affected regops rankings, with every measured magnitude within **0.277
percentage points** of the full build. The empty-loop control is inconclusive.

## Command and scope

The [build guide](../agents/hermetic-build.md#vm-candidate-iteration) contains
the command. Add `-c kiln.perf_iteration=true` to the ordinary profiling build.
It retains optimization level 3 for the VM, worker adapter, dialect/native
crates and third-party code; the VM pins rustc's optimized default of 16 codegen
units. Five downstream host libraries use level 0. The harness executable uses
level 1 to retain optimized global-allocation entrypoints. An executable at
level 0 biased some allocation-heavy absolute counts by up to 9%.

Debug assertions and overflow checks stay disabled in the changed crates.
Host build tools, features, all five execution properties, compile reservations
and separate test budgets are unchanged. The default build is unchanged.
Qualification covers TypeScript with the default `stats_alloc` allocator.
Use the full optimized build for final numbers when results differ, and for
runtime, durable, startup and throughput measurements.

## Matched rebuild and pool load

Base: `2ec8e492978f`, Rust 1.98.1, shared NativeLink pool. Both builds changed
only `KEPT_MEASUREMENTS` from 8 to 19 in
`crates/lash-kernel-vm/src/compile/formula.rs`. Each VM target compiled on a cold
action key; the iteration's explicit default codegen-unit flag separates its
key without changing the optimization level. Unchanged consumer artifacts from
the reference build can be reused, and action counts disclose that reuse.
The constant is restored in the landed change.

| Build | Wall s | Executed remote actions | Reserved CPUs before / after | Queued before / after |
|---|---:|---:|---:|---:|
| final-constant-full | 1366.2 | 25 | 143 / 95 of 144 | 81 / 2 |
| final-constant-iteration | 261.3 | 14 | 95 / 133 of 144 | 2 / 0 |

The further edit, 19 to 20 at the same constant, took **381.8 seconds
(6.4 minutes)** with **25 remote actions and zero cache hits**. It reused no
reference candidate's worker consumers. Pool reservation moved from
136/144 to 126/144 CPUs; queued actions moved from
1 to 119. This checks the actual next-candidate cost,
including the fully optimized worker/client prefix. Both constant edits were
restored. The earlier 4.35-minute matched run reuses unchanged consumers;
the 6.4-minute run measures a wholly fresh candidate key.

The pool's load changes between runs. These are observed build waits, not a
quiet-host runtime speedup or a claim that queue conditions were identical.
The action breakdown attributes the largest cost to downstream compilation
and the executable action. The separate fresh-key result above checks the actual next-candidate cost.

## Per-action attribution for the full matched rebuild

All 25 executed actions are listed. Spans include Buck preparation and executor
stages; execute and queue values come only from uncached remote attempts.
Cached producer statistics are excluded. Spans overlap and must not be summed
as build wall time. The executable action includes rustc code generation and
linking; this trace does not split the final linker from its parent rustc.

| Target | Configuration | Action | Span s | Execute s | Queue s |
|---|---|---|---:|---:|---:|
| `crates/lash-perf:lash-perf` | profiling | rlib [pic] | 667.67 | 624.08 | 38.19 |
| `crates/lash-perf:lash-perf__bin` | profiling | link [pic] | 402.36 | 362.20 | 9.41 |
| `crates/lash:lash` | profiling | rlib [pic] | 202.00 | 187.35 | 11.63 |
| `crates/lash-plugin-mcp:lash-plugin-mcp` | profiling | rlib [pic] | 169.81 | 165.00 | 3.15 |
| `crates/lash-protocol-rlm:lash-protocol-rlm` | profiling | rlib [pic] | 149.16 | 141.17 | 5.60 |
| `crates/lash-vm-worker:lash-vm-worker` | profiling | rlib [pic] | 112.93 | 108.91 | 0.00 |
| `crates/lash-vm-runtime:lash-vm-runtime` | profiling | rlib [pic] | 103.45 | 94.80 | 7.08 |
| `crates/lash-kernel-vm:lash-kernel-vm` | profiling | rlib [pic] | 73.10 | 36.53 | 34.89 |
| `crates/lash-kernel-vm:lash-kernel-vm` | host | rlib [pic] | 73.03 | 33.14 | 39.38 |
| `crates/lash:lash` | profiling | metadata [pic] | 68.81 | 56.21 | 10.05 |
| `crates/lash-vm-client:lash-vm-client` | profiling | rlib [pic] | 55.86 | 54.59 | 0.00 |
| `crates/lash-protocol-rlm:lash-protocol-rlm` | profiling | metadata [pic] | 53.17 | 31.97 | 20.64 |
| `crates/lash-vm-runtime:lash-vm-runtime` | profiling | metadata [pic] | 44.63 | 43.03 | 0.00 |
| `crates/lash-plugin-mcp:lash-plugin-mcp` | profiling | metadata [pic] | 40.91 | 32.34 | 7.34 |
| `crates/lash-vm-worker:lash-vm-worker__bin` | profiling | link [pic] | 28.86 | 5.07 | 0.62 |
| `examples/delegation:delegation` | profiling | rlib [pic] | 25.98 | 24.65 | 0.21 |
| `crates/lash-vm-broker:lash-vm-broker` | profiling | rlib [pic] | 21.76 | 21.25 | 0.00 |
| `crates/lash-kernel-releases:build_script` | host | buildscript | 12.55 | 11.19 | 0.00 |
| `crates/lash-kernel-vm:lash-kernel-vm` | profiling | metadata [pic] | 12.00 | 11.25 | 0.52 |
| `crates/lash-kernel-releases:build_script__build` | host | link [pic] | 11.50 | 10.94 | 0.00 |
| `crates/lash-kernel-releases:build_script` | profiling | buildscript | 11.40 | 10.02 | 0.00 |
| `examples/delegation:delegation` | profiling | metadata [pic] | 8.66 | 7.54 | 0.43 |
| `crates/lash-vm-worker:lash-vm-worker` | profiling | metadata [pic] | 8.07 | 7.64 | 0.00 |
| `crates/lash-vm-client:lash-vm-client` | profiling | metadata [pic] | 7.34 | 6.81 | 0.00 |
| `crates/lash-vm-broker:lash-vm-broker` | profiling | metadata [pic] | 4.41 | 4.06 | 0.00 |

## Production candidate ranking

Two production patches were replayed on the fixed `2ec8e492978f` dependency
graph, then restored as concrete parent/child commits. These are controlled
replays, not measurements of the original historical ancestors:

- `2189e05c1b` to `b302f0b31f`: restore directly addressed call arithmetic from
  production patch `7165a8a33c`.
- `b302f0b31f` to `69885099e1`: restore computed-read sorting from production
  patch `e79a2b9740`.

Restoring the arithmetic patch reproduced both previously measured binaries
byte for byte. Restoring sorting reproduced the original full-build binary
byte for byte. This confirms the common source states, independently of
commit messages. No experimental runtime commits belong to the landing diff.

Qualification uses the default `stats_alloc` allocator and TypeScript rows.
Each artifact ran all 17 TypeScript workloads with three measured low/high
pairs, one warmup pair and factor 10. Guest sources, trip/operation counts and
every result checksum agree across builds and variants. Processes ran under
an 8 GiB virtual-memory cap and `perf stat -e instructions:u`. The comparison
driver's existing scoped user-instruction counter supplies the differential
median instructions/op; outer perf counts retain the whole-process evidence.
No runtime wall-time result is claimed on this shared host.

The arithmetic pair matches all 16 non-control row signs; sorting matches all 17.
The only sign discrepancy is arithmetic's unaffected `empty_loop` control:
+0.051% versus -0.027%. A separate factor-1000 probe puts both changes below
0.001%, with opposite signs, so this control is inconclusive rather than a
candidate win/loss. The maximum magnitude disagreement across every original
row of both pairs is **0.277 percentage points**, below the 1-point limit.
The affected regops rows retain their signs and magnitudes.

| Workload | Arithmetic full % | Iteration % | Sorting full % | Iteration % | Largest error pp |
|---|---:|---:|---:|---:|---:|
| empty_loop | +0.051 | -0.027 | +0.025 | +0.013 | 0.078 |
| prop_read | -12.397 | -12.397 | -0.451 | -0.450 | 0.001 |
| prop_write | -5.632 | -5.632 | -0.335 | -0.333 | 0.002 |
| array_read | -8.516 | -8.516 | -0.385 | -0.385 | 0.000 |
| func_call | -21.915 | -21.914 | -0.932 | -0.935 | 0.003 |
| func_closure_call | -15.371 | -15.371 | -0.437 | -0.437 | 0.000 |
| int_arith | -0.091 | -0.116 | +0.002 | +0.008 | 0.025 |
| float_arith | -0.085 | -0.119 | +0.003 | +0.003 | 0.034 |
| map_set_string | -2.069 | -2.069 | -0.030 | -0.033 | 0.003 |
| string_build1 | -15.107 | -15.119 | -0.468 | -0.511 | 0.043 |
| int_to_string | -14.589 | -14.587 | -0.585 | -0.585 | 0.002 |
| sort_bench | -1.554 | -1.557 | +0.471 | +0.402 | 0.069 |
| json_transform | -0.953 | -0.676 | -0.181 | -0.361 | 0.277 |
| records | -1.987 | -1.987 | -0.123 | -0.120 | 0.003 |
| templating | -1.185 | -0.962 | -0.141 | -0.196 | 0.223 |
| object_build | -8.268 | -8.268 | -0.552 | -0.569 | 0.017 |
| object_sort | -5.748 | -5.748 | -0.332 | -0.334 | 0.002 |

Negative values mean fewer instructions/op. The sorting replay correctly retains its small positive `sort_bench` regression in both builds. Use the full optimized build for final counts when absolute results differ.

## Validation and retained evidence

No tests, manifests, dependencies, public exports or serialized shapes changed.
The existing instruction measurements provide the build-mode qualification;
no timing-sensitive test was added. Generated-file sync is clean.
`scripts/land` runs the required workspace Clippy before pushing.

Raw action logs, build reports, commands, pool snapshots, source patches,
scoped and outer instruction receipts, artifact hashes and the experimental
commit bundle are retained on this host at
`/workspace/notes/lash/tasks/lanes/r33-optbuild.evidence/`. The lane report records
the landed SHA and final sync/Clippy result. Executable copies are excluded
from the archive; their exact hashes and build reports are retained.

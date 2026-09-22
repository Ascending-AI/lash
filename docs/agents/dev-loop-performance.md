# Development loop experiments, September 2026

This change follows PR #1908 and the investigation of the effect-group lanes.
Measurements used Rust 1.98.1, Bazel 9.1.0 and the shared NativeLink pool,
starting at Lash `ee9ef46ff62543681d58301c851cd55c214e493a`.
Compiler-action counts establish invalidation behavior. Individual elapsed
samples include shared-pool load and are not a predicted full-floor speedup.
The reported build times are Bazel elapsed times. An overlapping local Cargo
check affected part of the source-ownership measurement window, so those elapsed
samples are not controlled throughput comparisons.

## Retained changes

| Experiment | Before | Result |
| --- | --- | --- |
| Edit a runtime-scenarios-only source | Both scenario and effect test binaries compiled | Only the owning binary compiled; 2 Rustc actions became 1 |
| Edit their shared helper | Both binaries compiled | Both still compile |
| Edit an external core-execution test-only module | Unit test and production library compiled | Only the unit test compiled; 2 Rustc actions became 1 |
| Schema generation | Cargo compilation inside Python checks | Declared Bazel generation and comparison actions share the existing generator binaries |
| Inherited action resource policy | Local 1 CPU/2 GiB; CI 4 CPU/4 GiB | Identical defaults and remote action-cache reuse |
| Concurrent launcher tests | Shared host ownership locks | Private mount, PID and network namespaces with production locking unchanged |

The core test-only edit still costs a real large unit compilation. Its fresh
green sample spent 71.2 seconds compiling the unit target. Source ownership
removes an unnecessary library compile; it does not make that frontend cheap.
The initial manifest excluded 43 audited external `cfg(test)` modules from
production libraries; 42 remain after relocating the lease suite. Testing-feature
fixtures and all unit inputs remain covered.

All four schema outputs were byte-identical across the old Cargo route, the
Bazel route and checked-in artifacts. Obsolete and changed schema mutations
failed validation. A second fork with the same frontend dependency state
completed in 3.662 seconds with zero remote executions, two remote cache hits
for the example generator library/binary and a disk hit for schema generation.
An earlier mismatched fork recompiled: ignored frontend files had changed the
broad package inputs. Python bytecode caches and frontend `node_modules` are
now excluded from those globs.
This is cache-reuse evidence, not a controlled end-to-end CI speedup.

For a real inherited genrule, changing only CPU/memory defaults changed its
action digest. Restoring the unified policy reused the original remote action
cache entry with the local disk cache disabled. Explicitly sized first-party
targets retain their resource floors. This proves reuse for the measured action;
it does not quantify a whole-graph hit-rate improvement.

The full launcher reset suite and process-identity suite passed concurrently
inside Bubblewrap. Separate probes verified private locks, hidden host `/tmp`
sentinels and an inaccessible host Docker socket. These are isolated shell
fixtures; they do not run against live services.

## Bounded test split

Fifteen lease serialization tests move into a separate integration crate using
existing public types. Their assertions, helpers and fully qualified names are
preserved. The before/after multiset contains the same 861 tests, now divided
between 846 unit tests and 15 integration tests. Model-filter tests stay in the
unit crate because moving them would require an extra testing feature.

A matched lease-test comment edit took 84.9 seconds before and 13.3 seconds
after. Compiler worker execution for the changed test fell from 76.0 seconds
to 0.8 seconds. A production comment edit took 78.3 seconds before and 89.1
seconds after. Aggregate compiler worker execution fell from 122.5 to 104.4
seconds; the new integration compile added 2.4 seconds. These are single
shared-pool samples. They establish a cheaper edit boundary for this test group,
but do not establish improved production-edit latency or eliminate the large
unit frontend. Keep the existing test batching and resource policy.

## Diagnostics and placement

The opt-in `scripts/bazel-diagnose.py` captures profiles, compact execution logs
and REAPI metadata. The checksum-pinned BuildBuddy CLI decodes Bazel 9.1 logs
and explains source, argument, environment and execution-property changes
without a BuildBuddy service. Actual remote metadata identified a worker and
separated queue, input fetch, execution and upload. Negative cross-host phase
timestamps are reported as clock skew. Failed and interrupted captures retain
the invocation exit status.

A 1,059-action cached log decoded successfully. Three tiny paired probes showed
about 1.7 seconds median paired capture overhead, with substantial noise. Capture
therefore remains opt-in. Keep raw bundles private and outside the checkout.

Dedicated-worker placement remains unchanged and unbenchmarked. The current
scheduler has no eligible-worker class selector. Increasing resource requests
to exclude the development host changes admission and action identity, so it
would not measure placement alone. A valid experiment needs a coordinated
scheduler/worker capability such as an exact `kiln_execution_class` property,
registered before any client requests it. Then interleave mixed and dedicated
samples with identical resources/features, separate cold and warm cache states,
and measure latency, throughput and interactive-host load before choosing a
policy. No live fleet configuration was changed in this PR.

## Peer mechanisms

- [rules_rust test source ownership](https://github.com/bazelbuild/rules_rust/blob/88f6b08714cb251638f5676e10ac121e972aa6a4/rust/private/rust.bzl#L1784)
  informed explicit root/shared source declarations. Lash still uses its pinned
  rules_rs graph; this is not a build-rule migration.
- [BuildBuddy offline explain](https://github.com/buildbuddy-io/buildbuddy/blob/51a570d9a2e641262e5912b6ccff7336b1b734ad/cli/explain/explain.go#L38)
  supplies execution-log comparison.
- [NativeLink scheduler properties](https://github.com/TraceMachina/nativelink/blob/0d5f173fd39edbf5b284e550aede94c60e93479a/nativelink-config/src/schedulers.rs#L43)
  and [worker documentation](https://docs.nativelink.com/configuration/scheduler-and-workers)
  define eligibility and attribution constraints.
- [Bazel remote-cache diagnostics](https://bazel.build/remote/cache-remote)
  describe paired execution-log investigation.

The earlier lane already rejected jobs=64, generic linker/debuginfo tuning and
watchfs as useful changes for its measurements. Global parallel rustc frontend
execution caused an ICE with the pinned compiler. Those settings remain unchanged.

# Compiler self-profiles

From a Kiln fork with `env.sh` sourced, run:

```sh
python3 scripts/compiler_self_profile.py \
  //crates/lash-core-execution:lash-core-execution \
  --summarize "$HOME/.cargo/bin/summarize" \
  --out-dir .kiln/compiler-self-profile/execution
```

This builds the chosen crate, downloads its rustc self-profile from NativeLink,
and runs measureme's `summarize summarize` on the `.mm_profdata` output. It prints
the top 15 query/phase labels by **self time**, their inclusive time and counts,
and the decoder's total CPU time. `--top N` changes the displayed row count.
The new output directory holds the full text summary, Buck2 build report and
the build command, decoder path and build wall time in `invocation.json`.
Profile data stays in the declared Buck2 output tree; it is not copied into
the evidence directory. Decoder failures fail the command.

Pass multiple explicit first-party library or binary labels to collect one
profile per crate. Patterns, workspace aggregates, tests, third-party labels,
and labels with an existing subtarget are rejected. Dependencies compile
normally. This command does not change optimization levels or resource budgets.

The underlying opt-in Kiln configuration is:

```sh
kiln build --config=rust-self-profile --materializations=final \
  //crates/lash-core-execution:lash-core-execution \
  --build-report .kiln/compiler-self-profile/build.json
```

The driver selects the pinned prelude's `[profile][rustc_stages][raw]` subtarget.
The prelude already declares a directory output and passes its output artifact
to `-Zself-profile=...`, with `-Zself-profile-events=default,args`. A subsequent
`find_profdata` action exposes the file as `self_profile.mm_profdata`. Both
actions run remotely with the crate's existing execution properties. The
report's output group is `profile|rustc_stages|raw`; final materialization
downloads the declared output. No prelude fork or local compiler fallback is
needed. The normal default/static actions, toolchain flags and third-party
rules retain their existing action keys. `check`, `test` and `clippy` do not
accept the instrumentation configuration.

The profile subtarget compiles PIC library code; for a binary it collects
compiler work rather than a final executable link. It does not instrument
every dependency or represent end-to-end linker time. The table groups compiler
events by label; it is not an application runtime profile. Inclusive times
overlap, and CPU time can exceed wall time when LLVM workers run concurrently.
The profile's own string/event collection also consumes compiler time.

## Decoder compatibility

FIG-5723 decoded a profile from the pinned Rust 1.98.1 compiler with
`summarize` 12.0.3 from measureme commit
`e22edccd0364a3058e8e618f51988d64db0b3e10`. The installed decoder was built
with Rust 1.97.0; the profile producer's version determines format compatibility.
No replacement decoder was needed. To use another installation, pass
`--summarize /path/to/summarize`. See the
[compiler profiling guide](https://rustc-dev-guide.rust-lang.org/profiling.html)
and [measureme](https://github.com/rust-lang/measureme) for the format and tools.

## One cold overhead pair

For a library, compare the unprofiled PIC output with the profile subtarget:

```sh
/usr/bin/time -p kiln build \
  '//crates/lash-core-execution:lash-core-execution[static_pic]' \
  --isolation-dir "self-profile-off-$(date +%s%N)" \
  --no-remote-cache --materializations=final \
  --build-report .kiln/compiler-self-profile/off.json

python3 scripts/compiler_self_profile.py \
  //crates/lash-core-execution:lash-core-execution \
  --cold --summarize "$HOME/.cargo/bin/summarize" \
  --out-dir .kiln/compiler-self-profile/on
```

Create the parent evidence directory before the first command. `--cold` uses
a fresh isolation directory and `--no-remote-cache`, so daemon reuse and cached
outputs are bypassed in both legs. Fresh isolation also changes output paths,
preventing an identical previously cached remote action from being reused.
Each leg includes cold dependency compilation. The first command's `real` and
the second command's `build_wall_seconds` both cover the build invocation,
including materialization; the second excludes decoding.

These builds run on shared NativeLink hosts. Record both wall times, selected
compiler action durations, revision, compiler and decoder versions, and any
resource retry. One pair measures an observation that includes dependency
work, queueing, network and host contention; it does not establish an isolated
profiler overhead percentage or a regression threshold. Use the action log
to verify that both chosen compiler actions executed instead of hitting cache.

## Complete build costs from an existing receipt

The offline reader needs no new build. From a fork with `env.sh` sourced:

```sh
(ulimit -v 8388608; /usr/bin/time -v \
  python3 scripts/ci/buck2_event_digest.py path/to/clean.events.pb.zst \
    --out-dir .kiln/build-cost/clean \
    --usage-log path/to/captured-worker-actions.log) \
  > .kiln/build-cost-clean.txt 2>&1
```

Create `.kiln/` first. Omit `--usage-log` when no captured worker telemetry is
available; repeat it for multiple workers. The input can also be saved Buck
JSON-lines output. `--out-dir` defaults to `<event_log>.build-cost/`, keeping
multiple CI receipts separate. `--summary FILE` appends the complete text to a
GitHub summary. Missing files, malformed events and failed Buck decodes exit
nonzero. The pinned client is bootstrapped without preparing compiler tools;
no daemon build, pool action or application instrumentation is needed.

Read `build-cost.txt` for invocation identity, population counts, command
elapsed microseconds, Buck's summary, the **entire** critical path and all action
rows. `critical-path.txt` retains Buck's complete readable path separately.
The action table emits one row per command attempt (or one for an action with
no command). Its `rustc link` category includes compiler and linker work, even
for a test binary; it is not linker-only time. Use the self-profile route above
when compiler phase evidence is required.

`build-cost.json` preserves the full start/end event payloads in `spans`, not
just the displayed columns. Each span retains its ID, parent, timestamps,
configuration/owner, action key, category, identifier, commands, cache details,
queue and execution metadata, platform properties and materialization payloads
where Buck emitted them. Executor stages join through exact span ancestry;
`action_join` and `missing_endpoints` disclose missing evidence. Configuration,
revision, session, system, command options and complete `BuildGraphInfo` payloads
remain in `identity_events`. `critical_path_joins` uses Buck's exact span IDs;
synthetic/reused entries without IDs and missing spans remain explicit.

`action_intervals` retains daemon-clock interval endpoints in event order so
overlap remains observable without enumerating every pair or sorting events.
Use the `commands[].elapsed_us` interval for command elapsed time. Action walls,
worker execution, queues and materialization can overlap; adding their durations
does not produce elapsed time. The reader makes linear passes over event/input
bytes, spans, commands, critical entries and usage records, with indexed joins;
it retains O(events + usage records) data. It is diagnostic evidence, with no
regression predicate, threshold or automatic comparison admission.

Resource joins reuse the sizing tools' digest normalization, Cargo target
identity and usage parser. CPU microseconds and cgroup/anonymous peak bytes enter
only when one uncached remote command matches one usage record by exact action
digest, category and Cargo identity. Ambiguous attempts, missing inventory
identities and absent digests stay explicit. There is no reservation/time
fallback. Cached producer statistics do not describe current execution. Worker
cgroup and anonymous peaks **are not RSS**: `rss_peak_bytes` stays absent because
the current worker log does not report it. Raw command execution statistics are
retained without interpreting unsupported counters as measured zero.

Source-log, decoder, reader, inventory and optional usage-log SHA-256 identities
travel in the receipt, alongside the original trace ID, command, revision,
configuration, sessions and terminal payloads. These establish which observation
was decoded. A clean/private-edit comparison still requires external edit and
restoration provenance, matching build/worker/observer settings and a verified
successful terminal outcome. Missing evidence cannot qualify a comparison.

The reader runs after the build, so it adds no observed application or remote
compiler work. To measure its local cost, pair the same pinned client's `log
summary`, `log critical-path --format readable` and `log show` decodes with and
without the reader's retention/joins. Use the **same immutable event log**, capture
`perf stat -e instructions:u`, load1 and `/usr/bin/time -v` peak RSS under the
8 GiB virtual-memory cap. This isolates the extra offline reading/output work
from the underlying build workload; it is one diagnostic pair, not a quiet-host
wall-time claim or a build-speed improvement. Keep compiler self-profile overhead
and this offline reader overhead separate.

When worker rows lack a digest, add `--scheduler-trace dispatches.jsonl` to
bridge their exact `operation_id` through a captured scheduler export. The
export contains `operation_id`, `action_digest` (`hash-size`), `worker`,
`effective_cpu`, `effective_memory_kb`, `effective_priority`, `dispatch_s` and
`identity`. Only `repo:lash` identities enter this join. All input files retain
SHA-256 identities; only joined dispatch rows enter action receipts.

The bridge preserves repeated dispatches. Completed usage samples and dispatch
rows must have equal counts and already be chronological per operation; repeated
usage attempt IDs refuse the join. Otherwise, attempt N joins dispatch N.
Missing operations, partial captures and ambiguous ordering remain unjoined.
Multiple worker attempts under one operation are retained in `worker_attempts`
and printed as `worker_attempt` text rows; their CPU/memory values are not
collapsed into a command scalar. Multiple operations or commands sharing a digest
still refuse attribution. Scheduler CPU/memory are effective placements after
hints/escalation, distinct from requested CPU/memory in Buck's platform or the
worker record.

`scheduler_coverage` discloses the export's dispatch population, time bounds,
command-window coverage and operation-attempt join counts. An uncovered/partial
window cannot qualify a missing-operation claim. Priorities remain unavailable
unless `--scheduler-priority-valid-from UNIX_SECONDS` supplies a trusted cutoff
from the export's provenance; reported values remain separate. For the dated
2026-10-17 export, the pool owner identifies 1792190211 (2026-10-16 22:36:51 UTC)
as the first trustworthy priority timestamp. Earlier reported zero priorities
are not measurements of actual priority. These are offline captured inputs;
this command does not query or change the scheduler.

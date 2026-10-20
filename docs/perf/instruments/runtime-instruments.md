# Runtime performance instruments

Latency samples finish at host outcome. A case owns at most 256 outstanding
marker tasks and reaps ready results without awaiting them in a lane. The
completed sample carries the exact `(session_id,input_id)` diagnostic key.
After every lane has finished, the case joins all diagnostic dispositions
before exporting witnesses or closing stores. Provider duration and host tap
state are captured at completion, before the next send can reuse a session.
A 120-second outer deadline covers awaited store reads as well as poll sleeps.
Saturation refuses diagnostic admission immediately; it does not delay a send.
The sample retains `complete`, `timed-out`, `failed` or `refused`, and a refused
or incomplete diagnostic enters the existing population error refusal.

The case `diagnostics` receipt reports `marker_waits_before_next_send` (zero),
`pending_limit`, `pending_peak`, `joined`, `refused`, `timed_out`, `failed` and
`pending_at_teardown`. The four terminal counts sum to the sample population;
missing, duplicate and unmatched joins refuse export. Pending ownership is
established at spawn; cancellation drops the owning JoinSet and aborts tasks.
The named law `latency::runner::tests::diagnostic_latency_cannot_pace_a_completed_lane`
holds the observer's binding read while two real product sends complete.

The maintained durable-substrate populations are:

| Case | Per sample | Counted purpose |
| --- | --- | --- |
| `process-steps-10` | One process, ten dependent Once bodies | Cold hydration/root loads, restore/warm fact reads, fresh process reads, start/outcome facts |
| `process-timers-10` | One process, ten 100 ms durable sleeps | Due-to-engine clocks, claim source/cause and owner SQL |
| `process-fanout-10` | One process, ten immediate independent Once bodies | Actual body entries, settlements and outcome transactions |
| `process-ready-fanout-10` | Same fanout with all ten entries behind one barrier | Explicit simultaneous release, with actual outcome-transaction counts |

`process_population` rows retain the actor identity, body/settlement count,
claims and owner transactions. Each owner transaction retains its SQL receipt
and `run_facts` counts. `process_trace` exports physical SQL execution joins,
graph decoded nodes/bytes, async queue receipts, `port_calls` and `discoveries`.
Port calls count attempts, including refusals: `hydrate` and `exec_root` only
for process roots, `process`, `registry.get_process`, `begin`, `now`, and
`run_facts.restore`/`run_facts.warm`. The first fact read in an activation is a
restore; later reads are warm. Calls outside activation are explicitly tagged.
Discovery rows retain actor/epoch, the runner's exact `Wake` debug name and
claim cause. Hints, polls and deadlines request authoritative reads; these
instruments never choose work. Initial startup waits for a successful recorded
claim instead of guessing readiness from a sleep.

The fanout entry barrier proves that every body entered before release. It
does not inspect the lifecycle's private JoinSet, so a one-outcome-transaction
law still belongs to the process implementation. Count actual transactions;
do not infer ready-result count from the barrier or timing.

Build through Kiln, resolve executables from the build report, then run each
population under the memory cap. Keep runtime stores outside the evidence tree.

```sh
kiln build //crates/lash-perf:durable-substrate__bin //crates/lash-vm-worker:lash-vm-worker__bin \
  --target-platforms //tools/buck2:profiling -c kiln.rust_profile=optimized \
  --materializations final --build-report "$PWD/.kiln/FIG-7054/build.json"
B=$(python3 tools/buck2/outputs.py --report .kiln/FIG-7054/build.json --label //crates/lash-perf:durable-substrate__bin --single)
W=$(python3 tools/buck2/outputs.py --report .kiln/FIG-7054/build.json --label //crates/lash-vm-worker:lash-vm-worker__bin --single)
(ulimit -v 8388608; /usr/bin/time -v "$B" --store sqlite \
  --case process-steps-10 --case process-timers-10 --samples 5 \
  --vm-worker "$W" --sqlite-dir target/FIG-7054/process \
  --out .kiln/FIG-7054/process.jsonl)
```

Detailed SQL observation reuses immutable worker labels per thread/collector
epoch and statement/relation descriptors per exact SQL/backend/epoch. The
receipt exports cumulative `worker_label_formats`, `worker_descriptor_bytes`,
`relation_analyses` and `statement_descriptor_bytes`, including drained windows.
Bounded admission preserves unknown relations and explicit loss/refusal; no
parameter value or mutable store fact is cached. A stale execution token
cannot join a successor epoch. SQL text, bind identity and dynamic row counts
retain their existing semantics.

For independent observer attribution, replay the same fixture four ways:
SQL off/queue off, SQL only, queue only, both. SQL observation is enabled with
`--sql-shapes`; queue observation uses `LASH_PERF_ASYNC_RECEIPT` and
`LASH_PERF_ASYNC_TIMING=on` (`off` emits the disabled receipt). Record instruction
counts with `perf stat -e instructions:u`, load1 and peak RSS for each command.
Ordinary mode has no detailed SQL execution or queue timer records. A change
in background SQL population is reported separately from descriptor savings;
after-completion marker time is never claimed as reduced send-to-completion.

Copy attribution keeps input-copy, serializer-output and semantic-tree
populations separate. The historical 827,908-byte commit-budget allocation
family comprises 823,340 B of JSON output (86 blocks) and 4,568 B of named
MessagePack output (34 blocks). Classify replacement allocations by the real
serializer and owning budget/trace path; absence of an old symbol is not proof
of eliminated allocation. Typed host-requested materialization, argument
parsing, path/mark allocations, graph/hash work and exporter text remain separate.
Phase canonical text and parent worker unwrap counts name input bytes/calls;
they are not token output or worker-side totals. Replay fanout decode counters
count calls to the row-payload decoder and payload bytes; fetch counters count
executions of the head-plus-rows SQL, independently of decode fanout.

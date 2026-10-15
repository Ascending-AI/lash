# Kernel work counters and guest-context profiles

Rust CPU profiles of the kernel show interpreter symbols, not the guest
functions they run. These two instruments attribute kernel work and time to
guest functions: `main`, declared functions, closures, TypeScript and Python
helpers and kernel library bodies, and to the natives they call. Both are
opt-in per machine (`KernelMachine::count_work`,
`KernelMachine::publish_context`) and derived: nothing they hold enters a
parked run, and neither changes a result, error, charge, park or seal
(`observing_a_run_changes_nothing_it_can_observe`). The machine starts no
thread for them; the sampler is the harness's.

Both commands run the gate 6 programs of
`crates/lash-dialect-typescript/examples/kernel_perf.rs` (any program name it
lists, or a `.ts` path) on one optimized, symbolized build:

```sh
. ./env.sh
E=.kiln/<ticket>
kiln build //crates/lash-dialect-typescript:kernel_perf__example \
  --target-platforms //tools/buck2:profiling -c kiln.rust_profile=optimized \
  --materializations final --build-report "$E/kernel-build.json"
B="$(python3 tools/buck2/outputs.py --report "$E/kernel-build.json" \
  --label //crates/lash-dialect-typescript:kernel_perf__example --single)"
```

## Exact work counters

```sh
(ulimit -v 8388608; "$B" work "$E/work" baseline image_host_environment heap_comprehension_build)
```

Each program runs once with its work counted and writes one entry of
`$E/work/kernel-work.json`. The counts are exact and deterministic: one run,
no statistic. Per guest function (`functions`, by inclusive charge):

| field | meaning |
|---|---|
| `name`, `kind`, `function`, `site` | the function's name; `main`, `function`, `closure` or `library_body`; its library `FunctionId` when it has one; the body's site |
| `calls` | frames pushed for it (`main`'s frame is the start's, not a call) |
| `statements`, `expressions`, `operations` | statements run, expressions and action operands evaluated, and their sum while it was the innermost frame |
| `uncharged_operations` | operations run while its forms were not charged (`K-CHG-007`) |
| `self_charge` | kernel charge units while it was the innermost frame and no native ran |
| `inclusive_charge` | units from each call's start to its end, callees and natives included, a recursive call counted once by its outermost frame; overlapping, never summed |
| `heap_insertions` | objects the heap took while it was innermost (a native's own are the native's) |
| `native_calls` | natives it called, by name, with counts |

Per frameless library function (`natives`, by self charge): `calls`,
`fast_calls` (answered by the allocation-free fast path), `zero_charge_calls`
(calls that charged nothing: work the charge does not show), `retries` (made
again after a collection), `guard_work` (units its general calls counted on
their work counter: a guarded native's guard units),
`formula_evaluations` (charge formulas and guard limits actually evaluated;
a constant fast-path charge evaluates none), `self_charge` and
`heap_insertions`.

Charge and insertions are read from the machine's own meters whenever the
running function changes, so the self charges and `unattributed_charge` add
up to `charged` exactly. The command refuses a program unless
`self_charge_residual` and `heap_insertion_residual` are 0,
`unattributed_charge` is 0 and the counted run's charge and result equal a
plain run's (`work_counters_attribute_every_unit_of_charge_once`).
`unattributed_heap_insertions` are objects the machine builds between frames:
a list `join` decided as its last member ends. A spawned task's work is its
own: a frame's inclusive charge covers only its task, not the tasks it
spawned. Operations count dispatches, not machine instructions; extra work
inside one operation changes no count. Use the CPU profiles for that.

## Sampled guest-context profile

```sh
(ulimit -v 8388608; "$B" guest "$E/guest" image_host_environment heap_comprehension_build language_host_environment)
```

Each program runs over and over with its machine publishing a guest context:
the running task, its frames as code and statement tokens, and the phase
(`interpret`, `native`, `formula`, `collect`, `park` or inactive). A harness
thread reads it at jittered intervals uniform in 5 to 15 ms (about 100 Hz,
seeded), until `KERNEL_PERF_GUEST_SAMPLES` (default 1000) samples of a running
machine are accepted or `KERNEL_PERF_GUEST_SECONDS` (default 60) pass. A
sample is where the machine was when the timer fired: **wall residency, not
CPU time**, including descheduling. Outputs:

- `guest.folded`: Inferno folded stacks, `program;outermost;...;innermost
  count`, ready for `inferno-flamegraph`. A native, formula or collection is
  the innermost frame (`native:<name>`, `formula:<name>`, `[collect]`);
  `[machine]` is the machine's own work with no frame, as when a task has
  just ended. The counts add up to the accepted samples.
- `guest-symbols.tsv`: per program and frame, self samples (innermost),
  inclusive samples (anywhere in the stack, once per sample), their shares
  of the accepted samples, kind, `FunctionId` and site.
- `guest-statements.tsv`: interpreted samples by the innermost frame's
  statement site.
- `guest-completeness.tsv`: per program the seconds and runs sampled and each
  tick's outcome. The outcomes are exclusive, `ticks = accepted + inactive +
  parked + lost_to_races + missed + unknown_phase`, and `accepted = interpret
  + native + formula + collect`; the command refuses a profile whose counts do
  not add up. `races` counts refused reads, a tick retrying up to 16 times
  before it is `lost_to_races`; `missed` counts deadlines that passed while the
  sampler ran late; `truncated` counts samples deeper than the context's 512
  frames (a capacity, not a guest bound); `unresolved` counts samples with a
  token the symbol table does not name. `population` is `insufficient` with
  zero accepted samples, otherwise `diagnostic_wall_residency`. Arithmetic
  completeness (`adds_up`) does not establish sampling sufficiency. An empty
  accepted population retains the raw files and exits nonzero; no shares are
  emitted. Nonempty populations remain diagnostic, with no statistical
  sufficiency claim.

The context is a preallocated block of atomics. The machine changes the
frame stack under a sequence lock and a read keeps only a whole one
(`a_concurrent_sampler_keeps_only_whole_contexts`); the phase and the
innermost statement are single stores, so a sample's statement may be one
the frame reached during the read. Each task has its own stack: spawn and
join are not call ancestry. One machine writes a context at a time; a
dropped machine leaves it inactive. The sampler lives in this harness only:
no worker or host drive publishes a context. Mixed Rust and guest stacks are
not produced; locate a hot helper's Rust work with the
[CPU profiles](cpu-profiles.md).

## Overhead

Measured 2026-10-10 on the shared 32-CPU Linux x86_64 host, on the three
gate rows, comparing the build before these instruments (main), the
instrumented build with nothing observing (off), and the same build with
`KERNEL_PERF_OBSERVE=work` or `=context` (the machine writes its context with
no sampler reading).

User-space instructions retired per iteration (one `start` and one run),
from `perf stat -e instructions:u` of `KERNEL_PERF_FIXED=1` runs of 100
iterations less 50; repeated three times, the spread was under 0.05%:

| row | main | off | work | context |
|---|---|---|---|---|
| baseline | 5,937,859 | 5,955,686 (+0.3%) | 6,455,075 (+8.4% over off) | 6,160,890 (+3.4%) |
| image_host_environment | 1,946,824 | 1,981,161 (+1.8%) | 2,501,523 (+26.3%) | 2,197,282 (+10.9%) |
| heap_comprehension_build | 49,070,980 | 50,491,386 (+2.9%) | 73,589,882 (+45.7%) | 59,547,121 (+17.9%) |

Wall time of the run alone (`ns_run`, mean of the timed iterations),
minimum of three paired rounds, each round running all four configurations
back to back (`KERNEL_PERF_SPLIT=0 "$B" 2000 <rows>`), at load averages 11
to 13:

| row | main ns | off ns | work ns | context ns |
|---|---|---|---|---|
| baseline | 149,428 | 150,756 | 198,141 (+31.4% over off) | 172,848 (+14.7%) |
| image_host_environment | 127,300 | 122,948 | 175,856 (+43.0%) | 152,398 (+24.0%) |
| heap_comprehension_build | 4,079,431 | 4,028,185 | 5,901,438 (+46.5%) | 5,248,556 (+30.3%) |

The off path is one branch at each observation point; its cost is the
instruction column's off-over-main share, and it is within the wall-time
noise of this host. Peak RSS was 54.0 to 55.5 MiB in every configuration.
These are diagnostic measurements of a loaded shared host, not calibrated
overheads.

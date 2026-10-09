# LashVm

```sh
python3 scripts/profile_lash_vm.py --report-only --scenario baseline --mode one_shot \
  --iterations 1 --profile-scenario baseline --profile-iterations 1 \
  --build-report "$E/lash-vm-build.json" --out "$E/lash_vm.json"
```

The script resolves `perf`, `profile` and `function_perf` independently from
one build report, rather than guessing `target/release/examples` paths.
The perf example reports allocation operations and requested-byte increments
within each counter-reset window (growing reallocations contribute their byte
increment); it emits no live-heap peak. Pre-window objects can be freed during
execution, so reset counters cannot establish a new-window live peak without
tracking allocation membership. Use DHAT for live-at-peak and live-at-end
quantities rather than deriving them from these counters.

The VM script certifies by default: any failed selected allocation, time,
scaling or opcode budget exits 1. `--enforce-budgets` selects that mode explicitly;
`just perf-guard`, release and manual perf workflows use it. `--report-only`
exits successfully after measurement and labels both output and receipt as
noncertifying. Skipped populations are not selected; a certifying invocation
must select at least one. Scaling ratios bind only when their mode and both
scenarios are selected, and missing measurements within that selection fail.

The profile example emits `vm_instructions_total` before the top-12 display.
This is the full count of executed VM opcodes in the profile subprocess's
selected execution window, not native CPU instructions. `instructions_per_iter`
divides that total by reported iterations. Standard sweeps profile each scenario
separately. **FIXED-SCENARIO-OPCODE-WORK** pins the full opcode work of one fixed
benchmark scenario from its seeded state, with zero padding. The 28 scenario
ceilings in `scripts/perf_guard_budgets.json` came from one execution of each
unchanged fixture; the baseline ceiling is 126. Hotspot timing/ranking never
contributes to this count. A fixture or bytecode change requires a reasoned
update to its named work invariant, not a shared default ceiling.

The standard runtime phase inventory includes `context_transform` and
`plugin_hook.context_pressure.standard_compaction`, required in the full geometry
of five measured runs, one warmup and twelve turns. **NESTED-PREPARATION** gives
each the existing configured `prepared_turn` upper bound of 108.842 ms: both
phases occur inside `RuntimeTurnServices::prepare`'s prepared-turn span. These
are advisory bounds inherited from the enclosing phase, not new measured timing
baselines. Existing allocation ceilings stay enforced.

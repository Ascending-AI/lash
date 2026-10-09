# Runtime, stack and allocation profiles

```sh
E="$PWD/.kiln/<ticket>"
mkdir -p "$E"
python3 scripts/profile_runtime.py --release --profile quick \
  --scenario standard --runs 1 --warmups 0 --turns 1 \
  --build-report "$E/runtime-build.json" --out "$E/runtime.json"
python3 scripts/profile_runtime_stack.py --release --scenario standard \
  --stack-bytes 2097152 --runs 1 --turns 1 \
  --no-build --build-report "$E/runtime-build.json" --out "$E/stack.json"
python3 scripts/profile_runtime.py --release --profile quick --dhat \
  --scenario standard --runs 1 --warmups 0 --turns 1 \
  --build-report "$E/allocation-build.json" --out "$E/allocation.json" \
  --dhat-out "$E/allocation.dhat.json"
```

The allocation recipe selects the inventory's `dhat-heap` feature target and
uses the symbolized profiling platform. The default runtime instrument retains
its stats_alloc counters. Allocation profiles cover the measured window.
`perfreport` displays each DHAT allocation site's `gb` as
`peak_live(at_tgmax)`: requested bytes live at the profiled process's global
heap peak. Its `eb` is `end_live`: requested bytes still live when the profiler
ends. Site `mb` is that site's own maximum; `max_live(sum-of-pps)` sums those
independent maxima and is not a simultaneous global peak.

Stack receipts use `configured_stack_capacity_bytes` and
`configured_stack_capacity_source`. The source identifies the explicit Tokio
worker reservation, `RUST_MIN_STACK` thread default, or process `RLIMIT_STACK`
soft limit, in priority order. `stack_budget_bytes` is a configured constraint,
not observed usage. There is no capacity-versus-budget usage verdict. These
settings do not bound every thread or child; constrained-execution crash laws
and the stack sweep establish survival at a configured size, not occupancy.

The scheduler's `runtime.global_queue_depth_endpoint_max_tasks` is the maximum
queued-task count at the two scenario-window endpoints in the executing Tokio
runtime. It misses intervening peaks. Process CPU covers every thread in the
parent process during that window; worker busy/park metrics sum worker deltas.

Every runtime-perf scenario is a `service-diagnostic`. The high-traffic
scenarios run a closed loop per session, and `high_traffic_knee_sqlite` steps
the session population and reports p95 service-latency growth: it is not an
offered-load knee. The former `--runtime-perf-load-arrival-rate` pacing is
gone; scheduled arrivals belong to `lash-perf offered-load`.

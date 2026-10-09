# Current performance instruments

Run from an isolated Kiln fork after `. ./env.sh`. Profiling scripts build
through Kiln, materialize final outputs, and resolve executables from the
output report. `--no-build --build-report <report>` consumes that report;
there is no Cargo target-directory fallback. Runtime and stack scripts also
accept an explicit prebuilt `--binary`. Keep reports and logs under the
fork's `.kiln/<ticket>/` directory, and run executables in place.

These reduced runs establish instrument functionality on the current source.
They do not establish timings, a noise floor, or a regression baseline. Use a
quiet host and the full workload for performance comparisons. The
committed historical baseline and substrate-comparison documents retain past
results; their old launchers have been retired.

## Runtime, stack and allocation profiles

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

## Lashlang

```sh
python3 scripts/profile_lashlang.py --scenario baseline --mode one_shot \
  --iterations 1 --profile-scenario baseline --profile-iterations 1 \
  --build-report "$E/lashlang-build.json" --out "$E/lashlang.json"
```

The script resolves `perf`, `profile` and `function_perf` independently from
one build report, rather than guessing `target/release/examples` paths.

## Send-to-completion latency

```sh
LASH_LATENCY_ARTIFACT_DIR="$E/cross-worker" \
  just latency-gate --cases cross-worker --lanes 1 --scale-down
LASH_LATENCY_ARTIFACT_DIR="$E/poll" \
  just latency-gate --cases poll --lanes 1 --scale-down
LASH_LATENCY_ARTIFACT_DIR="$E/grace" \
  just latency-gate --cases grace --lanes 1 --scale-down
```

Each remote case launches a child durable node over one shared SQLite file.
The submitting core and marker observer serve no sessions. Child startup has
an explicit readiness handshake; closing its stdin requests node shutdown,
with a bounded kill-and-reap fallback. A child is killed if the case exits early.
The host submits only through `send()` and follows the persisted answer.

`cross-worker` uses production observer pacing. `poll` delays the terminal
wait's poll so the normal follow poll observes settlement. The historical
`grace` case extends the follow-poll ceiling to 120 seconds while preserving
the 25 ms floor used by binding discovery, alongside the remote terminal wait.
The current follower has no shift attach or live-report grace.
Both delayed paths still perform their first store read immediately. The
report's `follower` field names the current mode, and `poll_detect_ms` remains
a simulated 25 ms to 1 s schedule, not another measured clock.

These diagnostic runs retain completed samples but exit 2 because the fast
case did not run. This is functional proof, not a passing latency gate. The default
`just latency-gate` still runs every case and requires 10,000 fast samples,
overhead p50 below 50 ms and p99 below 250 ms on a qualified quiet host.

## Symbolized CPU sampling

The opt-in `//tools/buck2:profiling` target platform compiles first-party and
third-party runtime code at opt-level 3 with line tables, retained symbols and
frame pointers. It uses the existing optimized compile budgets and all existing
pool properties; normal and optimized configurations retain their behavior.

```sh
kiln build //crates/lash-perf:lash-perf__bin \
  --target-platforms //tools/buck2:profiling -c kiln.rust_profile=optimized \
  --materializations final --build-report "$E/cpu-build.json"
B="$(python3 tools/buck2/outputs.py --report "$E/cpu-build.json" \
  --label //crates/lash-perf:lash-perf__bin --single)"
perf record -g --call-graph fp -o "$E/perf.data" -- "$B" \
  --runtime-perf-scenario standard --runtime-perf-runs 1 \
  --runtime-perf-warmups 0 --runtime-perf-turns 1 --runtime-perf-smoke
perf report --stdio -i "$E/perf.data"
```

`--cpu-profile` selects this platform in any of the three profiling scripts.
Sampling requires host perf permissions and belongs on a quiet host. Building
and running a symbolized executable is functional proof; no CPU optimization
claim follows from a smoke receipt.

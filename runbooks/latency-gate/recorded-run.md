# FIG-5989 — recorded latency operator rehearsal

One bounded deterministic SQLite-file rehearsal on 2026-10-12, cost **$0**,
source `6cdb45a4e75c09b0b2fa2900db4cf48b5951e5a6` with this lane's documentation
and matrix edits (`baseline.dirty=true`). No quiet window was allocated to
this lane: orchestrator message 264615 reserves qualification for FIG-3843's
planned window before FIG-5193. This run does **not** certify release latency.

```sh
LASH_LATENCY_ARTIFACT_DIR="$PWD/target/latency-operator-rehearsal" \
  bash -c 'ulimit -v 8388608; /usr/bin/time -v -o .kiln/FIG-5989/latency-rss.txt kiln gate lash rb-new-python-latency -- just latency-gate --cases fast --fast-samples 1050 --lanes 1'
```

The optimized build succeeded. Binary SHA-256:
`96df0384e73f526d7d2b7d037d6f8b16d8f48492301e85e91da4b4ae9d875b20`.
Baseline: `same-run-provider-duration`, same source SHA. Artifact root:
`.kiln/FIG-5989/run-root/latency-gate/`; stores remain under `target/`.

| Gate | Observed | Verdict/evidence |
| --- | --- | --- |
| Companions | Rust receipt 1/1; load laws 3/3; slope laws 2/2 | PASS, six executed/six passed |
| Initial quiet window | first load record 25.32, above 8; outside planned window | UNQUALIFIED for release |
| Automated core-count qualification | qualified=true, max load1=25.32 <32 cores; max CPU some avg10=1.59 | PASS for wrapper's narrower predicate; not quiet-window certification |
| Load coverage | fast window 46.049 s; 48 samples inside, including 46 interval samples | PASS for recorded coverage |
| Population/config | 1050 samples, actual one lane | FAIL for release: below 10000 and not the required 16-lane run |
| Budget measurements | overhead p50=39.578 ms, p99=74.568 ms | Observed below 50/250 ms; cannot certify with this population/configuration |
| Status/ledger | 1050 answered, non-answered=0; raw clocks and baseline retained | PASS for receipt integrity |
| Physical work | 5.41143 writes/send, writer busy=9.07043 ms/send, reader busy=4.02922 ms/send | Recorded diagnostics; reader sum is not elapsed latency |
| Slope | +0.0014 ms/turn, 95% CI [+0.0011,+0.0017], 1050 measured, none missing | Growth finding; no invented numerical threshold |
| Ordinal medians | 0–99: 0.935 ms (100); 500–599: 1.727 ms (100) | Populated/reviewed; retained as growth evidence |
| Exit/resource/cost | child=2, certification wrapper=2, just=2; max RSS=441224 KiB; $0 | Expected undersized-run refusal, cap respected |

**Overall: UNQUALIFIED rehearsal; release gate remains unproven.** The child
verdict is `pass=false`, with exactly one violation: `gated case fast ran
1050 samples (<10000)`. The automated load predicate is independently true;
the wrapper retains a failing child's status rather than replacing it with 3.
Neither the numeric budget measurements nor `qualified=true` turns this
rehearsal into a release pass. The positive slope is retained for the planned
quiet-window owner's review, not used to relax or reinterpret FIG-3843.

Receipts: `latency-report.json`, `latency-samples.json`, `latency-load.json`,
`latency-slope.txt`, `latency-gate.log`, `build-report.json`; outer status and
RSS in `.kiln/FIG-5989/latency-rehearsal.status` and `latency-rss.txt`.
No repeat measurement, PostgreSQL leg or paid provider call was made.

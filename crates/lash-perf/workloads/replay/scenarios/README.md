# Trace-shaped replay exploration

These synthetic sessions are an **explore suite**, outside the merge gate.
They are intentionally absent from `../budgets.json`, `../baseline.json` and
`CORPUS`. Pass their paths to the existing replay entry point. Every JSON file
in this directory is a `lash-perf-replay/1` workload; its `description` contains
its `provenance` note. No trace messages, commands, notebook code or identifiers
are copied into the workloads.

Run `bash crates/lash-perf/workloads/replay/scenarios/run.sh` from the fork.
The script builds the optimized harness and TypeScript worker through Kiln,
then runs each workload once on SQLite file stores, using the same `replay`
entry as the gate. The harness validates all scripts before executing any.
`LASH_SCENARIO_EVIDENCE` selects the receipt/log directory;
`LASH_SCENARIO_STORES` selects disposable stores. Fresh runs replace stores.
Both defaults are under `target/perf-replay-scenarios/`.

`measurements/distributions.json` contains measured source marginals, including
sample counts. `measurements/matrix.json` records the synthetic parameters.
`generate.py` rebuilds the workload JSON from those marginals, without network
access or raw traces. `measurements/results.json` contains the measured replay
summaries, main SHA and build identity. The research report explains sampling,
limitations and interpretation; it is recorded with FIG-6388's lane evidence.

## Sources and sampling

- [WildChat-1M](https://huggingface.co/datasets/allenai/WildChat-1M/tree/7d6490e462285cf85d91eabea0f9a954fbddcd1f),
  revision `7d6490e462285cf85d91eabea0f9a954fbddcd1f`, ODC-BY.
  500 distinct conversations: 100 rows at offsets 0, 1000, 10000, 30000, 60000.
- [SWE-agent trajectories](https://huggingface.co/datasets/nebius/SWE-agent-trajectories/tree/68195a1450865274106246d0d0296a1d6807b88e),
  revision `68195a1450865274106246d0d0296a1d6807b88e`, CC-BY-4.0
  (underlying repository and model-output terms also apply).
  128 trajectories from 128 issues: row `floor((i + .5) * 80036 / 128)`
  for `i = 0..127`. A preliminary clustered sample was discarded.
- [Jupyter agent dataset](https://huggingface.co/datasets/jupyter-agent/jupyter-agent-dataset/tree/10a8dd461bb6c580fa25b83cd3b2f6810f1d6385),
  revision `10a8dd461bb6c580fa25b83cd3b2f6810f1d6385`, Apache-2.0.
  First 100 `non_thinking` rows. The former `data-agents` URL redirects here.
- [tau-bench historical trajectories](https://github.com/sierra-research/tau-bench/tree/59a200c6d575d595120f1cb70fea53cef0632f6b/historical_trajectories),
  revision `59a200c6d575d595120f1cb70fea53cef0632f6b`, MIT.
  All 660 runs in `gpt-4o-airline.json` and `gpt-4o-retail.json`:
  four attempts each on 50 airline and 115 retail tasks.

All sources were public and ungated, accessed without credentials. Hugging Face
rows were read through its rows API; the saved response hashes and source
metadata pin the evidence. Sample percentiles use nearest rank, without
interpolation. These are exploratory samples, not population estimates with
confidence intervals. The Jupyter prefix and repeated tau attempts introduce
selection/dependence; combining their independent marginals does not reproduce
a real session's joint distribution.

## Adaptations

Small uses p10, typical uses p50 and heavy uses p99, except where stated:

| Family | Interpretation |
| --- | --- |
| `chat` | User-turn count and message byte sizes from WildChat. |
| `coding` | SWE action counts packed into sequential groups of 2 (small) or 4; a partial final turn preserves the action count. One final text reply adds a model call per Lash turn. |
| `analysis-ts`, `analysis-py` | Jupyter model-call counts become code-mode turns. Each turn retains another table chunk; state totals use printed dataframe cell counts × 8 bytes at p10/p50/**p90**. Tool results model backing file data; model-visible previews use the logged output-byte percentiles. |
| `workflow` | tau user-turn counts; 1/2/6 sequential asynchronous steps per turn, with real cell parks and resumes. Each wait is compressed to 5 ms. This measures parked cell continuations, not a custom registered workflow engine, multi-day scheduling or owner recovery. |
| `fanout` | tau session tool counts collapsed into one parallel batch. This is a counterfactual batching experiment: observed source batches have width one. |
| `reread` | SWE action counts, one stable synthetic file key and result across turns. Motivated by many actions against few distinct opened paths; actual reread frequencies are unobserved. |
| `output-burst` | SWE action counts, median-sized ordinary output and one final tail output (p90 small, p99 typical/heavy). Burst frequency and placement are synthetic. |

A code-mode setup turn initializes globals; it is included in session totals
and excluded from steady-turn medians/slopes by the existing `setup` field.
State is a growing synthetic string table, not pandas, NumPy or real files.
Eight bytes per printed cell is a sizing assumption, not measured heap usage.
Only 83 Jupyter sessions print parseable dimensions; the maximum matching
shape per session supplies those conditional percentiles. The p99 assumption
would be 3.29 GB and is outside this suite's state envelope. Distinct assigned
identifiers and dataframe bindings are syntax counts, not liveness measurements.
The retained values stay live at each await and across turns.

Chat/coding text adds a short turn ordinal to avoid identical-result deduplication.
Tool argument lengths include deterministic filler in `key`; fixture wrappers
and JSON overhead add bytes. Model output sizes in the native tool cases are
mapped to the final text reply; source reasoning/tool mixtures are not exact
byte-for-byte native response reproductions. The small analysis source can
exceed the source p10 because it must actually execute. Payloads preserve byte
shape rather than natural-language token entropy. Session token distributions
use fixed GPT-2 byte BPE (tokenizer revision
`607a30d783dfa663caf39e06633721c8d4cfcd7e`, MIT), including system text and
serialized tool calls, without chat-template framing; they are not the source
models' native usage counts.

## Reading the measurements

SQLite uses the replay harness's normal `synchronous=NORMAL` setting. Allocation
counts cover the host process only; IPC counts cover host/worker transport;
standard chat/native tool cases have no VM IPC. SQL rows are returned rows,
not bytes or database pages. Wall times exclude provider/network latency and
include workflow fixture waits. A turn's first worker initialization may be
charged in setup. These are one-run observations on a shared host, not latency
SLOs or statistically significant comparisons.

Session totals include setup and all turns; median/turn excludes setup.
Slopes use the harness's least-squares and Theil-Sen definitions. For growing
analysis state, depth and bytes covary, so the allocation-per-retained-byte
slope is descriptive, not a causal separation of history and state. One-turn
cases have no measured depth slope, even when the harness emits zero.

Retained-byte columns count the table string payload only; VM objects, metadata
and the latest chunk alias add overhead. `/usr/bin/time -v` reports maximum
per-process RSS, rather than the sum across concurrent host and worker processes.

# Cell request and outcome sharing (FIG-5925)

The landed sharing meets this ticket's outcome. At
`6b32e646b030838eb7dd6393058cb76a087a5097`, subsequent parks install no
additional large text for a repeated argument, and outcome commits install no
text leaves. No runtime, codec, schema, fixture, host API or test changes are
needed. The earlier amplification measurement predates archival call sharing.

The [machine-readable receipt](cell-value-sharing-2026-10-17.json) retains all
36 batches, actual installation bytes by commit label and node kind, verified
call/park counts, instruction counts, load and peak RSS. Raw evidence is archived
under the measuring fork's `.kiln/FIG-5925/`; the receipt includes SHA-256 hashes.

## Measurement

The existing `durable-substrate` cell workload ran on fresh SQLite files with
one production durable node and a TypeScript subprocess worker. The default
build has debug assertions enabled. Each of 18 populations ran two batches:
0/1,024/16,384-byte pads, repeated/fresh, and 1/3/10 awaited calls. Batch 0 is
cold; batch 1 reuses the same value population and is explicitly warm. Fresh
means distinct within a batch. An empty pad cannot be distinct.

Repeated pads are exactly P copies of `x`. Fresh nonempty pads are P−1 copies
of `x` followed by the single-digit await index. The temporary measurement
adapter uses `.slice(1) + i` to keep lengths equal. Each cell retains every
answer in `kept`. After settlement, a durable archive read verifies exactly N
ordered calls, exact `{i, pad}` arguments, and typed output equality with
`ToolCallOutput::success(args)`, including output metadata. All **168 calls and
168 parks** were verified across **36 completed batches**, with zero failed
populations. These are benchmark assertions, not libtest executions.

Temporary probes at the existing SQLite `durable/graph.rs` and
`durable/pages.rs` insertion sites record each node proved absent under the
writer lock. Joining their operation identities to accepted transactions in
`durable_substrate/recorder.rs` reconciles **every operation's** sum to the
maintained SQL receipt's `newly_installed_graph_bytes`. Thus the table counts
actual canonical envelope bytes inserted by successful commits, including
reinsertion after GC. It does not substitute staged bytes, first-seen NodeIds,
SQL bound bytes, file allocation or WAL growth. The temporary adapters and
probes are retained only as `measurement.patch` in the evidence archive.

## Result

Cold-batch installations in later `cell.snapshot+admit` commits are:

| Pad bytes | Parks | Repeated TextLeaf bytes | Fresh TextLeaf bytes |
| ---: | ---: | ---: | ---: |
| 0 | 1 | 0 | 0 |
| 0 | 3 | 0 | 0 |
| 0 | 10 | 0 | 0 |
| 1,024 | 1 | 0 | 0 |
| 1,024 | 3 | 0 | 2,070 |
| 1,024 | 10 | 0 | 9,315 |
| 16,384 | 1 | 0 | 0 |
| 16,384 | 3 | 0 | 32,794 |
| 16,384 | 10 | 0 | 147,573 |

Fresh text costs exactly one new leaf per later await: 1,035 or 16,397 bytes,
including its canonical envelope. The initial admission is separately labelled
`model.done+cell.snapshot+admit`; it includes source, document and initial heap
material. Large arguments occupy a 198-byte FrozenRequest per cold admission,
and the archival CallBody occupies 192 bytes per call, for both 1 and 16 KiB
pads. The large value leaves are already shared. Across every batch,
`round.outcome` installs **zero TextLeaf bytes**. For ten calls its complete
cold installation is 6,800/6,830 bytes for repeated 1/16 KiB pads and
6,920/6,950 bytes for fresh pads, including outcomes, facts, presentation
recipes and archival bodies.

Whole cold-batch bytes for ten calls are 1,463,476 / 1,479,104 / 1,575,284
for repeated 0/1/16 KiB pads; fresh totals are
1,464,304 / 1,491,903 / 1,777,948. Whole totals also include helper/document
material, semantic heap pages, model request/answer records, exact source and
prompt spellings, and structural nodes. The receipt attributes those costs
separately. Warm rows retain the effects of GC and reuse rather than pretending
that a process-wide first-seen set measures installations.

`GraphStaging` places large text tokens in shared leaves even when `args.encode`
is called inside a new body. `CallBody::stage_fields` shares success values in
FrozenValue children. `stage_payload` recognizes exact canonical completed
calls; its exact-text fallback preserves other payload spellings. The observed
small request/body nodes and absence of new outcome text leave no reproduced
request/outcome duplication to repair. Required exact source/model renderings
remain represented; this receipt does not claim they are free.

## Proof and limits

All 18 processes succeeded under an 8 GiB virtual-memory cap. Paired
`perf stat -e instructions:u` samples, including workers and observation, are
retained for every repeated/fresh population. Load1 endpoints ranged from
16.56 to 19.72; maximum peak RSS was 162,180 KiB. These single-revision diagnostic
samples include setup and both batches; they establish no optimization speedup
or quiet-host wall time.

Workspace `kiln clippy` passed. There are no new or changed tests, no bug red
side, and **zero libtest cases executed**. No runtime changes are retained, so
values, typed failures, effect/failure order and parks keep their existing code
paths. The echo population checks successful outputs and parks; it does not
exercise injected failures. No stored shape or export changed, so schema,
fixture re-recording and facade gates do not apply. PostgreSQL and quiet-host
wall time were not measured; this closes the byte-sharing question on the
cheapest tier and makes no backend timing claim.

## Reproduction

At the pinned revision, apply archived `measurement.patch`, source `env.sh`,
and build `//crates/lash-perf:durable-substrate__bin` and
`//crates/lash-vm-worker:lash-vm-worker__bin` through `kiln build`, with final
materialization and build report
`.kiln/FIG-5925/measurement-build-verified.json`. Run archived `measure.py`
under `(ulimit -v 8388608; python3 .kiln/FIG-5925/measure.py)`, then run
`python3 .kiln/FIG-5925/analyze.py`. The runner records the exact commands and
resource results; the analyzer refuses mismatched installation sums, missing
calls or missing parks. Restore the temporary probes after measurement.

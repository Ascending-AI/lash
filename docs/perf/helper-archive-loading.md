# Exact helper archive loading

FIG-7512 installs the authenticated union of current and archived helper code
for enabled packages. Source resolution keeps current roots. Exact native
identities bind their retained Rust implementation and callee environment;
archived declarations retain their own charge formulas. Plans select exact
bodies, with an interpreter fallback for other bodies. `restore_selection`
remains the authoritative availability walk.

Claim requirements are cached by document/run and claim-index identity. They
hold the maximum first generation for each archive lineage, with exact hashes
only for code outside the archive. Nodes advertise generation and release
coverage. Historical library aliases preserve explicit library requirements.
The final lease representation shares immutable served tokens, their prepared
JSON, and capability data across clones. Claims reuse that encoding.

Worker derived libraries retain at most 16 entries keyed by authored closure
and exact helper membership. Decoded stored ordinary definitions retain at
most 4,096 entries. Immutable registration facts are shared where available;
editing a frame-backed registry can materialize its registrations. Machine
entry sites retain only activated bodies. Resident collection cursors preview
one step through one dispatch and publish it after the existing admission
checks. Regex engines retain at most 256 patterns each. These are entry bounds;
bytes still depend on supported history, admitted code and pattern sizes.

## Host replay fix forward

FIG-7512's requested one-pair control used `317ed3c0ad^` (`d9fab3f459`)
and `8d2974c088`. Python host instructions rose from 86,469,071 to
87,844,099 (+1.590%); TypeScript rose from 124,348,908 to 125,402,343
(+0.847%). Workload digests and lengths matched.

The requested FIG-7519 pair, `8f905ce03c^` (`42f9c96f8a`) against
`8f905ce03c`, measured Python at 89,622,089 and 89,325,595, and TypeScript
at 127,092,728 and 127,282,761. The second leg reuses the existing exact-base
control. These two commits differ by automatic format witnesses; both contain
the FIG-7519 implementation. This pair does not isolate that implementation's
cost.

Paired instruction and allocation profiles exposed copied claim debt and
repeated JSON tree construction when publishing consumption receipts. Helper
debt now shares immutable maps, and resolved requirements share a published
snapshot. Protocol payload serialization streams its stored JSON without
building a temporary tree. Exact reach checks and `restore_selection` remain.

The remaining dominant cost was SQLite expanding streamed candidate node
bodies into SQL text for accounting, then parsing that text back into byte
counts. The ordinary graph installer already supplied known blob lengths;
the streamed installer bypassed that path. Both installers now supply exact
blob and JSON text binding lengths. Quotes, Unicode and SQLite's existing
first-NUL spelling semantics keep identical accounting. SQL, parameters,
publication behavior and stored bytes are unchanged.

The final instruction gate ran once on optimized `6be6b5a9f6`, with the
unchanged shared baseline and 1% ceilings, under the 8 GiB virtual-memory cap.
Every host row passed:

| Host replay | Instructions/turn | Unchanged ceiling |
| --- | ---: | ---: |
| chat-short | 35,802,271.5 | 36,162,534.395 |
| chat-long-compacting | 39,657,603.5 | 40,102,463.595 |
| ts-codemode | 107,272,796 | 125,584,456.460 |
| py-codemode | 72,842,706 | 86,670,950.220 |

Median SQL bytes written stayed exactly 127,681 for Python and 149,768 for
TypeScript relative to the preceding failed candidate. No baseline, ceiling
or workload changed. The full gate passed 37/40 rows; the remaining Python
kernel property/array reads and Python worker replay belong to r36-perfbis.
No quiet-host latency claim follows from these counts.

Eleven focused laws passed across the compact debt, payload serialization,
CPU/consumption decision, SQLite rolling-node and publication/accounting
changes. The debt-sharing, payload-serialization and known-text accounting
laws each failed semantically before their fixes. Schema checks passed after
preserving the original flat helper schema and regenerating worker v3 schemas
for the already allocated protocol witness. Earlier failed candidates remain
in the evidence and are not passing proof. See
`hload-evidence/fix-forward/candidate-gate/` in the lane's external evidence
root for the final decision and raw receipts.

## Retained instruction evidence

The following observations use optimized source `546b658504` and exact parent
`13c6be0c63`, before the final shared lease advertisement and resident cursor
changes. Both legs used CPU 0, `perf stat -e instructions:u`, internal host and
worker counters, equal-length receipt paths, the same store path, and an 8 GiB
virtual-memory cap. The first retained pair is shown for each row. Earlier
repeat samples remain in the evidence; no further repetitions were run after
the orchestrator's pool-budget ruling.

| Replay, process | Parent instructions/turn | Measured candidate | Change |
| --- | ---: | ---: | ---: |
| chat-short, host | 35,285,201 | 35,327,750 | +0.121% |
| ts-codemode, host | 123,361,712 | 124,616,148 | +1.017% |
| ts-codemode, worker | 40,447,643 | 40,341,285 | -0.263% |
| py-codemode, host | 85,392,906 | 86,953,023 | +1.827% |
| py-codemode, worker | 13,612,736 | 13,690,172 | +0.569% |

The unchanged `just perf-gate-instructions --no-record` run on that source
passed 36 of 40 rows. Python property reads, array reads, and Python replay
host/worker rows failed the unchanged 1% gate. Matched parent controls reproduce
both kernel rows exactly and most of the Python worker excess. An older
`381c5f5c30` control has the same Python replay digest and falls below those
references. These main regressions belong to the triage lane.

The Python host control also exposed remaining per-claim deep lease copies
and JSON token copies. The final `ServedClaims` snapshot removes those copies
and encodes JSON once per advertisement. LEASE-ADVERTISEMENT-SHARING failed
semantically before that change and passes after it. The resident cursor
change preserves the forms and control-exit laws. Both changes passed Clippy
and schema checks. Their performance effect has not been remeasured: the
orchestrator instructed this lane to stop repeats and land the available work.
No final warm-gate pass is claimed. No ceiling, baseline or workload was edited
by this lane.

The original `142c0b2af9` control against exact parent `2c9d554782` isolated a
TypeScript host increase of 138.294%, while chat-short changed by +0.009%.
Per-helper claim tokens caused the TypeScript regression; archive generation
floors replace them. The earlier chat failure also occurred on parent.

## Worker installation measurement

Source `546b658504` ran six interleaved current/archive pairs. All 12 selected
body result guards passed. Both modes start from the archive-enabled shipped
image: current adds a 30-function chain, and archive retains another historical
30-function chain. The installation counter covers `WorkerLibrary::from_frame`
and embedding binding after the parent has assembled its image. Parent fixture
authentication and first execution are separate.

The table gives median (minimum-maximum), six observations per mode.

| Metric | Current chain | With historical chain |
| --- | ---: | ---: |
| Installed functions | 3,116.0 (3,116-3,116) | 3,146.0 (3,146-3,146) |
| Frame bytes | 7,340,047.0 (7,340,047-7,340,047) | 7,354,790.0 (7,354,790-7,354,790) |
| Installation instructions | 86,229,485.5 (86,227,631-86,231,393) | 86,627,850.0 (86,617,178-86,647,770) |
| Observed installation us | 15,510.0 (14,600-16,152) | 15,280.5 (14,868-21,160) |
| RSS delta KiB | 13,620.0 (13,460-13,652) | 13,680.0 (13,584-13,776) |

The extra chain adds 14,743 frame bytes. Its median paired installation
instruction increase is 0.463%, with a 0.449-0.485% range. Archived parent
fixture authentication has a separate median 5,627 us. RSS ranges overlap.
Observed microseconds are shared-host observations and establish no quiet-host
latency claim or constant byte bound for arbitrary history.

The two checked-in release deltas hold 2,913 definitions and 340 distinct native
declaration FunctionIds across six packages. Of those native IDs, 332 name
version 1 and eight name version 2. Release metadata repeats some native IDs;
they are counted once here. The release JSON files occupy 12,866,316 raw bytes
or 1,964,860 bytes with default Python gzip compression and zero timestamps.
Workers receive the binary frame and do not parse those JSON files.

## Proof and reset scope

The earlier required suite population remains 204/208 passed. Parent controls
reproduce the 1,055,259-byte first-turn allocation excess and the three
`park_matrix.rs:1462` fixture panics. They were left outside this lane per the
orchestrator's ruling. The original SQLite durable families passed 17/17.
Targeted archive, claim, identity, worker and restore laws passed; the added
entry-site and lease-sharing laws each failed on the unfixed representation
before passing. The facade checks passed 70/70 on the earlier merge. Subsequent
new-code claim, diagnostic, forms and control-exit checks passed, followed by
Clippy and schema checks. Zero-case selectors and a build-failed invocation
are retained but contribute no executed proof.

The [host-upgrade entry](../host-upgrades/unreleased/FIG-7512.md) identifies
each changed stored/wire shape and its reset action. Landing allocates the
format witnesses against its rebased parent. There is no migration, old-shape
reader, crate/package version bump or semantic kernel version bump.

Raw measurements, failed attempts, profiles and test receipts are retained in
`/workspace/notes/lash/tasks/lanes/r33/hload-evidence/` and linked from FIG-7512.

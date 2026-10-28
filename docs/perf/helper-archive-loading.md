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

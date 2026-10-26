# Cell memory evidence for FIG-7432

Captured on 2026-10-26 in the isolated `r33-cellmem` Kiln fork over base `2ec8e492978f`. This is measurement work; no runtime optimization or version change.

## Result

`just profile-cell-mem FILE [--worker] [--stack]` builds a development-only, optimized symbolized instrument through Kiln. It reports source/inline allocation stacks, ordinary allocations and reallocations separately, guest-context operation joins, the capacity ledger at its observed mid-cell peak and end, process RSS and allocator/mapping reconciliation, and painted original-thread stack watermarks. `--cells 2` proves collection after a clean reset in the same real worker PID.

All required cells completed through the actual confined worker entry and IPC/page-service path, with identical results and exact charge in the same-executable observer-off/on pair. The 200k cell also completed after reset in the same observed PID, 3880065. Python direct and worker smoke captures completed. No new tests were added.

## 200,000 records

Useful result in all four worker executions: 200000 records, last key 192081, exact charge 9200191.

| Cell | Off allocations | Off per record | On allocations | On per record | Off/on reallocations |
| --- | ---: | ---: | ---: | ---: | ---: |
| 1 | 36,747,049 | 183.735245 | 37,347,935 | 186.739675 | 1,500,893 / 1,500,902 |
| 2 | 36,693,618 | 183.468090 | 37,294,645 | 186.473225 | 1,493,245 / 1,493,254 |

The first off cell reproduces the audit's approximately 184 allocations per record. The active pass adds 600886 allocations, 3.00443 per record. On-pass site counts are diagnostic and are not assigned to production cost. Every on-pass allocation resolves to a Rust source line. The first table prints 24 sites covering 95.4291%; the second prints 23 covering 95.0222%. Complete source stacks remain in the raw profiles.

The ten largest ordinary allocation sites in the first active cell are below. Reallocations are excluded from this denominator. The top ten cover 80.78%; the complete text table extends beyond them to exceed 95%.

| Rust allocation site | Allocations | Allocations per record |
| --- | ---: | ---: |
| `{closure#0} ./crates/lash-kernel-state/src/paged/tree.rs:1523` | 8,999,896 | 44.999480 |
| `clone ./crates/lash-kernel-state/src/paged/tree.rs:1507` | 5,010,720 | 25.053600 |
| `default ./crates/lash-kernel-doc/src/heap.rs:145` | 4,780,688 | 23.903440 |
| `decode ./crates/lash-kernel-doc/src/graph_codec.rs:482` | 4,380,673 | 21.903365 |
| `inline_node<lash_kernel_state::paged::directory::Entry> ./crates/lash-kernel-state/src/paged/tree/inline.rs:77` | 2,190,322 | 10.951610 |
| `inline_node<lash_kernel_state::paged::directory::Ordinal> ./crates/lash-kernel-state/src/paged/tree/inline.rs:77` | 1,590,322 | 7.951610 |
| `tag ./crates/lash-kernel-value/src/tokens.rs:379` | 826,860 | 4.134300 |
| `normal_bytes ./crates/lash-kernel-state/src/paged/heap_impl.rs:1029` | 800,004 | 4.000020 |
| `decode<lash_kernel_state::paged::directory::Ordinal> ./crates/lash-kernel-state/src/paged/tree/inline.rs:326` | 795,161 | 3.975805 |
| `decode<lash_kernel_state::paged::directory::Entry> ./crates/lash-kernel-state/src/paged/tree/inline.rs:326` | 795,161 | 3.975805 |

The first cell's observed guest peak ledger is 25,697,744 bytes, with 155,198,806 live window allocation bytes at that instant. End live window heap is 156,995,121 bytes and RSS is 299,347,968 bytes.

## Retained 100 MB of strings

The result reports 100 strings with endpoint lengths 1000001 and 1000002. The arena body allocation site retains exactly 100000190 bytes, matching the source's programmed ASCII payload.

| Quantity | At observed guest peak | At end |
| --- | ---: | ---: |
| Guest capacity ledger | 202,115,696 | 102,102,128 |
| Live window heap | 210,046,824 | 110,079,543 |
| Process requested Rust bytes | 235,680,353 | 135,713,072 |
| Process RSS | 271,060,992 | 271,163,392 |
| Free glibc arena bytes | 634,160 | 100,558,624 |

Ledger components, resident value slots / control / shared layouts / sealed-floor excess: peak `[202018960, 94912, 1824, 0]`, end `[102006960, 94912, 256, 0]`. The meter is a capacity ledger rather than an allocation-site heap counter.

The end gap, RSS minus window live heap, is 161083849 bytes. Its signed reconciliation is:

| Component | Bytes |
| --- | ---: |
| Preexisting and observer requested bytes | 25,633,529 |
| glibc in-use minus Rust requested | 636,848 |
| Free allocator arenas | 100,558,624 |
| Anonymous residency minus allocator capacity | 2,641,920 |
| File-backed RSS, executable and libraries included | 23,224,320 |
| Painted native stack RSS | 8,388,608 |

The remaining anonymous term is explicit. It combines nonallocator anonymous mappings and nonresident allocator capacity; these observations do not identify those separately. The guest-peak RSS gap is 61014168 bytes, split into 25633529 preexisting/observer requested bytes, 594031 glibc in-use-minus-requested bytes, 634160 free-arena bytes and 34152448 mapping/residency remainder. `clear_refs=5` succeeded for the cell-window RSS high-water reset.

## Native stack occupancy

| Cell | PID/TID | Painted written-stack high-water | Initial unpainted margin |
| --- | ---: | ---: | ---: |
| 200k, first | 3880065 | 111,096 B | 52,064 B |
| 200k, after reset | 3880065 | 99,688 B | 52,064 B |
| Depth 1000 | 3971773 | 109,544 B | 50,512 B |

All scans were unsaturated. These are written-byte watermarks on the original native worker thread, including observer frames. Paint commits stack pages. Separately reserved parser threads and alternate mappings are outside this window, and no sampler captured a causal Rust stack at the maximum. Guest recursion depth is not interpreted as native recursion.

## Observer overhead

Ordered whole-process `perf stat -e instructions:u` pairs use the same executable, allocator and useful cell work. The scope includes startup, waited workers, observation, source resolution and output. These ratios are diagnostic observer cost, not production costs. Shared-host wall time is unclaimed.

| Population | Off user instructions | On user instructions | On/off |
| --- | ---: | ---: | ---: |
| build200k | 97,882,859,549 | 5,312,731,008,860 | 54.276418 |
| retained100mb | 5,477,839,261 | 26,122,429,304 | 4.768747 |
| recursion1000 | 1,069,432,929 | 11,735,327,060 | 10.973411 |
| python-direct | 902,805,413 | 6,289,683,267 | 6.966821 |
| python-worker | 955,887,797 | 7,175,474,981 | 7.506608 |

Measured executable SHA-256: `fed23f9e709abc2c703ff0568273d1d3d99c4be8989c70911feb431ee5fb0ff8`. The second cell's baseline includes retained resolver caches from the first capture; each profile discloses its own process baseline.

## Scope and retained artifacts

- Capacity peaks are observed at machine statement verdict points. Native transient values can peak between these points; the report does not call them an exact byte-by-byte ledger peak.
- Native unwinds retain up to 32 frames and resolve inline frames offline at window close. Guest joins use the existing bounded published context, so contexts deeper than its retained capacity can be absent.
- Site counters cover the calling thread's allocations after window start. Preexisting objects, observer storage and allocations bypassing the Rust allocator remain separate from window live bytes.
- Raw `*.json`, counter CSVs, command logs and final `*.read.txt` tables are under `/workspace/notes/lash/tasks/lanes/r33/cellmem.evidence/`. The final offline reader separates allocation and reallocation counts; the original build200k launcher output predates that presentation correction. The raw captures and off/on measurement are unchanged.
- Build report: `/workspace/notes/lash/tasks/lanes/r33/cellmem-build.json`. Source identity: `/workspace/notes/lash/tasks/lanes/r33/cellmem.evidence/source-identity.json`.

## Validation and landing

Optimized symbolized Kiln build, targeted compiler checks and schema checks passed. No added/changed tests. The final workspace Clippy receipt and landed SHA are recorded in the FIG-7432 ticket comment and `/workspace/notes/lash/tasks/lanes/r33/cellmem-land.log`.

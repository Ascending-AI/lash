# VM value layout

`lash_kernel_doc::heap::PageValue` is the projection of a running arena
value into a saved tree row or page. A compile-time assertion in
`crates/lash-kernel-doc/src/heap.rs` pins it at 56 bytes.

Text descriptors use `TextPayload`. Fresh text shares an `Arc<str>`;
decoded resident text retains a range of its token page through a shared
owner, so decoding a field name allocates no string. All ranges from one
page share that owner. Paged text retains an `Arc<PageRef>`. The descriptor
keeps the 56-byte `PageValue` layout and the canonical text tokens are
unchanged. A retained range can keep its bounded token page alive.

## The layout

Before FIG-6502 a `Value` was 144 bytes. A `Payload::Paged` held its
80-byte `PageRef` (a 32-byte `PageId` and a 48-byte `PageGrant`) inline.
Every `Payload` was 88 bytes, even when it was resident and held a 16-byte
`Arc`. A `TextRef` was 120 bytes. `IntRef::Paged` repeated the same inline
`PageRef`, which made it 104 bytes. `TupleRef` was the widest variant: 144
bytes, of which 88 were its payload.

The cold parts are now shared:

| Type | Before | After | What changed |
| --- | ---: | ---: | --- |
| `Payload<T>` | 88 | 16 | `Paged(Arc<PageRef>)`: one shared page reference |
| `TextRef` | 120 | 48 | through `Payload` |
| `BytesRef` | 104 | 32 | through `Payload` |
| `IntRef` | 104 | 32 | `Paged { digits: Arc<PageRef>, .. }` |
| `Value::Tuple` | 144 | 8 | `Tuple(Arc<TupleRef>)` |
| `Value` | 144 | 56 | widest variant is now `Text`/`Function` |

A paged payload is cold: a value reaches it only after a seal or a page
read. Allocating its page reference once, behind an `Arc`, costs one
allocation per paged descriptor. A copy then costs a reference-count
increment where it used to copy 80 bytes. Resident payloads, which are
nearly every value a cell builds, are unchanged: they were already one
`Arc`. A tuple's descriptor (summary, family and normal-form cache) is
shared the same way. A `for` over a tuple already kept it in an `Arc`
(`Cursor::Tuple`). It now shares the value's own descriptor and no longer
clones it.

Grants are still never persisted. The page encoding writes a paged payload
as its bare `PageId`, exactly as before. No stored shape, encoding, native
identity or charge changes. Reading a page still grants each child
(`directory::value_grants`): `Arc::make_mut` updates the freshly decoded,
unshared reference in place.

### Alternatives not taken

- **Stable small handles for call operands.** A per-activation table of
  values addressed by `u32` would make operands 4 bytes. It would also
  change what a native receives: today a native gets `&[Value]` and keeps,
  clones or returns any of them. Handles would need a lifetime and release
  protocol at the native boundary, plus a table the census traces. That
  narrows what plugins can do cheaply. The ruling (2026-10-16) was
  "efficiency without limiting plugins", so this is out.
- **Boxing every descriptor** (`Text(Arc<TextRef>)` and the like) would
  give a 24-byte value. It would also allocate twice for every resident
  text a cell builds: the `Arc<str>` and the descriptor. Resident texts are
  the hot case, and the descriptor's summary is read on every length and
  key operation.
- **Narrower summaries** (`u32` lengths) would cap texts at 4 GiB inside
  the kernel's own metric. That would mean a semantic limit to save 12
  bytes.

## What natives and heap plugins build and read

The public vocabulary is unchanged except for the two shared pointers:

- Build a resident payload as before: `Payload::Resident(Arc::from(..))`,
  or `Value::text(..)`.
- Build a paged one with `Payload::paged(page_ref)`. That is the
  constructor; `Payload::Paged(arc)` still pattern-matches and derefs to
  the `PageRef`.
- `IntRef::Paged { digits, .. }` holds `Arc<PageRef>`. Read the page with
  `**digits` or `digits.id`.
- `Value::Tuple` holds `Arc<TupleRef>`. `NativeHeap::tuple` returns
  `Arc<TupleRef>`, so `heap.tuple(members).map(Value::Tuple)` is unchanged.
  Fields read through the `Arc` (`tuple.summary.len`). A plugin that edits a
  descriptor uses `Arc::make_mut`.

Every in-tree native and heap implementation (`PagedHeap`, the VM's call
heap, the release probe's recording heap, the JSON lab heap) uses these
forms.

## Measurements

These are debug-geometry builds (first-party `opt-level=0`, the test
profile the laws run in), on an AMD Ryzen 9 5950X worker under an 8 GiB
address-space cap. Instruction counts are the stable comparison. The shared
host ran at a load average of 40–55 on 32 threads, so cycles and wall time
are advisory. Evidence and scripts are under `.kiln/FIG-6502/` in the lane
record.

Callgrind instructions inside `KernelMachine::run_slice`:

| Workload | Before | After | Change | `memcpy` before → after |
| --- | ---: | ---: | ---: | ---: |
| Interpreted JSON walk (`kernel_perf host-json generic`) | 493,056,776 | 461,485,869 | -6.4% | 58.96M → 28.18M |
| URI prefix (16 iterations of S15.1.3.2_A1.12_T1's body) | 136,269,234 | 126,123,496 | -7.4% | 18.72M → 8.77M |
| Scalar loop, 12,000 iterations | 4,365,472,161 | 4,055,450,296 | -7.1% | 526.07M → 254.26M |

Charges are identical: 209,951; 22,532; 576,164.

Whole-process counters (`perf stat`, user):

| Workload | Before | After |
| --- | --- | --- |
| URI `selection::shard_33` | 173.01B instr, 243.6B cycles, 64.0 s | 163.33B instr, 246.4B cycles, 64.9 s |
| ID_Start `selection::shard_27` | 380.45B instr, 590.8B cycles, 156.8 s | 363.92B instr, 564.9B cycles, 148.6 s |
| Scalar loop, 120,000 iterations, two interleaved pairs | 49.26B / 49.31B instr; 5.13 s / 4.44 s timed run | 46.32B / 46.27B instr; 5.15 s / 5.42 s timed run |
| SQLite post-tool nested cell, three interleaved runs | 243.0, 206.8, 171.4 ms | 278.6, 187.1, 200.5 ms |

Both selections keep their recorded outcomes: URI ends at its
45,000,000 charge bound, and ID_Start ends with
TS_LONE_SURROGATE_UNSUPPORTED.

## Where the rest of the cost is

Value moves were the largest single self cost before this change: 12–14%
of instructions. They are now 6–7% of instructions and about 4% of sampled
cycles. The pre-heap 1.5× limits (URI 14.59 s, ID_Start 22.40 s) are 4–7×
below what this host measured. The gap is in the interpreter and in heap
persistence, not in the value representation. Sampled self cycles after the
change:

| Family | URI | ID_Start |
| --- | ---: | ---: |
| Interpreter dispatch (`lash_kernel_vm::machine`) | 22.7% | 13.6% |
| Paged tree and residency tracking (`paged::tree`) | 14.0% | 22.7% |
| Token encoding (`graph_tokens`, `TokenCodec`) | 5.7% | 6.3% |
| Frames | 5.1% | 3.7% |
| Allocator | 5.0% | 8.3% |
| Task lookup (`KernelMachine::task`, the task cache scan) | 4.6% | 3.3% |
| Census, publication and statement holds | 4.3% | 2.9% |
| `memcpy` | 4.1% | 4.1% |
| Debug-build UB and precondition checks | 3.1% | 4.3% |

The URI prefix shows where the time goes, inclusive. Each library call to a
helper body runs `push_frame` (17.4%), which builds a fresh `FrameRecord`
and `SiteRef` and boxes a `Frame`. `leave_frame` takes 16.0%,
`KernelMachine::task` 12.2% (looked up on every step), the terminal
carry/seal 7.9%, `bind` 6.9% and statement `hold` 6.6%.

The scalar loop's cost is session bindings. Each top-level assignment
calls `session_write` (36.5% inclusive), which clones the session root and
writes the binding through `Tree<BindingRecord>::write`. The shared node is
copied by `make_dirty` (12.6%) and the record is re-encoded to size it.
`session_get` takes another 9.1%.

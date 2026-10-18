# Kernel end state: the seams

Status: stage-1 seam design for the kernel end state (FIG-6691), on main at
`40bbde07e7`. It pins the contracts the stage-2 storage swaps and the stage-3
instruction-set and helper lanes build against, so they can proceed in
parallel. The direction is the kend spec v2
(`/workspace/notes/lash/tasks/lanes/kend/spec.md`): the machine executes on
plain memory and projects to the paged heap only at projection points (park,
slice return, end). Its §2 contracts are fixed here. This supersedes the
execution half of
[ADR 0145](../adr/0145-kernel-execution-and-its-checkpoints-share-one-persistent-heap.md)
("the machine executes on that heap"); the checkpoint half stands: a park seals
the changed pages and writes one root.

Rulings this design implements:

- **SAM-1** (2026-10-17): storage is swapped in place under the existing
  executor, one seam at a time, with no second engine. Each swap deletes the
  path it replaces in the same change: no dual paths, flags, adapters or
  shims.
- **SAM-2**: durable ids are assigned at the projection point (§C).
- **SAM-3**: the native ABI is whatever is truly optimal, decided on evidence
  (§F: typed views over call-scoped borrows, measured).
- **SAM-4**: the memory counter is rebased at every seal, plus statement kill
  sets (§I, §E).
- **FIG-6693 rulings** (2026-10-17): every library helper is charged its
  declared formula at completion (hcharge, FIG-6694, §B), and helper bodies
  compile to Rust at build time as kernel-owned execution plans (hgen,
  FIG-6678), with resumable segments for callback-capable helpers (§J).

kdec-tests, kdec-ktests and kend-harness (FIG-6689, FIG-6690, FIG-6692) are
on main. Two sibling lanes were not yet on main when this was written, and it
is built against their declared shapes: kdec-api (FIG-6688: start/open
factory, `checkpoint()`, `MachineOptions` with an observer,
`CellSaveContext`, `inspect_checkpoint`) and FIG-6672 (rl-rewiden: the
operation memory reservation and `FrameRecord.registers` deleted).

## 1. Rules every lane keeps

- **The §2 contracts of the spec.** Effect identity (K-EFF-008), exact charge
  (K-CHG-001..008), PLAN-INDEP, the equality domain, resume transparency
  (K-MACH-008), incremental park and lazy resume, layout-independent saves,
  atomic publication, one kernel. Memory is a conservative bound (§I).
- **The equality domain does not move.** Canonical continuation bytes are
  exactly equal and the persisted graph is equivalent (allocator headers,
  garbage and census metadata may differ). A swap that makes more of the graph
  byte-equal (§H) still states its laws over this domain.
- **The pinned types change only by an orchestrator ruling.** They are listed
  in §2. A lane that needs a change asks; the ruled change lands first and
  every lane rebases onto it.
- **Wholehog.** Every brief in §4 lists what its lane deletes. Code a swap
  supersedes is deleted in that swap, and so are the tests written for it (a
  property no current law covers is ported as a new law against the new code).
- **Stored shapes are one 1.0 baseline.** A swap that changes a stored or
  parked shape changes it in place and checks the affected same-build
  crash/resume laws. The first release images are recorded fresh at the 1.0
  cut; no version constant is bumped and no migration is added.
- **Error taxonomy** (unchanged from `persistent-heap-seams.md` §1): a guest
  error is a raised value; a guest bound is `Bound::*`; an infrastructure
  failure (`HeapFault`, a page refusal, a durable error) is never catchable and
  ends the activation, which resumes from its last committed root.

## 2. What is pinned, and where

| Seam | Pinned in code | Pinned in this document |
| --- | --- | --- |
| A coordinates | `lash_kernel_exec::coord` | the grammar, restore validation |
| B charge | `lash_kernel_exec::charge`; `KernelMachine::record_charges` and `lash_kernel_vm::ChargeTape`; the corpus law | region split rules, the library charge |
| C values | `lash_kernel_doc::arena::{ArenaHandle, Value, ShortText}` | the kind table, id assignment and walk order |
| D arena | `lash_kernel_doc::arena::{Objects, Body, Sequence, Table, LongText, Bytes, Chunk}` | ownership, proxies, worklist, cold edges |
| E calls | none | the call ABI, kill sets, cold frames |
| F natives | `lash_kernel_doc::arena::native` | the decision and its evidence |
| G publication | none (the `Machine::seal` contract stands) | projection and candidate rules |
| H pages | none | the canonical constructor |
| I memory | none | the rebase rule |
| J helper plans | none (hgen lands the types) | the plan interface and segment contract |

Nothing pinned is wired into execution, except the charge tape, which the
charge oracle law reads (§B). The pinned crates are kernel-private:
`lash-kernel-exec` is used only by `lash-kernel-vm` and the conformance tests,
and the `lash` facade re-exports none of these items.

## 3. The seams

Each seam gives its contract, its owner, its types, the invariant a law
checks, the lane that builds it and the code that lane deletes.

### A. The coordinate grammar and fail-closed restore

**Contract.** A saved run names where it stands by semantic coordinates only:
sites as path suffixes under a frame's statement site, phases, roles by the
site of the operand that produced them, and table cursors by insertion
ordinal. It never holds a pc, a register, an arena handle or a compiled
table's index, so any layout of any build of the same document restores it.
The grammar, production by production:

| Production | Coordinate | Saved as today |
| --- | --- | --- |
| Task | identity (lineage), state (`Ready`, `Resuming { incoming, requeue }`, `Waiting(wait)`, `Joining`, `JoiningMany`, `Ended`), joiners, `observed`/`passed`/`failed`, occurrences, frames | `TaskRecord`; ready order is `CheckpointRoot.ready` |
| Frame | statement site; `Boundary` or `Nested { action, phase, roles }`; loops; cleanups; retained arguments | `FrameRecord.statement`, `.pending`, `.loops`, `.finally`, `.departing`, `.arguments` |
| Nested action | action site suffix, `ActionPhase` (`Yielded`, `Called`, `Waiting`, `Spawned`, `Joining`), roles by site in evaluation order | `PendingRecord`, `PhaseRecord`, `RoleRecord` |
| Loop | statement site, `started`, `LoopPhase` (`Body`, `Open`, `Pull`, `Check`, `Exhausted`, `Item`), cursor (`Sequence { next }`, `Table { last ordinal, size }`, `Text { next }`, `Driver`), remaining guard, held item | `LoopRecord`, `CursorRecord`, `GuardRecord`, `IteratedRecord` |
| Cleanup | `try` site and phase; in `finally`, the departure (`Normal`, `Throw`, `Return`, `Break`, `Continue`, `ReturnVia(site)`) and its payload | `FinallyRecord`, `EnteredRecord`, `departing` |
| Call or formula continuation | the call's retained arguments by position | `FrameRecord.arguments` |
| Bindings | by name; a shared variable by its cell's durable id | `BindingRecord` |

A pc-to-site table alone is not enough: a statement can stand at several
nested actions, each with its own role set, inside loops and cleanups whose
phases matter. The code's side table states, per statement, every action
nested in it with its kind and role sites (`ActionEntry`), and per enclosing
loop and `try` the sites that can stand there. Slot homes are static; the
dynamic contents are live state.

- **Liveness.** Semantic bindings and roles stay live, and are saved, even when
  optimized code no longer reads them.
- **Holdings visitor.** Registers, borrowed transfers, pins and fresh objects
  are holdings, visited by a separate holdings visitor at allocation and native
  precision points. They are never checkpoint coordinates.

**Restore validation (fail closed).** In order:

1. Identity: document, kernel version, helper release and root status
   (`machine/checkpoint.rs`, `open_with_layout`). A mismatch is an `OpenError`.
2. Each frame's coordinate against the side table, refusing with a
   `RestoreRefusal`:
   - `UnknownSite`: the action is no action nested in the frame's own
     statement (one in a closure written inside it belongs to the closure's);
   - `IllegalPhase`: the action's kind cannot stand at the phase
     (`ActionKind::stands_at`), or the frame is `Fresh`;
   - `TaskState`: the phase disagrees with how the task arrives (`Arrival`, from
     its saved state: only a `then yield` wait owes a turn once delivered);
   - `MissingRole`, `ExtraRole`: the saved role set differs from the entry's,
     including a role saved twice;
   - `UnknownLoop`, `IllegalLoop`: no enclosing loop at the site, or a header
     phase the cursor cannot stand at (`Exhausted` without a guard, `Item`
     without a held item);
   - `UnknownCleanup`: no enclosing `try` at the site;
   - `Arguments`: a body frame keeps another number of arguments than its
     function takes.

Restore never guesses a nearest site or phase. The checks are today's
(`machine/frames.rs`: `continuation`, the loop and cleanup decoders), over the
pinned types.

**Owner and types.** `lash_kernel_exec::coord`: `SitePath`, `ActionPhase`,
`ActionKind`, `Arrival`, `ActionEntry`, `StatementCoordinate`, `LoopPhase`,
`Cursor`, `LoopCoordinate`, `Departure`, `CleanupCoordinate`,
`FrameCoordinate`, `RestoreRefusal`. The mapping from records to coordinates
stays in `lash-kernel-vm` (`machine/frames.rs`); the validation function moves
to `lash_kernel_exec::coord` as
`fn admit(entry: &ActionEntry, saved: &StatementCoordinate, arrival: Arrival) -> Result<(), RestoreRefusal>`
with its loop and cleanup siblings.

**Invariant.** Every corruption of a saved coordinate (a site suffix moved, a
phase changed, a role dropped, added or duplicated, a loop header moved past
its cursor) refuses the open with the refusal that names it, under every
layout, and every uncorrupted image opens under every layout to the same
canonical continuation bytes. The existing refusal laws in
`lash-kernel-vm/src/laws/continuation.rs` are the witnesses; the frame swap
re-homes them on the pinned types.

**Built by** swap 2.3 (frames). **Deletes** `ContinuationRefusal`, `ParkKind`
and `continuation` in `machine/frames.rs` (moved, not copied), and, with
FIG-6672, `FrameRecord.registers` and `HeapFault::FrameRegisters`.

### B. The charge event program

**Contract.** Charge is the ordered sequence of a run's semantic charge
events. Passing the bound ends the run at the event that passes it, charged
with that event; nothing after it is charged.

| Event | Weight | Where today |
| --- | --- | --- |
| Statement | 1, atomic | `advance` (`machine/exec.rs`) |
| Expression node, atom argument | 1 each, in pre-order, as a unit run | the stream's charge regions (`Op::Region`, `RegionTable`; `machine/stream.rs`); an action's atom operands, `machine/actions.rs` |
| Construction | its members, atomic | the charge region the construction stands in (`machine/stream.rs`) |
| Loop test | the cost table's, atomic | `machine/loops.rs` |
| Library formula | its formula at completion (on a raise, with an absent result), atomic | `charge_call` (`machine/exec.rs`, `machine/ops.rs`) |
| Copy-out, decode | the value's deep size, atomic | `machine/exec.rs` |

- **Weights.** `Weight::Units(n)` is n one-unit events: a run passing the bound
  inside it is charged up to the unit that passes. `Weight::Atomic(n)` is one
  event charged whole (`K-CHG-003`). Addition saturates and the bound is
  strict: a run is over its bound when it is charged more than the bound
  (`machine/ops.rs`, `charge`).
- **Anchors** (which units are tested together) are a function of the
  document: adjacent one-unit events with nothing between them in pre-order
  (`compile/lower.rs`, `Ops::charge`). The per-unit expansion of the program
  (`Weight::expand`) is equal under every layout; the oracle law checks it.
- **Start.** What a start decodes (an entry's arguments, a cell's host
  bindings) charges nothing, so no charge bound stops it; the run's first
  statement meets the bound. The oracle found that a bound below the decode's
  deep size refused the start as `StartError::Arguments`; FIG-6691 fixed it in
  `machine/session.rs`.

**Library charge (FIG-6694, hcharge).** Every library function, native-stating
or body-only, is charged its declared formula at completion, as one atomic
event. Its own body forms are uncharged. Guest callbacks it calls and closures
it creates stay ordinary charged code: charging is restored on their entry and
on the body's return through handlers. A helper call is therefore one atomic
formula event plus the events of its callbacks. Body-trace charging is
deleted: today `compile.rs` sets `charged = !definition.has_native()` and
`call_units` answers 0 for a body-only function, so 569 helpers charge their
body trace and bill no formula. A helper is atomic for the bound and for
slices; its work guard still caps it. The 569 formulas are seeded from their
bodies, and charge numbers change (dev phase: images re-recorded).

**Regions (stage 3).** A region is a maximal straight-line run of fixed events
(statement, expression node, atom argument, construction, loop test) and the
ops between them. It ends before:

- a dynamic event: a formula, a copy-out, a decode;
- a call, a nested action or a statement's action (the callee charges, or the
  run parks);
- a host observation: `HostRead`, `Clock`, `Random`, `Print`;
- a handler entry: a `catch` or `finally` block, or a raise's unwinding;
- a back edge (a loop's next iteration is a new region).

An op inside a region that can raise or halt is an exit. Its executed prefix
(how many of the region's events precede it) is static.

- **Executed prefix.** The machine keeps the entry charge `c0` and commits
  `c0 + prefix` at every exit (`RegionSummary::commit`). It never charges a
  suffix ahead and gives it back, and never subtracts from a saturated total.
- **Fast admission** (`RegionSummary::admit`): with checked arithmetic, if
  `c0 + total <= bound` and `c0 + total < slice_end`, the region runs without
  internal checks. Strictly below the slice threshold, no step boundary inside
  the region can reach it, so slice returns land on the same step under every
  layout. Otherwise the region replays its events in order, as the reference
  charges them: a unit run stops at its first crossing unit and a formula
  charges atomically.
- **Verifier.** `RegionSummary::verify` checks a region's summary against its
  events (count, every prefix, no dynamic event) before the region is
  admitted to the fast path.
- **The tape under regions.** With a charge tape attached, a fast region
  records the events it executed from its `RegionEvent` list at each exit.
  Recording never changes the admission.

**The oracle.** `ReferenceCharger` charges one unit at a time with
`K-CHG-002`'s saturating, strict rule. It is the oracle stage 3 keeps.

- **Today's engine.** `KernelMachine::record_charges(ChargeTape)` records every
  charge the run makes, in order: each unit run as `Units`, every other charge
  as `Atomic`, and, when a run passes its bound inside a unit run, the units
  within the bound and then the unit that passes. It is an observation: one
  branch on whether anything observes, at the anchors (`charge` and a
  region's close, exit and replay), and it changes no result, charge, park or export. Under
  kdec-api it becomes one of `MachineOptions`' observers.
- **Law** (`lash-kernel-conformance`,
  `tests::charge::every_corpus_charge_bound_ends_where_the_per_unit_reference_charger_ends`):
  on every corpus case, layouts 0 to 3 make the same run and the same expanded
  charge program. A run bounded at the least and the greatest bound each event
  of that program passes, and at its whole charge, ends where the reference
  ends it: charged with the event that passes, after the same charges,
  prints, parks and requests; past its whole charge, exactly as the unbounded
  run. At FIG-6691: 296 cases, 29,660 bounded runs, 28,480 of them crossing.
  Under-counting a unit run's crossing unit (`cross_nodes`) makes 12,268 of
  them disagree.
- **Law** (`lash-kernel-exec`,
  `charge::tests::a_fast_region_commits_what_the_per_unit_reference_charges`):
  a region admitted fast commits at every exit exactly what the reference has
  charged there, never passes the bound or the slice threshold, and a region
  the reference would stop inside replays. Entries run to `u64::MAX`.

**Owner and types.** `lash_kernel_exec::charge`: `Weight`, `Ending`,
`ReferenceCharger`, `Crossed`, `RegionEvent`, `RegionSummary`, `Admission`,
`RegionError`. The anchors stay in `lash-kernel-vm` (`machine/ops.rs`,
`machine/stream.rs`) until stage 3.

**Built by** hcharge (FIG-6694: library charge) and the stage-3 instruction-set
lane (regions). **Deletes** body-trace charging (`Code.charged` as
`!has_native`, `call_units`' zero for body-only functions), `charge_nodes` and
`cross_nodes` (replaced by region commit and replay), and `Op::Charge`
coalescing in `compile/lower.rs` (replaced by the region side table).

### C. Values, kinds and the three namespaces

**Contract.** A running value is 16 bytes (`lash_kernel_doc::arena::Value`,
size asserted). Immediates are inline; everything else is an arena handle.
`Value` is `Copy`: the machine keeps reference counts at the holding locations
it writes, so no value carries a release protocol (§F).

| Kind (K-VAL-001) | End-state value | Sharing | Projection |
| --- | --- | --- | --- |
| null, absent, bool | immediate | value | inline token |
| int | `Int(i64)`; a larger one `BigInt(handle)` | immutable, shared | inline digits up to 256 bytes, else a payload tree (`IntRef`) |
| float | `Float` | value | its kernel spelling |
| text | `Text(ShortText)` up to 14 bytes; else `LongText(handle)`, scalar-bounded chunks | immutable, shared | `TextRef`: inline up to 256 bytes, else canonical payload leaves |
| bytes | `Bytes(handle)` | immutable, shared | `BytesRef`, as text |
| timestamp | `Timestamp(i64)` nanoseconds; else `BigTimestamp(handle)` | value | `Timestamp` |
| tuple | `Tuple(handle)` | immutable, shared | `TupleRef`, frozen family kept |
| list | `List(handle)`, positional chunks | mutable, by identity | directory head and body tree |
| map, set, record | `Map`, `Set`, `Record(handle)`, content-placed chunks of entries | mutable, by identity | directory head and body tree |
| closure | `Closure(handle)`: code site and capture cells | by identity | `ObjectInit::Closure` |
| error | `Error(handle)`: kind, message, data | immutable, shared | `ErrorRef` |
| task | `Task(TaskId)` | value; a task edge | task number |
| function | `Function(ShortText)`; else `LongFunction(handle)` | value | the function name |
| handle | `Handle(handle)` | immutable, shared | `Handle` |
| ref | `TaskRef(TaskId)`, `ObjectRef(handle)`: strong (K-KEY-004) | value | the referent's identity |
| variable cell (not a kind) | `Body::Cell(Value)`: a closure's shared variable (K-CLO-001), a fresh cell per `for` iteration (K-CLO-002) | by identity | a variable entity |

- **Canonical immediates.** A text, function name, integer or timestamp that
  fits its immediate is always the immediate, never an arena object, so an
  immediate and a boxed value of one kind are never equal.
- **Text.** UTF-8 with its summary (bytes, chars, UTF-16 units). An ASCII text
  answers char positions by byte offset. UTF-16 indexing metadata beyond the
  summary is added only if string workloads show it is needed.
- **Interning.** Static keys are interned at compile time. Intern ids are cache
  state, never persisted.

**Three namespaces**, none convertible to another:

1. `ArenaHandle`: slot index and generation, process-local, never persisted. It
   has no `Hash` or `Ord`, so nothing observed, keyed or saved can depend on
   where the arena put an object.
2. `EntityId (family, ordinal)`: a mutable object's durable identity.
3. `PageRef`: an immutable page, by content.

**Ids at projection [SAM-2].** A primary-family object gets its durable id at
the first projection point that saves it, never at creation, and keeps it.
Objects that die before a projection point never get one, so a native and a
body allocating different temporaries no longer shift ids in the continuation
bytes.

The canonical walk assigns them. From the semantic roots, in order:

1. session bindings written since the last projection point, in the order
   their values were last bound. A write keeps its value in the arena until a
   projection point (FIG-6748), and a value the run saves first, as a task's
   result or an effect's argument, walks the bindings written so far in the
   same order: so where those points fall moves no id;
2. tasks by number; in each task, its incoming outcome or ended result, then
   its frames by index from the bottom. In each frame, in this order: bindings
   by name, retained arguments by position, roles by site, loop held values by
   loop site, departing payloads by `try` site;
3. inside each object, its members in canonical order: a sequence by index; a
   table by insertion order, key before value; a closure's captures by name;
   an error's kind, message and data.

The walk is depth-first and pre-order. It visits only new or dirty objects: a
clean object cannot reach a new one without having been written, so it is not
descended. Each object without an id takes the next primary ordinal when the
walk first reaches it. Objects of frozen or lazy families keep their
family-relative ids (`family_impl/mod.rs`); a fresh result family and session
adoption follow today's identity rules.

A ref used as a map key is keyed in the arena by its referent's
process-local token, whether or not the referent has an id yet. A page keys
it by the referent's durable id, which the projection that writes the table
computes from the entries' page forms, so the arena never re-keys a table.
No page ever holds a handle or a token.

**Invariant.** PLAN-INDEP over ids: layout 3, where every native beside a body
declines and the body allocates other temporaries, produces equal canonical
continuation bytes, ids included, at every projection point of the corpus and
the helper differential. Resume under another layout keeps every assigned id.

**Built by** swap 2.4(c). **Deletes** ordinal assignment at allocation
(`install_object` taking `next_ordinal`), the machine's `fresh: Vec<EntityId>`,
and `heap::Value`'s `EntityId` operands (with 2.4(a)).

### D. Arena, chunks and proxies

**Contract.** Running state lives in the arena; pages are its projection.

- **Bodies and lifecycle are split.** What a native reads (bodies by handle)
  is `lash_kernel_doc::arena::Objects`. The arena's per-slot facts (reference
  count, durable id, base page, dirty chunks, residency) live beside it, in
  `lash-kernel-state`'s arena module, so a native call's reads touch only
  bodies.
- **Small objects** (one chunk) materialize whole on first read and carry one
  dirty bit.
- **Large collections** keep a base `PageRef` per chunk. A chunk is one leaf of
  the object's canonical body tree (§H): a sequence chunk holds
  `Sequence::CHUNK` (64) values by position, a table chunk the whole entries
  between two content-placed boundaries. Chunks materialize lazily
  (`Chunk::Cold { page, len }` to `Chunk::Resident`).
- **Resident chunks own their storage by value** (ruled 2026-10-17, FIG-6708):
  a list's, map's, set's or record's chunks are `Vec<Chunk<Vec<Value>>>`,
  and immutables keep boxed exact-size storage. A boxed slice cannot grow,
  so an append would copy the whole object; amortized O(1) append is
  required. `Objects::get_mut` and `Objects::insert` serve the machine's own
  edits and allocations outside a native call.
- **Dirty-chunk worklist.** Each object keeps its dirty chunk indexes; the arena
  keeps the set of objects with dirty chunks (cards). The worklist says which
  chunks; the canonical walk (§C) says in what order they are projected. A
  point write to a million-element list decodes and encodes one chunk plus its
  index path: a based list or table is projected by its dirty 64-wide cards
  (`StoredCards`, `store_cards`; FIG-6726).
- **Materialization happens outside native calls.** A view that reaches a cold
  chunk answers `Retry::Cold`; the machine decodes that chunk, and, doubling,
  the ones after it in the same object, then runs the call again (§F). A scan
  of a cold object therefore costs O(n) decode in O(log n) retries, and a point
  read decodes one chunk.
- **Proxies** are interned per entity within one heap version: decoding a page
  that names entity X gives X's one handle, creating a cold slot from X's
  directory head (kind, length, cost, edge summary) whose chunks are all cold.
- **Lazy families** keep their membership. Session adoption and carry follow
  `family_impl/mod.rs` (`carry`, `adopt`) and `machine/session.rs`.
- **Eviction** happens only at statement boundaries: a clean resident chunk may
  drop to `Cold` on its retained base page.

**Reachability.** Acyclic objects are released by reference count over the
whole accounted RAM domain. A cold object's outgoing edges are its persisted
summaries (`EntityCost.has_edges`, task edges, local cost), never decoded
eagerly. Cycles are collected on allocation pressure or bounded debt, with a
fast exit when there are no candidates. An ordinary projection does no global
traversal (today a park's census refinement walks the reachable graph,
`paged/census_graph.rs`).

**Invariants** (spec §5, incremental):

1. A point write to one large list and to one large table reads, encodes and
   seals at most one chunk plus its index path.
2. A growing retained task, frame and binding state with one changed frame
   projects with visits bounded by the change.
3. A no-op session carry over a million names reads nothing.
4. An ordinary park performs no global traversal.

**Owner.** `lash_kernel_doc::arena` (bodies, views), `lash-kernel-state`
(lifecycle, projection), `lash-kernel-vm` (holding locations). **Built by**
swap 2.4(a) for small objects, 2.4(b) for chunks, 2.4(c) for ids and
reachability. **Deletes** execution through the paged heap's object trees (the
machine's `SemanticHeap` reads and writes, `heap_write` and its refusal and
retry protocol), `Roots::publish` and `publish_holdings` on the statement path,
and the census trace at ordinary park.

### E. The call ABI, kill sets and cold frames

**Contract.**

- **Task stacks.** Each task has a register stack and a frame array; a frame is
  code, pc, base, and its loop and cleanup state.
- **Calls.** Arguments are evaluated directly into the callee's parameter
  slots: the caller's outgoing window overlaps the callee's incoming one.
  Ownership moves at call and at return (no clone, no count traffic), and
  capacity is reused; growth is a slow path. Depth is checked at every logical
  entry, before a native, an opcode or a body runs, even when a physical frame
  is reused (K-BND-002).
- **Statement kill sets.** Registers hold one statement's temporaries only. At a
  statement's end (and at its step's end for `Op::Pin`) the machine releases
  every value still in its window, from a static set the statement states. The
  set is the statement's, not the register allocator's, so the release order
  and the bytes released are equal under every layout, and no dead register
  holds memory across statements.
- **Mutable frames.** A resident frame's slots are plain values. It is
  projected to its `FrameRecord` only at a projection point, and only when it
  is dirty or is a task's top frame.
- **Cold frames.** Frames below the top stay cold descriptors (their saved
  record's page) until a return reaches them; today's `Frames` cache already
  keeps lower frames in rows until then.

**Owner.** `lash-kernel-vm`: `machine/frames.rs`, `machine/registers.rs`,
`machine/exec.rs`.

**Invariants.** CONT-EXACT and K-BND-002 as today; projection visits bounded
by dirty frames (§D invariant 2); after any statement the machine's register
holdings are zero (a holdings-visitor law).

**Built by** swap 2.3 (mutable and cold frames), swap 2.5 (kill sets), stage 3
(register windows). **Deletes** per-return `Frames::publish`
(`machine/frames.rs`) and the FIG-6586 publication comparisons, the frame
working trees written during execution, `recycle`'s tree resets,
`Registers.prepaid`, and, with FIG-6672, `FrameRecord.registers`.

### F. The native ABI: typed views over call-scoped borrows

**Decision [SAM-3].** A native receives `&'call [Value]` and reads what the
values name through concrete views that borrow the arena for the call: a list's
or tuple's values as `&'call [Value]` chunks, a table's entries as chunks of
pairs, a text as `&'call str` chunks. There is no dynamic dispatch per element
and no value cloned out. It builds into a `Nursery` whose slots the arena
reserves without writing; the machine installs them after the call returns.
Guest semantics that are not immediates (deep equality, key checks, keyed
lookup) go through the arena's `Semantics`, one dynamic call per question;
immediates are compared inline (`Args::equal`).

- **No release protocol.** Every borrow ends with the call. A result is
  returned by value (16 bytes, `Copy`); the machine counts it where it installs
  it.
- **No limit on plugins.** Any crate implements the trait with full read access
  to every kind and full allocation. Reads and allocations interleave freely,
  because reads borrow `&Objects` and allocation borrows `&mut Nursery`.
- **No unsafe aliasing.** No `unsafe` anywhere. A call never writes an arena
  slot: reservation pops a free index through a `RefCell` or counts past the end
  through a `Cell`, and the install writes after the borrows end.
- **Retry, not fault.** A call never reads a page. A view reaching a cold chunk
  answers `Retry::Cold`, and a reservation past the granted headroom answers
  `Retry::Room`. The machine decodes or collects and runs the call again.
  Natives are pure, so the retried call does what it did; the abandoned
  attempt's nursery is discarded (`Nursery::discard`). This extends today's
  "retry after a collection" (`K-LIB-002`).
- **Kept from today:** `prepare` (a heap-independent entry over immediates for
  an opcode's guarded fast path), the purity rules, `Decline` before anything
  observable, and `WorkCounter`.
- **Dropped:** `returns_borrowed` and `record_read_marker`: borrowed returns
  are free with `Copy` values, and record reads become opcodes (stage 3).

**Evidence.** Three ABIs were prototyped over one arena, each running the
logic of three real natives from `lash-kernel-lib`: `list.index_of` (64-int
list, found at index 48), `text.find` (206-character text, start 0) and
`list.slice` (32 of 64 values, allocating the result). A fourth variant
isolates dispatch from borrowing:

- **A, call-scoped handles through `&mut dyn NativeHeap`:** today's ABI with the
  arena handle for `EntityId`. Values are cloned out, and lists are visited
  through a `dyn FnMut` with `dyn` equality.
- **B, durable ids with an `EntityId` to handle map:** A plus a map lookup on
  every object access and a map insert per allocation.
- **C, typed views (chosen):** concrete views, inline equality on immediates,
  nursery allocation.
- **D, `dyn` heap with borrowed bulk reads:** one dynamic call per object
  returning `&[Value]` chunks or `&str`; equality stays `dyn`.

Paired `perf stat -e instructions:u` per call: N = 2,000,000 calls minus an
empty run, 5 pairs in alternating ABI order. Every pair gave the same count to
within 0.2% (B's slice) and exactly otherwise. The build was Kiln
`--config=optimized` (opt-level 3), rustc 1.98.1, on an AMD Ryzen 9 5950X with
load1 24.5 to 34.6 and peak RSS 2.3 MiB, under an 8 GiB address-space cap.

| Native | A (`dyn` heap) | B (id map) | C (typed views) | D (`dyn`, borrowed) |
| --- | ---: | ---: | ---: | ---: |
| `list.index_of`, 64 ints | 2,912 | 3,027 | **941** | 2,258 |
| `text.find`, 206 chars | 1,518 | 1,748 | **1,483** | 1,510 |
| `list.slice`, 32 of 64 | 4,439 | 8,559 | 2,098 | **2,005** |

C reads a collection 3.1x cheaper than A and 2.4x cheaper than D: per element,
about 18 instructions against 57 (A) and 44 (D). On text the shared search
dominates and C is cheapest by the Arc clone it avoids. On allocation C is 2.1x
cheaper than A and within 5% of D: 93 instructions is the nursery's staging,
the price of letting reads and allocation interleave. In D the borrow checker
forces a native to finish its reads before it allocates, so a native that
builds while it walks (entries, map, filter) must clone out first. B is the
slowest everywhere. It also needs an id for every object a native can see,
which contradicts SAM-2, and its map entries need a release protocol. The
prototype is not landed; its source, script and samples are in the lane record
under `.kiln/FIG-6691/abi-proto/`.

**Owner and types.** `lash_kernel_doc::arena::native`: `NativeFunction` (its
final name), `NativeCall`, `Args`, `SequenceView`, `TableView`, `TextView`,
`Semantics`, `Nursery`, `NewObject`, `NativeError`, `Retry`, `PreparedNative`.

**Invariant.** Natives answer the same through the views as through today's
heap: the native conformance harness (`check_native`, cold and warm), the
helper differential and the corpus, with retries forced at every chunk (a law
that makes every chunk cold).

**Built by** swap 2.4(a): every `NativeFunction` implementation moves once
(`lash-kernel-lib` numbers, text and JSON, collections; the `lash-ext-*`
regex, URL, date, py-core and ts-core; the machine's `RunByTheMachine`).
**Deletes** `heap::NativeHeap`, `heap::NativeCall`, `heap::NativeFunction`,
`heap::NativeError`, the machine's call heap (`machine/native_heap.rs`). The hand twins (`py.truth`, `py.add`, ten
`ts.*`) are deleted by hgen as generated plans replace them (§J). Hand natives
remain only for external algorithms: regex, date, URL and JSON parse.

### G. Projection and candidate publication

**Contract.** A projection is a producer of the existing fenced owner
candidate. Nothing about publication changes.

- **Projection points:** park, slice return, end. A seal is legal at each
  (K-MACH-007). `Machine::seal(budget) -> Result<Option<SealedRoot>, SealError>`
  stays the interface; kdec-api's `checkpoint()` wraps it.
- **What a projection walks:** dirty tasks, dirty frames and every task's top
  frame, dirty session slots, the dirty object worklist, and objects newly
  reachable from them. It assigns ids (§C) and encodes in canonical order,
  independent of arena slots, page-in order and cache warmth. It produces
  today's `CheckpointRoot` (without `FrameRecord.registers`). Clean references
  are retained as they are.
- **Resumable under `SealBudget`.** A projection that runs out of budget
  returns `None` and keeps its walk cursor and every dirty mark; the next seal
  continues it. No guest step runs between an incomplete seal and its
  completion: `run` refuses while a projection is incomplete. Otherwise ids
  assigned by the abandoned walk would depend on when the budget ran out.
- **Dirty state and the baseline.** Pages staged by a projection go to the
  worker's spool (D-SPOOL). The embedder publishes the root with the owner
  commit that carries it (`CandidatePublication`, `lash-durable`
  `domain/pages.rs`; `commit_candidate`, `lash-vm-broker` `kernel/store.rs`).
  On commit the spool settles and the activation's baseline is the new root.
  On abort or an ambiguous acknowledgement the activation ends and the next one
  resumes from whichever root the row holds. So no machine ever runs on past an
  uncommitted candidate, and the machine clears its dirty marks when seal
  returns a root. No dispatch escapes from an uncommitted candidate: admissions
  commit with the root.
- **Resume:** fail-closed identity checks, compile or fetch the code, map
  coordinates by the grammar (§A), load only the top frames and touched state.

**Owner.** `lash-kernel-vm` (`machine/checkpoint.rs`, `seal`) and a projection
module in `lash-kernel-state`. The durable and broker sides are unchanged.

**Invariant** (spec §5, publication). A crash before and after the candidate
commit, a lost acknowledgement, and a seal budget exhausted mid-projection all
select the committed root, and no effect dispatches from an uncommitted
candidate. Plus: seal at every slice return under small slices over the
corpus, and resume under another layout warm and cold, with equal canonical
continuation bytes, an equivalent graph and equal traces (kend-harness builds
the harness).

**Built by** swaps 2.3 and 2.4 (the projection producers); the publication
laws run in 2.4. **Deletes** per-statement root and holdings publication
(`StatementRoots`, `machine/mod.rs` `publish`), and the working-tree seal path
(`seal_task_root` over trees written during execution).

### H. The canonical page constructor

**Contract.** Equal semantic state seals to the same pages, whatever history
built it. Before FIG-6717 the trees split a full leaf in half where a mutation
overflowed it, so the same map built in two orders could seal to different
pages.

- **Sequences** (lists, tuples, a sequence tree) chunk by position: leaf `k`
  holds indexes `64k ..< 64(k+1)` (`SEQUENCE_LEAF_CELLS`). This is
  history-independent and makes index arithmetic O(1) on the hot path. A point
  write or an append rewrites one leaf and its path. An insert or removal in
  the middle rewrites the leaves after it at the next projection. If kbench
  shows queue workloads (`shift`, `pop(0)`) paying for it, sequences move to
  content-placed boundaries with a rank index, as tables have.
- **Keyed trees** (tables, directories, bindings) place a leaf boundary after
  row `i` when the low 6 bits of `H(0, key_i)` are zero, which gives a mean of
  64 rows per leaf (`KEYED_LEAF_ROWS`). A leaf that would pass 128 rows or
  `PAGE_TARGET_BYTES` (8,192) instead ends at that cap, counted from its own
  start. A cap moves only the boundaries up to the next natural one, so an
  edit stays local. Index levels apply the same rule to their children's first
  keys under `H(level, key)`, with fanout as the mean (`INDEX_FANOUT`, 32). `H`
  is a fixed, seedless 64-bit hash written in the kernel (no dependency), never
  a page digest (D-SEAL).
- **Inline bodies** stay inline exactly when their encoding fits
  `INLINE_BODY_MAX` (256 bytes) or `INLINE_TREE_BYTES`: a function of content.

**Locality.** An insert, removal or update of a keyed tree changes O(1)
expected leaves and its index path, independent of the tree's history.

**Owner.** `lash-kernel-state/src/paged/tree.rs`: a `canonical` boundary
function replaces the overflow splits.

**Invariant.** The same table built by different insert and delete orders,
including deletes of keys that were present, seals to equal root `PageId`s.
A point edit re-encodes at most a bounded number of leaves plus the index path.
The equality domain's laws stay stated over graph equivalence (§1).

**Format.** Page shapes change: this is a dev-phase format change. The lane
checks the affected same-build crash/resume laws.

**Built by** swap 2.4(b). **Deletes** the overflow split and the
removal rebalancing in `tree.rs`.

**As built** (FIG-6717, `tree/canonical.rs`). Every cap closes a chunk
*after* the item that reaches it (the 128th row, or the row that brings the
chunk's encoded rows to 8,192 bytes), so a boundary depends only on the items
since the previous one; closing before an overflowing item would make it
depend on the next chunk's first row. Index caps are twice the mean fanout
(64) for keyed trees and the fanout (32) for sequences, and an index entry
counts its first key plus 96 bytes. Sequence chunks also close at the byte
cap, which a run of large cells (role chunks) needs to stay within the node
bound; small cells never reach it, so lists keep the positional 64. `H` is
FNV-1a over the level's little-endian bytes and the key, then the SplitMix64
finalizer. Edits keep working nodes bounded by cutting an overflowing node
with the same rule from its own start, and a seal streams the dirty regions
bottom-up through one chunker per level, taking a sealed subtree whole once
every level below it is at a boundary. A budget-exhausted seal keeps the
chunks it sealed as pages and the rest as working nodes, and the next seal
continues. Only leaves stage under dirty pressure, sealed as they are; the
seal reads a staged leaf back and places its rows canonically.

### I. The memory counter rebase at seal

**Contract.**

- **Accounted domain.** The logical holdings metric
  (`AccountingMetric::LocalCostV1`), checked before growth at defined admission
  points: arena installs, nursery reservations (`Nursery::reserve` against the
  granted headroom), frame pushes, binding growth and register-window growth.
  Reserved capacity, proxy metadata, worklists and native transient buffers
  count.
- **Rebase [SAM-4].** The counter is `base + young`. At every seal `base`
  becomes the census stored in the sealed root (`AccountingCertificate.upper`)
  and `young` becomes 0. Only objects created since the last seal are released
  by reference count against `young`. An older object that dies stays in `base`
  until a collection refines it. A cold open sets `base` from the root it
  opens. Warm and cold runs therefore hold identical counters from every seal
  onward, and K-MACH-008 holds for bound errors too.
- **Limiters at a bound (FIG-6768).** The run carries `MemoryMeter` with its
  bounds. Under `On`, an admission that would pass the conservative bound
  first collects cycles and refines the census exactly, freeing what nothing
  holds; only when the exact count with what the admission asks for still
  does not fit does the machine ask the host's `MemoryLimiter`. Recounting is
  not a host choice. The charge bound is decided the same way by the host's
  `ChargeLimiter`, at the unit the reference charger crosses (§B), the
  crossing unit charged. Both are shaped as Wasmtime's `ResourceLimiter` and
  `epoch_deadline_callback` (wasmtime 25.0.0): `limit_reached(used, limit)`
  answers `Grow(by)`, which raises the limit by `by` and runs on (a limit
  still reached is asked again), or `Refuse`, the default and a segment
  without a limiter, which ends the run with the typed bound exactly as
  before. The ask is at an exact, deterministic point before the
  statement's effect. An embedded host's limiters are
  `MachineOptions::{memory_limiter, charge_limiter}`; a worker asks its
  parent with one `HostReadKind::Bound` read naming the bound, what the run
  used and its limit, the same exchange a read-bound `perform` makes, when
  the start says the parent decides (`Start::{memory_limiter,
  charge_limiter}`). The checkpoint root saves the run's current limits
  (`SavedLimits`, the memory limit none when unmetered), and an open
  continues with them whatever bounds its machine was given, so a grown
  limit survives a resume. A crash before the seal that would save a grown
  limit resumes from the seal before it, which reaches the same point and
  asks again, with nothing observable done in between. Seals and explicit
  checkpoints rebase as before. `Off` removes reservations, holds, arena
  memory pricing, census work and every limiter call; its memory readout
  is zero and its checkpoint carries an unmetered certificate. Semantic
  value summaries, persistence and reference ownership remain active.
  Carry collection is paced by both bytes and entity count (FIG-6750): the
  heap header retains each last-live estimate across cold opens. It collects
  when byte growth reaches `max(last_live_bytes, 128 KiB)` or entity growth
  reaches `max(ceil(last_live_entities / 2), 1)`. Large immutable scalars cannot
  starve cleanup of retired frozen families. Ordinary parks and no-op carries
  collect nothing merely for crossing a boundary.
  Refinement prices cold objects by their persisted summaries and resident ones
  by their bodies, and both give the same exact live cost, so the verdict does
  not depend on residency.
- **Plan dependence.** The refusal point may depend on the plan, within a slack
  of the simultaneously active transient holdings (registers, nursery buffers,
  pins). Kill sets (§E) make register releases plan-independent.
- **Kept:** operation depth (K-BND-002, checked before a native, opcode or
  body), shared parameter cells, result-transfer ownership, native guards and
  infrastructure-fault classification.

**Owner.** `lash-kernel-state` (`census.rs`, `publication.rs`: `hold`,
`refine`) and `lash-kernel-vm` (`machine/ops.rs`: `reserve`, `hold`).

**Invariant.** Two plans agree up to the earlier refusal: the same host
answers, delivery order and committed waits, and no committed effect repeats.
After every seal, a warm run's `meters().memory` equals that of a cold reopen of
its root (the memory verdict kend-harness separates from semantic equality).

**Built by** swap 2.5, after FIG-6672. **Deletes** per-statement exact
refinement at the collection point, `reserve_held`'s frame pricing at frame
publication (FIG-6318), and `Pins.reservations`. FIG-6672 removes the
operation memory reservation and native-domain narrowing.

### J. Generated helper execution plans and resumable segments

**Contract (FIG-6693).** Helper bodies (`.kernel` library functions) stay the
single semantic source: the reference, the fallback and the restore authority.
The build compiles them to Rust as kernel-owned execution plans (hgen,
FIG-6678). Extension natives (§F) and generated plans are different things:

| | Extension native | Generated helper plan |
| --- | --- | --- |
| What it is | an external algorithm (regex, date, URL, JSON parse) | the compiled form of one helper body |
| Semantic source | its own code | the helper body, which stays compilable |
| Interface | `arena::native::NativeFunction` | `HelperPlan` (below) |
| Calls guest code | never | at callback sites, through segments |
| Charge | its declared formula (§B) | its helper's declared formula (§B); callbacks charge themselves |
| Registry | open, per extension | open: generated descriptors per extension, never one closed enum |

**Plan interface** (pinned here; hgen lands the types in `lash-kernel-exec`,
next to the coordinate grammar, with `lash-kernel-exec` gaining a dependency on
`lash-kernel-doc` for the arena views):

```rust
/// A helper body's generated plan. It is valid only for the exact body it was
/// generated from: the release pins `(FunctionId, body digest)`, and admission
/// refuses a plan whose digest differs from the registered body's.
pub trait HelperPlan: Send + Sync {
    fn function(&self) -> FunctionId;
    fn body_digest(&self) -> [u8; 32];
    /// Runs from the call's start or from a callback's outcome to its next
    /// yield.
    fn run(&self, cx: &mut PlanCall<'_, '_>, entry: PlanEntry<'_>) -> Result<PlanStep, PlanError>;
}

pub enum PlanEntry<'a> {
    Start { args: &'a [Value] },
    /// Back from the callback the plan yielded at `at`.
    Resume { at: &'a FrameCoordinate, live: &'a [Value], outcome: CallbackOutcome },
}

pub enum CallbackOutcome { Returned(Value), Raised(Value) }

pub enum PlanStep {
    Return(Value),
    Raise(Value),
    /// Yield to the kernel at a callback site: the machine calls `callee`
    /// with `args` as ordinary charged code and resumes the plan with its
    /// outcome.
    Callback { at: FrameCoordinate, live: Vec<Value>, callee: Value, args: Vec<Value> },
    /// The plan's guards decline before anything observable: the body runs.
    Decline,
}
```

`PlanCall` gives a plan the same `Args` views and `Nursery` as a native (§F),
with the same `Retry` protocol. A plan is kernel-owned, so it may also use the
kernel's primitives directly (arithmetic, key normalization, the cost table).

**Segment yield/resume, in the coordinate grammar (§A).** A plan yields only
at its body's callback sites. What it yields is exactly the helper body
frame's `FrameCoordinate` at that site, the one the interpreted body would save
standing at the same call:

- **helper identity**: the frame's code is the helper's library unit
  (`Unit::Library(function)`), and the statement site is under it;
- **site and phase**: `StatementCoordinate::Nested { action, phase: Called, roles }`,
  with `action` the callback call's site in the body;
- **retained operands**: `roles`, the live roles of that site by site, their
  values in `live`;
- **loop cursor**: a `LoopCoordinate` per enclosing loop of the body (site,
  `started`, phase, cursor by index or insertion ordinal, remaining guard, held
  item);
- **handler state**: a `CleanupCoordinate` per enclosing `try` of the body, with
  its departure and payload in `finally`;
- **arguments**: the helper call's retained arguments.

Consequences:

- **No Rust stack is ever saved.** Between a yield and its resume the machine
  holds the coordinate and `live` as the helper frame's live state. They are
  projected only at projection points, like any frame.
- **Cross-plan restore.** A run parked inside a callback of a generated segment
  restores into the interpreted body, and one parked in the body restores into
  the plan, under any layout or build of the same release: the coordinate is
  validated against the body's side table (§A, fail closed), and the plan's
  resume entry must accept exactly the coordinates its body produces at its
  callback sites.
- **Charge.** The helper's formula is one atomic event at completion, on a raise
  with an absent result (§B). The callback's events are charged in between,
  as the body's would be. A `Decline` happens before any allocation or
  callback, and its nursery is discarded.
- **Occurrences.** Callback calls inside library units are not occurrence sites
  (today's rule), so a plan yields no effect identity of its own.

**Invariants.**

1. The plan and the body agree on values, errors, charge, callbacks in order
   and continuation bytes: run the corpus and the helper differential with
   plans (the default) and with bodies forced (layout 3), and compare.
2. A run parked at every callback of every generated segment reopens under the
   body-forcing layout, and the reverse, with equal canonical continuation
   bytes and results.

**Built by** hgen (FIG-6678) for closed helpers; then the callback-segments
lane (after hgen and this lane) for the 284 callback-capable helpers. Until
then callback helpers stay interpreted. **Deletes** the hand twins as plans
replace them, and hand opcodes except for the hottest inline operators.

**As landed (FIG-6709).** A segment is a generated plan whose `run` takes an
entry: 0 from the call's start, `k + 1` after callback site `k`'s callee
returned. Each `PlanSite` states the site's path in the body, its callee, the
register the value goes to, its roles, the plan variables in scope and the
loops it stands in; preparation resolves every site against the body's tables
under each layout and runs the body of any plan that does not resolve. At a
yield every standing plan becomes its body's frame at once, outermost first,
and the callee runs as ordinary code; the coordinate is that frame's, so
projection and restore are the frame's own. A callee that returns to a frame
standing at a site of a segment the layout runs resumes the plan there; a
raise unwinds the frame as the body's. A segment stands in a `try`'s body or
handler, never in its `finally`. Helpers that reach the host or a task, print,
or act other than by a call stay interpreted, as the manifests beside each
dialect's plans list.

## 4. Stage-2 and stage-3 briefs

Each brief can be started from this document alone. Every lane works in its
own Kiln fork on current main and reads §1 and its seams first.

**The lean gate** (every lane, unless its brief adds to it):

```sh
kiln check //crates/lash-kernel-vm:lash-kernel-vm //crates/lash-kernel-conformance:lash-kernel-conformance
kiln test //crates/lash-kernel-conformance:lash-kernel-conformance__unit_test
kiln clippy
```

The conformance target holds the corpus, the native cases, the 64-seed smith
slice law and the charge oracle (§B). Add the exec, state or vm owning target,
by selector, when charge declarations or the projection change. Add
`//crates/lash-dialect-typescript:helper_differential__test` and
`helper_plan_independence__test` when operation behaviour changes. Run the
durable image targets (§1) when a stored shape changes. Each swap ends with
its kbench row (`scripts/perf/kernel/embedded.py`), measured as paired
`instructions:u`.

### 2.1 Dense task-wide occurrences

- **Seams:** A (the task production), B (unchanged charge).
- **Do:** count occurrences in a per-task dense `Box<[u64]>`, indexed by a
  compile-time action-site index (an `Executable` side table from site to
  index, built from sites so that every layout agrees). Increment at every site
  counted today: `Call`, `Join`, `JoinMany`, `Yield` and `Cancel` outside
  library units, `perform` and `sleep`, `spawn`. Project into
  `TaskRecord.occurrences` (unchanged format: site key to count) for dirty
  tasks at projection points only. Restore a task's counters when the task is
  loaded. No static elision.
- **Done when:** corpus traces are unchanged, and a new law passes: the same
  effectful function called sequentially and recursively, with a cold reopen
  between calls and a changed layout, keeps effect identities distinct and
  equal to today's (K-EFF-008).
- **Deletes:** `occurrence` (`machine/actions.rs`), its `metadata_key` build and
  tree read and write per action, and the `Tree<u64>` occurrences as working
  state.
- **Gate:** lean, plus
  `kiln test //crates/lash-kernel-vm:lash-kernel-vm__unit_test --test_arg=--exact --test_arg=laws::continuation::nested_await_resumes_exactly`.

### 2.2 Session name-to-slot overlay

- **Seams:** C (bindings by name), D (lazy families), G.
- **Do:** give a session cell a name-to-slot overlay over its session root.
  Untouched names stay cold, and a no-op carry loads nothing. Maintain the
  `DataOnlyCertificate` (closure and task-edge counts) on allocation and
  release, so carry classification stays O(1). Keep the carry and alias rules
  (`family_impl/mod.rs` `carry`/`adopt`, `machine/session.rs`). Project dirty
  slots at projection points.
- **Done when:** a no-op carry over a million names reads no page (new
  incremental law), and the session-image law across layouts and cold caches
  passes (`laws/layout.rs`).
- **Deletes:** session reads and writes through heap trees during execution
  (`session_get`/`session_write`'s tree path), and the per-write root
  maintenance FIG-6677 narrowed.
- **Gate:** lean, plus the `lash-kernel-state` unit target by its session
  selectors.

### 2.3 Mutable frames with cold descriptors

- **Seams:** A (fully), E (frames), G (frame producer).
- **Do:** make resident frames plain mutable structures and keep lower frames
  as cold descriptors until a return reaches them. Project dirty frames and
  every top frame at projection points. Move restore validation onto
  `lash_kernel_exec::coord` (§A).
- **Done when:** CONT-EXACT, the restore refusal laws (re-homed on the pinned
  types) and the frame-projection visit law (§D invariant 2) pass, and park
  after every statement (kend-harness) resumes exactly.
- **Deletes:** per-return `Frames::publish`, the FIG-6586 publication
  comparisons, frame working trees written during execution, `recycle`'s tree
  resets, and `ContinuationRefusal`, `ParkKind` and `continuation` in
  `machine/frames.rs`.
- **Gate:** lean, plus the vm continuation laws by selector.

### 2.4 The arena

Three lanes, in order.

**(a) Small objects and the native ABI.**

- **Seams:** C, D, F.
- **Do:** move `lash_kernel_doc::arena::Value` and `arena::native` to the crate
  root in place of `heap::Value` and the heap native ABI. Run every object in
  the arena as one resident chunk with one dirty bit. Move every
  `NativeFunction` implementation once (§F list). Implement `Semantics` by
  porting `paged/equality.rs` and the key rules.
- **Done when:** the native harness (`check_native`), the corpus, the helper
  differential and PLAN-INDEP pass; a law forcing `Retry::Cold` at every chunk
  passes.
- **Deletes:** `heap::{Value, NativeHeap, NativeCall, NativeFunction, NativeError}`,
  `machine/native_heap.rs` and the
  machine's `SemanticHeap` object reads and writes.
- **Gate:** lean, plus the helper differential and plan-independence targets,
  and the durable image targets (shapes are unchanged; images must still
  resume).

**(b) Large-object chunks and canonical pages.**

- **Seams:** D (chunks, worklist, eviction), H.
- **Do:** give large objects lazy chunks with base pages, the dirty-chunk
  worklist and eviction at statement boundaries, and build the canonical page
  constructor.
- **Done when:** the point-write law on a large list and a large table, and the
  history-independence law (§H), pass.
- **Deletes:** the overflow splits and removal rebalancing in `tree.rs`, and
  `list_write`'s row-tree path.
- **Gate:** lean, plus the `lash-kernel-state` unit target by selectors, and the
  durable image targets after re-recording (format change, §1).

**(c) Ids at projection and reachability.**

- **Seams:** C (walk, ids), D (reference counts, cold edges, cycles), G.
- **Do:** assign ids at projection in canonical walk order; release acyclic
  objects by reference count; collect cycles on pressure; never traverse
  globally at an ordinary park.
- **Done when:** PLAN-INDEP over ids under layout 3, the no-global-traversal law
  and the publication laws (§G) pass.
- **Deletes:** ordinal assignment at allocation, the machine's `fresh` list, and
  the census trace at ordinary park.
- **Gate:** lean, plus the durable image targets after re-recording (ordinals
  change meaning: creation order becomes walk order).

### 2.5 Memory rebase at seal and kill sets

- **Seams:** I, E (kill sets).
- **Do:** split the counter into `base + young`, rebase at every seal, and
  release only young objects by count; add static statement kill sets. Depends
  on FIG-6672 being on main.
- **Done when:** the two-plan agreement law and the warm-equals-cold counter law
  after every seal pass, and register holdings are zero after every statement.
- **Deletes:** per-statement exact refinement, `reserve_held` frame pricing,
  `Pins.reservations`, and the memory-refusal clauses of the PLAN-INDEP laws
  FIG-6672 leaves (the bound is now conservative).
- **Gate:** lean, plus the vm bounds laws by selector.

### 3.1 hcharge (FIG-6694): formula charge for every helper

- **Seam:** B (library charge).
- **Do:** charge every library function its declared formula at completion and
  leave its body uncharged; seed the 569 body-only formulas from their bodies.
- **Done when:** the charge oracle (§B) and the helper differential pass with
  the new numbers, and every helper has a formula that bounds its body's work.
- **Deletes:** body-trace charging (`charged = !has_native()`, `call_units`'
  zero for body-only functions).
- **Gate:** lean, plus the helper differential, the vm bounds laws, and the
  durable image targets after re-recording (charges are saved meters).

### 3.2 hgen (FIG-6678): generated plans for closed helpers

- **Seams:** J, F (views), B.
- **Do:** land the `HelperPlan` types and generate plans for the 325 closed
  helpers, first `py.sub`, `py.mod` and `py.is_number`; guards decline before
  anything observable.
- **Done when:** plan-against-body equality (§J invariant 1) passes on the corpus
  and the helper differential.
- **Deletes:** each hand twin its plan replaces.
- **Gate:** lean, plus the helper differential and plan-independence targets.

### 3.3 Callback segments

- **Seams:** J, A.
- **Do:** generate resumable segments that yield at each callback site with the
  body frame's coordinate.
- **Done when:** both §J invariants pass for every callback-capable helper.
- **Deletes:** the interpretation of each helper its segment covers (the body
  stays as reference and fallback).
- **Gate:** lean, plus the helper differential and the durable image targets.

### 3.4 The instruction set

- **Seams:** B (regions), E (register windows), A (side tables).
- **Do:** a compact common encoding with a wide extension, and a cold,
  pc-indexed side table holding sites, roles, kill sets and charge events.
  Opcodes for the hottest native-stating operators, with the depth check first.
  Per-site monomorphic property caches holding a guard and a slot location,
  discarded on restore. Charge regions with fast admission and the verifier.
  Overlapping call windows. Rust `match` dispatch.
- **Done when:** the charge oracle passes under random region layouts, along
  with the lean gate and the §1 kbench rows.
- **Deletes:** Kir block versions and numeric specialization, `charge_nodes`
  and `cross_nodes`, `Op::Charge` coalescing, and `returns_borrowed` and
  `record_read_marker`.
- **Gate:** lean, plus the exec unit target, and the helper differential
  (operation dispatch changes).

## 5. Open questions

- **The tape under kdec-api.** `KernelMachine::record_charges` is public, like
  `verify_step_structure`. If kdec-api moves observers into `MachineOptions`,
  the tape becomes one of them; its contract does not change.
- **Sequence boundaries.** Positional chunks make middle inserts rewrite the
  suffix at the next projection (§H). Switching to content-placed boundaries
  with a rank index is a measured follow-on, not a stage-2 decision.
- **The record representation** (hash-indexed dense ordered, or small vectors)
  stays a stage-3 measurement. The pinned `Table` (entries in insertion order,
  index behind `Semantics::lookup`) fits both.

The 1.0 cut records and seals the first helper release fresh.

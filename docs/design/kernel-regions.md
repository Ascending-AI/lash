# Kernel regions: guarded compiled synchronous regions

Status: pinned design (FIG-6741) for the stage-4 follow-on of the kend spec
(`/workspace/notes/lash/tasks/lanes/kend/spec.md` §6: generated fused regions,
selective continuation conversion, guarded typed loop batches, build-staged
helper templates). It builds on [the kernel end state](kernel-end-state.md):
§B (the charge event program and its regions), §A (the coordinate grammar), §E
(the call ABI), §I (memory as a conservative bound) and §J (generated helper
plans). Every number below was measured on a prototype applied to
`415edf2ae2`. The prototype is not committed; its patch is
`/workspace/notes/lash/tasks/lanes/kend/smash/patches/fix-regions.patch`. The
active task and frame cursor (FIG-6737) landed after these measurements, and
they do not include it.

The FIG-6730 attribution showed that the warm rows miss their goals even if
every frame, allocation, memory, charge and projection instruction were free.
The cost is in the per-statement execution shape. Each statement is a step:
`run_slice`, `step`, `step_protected`, `advance`, the stream loop, then
`release_statement_roots` and `collect_arena`. Inside the step, every op pays
for a task and frame lookup, region admission, the operator's entry, its pins
and its formula. A region removes that shape for code that cannot observe it,
while the reference interpreter stays as the oracle and the fallback.

## 1. The region model

**A region** is a maximal synchronous span of code whose fast path contains no
effect, no park point, no task switch, no projection point and no slice or
bound crossing. Its whole effect on the run is a set of values written to slots
and registers, a charge, and occurrence counts. It runs inside one step. It is
committed at a statement boundary that the reference also reaches. The kinds
of region are built from one unit.

- **Region statement.** A statement that contains only region ops:
  - reads of unshared frame slots, resident constants and writes to unshared
    slots;
  - reads and writes of session bindings, which are region slots between
    boundaries (§3.6);
  - prepared operators (`Op::Operator`, or `Op::Nested` with `operator` set),
    that is, natives that state a prepared entry and a scalar charge table;
  - helper templates (§2.3);
  - `if`, conditional expressions (`JumpIfFalse`/`Jump` arms), discarded
    expression locals (`LetBind`/`LetEnd` that nothing reads) and `return`;
  - calls to region functions (call islands, below).
- **Loop region.** A `while` or `for` whose test and body are region
  statements. It runs as a batch of whole iterations.
- **Call island.** A call whose callee's body consists of region statements and
  calls to region functions. It runs as virtual frames. Self-recursion was
  prototyped; mutual recursion goes through a per-site plan cache (§2.2).
  Islands enter at nested calls, at root call actions (`call f(...)` bound
  by `let` or discarded by `do`, counting the action's occurrence), and at
  the callback sites of helper plans (`map`, `filter` and the other
  collection helpers whose plans apply a guest closure), where the helper's
  site stands for the call, the island counts no occurrence of its own, and
  the callback charges by its own code's charging.
  Inside, an island runs held-value operators by their prepared entries and
  measured formulas, natives with no operation by their prepared entry or a
  call that builds nothing, list and tuple item reads, and reads of session
  bindings (§3.6), so a top-level function that calls itself through its
  binding runs as an island.

**Barriers.** These ops end a region, so the reference runs them: every action
and effect (`perform`, `sleep`, `spawn`, `join`, `yield`, `cancel`); the host
observations `Clock`, `Random`, `HostRead` and `Print`; `Finish` and `Fail`;
`try`, `catch` and `finally` (handler entry); a `for` loop's protocol hooks,
which are callbacks; closure creation; and, in v1, allocation and heap
writes. Session reads and writes were barriers in v1; they are region ops
since R6 (§3.6). Allocation becomes a region op in a later lane
(§5, R5), with admission before growth (§3.5). An op that may raise is not a
barrier. A raise is a guard miss: the region exits and the reference raises
the error with the exact charge prefix.

**Formation at lowering.** `compile/lower.rs` classifies each op as a region
op or a barrier and writes a cold side table entry (`RegionPlan`) for each
loop statement and each function code whose statements all qualify. The entry
holds:

- per statement, the fixed units on each path: its statement unit (`advance`),
  the totals of the `Op::Region`s on that path, and the loop test's
  `costs.loop_test + metered`;
- per operator site, the maximum of its scalar charge table;
- for a loop, `iteration_max`: the test's plus every body statement's fixed
  units plus their formula maxima, with checked arithmetic;
- the call sites with their occurrence indexes, and the slots each statement
  reads and writes: frame positions, and session bindings by their index in
  the plan's own table of the bindings its statements use (§3.6).

A block's statements after its first barrier are left to the reference
unread. The region stops at the barrier, so it never reaches them, and
they neither make the plan allocate nor name its session bindings. Without
this rule, an allocating statement past a barrier would put every entry of
a loop whose region always stops at that barrier on the allocating path
for nothing (cold `dict_record.ts`, FIG-6755).

A verifier checks each summary against the stream's `RegionTable` events and
the statement units before the plan is admitted, as `RegionSummary::verify`
does for single regions. Plans belong to the `Executable`. They are
deterministic, immutable and never saved. Region formation is a plan decision
like the layout, so a layout bit turns regions off (§3.3). The compiled form
(§2) is built lazily on first entry and cached in the plan. The measured fixed
cost of compiling at first entry is within noise: the non-scaling part of
sum1m.ts is 1215.7 M instructions on base and 1215.2 M with regions; fib25.ts
is 1211.6 M and 1216.1 M.

**Guards.** A region carries the guards listed here. A failed guard exits the
region (§3.1), and nothing a run can see has changed.

| Guard | Checked | Prototype rule |
| --- | --- | --- |
| Kind and shape | at each operator site and slot access | The prepared entry's own guard. Operands and the result are scalars (`Int`, `Float`, `Bool`, `Null`, `Absent`), so the scalar charge table applies. A slot read finds `SlotState::Value`. A slot write's new value costs no more than the old one (`cost_of`); growth exits before the write, and the reference then calls `reserve_growth`. |
| Call target | at each island call site | The callee value is the island's closure handle (monomorphic) and the arity fits. |
| Charge headroom | per loop iteration; per island call and at commit | Loop: `c0 + iteration_max <= bound` and `c0 + iteration_max < slice_end`. Island: running units `<= min(bound, slice_end - 1) - c0`. |
| Memory headroom | at region entry | The census admits the region's maximum transient holdings: `bounds.memory - upper >= REGION_HOLD + depth_max * FRAME_BYTES`. |
| Depth | at each virtual call and each operation | `base + virtual depth <= call_depth`, where `base` is frames plus plans plus the session flag (K-BND-002). Helper templates check two more units, conservatively. |
| Environment | at region entry | No observer except the charge tape, no statement counts, no step verifier, no boundary hook, and the host does not observe sites (§3.2). |

## 2. Execution: closure-compiled guest regions, build-time helper templates

### 2.1 The decision

Guest regions run as **closure-compiled trees**. Library helpers on a region's
fast path run as **build-time generated scalar templates** emitted by the
helper compiler (FIG-6678). There is **no tier**: a region compiles on its
first entry.

- **Expressions.** The compiler evaluates a statement's register ops
  symbolically into a tree of `Box<dyn Fn>` nodes. Each node captures what is
  static: slot positions, resident constants, the prepared entry's function
  pointer, the scalar charge table and its mask sources, and the call site's
  occurrence slot. A node returns `Option<Value>`; `None` is a guard miss.
- **Statements and control.** These are small enum-dispatched trees: write,
  `if`, `return`, conditional arms.
- **Frames.** A region reads and writes the frame's `Slots` directly through
  one frame borrow per batch. An island's virtual frame is an array of values
  on the native stack, bounded by a virtual depth (400 in the prototype).

The evidence, as paired warm `instructions:u` (§4), M per run:

| sum1m row | Base | Interpretive batch | Compiled region |
| --- | ---: | ---: | ---: |
| TS | 7048.9 | 5210.0 (−26.1%) | 1136.0 (−83.9%) |
| Python | 7078.0 | 5238.0 (−26.0%) | 1317.1 (−81.4%) |

The interpretive batch is the ablation of compilation: the same loop batch,
guards and headroom, but each statement's Kir ops run through the reference
stream loop (`run`). It removes the step scaffolding, which is worth 26%. The
remaining per-op machinery (task and frame lookups, region admission, operator
entry with `pin_value`, `Fast::charge` and `charge_call`, and register moves)
is what the compiled form removes. After compilation, the sum1m profile is
dominated by the numeric semantics themselves: the operator closures with
their masks are 32%, and the prepared numeric entries (`numbers::plain`, the
prepared add and lt runners, `ts.add`) together about 25%.

### 2.2 Why the other forms lose

- **Build-time generation of guest code.** It is not available. Guest code is
  agent-written at run time, and Lash has no run-time code generation (unsafe
  code is denied and there is no JIT). Build-time generation is the right
  form for library helpers, because there it keeps one source of truth with
  the helper body under the plan-against-body law (§J). Hence §2.3.
- **Interpreting Kir inside a bigger step.** Measured above: −26% against
  −84%. Removing scaffolding alone leaves every row far from its goal.
- **Tiering** (interpret first, compile when hot). Nothing measured asks for
  it. Compiling at first entry has no measurable fixed cost (§1). A tier adds
  a counter per entry and a second state per site. Revisit it only if the cold
  row attributes region compile cost.
- **A typed micro-op array with `match` dispatch.** This was not prototyped.
  It uses the same guards and charge tables and adds a decode per op. It is
  the documented fallback form if closure code size or `Send + Sync` sharing
  of plans across machines becomes a problem.

Mutual recursion and calls to other functions use a per-call-site inline
cache from the closure handle (or the declared `CodeId`) to the callee's
plan. A miss exits the island.

### 2.3 Build-staged helper templates

Python's `n - 1` is `call lib py.sub`. That is a generated helper plan
(`LibRun::Plan`) with no prepared entry: it dispatches on `kind` and, for
integer by integer, calls its callee 2, the `sub` native. To be part of a
region, a helper needs a **template**: a prepared entry for each typed path
that reduces to one native call, plus that helper's scalar charge table. A
helper that answers a text composes up to eight prepared calls over
immediates in one path, since no single native gives its answer: TS
`String(n)`'s number-to-text is floor, compare, integer and digits in one
prepared step, not the heap paths (FIG-6777). The
table is `Fast::new(definition.charge, params, charge)`, so the charge is the
helper's formula at completion (hcharge, §B). Its guard declines on every
other kind.

The prototype hand-wrote one template for `py.sub` and `py.add`: an
`Int × Int` guard that delegates to the plan's callee 2's prepared entry and
charges the helper's own formula. Production generates these in hgen from the
helper body, under the plan-against-body and helper differential laws, and
never by hand. The same template gives `Op::Nested` calls of those helpers an
opcode fast path outside regions too.

## 3. Contracts

### 3.1 Exact charge

- **Summary inside the bound.** A loop iteration charges its test's fixed
  units plus their formula units when the test holds, then each body
  statement's fixed units plus its formula units. The formula units come from
  the operator's scalar table, indexed by the zero/nonzero small-integer mask
  over its measured sources. That is the table `Fast::charge` uses, so the
  region charges what the reference charges, by construction. An island sums
  the same quantities over its virtual frames.
- **No crossing inside.** The headroom guard admits an iteration only when
  `c0 + iteration_max` passes neither the bound nor the slice threshold.
  Charge is monotone, so no event inside can pass the bound, and no step
  boundary inside can reach the slice threshold. An island instead checks
  each event against the bound as it charges it, and the slice threshold at
  each step end, and exits before either is crossed.
- **Exits.** On a crossing risk, a guard miss, an error or a barrier, the
  region exits at the last committed statement boundary. The reference then
  replays from there, event by event, through `enter_region`,
  `admit_slowly` and `replay`: unit runs stop at the crossing unit, formulas
  charge atomically, and raises are charged their absent-result formula. This
  is FIG-6718's executed-prefix rule lifted to statements. The region never
  commits a suffix ahead and never subtracts.
  - **Loop exits** materialize the exact control state. `Control::While`'s
    `started` advances by the committed iterations. An exit before body
    statement `k` pushes `Control::Block { block: body, next: k }`, as
    `tested(true)` followed by `k` statement steps would leave it. An exit
    before the test leaves the loop where `next_iteration` is due.
  - **Island exits** convert the virtual frames (selective continuation
    conversion, §3.4). An island charges each event as the reference does,
    in its order, so nothing it charged is ever taken back. At a step end
    where the slice threshold is reached, or before a statement whose unit
    would pass the bound, the frames stand at that step end and the step
    ends. At an op the island cannot run (a guard miss, a value that would
    raise or that no island frame holds (§3.5), a charge that would pass the
    bound, a callee with no island, a depth bound), the top frame stands in its statement at that
    op with the registers it holds there, and the reference runs the
    statement on from that op in the same step, with islands held off until
    the step ends. The v1 all-or-nothing exit, with its per-slice abort
    rule, was never built: R4 went to conversion directly.
- **The tape.** With a charge tape attached, a region records its static event
  program at commit: the `RegionEvent`s of each executed path, the statement
  and loop-test units, and each formula's atomic units, in order. Recording
  never changes admission (§B). An island records each event as it charges
  it. This is required, not optional (§3.3).

### 3.2 Effect identity and coordinates

- **No effect inside.** Effects, waits, spawns, host reads and prints are
  barriers. A region does not park, so no request, wait id or delivery is ever
  produced inside one.
- **Occurrences.** Calls are counted occurrences (K-EFF-008: Call and Nested
  Call outside library units, including `call lib` helper actions). In a
  durable run an island advances the task's dense counters
  (`Occurrences::advance`) at the root call and at each call and counted
  helper site, where the reference counts them, so the next effect gets
  exactly the identity the reference would give it, and an exit leaves the
  counts the reference holds there.
- **Coordinates on exit.** A loop exit writes the semantic coordinates of
  §A: the `while` with its `started`, and the body block with its `next`. A
  committed island leaves no frame. The caller's statement continues in its
  registers, and its roles were never saved, so the caller's frame equals the
  reference's after `finish_action` (which clears `awaiting`, `nested` and
  `pending`). An island that exits leaves the converted frames (§3.4), each
  below the top standing at its call with its roles. Registers, closure
  handles and plan pointers are never coordinates.
- **Host site observation.** `Host::enter_site` is a host-visible event per
  occurrence, so regions are off while the host observes sites. A later lane
  may report the committed counts instead, if a host that observes sites needs
  speed.

### 3.3 PLAN-INDEP: the law to add

`lash-kernel-conformance`,
`tests::regions::regions_on_and_off_agree_programs_0` through `_7`:

The eight laws partition the region programs, the call-island programs,
corpus cases and smith seeds by program, so Buck can schedule them
independently within the existing test budget. Each keeps its program's
admitted document and both prepared layouts across the sweep; executions,
page stores and host scripts start fresh. Three region exit witnesses (nested
arms with a declaration, list iteration with continue, tuple iteration with a
declaration and break) run on every shard, so every law catches the R1/R2
charge and exit mutants on its own.

- **The cases.** Every corpus case, the 64-seed smith programs, and the
  region, allocating-region and island programs, each asserting that its
  regions or islands form with regions on and none with them off. Each
  allocating program also runs with memory off and under a 96 KiB memory
  bound. Each runs
  as written and wrapped in a function, as `scripts/perf/kernel/embedded.py`
  wraps its bodies. At top level, variables are session bindings, which
  regions hold as slots (§3.6). The prototype formed no region at top
  level, which is why its first check saw nothing.
- **Top-level sessions (R6).** The cold `sum1m` and `branch1m` shapes as
  kernel programs and lowered from both dialects, a session alias of a list
  written through and rebound, a binding that shrinks and grows by turns, a
  top-level loop beside a function's loop over the same names, and a park
  between two top-level loops. The sum shape and the park run on every
  shard. Each program that does not park also runs to its end unsliced and
  under slices 7 and 50, with memory accounted and off, and its memory
  counter must equal the reference's at every slice return and at its end:
  PLAN-INDEP leaves memory out, and this is what sees a session write's
  admission.
- **The two plans.** Regions on, and regions off through the layout bit, with
  everything else equal.
- **Equal across the plans:**
  - the end (value, error or bound) and the charge;
  - the expanded charge program (the tape);
  - prints, parks, requests, effect ids and ready order;
  - at every slice return and park, the sealed semantic checkpoint
    (`kernel_internals::conformance::same_checkpoint`: continuation bytes
    exact, graph equivalent).
- **The sweep.**
  - Bounds: the least and greatest bound each event of the program passes,
    its whole charge, and past it.
  - Slices: 1, 2, 3, 7, 50, 333 and unbounded.
  - Cross-resume: a checkpoint sealed under one plan resumes under the other.
- **Observers.** The law runs with the charge tape attached, so §3.1's tape
  recording is part of what it checks. Today's charge-bound law
  (`every_corpus_charge_bound_ends_where_the_per_unit_reference_charger_ends`)
  attaches the tape. The prototype turned regions off under any observer, and
  a +1-unit mutant passed that law unseen.
- **Mutants.** Each must fail the law once:
  - +1 unit on a loop commit;
  - +1 unit on an island commit;
  - a dropped occurrence count (a call site's, a helper site's, the root
    call's);
  - a skipped `started` increment;
  - an exit that materializes `next` off by one (at an op, at a step end);
  - a converted frame's roles dropped;
  - the island depth guard off by one;
  - a session write not admitted (the growth exit and the shrink's census
    move skipped);
  - a region spanning a park (its writes published without their write
    marks, so the park's projection does not see them);
  - a stale session read after a region exit (the exit publishes nothing).

The prototype checker (`LASH_PROTO_CHECK` in the patch's
`kernel_perf/embedded.rs`) checked a subset of this law. It used 9 wrapped
programs: the sum loop in both dialects, a mixed TS loop with a float step and
a string loop, Python int overflow to bigint inside a batch, fib(9) in both
dialects, a depth bound crossed inside recursion (`f(250)` against
`call_depth` 200) in both dialects, and Python calls inside a loop. It swept:

- every bound from 0 to 3000, plus the last 300 before the whole charge, and
  past it, with an unbounded slice;
- slices 1, 3, 7, 50 and 333 with an unbounded bound;
- three bounds for each of those slices;

sealing at every slice return and park. The result: **29,898 cases, 0
differences**, peak RSS at most 237 MiB under the 8 GiB cap. A +1-unit commit
mutant differed in 215 to 3,120 cases per program on all 6 programs it was run
on (`.kiln/FIG-6741/check/`).

### 3.4 Durability

- **No region spans a projection point** (park, slice return, end, seal).
  Projection points occur only between steps (`run_slice`), and a region lies
  inside one step: barriers exclude parks, there are no yields, and the
  headroom guard keeps the slice threshold out.
- **No pressure refinement inside.** `refine_resident` refines at a
  collection point, and only when an admission refuses. The memory headroom
  guard means no admission inside a region refuses.
- **Per-step bookkeeping.** Inside a batch, the per-statement
  `release_statement_roots`, `collect_arena`, kill sets and pin clearing are
  skipped. That is sound because a v1 region statement holds no heap value:
  its statement roots and pins are empty, and its scalar holds net to zero at
  statement end. An island builds nothing: the heap handles it holds (§3.5)
  are its arguments, captures and the items of those, all rooted by the
  frames and plans below it, and a native whose result it built exits. A region that allocates (R5) runs the statement's kill set at
  each statement end, or holds its temporaries in region-owned storage
  admitted at entry.
- **Selective continuation conversion (v2).** An island keeps each virtual
  frame in the coordinate grammar's shape: code, control position, and the
  pending call with its roles by site (the operands evaluated before it, for
  example `fib(n - 1)`'s value while `fib(n - 2)` runs). On an exit, at the
  slice threshold at a statement end or at a guard miss at a statement
  boundary, the virtual frames are converted into machine frames with
  `Pending` continuations. They are exactly what the reference would hold
  there, so a park after the exit projects the same bytes. Only islands
  convert; everything else stays a real frame.

### 3.5 Memory

- **Admission before growth.** A region's maximum transient holdings are
  admitted before entry, by the headroom guard over the census's own admission
  predicate (the prototype approximates it with `bounds.memory - upper`). The
  holdings are the scalar pins of its statements, and for an island its
  virtual stack with its variables, `depth_max × (FRAME_BYTES + 12 × 32)`:
  an island's variables hold only held values, at most 32 bytes each
  (`compile::islands::held`): scalars, texts and function names of at most 14
  bytes, task handles, and the identity handles of lists, maps, sets,
  records, closures and object references, whose content stays where the
  caller rooted it. A value no island frame holds exits before it is bound,
  written, passed or returned. A loop region's slot
  growth (an integer widening its `cost_of`) exits before the write, so the
  reference admits it with `reserve_growth`.
- **Plan dependence.** An island does not make the reference's per-call
  `FRAME_BYTES` and parameter growth reservations; it admits its bounded
  virtual stack once instead. Its ledger is therefore lower, still
  conservative. Memory is a conservative bound whose refusal point may depend
  on the plan within the transient slack (spec §2), and the counters are equal
  again from the next seal (SAM-4). PLAN-INDEP leaves memory refusal out; the
  two-plan agreement law covers it.
- **Allocating regions (R5).** A loop region whose statements build records,
  lists, maps or tuples, read members through property caches, write members
  or call natives enters only with a nursery of headroom (64 KiB) and no
  statement credit outstanding. Each statement holds what the reference
  holds, at the same anchors: a value's growth through the census before it
  is built, a member write's growth with its dirty mark, a native's held
  bytes before it runs and its result's pin after. At each statement end the
  region runs the reference's kill set: it releases the statement's roots and
  closes the arena's statement. A statement that may allocate runs whole,
  past the slice threshold, and the region exits at the next step boundary,
  where the reference would yield. A plan whose entries keep exiting on their
  first statement retires. The law
  `tests::regions::allocating_regions_hold_what_the_reference_holds` runs
  each allocating program unsliced and under two slices, with memory
  accounted and off, and requires the memory counter to equal the
  reference's at every slice return and at the end.

### 3.6 Session bindings (R6)

Top-level cell code reads and writes session bindings, not frame slots
(`K-SES-001`). Since FIG-6748 a binding the cell read or wrote is resident:
the session overlay holds its arena value, a write keeps the arena value
and admits only the binding's holding, and its page form, ids and dirty
marks come only at a boundary's projection. Between boundaries a binding is
therefore a plain resident value, as a frame local is, and a region holds
it as a slot.

- **Formation.** `Op::Session` and `Op::WriteSession` are region ops, and a
  Python statement-root invoke may take session atoms and write a session
  binding. A plan names the bindings its statements use, in the order they
  first use them (`LoopPlan::sessions`), and each summary states its reads
  and its write as frame positions or indexes into that table. The verifier
  checks both against the ops, as it checks frame slots.
- **Entry.** The region takes each binding the plan names at its resident
  value in the overlay. A binding the cell holds no resident value for (not
  yet read in this cell, or a binding shared through a cell) is a missing
  slot: a read or a write of it misses, the reference loads it, and the next
  entry finds it resident.
- **Aliasing.** A slot holds the binding's arena value, never a copy. A
  list read through a session binding is the object a frame slot or another
  binding holds, and a member write through either reaches that one object
  with its dirty mark, as the reference's does. While the region runs,
  nothing else reads or writes the bindings: it runs inside one step, and
  calls only natives and prepared operators, which reach no session, and no
  guest code (closures and islands are barriers to loop regions). So a
  binding's slot is the binding for the region's duration.
- **Islands.** An island reads a session binding but never writes one: the
  closure bodies of top-level code read their own and other top-level
  functions through their bindings. A read takes the binding's resident
  value when the island frame may hold it (§3.5), and otherwise misses at
  the head of its charge region, as an unbound frame variable does, and the
  reference loads it. Nothing writes a binding while an island runs: its
  statements write only frame variables, and its calls reach only islands,
  natives and prepared operators. Each entry therefore keeps the values it
  has read, and starts with none, since the reference may have written the
  binding between two entries.
- **Admission and memory.** A durable run that accounts memory moves the
  census from a binding's old holding to its new one at each write
  (`hold_session_binding`), admitting growth first. In a region, a write
  that would grow the holding misses and exits before its statement, and
  the reference admits it, as a frame slot's growth exits (§3.5). A write
  that shrinks the holding moves the census exactly as the reference's
  write does: `hold_session_binding(before, after)` with no growth, which
  takes no credit and cannot refuse. An equal write moves nothing. The
  reference's statement hold of the written value's cost is released at
  the statement's end and nets to zero, as scalar pins do (§3.4). A run
  with memory off (FIG-6731) holds no binding, and the region admits
  nothing.
- **Charge.** A session read is an op inside its statement's charge region,
  and a write ends its statement, whose unit is the statement's fixed unit.
  A session atom of an invoke charges its atom unit, as a local's does. So
  session reads and writes are fixed events of the region's static program,
  checked against the stream's `RegionTable` events like any other, and the
  tape records them unchanged. With charge off (FIG-6732), regions still
  form and run, paced (FIG-6775): they record nothing, measure no formula,
  and count the run's pace by their statements' fixed units, which lowering
  under Off leaves at their statement, action-atom and loop-test units, so
  the slice's end stops them where it stops the reference.
- **Exit.** Every exit publishes, before the step returns, each binding the
  region wrote: its last value, with a write mark that orders it among the
  bindings written since the last projection point, in the order of the
  region's last writes, after every binding written before the region. The
  reference orders bindings by their last writes too, so the next
  projection gives the same ids in the same order. A binding the region only
  read publishes nothing. The loop's control materializes as §3.1 states.
- **No region spans a boundary.** Projection points (a park, a slice
  return, the end, an explicit checkpoint) occur only between steps. A
  region lies inside one step: effects and waits are barriers, and the
  headroom guard keeps the slice threshold out (§3.4). Its publication
  happens at its exit, so every projection sees each binding at the value
  the reference would hold, and page forms, ids and dirty marks are given
  only there. A region whose slots outlived a boundary would leave that
  boundary's projection without its writes.

## 4. Prototype numbers

Prototype: the patch above on `415edf2ae2`. It contains:

- the loop batch (`machine/regions.rs`, `loop_batch`, hooked at the top of
  `next_iteration`);
- the call island (`call_island`, hooked at `Op::Nested`), with v1
  all-or-nothing exits;
- the hand-written `py.sub`/`py.add` templates;
- the interpretive ablation (`LASH_REGIONS=2`) and the checker.

Eligibility was decided at run time and cached per machine; production
decides it at lowering (§1). Both binaries were optimized Kiln builds of
`//crates/lash-dialect-typescript:kernel_perf__example`. The method was warm
differential `(I(2N) − I(N)) / N`, N = 2, two pairs with reversed order in
pair 2, base and prototype alternating, every process under an 8 GiB
address-space cap, with GNU time peak RSS
(`.kiln/FIG-6741/measure.py`, receipts in `.kiln/FIG-6741/paired6/`). Peers
come from the maintained `scripts/perf/kernel/embedded.py --differential-warm`
run on both binaries (48/48 samples, digests match across six engines; host
turbo2-sam, Ryzen 9 5950X). Results and charge are equal, base against
prototype, in every sample.

| Row | Base M | Prototype M | Change | Peer M | Base ratio | Prototype ratio | Goal | Peak RSS MiB | Load1 |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| sum1m TS / QuickJS | 7050.023 | 1136.029 | −83.9% | 416.846 | 16.91× | **2.73×** | ≤3× | 46.50 | 15.69–16.72 |
| sum1m Python / Monty | 7078.025 | 1317.064 | −81.4% | 983.627 | 7.20× | **1.34×** | ≤2× | 45.50 | 15.39–15.55 |
| fib25 TS / QuickJS | 2666.231 | 178.008 | −93.3% | 89.107 | 29.92× | **2.00×** | ≤3× | 46.75 | 15.39–17.44 |
| fib25 Python / Monty | 4847.929 | 354.010 | −92.7% | 432.556 | 11.21× | **0.82×** | ≤2× | 45.75 | 17.25–17.47 |

Pairs agree to within 0.2 M on every row. A build without the ablation switch
measured 1112.0 M on sum1m.ts and 1293.1 M on sum1m.py: the switch's check in
`next_iteration` costs about 24 instructions per iteration. Wall time was not
measured on a quiet host.

**How far it gets.** Both shapes reach their goals. The loop shape takes sum1m
to 2.73× and 1.34×; the call shape takes fib25 to 2.00× and 0.82×. No contract
was relaxed: exact charge, effect identity, PLAN-INDEP and durability hold as
§3 states, and §3.3's checker found no difference.

**What the other rows need.** The prototype does not reach them, and no
contract stops them:

- **branch1m:** `if`/`else`, `break` and `continue` inside loop bodies. The
  island already compiles `if`; loop bodies need the same, plus exits from
  inside nested blocks, which materialize `Control::Block` chains.
- **for loops:** `range` and list cursors stepped by the region, with cursor
  coordinates materialized on exit (`TableCursor`, `started`).
- **dict_record and allocation_churn:** allocation and heap writes inside
  regions (R5), with nursery admission before growth, per-site property caches
  (§3.6 of the spec) and dirty marks. Dirty marks are bookkeeping, not
  projection points, so durability permits them.
- **Python operators beyond add and sub:** generated templates for the rest
  of the arithmetic, comparison and truth helpers (R3).

## 5. Rollout and deletions

Each lane works on current main in its own fork. Each gates with the lean gate
of [the kernel end state](kernel-end-state.md) §4, plus the regions law (§3.3)
from R1 on, plus its kbench rows as paired `instructions:u` (no row above +1%).
Prototype switches (`PROTO_REGIONS`, `PROTO_INTERPRET`, `LASH_REGIONS`,
`LASH_PROTO_CHECK`, `loop_interpret`) never land.

**R1. Region formation, verifier and the law** (opus: exactness contract).

- **Do:**
  - the `RegionPlan` side table at lowering (§1), with eligibility, summaries
    and the verifier;
  - the layout bit that turns regions off;
  - tape recording for regions (§3.1);
  - `regions_on_and_off_agree_programs_0` through `_7` with their mutants (§3.3).
- **Deletes:** the prototype's run-time eligibility lookup and per-machine
  caches; plans are per `Executable`.
- **Coordinate with:** fix-formula. The region's scalar table and `Fast` are
  one table, so `Fast::charge`'s `unit` and `small` paths become that table's
  only implementation.

**R2. Loop regions** (opus).

- **Do:**
  - the closure compiler for region statements;
  - `while` batches with `if`/`else`, `break` and `continue` in bodies;
  - `for` over `range` and lists;
  - exit materialization (§3.1).
- **Deletes:** `Via.fuse_list` and the fused list-cursor handoff in
  `machine/loops.rs`. A list-walking loop region does that handoff, and the
  layout-gated special case goes.

**R3. Helper templates in hgen** (sol: specced by §2.3).

- **Do:**
  - generate a prepared entry and a scalar table for every closed helper whose
    typed paths reduce to one native call (Python arithmetic, comparison and
    truth helpers; TS `ts.*` operators);
  - extend the plan-against-body law to templates.
- **Deletes:** the hand-written prototype templates. Subject to the lane's
  measurement, it also deletes the Python lowering's static-type tier
  (`lower/expressions.rs`, `binary`: direct `add`, `sub`, `mul` and
  `text.concat` when both operand types are inferred), keeping one lowering per
  operator.
  - **When to delete:** only if the template path is within 1% of the direct
    native on sum1m.py.
  - **Cost:** the change alters documents and charges (the helper's formula
    replaces the native's). Dev phase: images re-recorded.

**R4. Call islands, then selective continuation conversion** (opus:
concurrency, durability and coordinates).

- **Do:**
  - islands with per-site plan caches (mutual recursion, declared and closure
    callees);
  - occurrence counting, the depth guard and the v1 per-slice abort rule;
  - then v2 conversion of virtual frames into machine frames at exits (§3.4),
    with the §A restore validation over converted frames.
- **Deletes:** v1's abort-and-replay path for headroom exits, once v2
  converts.
- **Done when:** the regions law passes with islands under every slice and
  bound in its sweep, and fib25 holds its rows.

**R5. Allocating regions** (opus: memory admission).

- **Do:**
  - records, lists, maps and tuples built inside regions, with nursery
    admission at entry;
  - property-cache guards;
  - dirty marks;
  - statement kill sets at region statement ends.
- **Done when:** dict_record and allocation_churn move toward their goals,
  with the two-plan memory agreement law green.
- **Deletes:** nothing beyond what R2 and R3 delete, unless its measurement
  shows a fast path it subsumes. The reference interpreter stays: it is the
  oracle and the fallback every region exits to.

**R6. Top-level session regions** (opus).

- **Do:** session reads and writes as region ops in loop regions, with the
  rules of §3.6; the top-level programs and the session mutants of the
  regions law (§3.3).
- **Deletes:** nothing; the formation rule can exclude sessions again.

## 6. Open questions

- **Sessions.** Resolved (FIG-6755): session reads and writes are region
  ops, and bindings are region slots between boundaries (§3.6).
- **Site observation.** Can a host that observes sites take committed counts
  (§3.2) in place of per-occurrence `enter_site`?
- **Quiet-host wall time.** It is unmeasured.

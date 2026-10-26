# 0139: The Lash VM is a dialect-free kernel

Recovery and external-effect guarantees follow [What survives a crash](../../README.md#what-survives-a-crash).

Status: Accepted. Design: [the kernel design](../kernel/design.md); rules:
[the kernel semantics](../kernel/semantics.md).

## Context

Hosts read, edit and run workflows, and models write them in more than one
language. A representation shaped by one language makes every other language
emulate it, makes a host learn that language to edit a workflow, and binds
saved runs to one implementation's layout. Running a stock language runtime
behind a sandbox cannot save a run at a wait without replaying code, which
[ADR 0132](0132-durability-is-state-first-over-the-lash-store.md) §2 rules
out.

## Decision

### One kernel, no source language

The Lash VM is one small language with one meaning: the kernel. Its values,
forms, evaluation order, statement rule, task scheduling, site derivation,
cost table and parked-run schema are written down rule by rule in
`docs/kernel/semantics.md` (`K-…` ids) and pinned by a kernel version
(`K-VER-001`). Nothing in a document's meaning names a dialect.

- **Values.** Null and absent are distinct; integers have arbitrary precision
  and floats are IEEE doubles; numbers compare by mathematical value. Lists,
  maps, sets and records are mutable heap objects with identity; closures
  capture variables by reference. Every operation is total over its declared
  operand types and raises a typed error on any other value: nothing coerces.
- **Forms.** The forms are closed. Actions include `perform`, task joins, `yield`, `spawn`, `cancel`, calls
  and dialect-policy `await`. An action may occupy a statement's right-hand
  side or a typed nested `Expr::Act` position. Saved frames retain the nested
  action's semantic site and operand roles (`K-EVAL-008`), not compiled slots.
  Clock, random and sleep are ordinary operations, never kernel forms.
- **Tasks.** A run is a set of tasks scheduled first in first out, one at a
  time. A run that ends with an unfinished task, or with an error no `join`
  observed, ends in a typed error.
- **Library functions.** A library function is a content-addressed definition
  with a typed signature, its error kinds, a charge formula and a kernel-code
  body, a native implementation, or both. A Document names its direct build-library functions in `LibraryDependencies`.
  Document-local Imports name ordinary code, operations, types and errors;
  fulfilment checks kind, signature, visibility and package ownership and
  records the fills in an immutable Link. Admission refuses unmet requirements.
- **Documents.** A workflow Document is typed kernel code with a content identity. Hosts edit it only through Draft's typed operation batches, then independently publish its resulting content. A saved callable opens by materializing frozen values and reachable code into that representation.
- **Parked runs.** A run executes on one persistent heap and is saved, in
  the document's terms, as a checkpoint root over that heap's pages
  whenever it parks
  ([ADR 0145](0145-kernel-execution-and-its-checkpoints-share-one-persistent-heap.md)).
  It resumes exactly after the executable is rebuilt, reading only the
  pages its next steps touch. Committed work never runs again.
- **Versions.** A kernel version may break when a pinned rule is wrong,
  unsafe or unbounded. A breaking version ships a document migration and a
  parked-run migration under
  [ADR 0106](0106-durable-formats-upgrade-by-migration-or-drain.md) and
  [ADR 0115](0115-the-1-0-binary-carries-its-half-of-every-upgrade.md).

### Dialects lower to the kernel

A dialect lowers source to kernel Documents and prints supported Documents as equivalent standalone source, refusing unrepresentable content with a typed reason. Build helpers remain exact FunctionId dependencies outside package Documents and Links. Source print/import does not grant helper authority; fresh publication checks the installed environment, while parked restore retains its existing compatibility proof. A definition can be printed in a selected dialect only when the printer proves that representation faithful.

Packages remain the distribution mechanism for user- and host-installed code.
A Package Publication supplies scoped provenance and members; its presence does
not put them in the Tool Catalog. Extra visible members do not enlarge a
selected execution's reach. Links retain the full reached DAG, so calls and
callbacks use their defining Link and Import slot, including after a cold
restore. `OperationUse` preserves that identity, while a process permits one
provider and tool per operation (ADR 0141 §5).

A parked root records its HelperLibraryId. An equal installed identity requires
no compatibility walk; a changed identity checks the saved live helper reach
and refuses a missing FunctionId before execution resumes.

For each construct a front end picks the first tier that applies: a kernel form or
library function with the same meaning, the difference compiled in as kernel
code or a helper call, an extension function for what kernel code cannot
reasonably express, or a typed refusal with a repair hint. A dialect needs no
kernel change; Python is the proof.

A front end may trust declared and inferred types to choose a direct
mapping. Where the type is wrong at run time the strict operation raises a
typed error instead of coercing, so a dialect is deliberately stricter than
its language there. Each dialect keeps a deviation register with a test per
entry.

### The TypeScript dialect

The TypeScript dialect (`lash-dialect-typescript`) accepts a broad subset of
TypeScript and refuses the rest by stable `TS_*` diagnostic. Async functions
run as kernel tasks with the interleaving Node gives. Every way it differs
from ECMAScript on code it accepts is a row of its deviation register
(`crates/lash-dialect-typescript/deviations.md`). Test262 is the oracle:
lash vendors the tests the dialect supports and records one outcome per test,
and every test the record marks as passing passes on the kernel or is named
by a register row (`scripts/check_test262_ratchet.py`). A conflict between a
Test262 case and a kernel rule is a register row, never a kernel change.

Async bodies use the generic kernel `return value via policy`: the value
is evaluated at the return site, held while source `finally` blocks unwind,
and passed to the policy only if that return survives. A finalizer's own
departure, including cancellation raised at its wait, replaces the held
return and drops its policy. The TypeScript dialect supplies
`ts.promise.adopt` for every async body, including entries and frozen
function values; `ts.promise.run` only settles the enclosing task's promise.
The policy runs through ordinary library call frames after the source scopes
have gone, so its result or raise cannot re-enter those handlers. A checkpoint
inside cleanup records the held value and return site as a typed deferred
completion; a checkpoint inside the policy records the ordinary call frames.
Entry argument binding and final Absent-to-Null normalization remain
synchronous and carry no continuation state.

### Processes are entries started by effects

The kernel has no process form. A host starts `main` or a function Export.
A function Export is startable exactly when its parameters and settled result
are data-only; synchronous functions qualify too. The Export's signature and
`StartConvention` determine argument binding. An Export start needs no `main`
and never runs it. The checker derives eligibility, and runtime validates the
actual arguments and result. Starting and awaiting a durable
process are effects lash supplies (`processes.start`, `processes.await`); the
start effect takes a function reference to an entry of the document. A
process definition is an immutable value: its id is
`lash.definition:sha256:<hex>` over the engine kind, the canonical value
naming the selected linked code, and the artifacts it holds, and the
document it runs is held as an artifact like any other
([ADR 0113](0113-artifacts-are-kept-alive-only-by-their-referrers.md)). A
process handle is `{ "__handle__": "lash", "id": "p.<process id>" }`. A start
is identified by its recorded start key
([ADR 0107](0107-a-process-is-named-by-a-minted-id-a-start-by-its-key.md)).
Hosts own definition names and versions.

Entry promotion requires written types for every public parameter and the
settled result (FIG-7141). Entry parameter record annotations default to
accepting and dropping undeclared fields before the body runs, recursively
through declared fields and containers (FIG-7206). Required fields remain
required and declared fields remain type-checked. TypeScript
`[key: string]: never` or Python `TypedDict, closed=True` explicitly closes
a record; `[key: string]: JsonValue` or `extra_items=JsonValue` retains extras.
Settled results and non-entry types keep their existing meaning. The kernel
records the choice as `RecordRest::{Closed, Drop, Keep}` and its decoder owns
the projection, under `K-ENTRY-CONTRACT`.

### The kernel crates stand alone

The kernel is the crates `lash-kernel-*` (document, checker, editor,
library, machine, parked state, dialect interfaces, conformance) with the
dialect packages `lash-dialect-*` and the extension packages `lash-ext-*`
beside them. These crates depend only on each other and on third-party
crates, so the set can move to its own repository; the machine does no I/O,
keeps no global state and returns to its embedder at every park and fuel
slice. Lash embeds it: model code runs and lowers only in resettable worker
processes ([ADR 0123](0123-model-code-runs-in-resettable-worker-processes.md)),
and the broker commits every park with the admission of the effects it
requested. `scripts/check-kernel-boundary.py` holds three rules: the kernel
set reaches nothing else in lash; no language-neutral crate (a kernel crate,
a lash core crate or a store) links a dialect, except the composition crate
`lash-kernel-engine`; and no dialect links the
machine or its parked state outside its tests.

## Consequences

- A host reads and edits one representation whatever the workflow was written
  in, and needs no dialect code to change an effect argument, insert a
  statement or replace a condition.
- A dialect is added without touching the kernel, the broker or the stores.
- A parked run survives a rebuilt executable and a swapped native
  implementation, because it names sites and identities, never bytecode
  positions.
- A TypeScript program that relied on coercion of a mistyped value gets a
  typed error; the register names each such case.
- State written before the kernel is unreadable by design.

## Alternatives considered

- **An ECMAScript-shaped IR.** Every other language would emulate
  ECMAScript, and every host would need ECMAScript to edit a workflow.
- **A stock runtime behind a sandbox.** It cannot save a run at a wait
  without replaying its code (ADR 0132 §2).
- **Declared pure functions.** Whether a call waits is syntactic under the
  statement rule; a second function kind adds a link-time ban and nothing
  the rule does not already give.

## Executable evidence

- The kernel corpus: `crates/lash-kernel-conformance`, one named case per
  rule, form, parked-run row and edit.
- Test262 on the kernel: `//crates/lash-dialect-typescript:test262_kernel__test`
  and `scripts/check_test262_ratchet.py --kernel-outcomes`.
- The minimal standalone embedder: `examples/kernel-embedder`.
- The crate boundary: `scripts/check-kernel-boundary.py` and its test.
- Worker-owned execution: `scripts/check-vm-parent-paths.py` and
  `scripts/check-vm-static-state.py`.

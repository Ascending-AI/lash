# 0139: The Lash VM is a dialect-free kernel

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
- **Forms.** The forms are closed. Every wait (`perform`, `sleep`, `join`,
  `yield`, `spawn`, `cancel`) and every call that may wait is the whole
  right-hand side of its own statement, so a task pauses only between
  statements and no unnamed value is live when it does.
- **Tasks.** A run is a set of tasks scheduled first in first out, one at a
  time. A run that ends with an unfinished task, or with an error no `join`
  observed, ends in a typed error.
- **Library functions.** A library function is a content-addressed definition
  with a typed signature, its error kinds, a charge formula and a kernel-code
  body, a native implementation, or both. A document's manifest names every
  function and effect it needs by identity; admission refuses a document the
  environment cannot meet.
- **Documents.** A workflow document is a kernel program with typed entries.
  Its identity is the hash of its content without annotations. Hosts edit it
  through typed, serialisable transactions and read correspondence for every
  node they touch.
- **Parked runs.** A run is saved in the document's terms, one fragment per
  root, whenever it parks, and resumes exactly after the executable is
  rebuilt. Committed work never runs again.
- **Versions.** A kernel version may break when a pinned rule is wrong,
  unsafe or unbounded. A breaking version ships a document migration and a
  parked-run migration under
  [ADR 0106](0106-durable-formats-upgrade-by-migration-or-drain.md) and
  [ADR 0115](0115-the-1-0-binary-carries-its-half-of-every-upgrade.md).

### Dialects lower to the kernel

A dialect is a package: a front end that lowers source to a kernel document,
a printer that spells any document as equivalent source, and the helper
functions its front end emits. A workflow has exactly one dialect. For each
construct a front end picks the first tier that applies: a kernel form or
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

### Processes are entries started by effects

The kernel has no process form. A document lists its entries, the functions
a host may start, with typed signatures. Starting and awaiting a durable
process are effects lash supplies (`processes.start`, `processes.await`); the
start effect takes a function reference to an entry of the document. A
process definition is an immutable value: its id is
`lash.definition:sha256:<hex>` over the engine kind, the canonical value
naming the document and the entry, and the artifacts it holds, and the
document it runs is held as an artifact like any other
([ADR 0113](0113-artifacts-are-kept-alive-only-by-their-referrers.md)). A
process handle is `{ "__handle__": "lash", "id": "p.<process id>" }`. A start
is identified by its recorded call and attempt
([ADR 0107](0107-a-process-is-named-by-a-minted-id-a-start-by-its-key.md)).
Hosts own definition names and versions.

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
a lash core crate or a store) links a dialect; and no dialect links the
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

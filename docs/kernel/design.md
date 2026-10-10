# Lash kernel: one dialect-free workflow language, many front ends

Status: proposal for Sam, revision 4, 2026-10-09. Whole-hog: this is the end state and a clean cutover. No dual IR, no reader for today's graph, continuation or snapshot formats, no transition plan. Research: `/workspace/notes/lash/tasks/lanes/wfrep/`. Reviews of revisions 1 to 3: `/workspace/notes/lash/tasks/lanes/kspec/`.

## 1. What this is for

1. A stable host-facing representation of a workflow that a host can read, edit and interpret.
2. Several source dialects (TypeScript, Python, others) running on one engine.
3. A dialect author can support any reasonable subset of their language.
4. The representation is dialect-free. A host supplies effects; installed packages supply library functions; the document's manifest says which of each it needs. Nothing in a document's meaning names a dialect.

Out of scope, by standing ruling: running a stock runtime behind a sandbox; journal-and-replay of code (ADR 0132 §2); a wire protocol (ADR 0136); host event routing and scheduling (ADR 0137).

**Settled by Sam (2026-10-09).** Three tiers; an expansive kernel library; trusted dialect authors; regex outside the kernel; functions shipped by packages in their own registry, not mixed into tools, and any package may define them; one dialect per workflow; arbitrary-precision integers; every wait is its own statement; effects may appear in any function; one mechanism for library functions; a dialect can print any document as equivalent source, with no promise to reproduce the original text; hosts edit kernel statements directly, with no grouping mechanism; async functions run concurrently as the dialect's language would run them, through kernel tasks; a run that ends with an unjoined task is an error, and a dialect compiles in any other behaviour; kernel versions may break and each break ships a migration; front ends trust type annotations and document the deviation; crates named `lash-kernel-*`; build it directly and test the TypeScript dialect against the ECMAScript tests lash supports.

**Written in on the reviewers' agreement, open to Sam's objection.** Numbers compare by mathematical value; the statement rule is syntactic; no typed-binding form; a task's error is data on its handle; outcomes may be delivered in any order.

## 2. The kernel

The kernel is one small language with one meaning. It replaces the ECMAScript-shaped IR.

### 2.1 Values

| Kind | Notes |
| --- | --- |
| null, absent | Distinct. Absent is the result of reading a missing optional field and the value of an omitted optional argument. |
| bool | |
| integer | Arbitrary precision. |
| float | 64-bit IEEE. Mixed integer and float arithmetic converts the integer to a float. Division is explicit: `div` (float result), `div_floor`, `div_trunc`, each with its matching remainder. Integer division by zero is an error; float division by zero follows IEEE. |
| text | Unicode scalar values, ordered by code point. Library functions address it by code point and, separately, by UTF-16 unit. |
| bytes, timestamp | Immutable. |
| tuple | Immutable, fixed length. |
| list, map, set, record | Mutable heap objects with identity. Map and set keep insertion order. |
| closure | Captures variables by reference: a closure and its defining scope share the variable. |
| error | `kind`, `message`, `data`. |
| task handle | A task started by `spawn` (§2.3). It holds the task's state and, once the task ends, its result or error. Joining it twice gives the same answer. |
| function reference | A closed declared function of the document, by name. It is data: it captures nothing and may be passed to an effect. |
| handle | A typed reference to a host resource or a host projection. |

**Equality and keys.** `eq` is structural on immutable values and on the contents of lists, maps, sets and records; `same` is identity. Numbers compare by mathematical value: integer 1 equals float 1.0, and `-0.0` equals `0.0`. An integer is compared with a float exactly, against the float's true value and without converting the integer first, and equal numbers hash alike. NaN is not `eq` to itself. A map key or set member is an immutable value (null, bool, number, text, bytes, timestamp, or a tuple of these), compared by `eq` except that NaN is one key, or the identity of a heap object, taken explicitly with `ref(x)`.

**Strict operations.** Every kernel operation and library function is total over its declared operand types and raises a typed error on any other value. Nothing coerces. This is what makes a tier 1 mapping sound: a front end that chose `add` because it believed both operands were numbers gets a typed error, not a wrong answer, if one is text. A type-directed tier 1 mapping may only target a function whose domain is exactly the believed type, so the library carries type-specific functions (`num.eq`, `num.lt`, `list.get`, `text.len`) beside the generic ones. There is no typed-binding form; a front end that wants an earlier error emits an explicit check call.

**Cycles.** A heap graph may share objects; a cycle is refused where a value must be saved or passed to an effect, as today.

**The effect boundary.** Effect arguments and results are copied: a tool never shares identity with guest objects, and a result is a fresh graph. Aliases between guest objects survive a wait. Each `perform` states the result type it expects, and the result is decoded by that type: `Int` and `Float` fields decode exactly as declared, and a bare `Number` decodes a JSON number with no fraction or exponent as an integer and any other as a float. Most tool results are untyped, so the manifest states the document's policy for a bare number once (float for a TypeScript document, by spelling for a Python one), and a site overrides it only where the effect's signature says more. The effect-value adapters that today reduce every number to an f64 before the worker sees it are rewritten to carry the number token to this decoder.

**Projections.** A host projection is a handle. Reading through it is a request to the host that returns kernel data (ADR 0132 §9). Providers answer no language-shaped question.

### 2.2 Forms

Closed and few. A form is added only by a new kernel version.

- **Data and binding:** literal, variable, `let`, assign (to a variable, field or index), tuple, list, map, set and record construction, field and index read.
- **Control:** `if`, loop over a collection, `while`, `break`, `continue`, `return`, `try`/`catch`/`finally`, `throw`.
- **Functions:** function and closure, call. There is one kind of function. Whether a call is free of effects is derived, never declared.
- **Waits:** `perform` (run one effect and bind its result), `sleep`, `join` (wait on one task handle, or on a list with mode all, all-settled, race or any), `yield` (let other ready tasks run).
- **Tasks:** `spawn` and `cancel` (§2.3).
- **Host reads:** clock, random and a read through a projection handle. They are answered at once by the embedder and are not waits. A read in a stretch that was not saved is drawn again after a crash (ADR 0132 §8).
- **Terminals:** `print`, `finish`, `fail`.

There is no process form. Starting and awaiting a durable process are effects that lash supplies; the start effect takes a function reference to a declared function of the document. A document lists its entries (the functions a host may start, with typed signatures). The kernel does not know lash's process model.

**The statement rule (syntactic).** A call may sit inside an expression only when its callee is a library function that has a native implementation, which cannot wait by construction (§2.4). Every other call (declared functions, closures, helpers and any library function with only a kernel-code body) and every `perform`, `sleep`, `join`, `yield`, `spawn` and `cancel` is the whole right-hand side of its own statement, with variables or literals as arguments. The rule is checked locally on each statement at admission, for documents, host edits and library bodies alike; nothing is derived about what may wait and no edit elsewhere can invalidate a statement. A task therefore pauses only between statements, and no unnamed intermediate value is live when it does.

**Hoisting.** A front end hoists to satisfy the rule, in the source's evaluation order. Every operand the source evaluates before a statement-position call is bound to a temporary first: `f(a, g(b))` becomes `t1 = a; t2 = g(b); f(t1, t2)`, because another task may change `a` while `g` waits.

**Evaluation order** is left to right, operands before operation, and is part of the kernel version.

**Iteration.** A loop over a list reads live by index. A loop over a map or set visits in insertion order, sees entries added during the loop and skips entries removed before their turn. A dialect whose language differs compiles the difference in.

### 2.3 Tasks

A run is a set of tasks. `main` is the first.

- `spawn f(args)` creates a task that runs `f`, and returns its handle. The new task runs at once, up to its first wait or its end; then the spawning task continues. (This is how a JavaScript async function behaves. A dialect whose language queues a new task instead, as Python's `create_task` does, makes `yield` the task's first statement. A Python coroutine that is only awaited is an ordinary call.)
- One task runs at a time, until it reaches a wait or ends. Then the next ready task runs, first in first out. There is no parallelism inside a run.
- A wait that completes makes its task ready, at the back of the queue. A `join` on a handle that has already ended continues at once. When a task ends, every waiting join it decides, on its handle alone or on a list, becomes ready in join-start order.
- `join` on a list: all returns when every member has ended or at the first failure; all-settled when every member has ended; race at the first member to end; any at the first to succeed. Members not yet ended keep running.
- **Errors.** An error that ends a task is data on its handle, raised at each `join` that observes it, and nothing more.
- `cancel h` raises a cancellation error in the task at its current wait. Its cleanup blocks run and may wait. Cancelling an ended task does nothing.
- **Parks and delivery.** When no task is ready the run parks. Every effect requested since the last park is admitted together with the saved state, in one transaction. The embedder delivers committed outcomes in any order, and several may be delivered before the next save; a save is required only before effects are admitted. Because lash saves state and never replays, the delivery order need not be reproducible.
- **Ending.** A run ends when `main` ends, or at `finish` or `fail`. A task that was a member of a `join` the code has already passed is cancelled then. Any other task that is unfinished, or that ended in an error no `join` observed, makes the end a typed error naming those tasks. The library function `tasks.unfinished()` returns the unfinished handles, so a dialect whose language waits for them (Node) or cancels them (asyncio) compiles that in.
- **Identity.** A task is identified by its spawn site, that site's occurrence in the spawning task, and the spawning task. An effect is identified by its task, its site, the site's occurrence within that task, and the loop context. So a host can tell which element of a fan-out an effect belongs to.
- **Bounds.** The number of live tasks, of effects admitted at one park and of members of one `join` are execution bounds the embedder states, like instructions and memory.

The scheduling rules above are part of the kernel version.

How source maps: `await tool(x)` is one `perform`. `const p = tool(x)` is a `spawn` of a function that performs it, and `await p` is a `join`. `Promise.all(xs.map(async x => …))` spawns one task per element and joins the list; `asyncio.gather` does the same.

### 2.4 Library functions (one mechanism)

A library function is a content-addressed definition with a name, a typed signature over kernel values, its error kinds, and either a kernel-code body, a native implementation, or both.

- **The kernel library** is the set the engine ships: arithmetic, comparison, text, slicing and negative indexing, `sum`/`any`/`all`/`map`/`filter`/`sort` (stable), membership, split/join/format, JSON, math.
- **A helper** is a function a dialect package ships with a kernel-code body, to carry a difference between its language and the kernel.
- **An extension function** is one a package ships with only a native implementation, for what kernel code cannot reasonably express: `regex.ecma.exec`, `url.whatwg.parse`.

A document references functions by identity and lists them in its manifest; it does not contain their bodies. A host can resolve and read any body. Changing behaviour means a new definition with a new identity. A host adopts a corrected function with one edit that replaces identity A with B across a document.

A function with a kernel-code body is ordinary kernel code: it obeys the statement rule, may call a function argument, and so may wait if its callback does. Nothing about the registry makes a call effect-free.

**Admission test for the kernel library.** A function belongs in it only if its meaning fits in a sentence that names no language, two dialects would map to it directly or expansions need it as a building block, and every edge is pinned. Coercing addition, any language's truthiness and operand-returning logical operators fail the test and are helpers.

**Native implementations.**
1. **Charge.** A function's identity includes a charge formula over the sizes of its arguments and result. That formula is what the caller is charged, whichever implementation runs and whether engine caches are cold or warm.
2. **Guard.** A function that can run away (a backtracking regex) also states, in its identity, a work unit and a limit as a formula over its arguments. The implementation counts that unit deterministically, independent of caches, and a call that passes the limit ends with a typed bound error. Because the unit and limit are in the identity, the point of failure is pinned like any other behaviour. Memory for engine-side caches is the embedder's, bounded by its worker limits, and is never a guest-visible failure.
3. **Values in, values out.** A native function sees only its arguments and its counter, does no I/O and is deterministic. The conformance harness re-runs every native function and compares results. Native functions are registered by the embedder in Rust at startup, never loaded from a document.
4. **No callbacks.** A function that takes a function argument has a kernel-code body and no native implementation.
5. **No mutation.** Extension values are kernel data: a regex is a record `{brand, pattern, flags, lastIndex}`. `regex.ecma.exec` returns the match and the next `lastIndex`; the helper that wraps it writes `lastIndex` back to the record, so aliases see the update.

## 3. The document

- The workflow document is a kernel program: `main`, declared functions (including those run as processes), private bindings. It is total (every construct is a typed node or a typed expression inside one) and it is the only authority.
- Derived, never written by a host: node ids, edges, scopes, type facets, effect sets, execution sites. lash recomputes them on every change.
- **Requirements manifest:** the kernel version, the effects called (name and signature) and every library function referenced, directly or through another function's body (identity). Admission refuses a document whose requirements the environment does not meet, naming what is missing.
- **Identity.** A document's behavioural identity is the hash of its content without annotations. Annotations (labels, layout, the dialect it was written in, authored source) are a separate layer keyed to that identity and attached to nodes; they move with a node as a label does, never change behaviour, and may be dropped.
- **Edits.** Typed, transactional edits and re-admission keep their present shape. Every expression payload changes to kernel forms, and edits and transactions become serialisable data; both are new work. A host edits kernel statements directly. Hoisted temporaries are ordinary variables, and the scope check refuses an edit that uses one before it is bound.
- **Sessions.** Each REPL cell is its own kernel program. Session state is the session's bindings. A closure or task handle does not outlive the cell that created it, so a parked run names one document. A host keeps a function across cells as a saved function: the closure's code and the variables it read, frozen as data at its cell's end, declared again in the document of each cell that names it (`lash-kernel-dialect`, `SavedFunction`).
- **Kernel text.** lash owns one plain text notation for the kernel, for logs, diffs and showing a document to a model.

## 4. Dialects

A dialect is a package of a front end, a printer and the functions (helpers and extension functions) its front end emits. A workflow has exactly one dialect. Any package may define a dialect or library functions, including a host's own; their authors are trusted.

**Front end.** For each source construct it picks the first tier that applies:

| Tier | Rule | Example |
| --- | --- | --- |
| 1. Direct | The construct means the same as a kernel form or library function. | `for x of xs`; `a + b` on two typed numbers; `xs[-1]` in Python |
| 2. Compiled in | It differs from the kernel. Emit the difference as kernel code, or a call to a helper. | `if xs:` becomes `len(xs) != 0`; `"n=" + n` becomes a concat with an explicit number-to-text |
| 3. Extension | Kernel code cannot reasonably express it. Call an extension function. | A regex match, WHATWG URL parsing |
| Refused | The author chose not to support it. | A typed diagnostic with a repair hint |

**Types.** A front end may use declared and inferred types to choose tier 1. If the type was wrong at run time, the strict operation raises a typed error (§2.1). Where no type is known the front end emits a helper that tests at run time, or refuses. Using types this way needs a type analysis per dialect; today's TypeScript front end erases types. Trusting types makes a dialect deliberately stricter than its language: where JavaScript would coerce a mistyped value, lash raises a typed error. Each dialect lists these in a deviation register, with a test per entry.

**Printer.** A dialect can print any document as source in its language, including one edited by a host. The one law: lowering the printed source gives a program that behaves the same (values, errors, mutation, order of effects, task interleaving). There is no promise to reproduce original text. To make this total, each dialect reserves a namespace that reaches the kernel by name (`k.add(a, b)`, `k.tuple(a, b)`, `k.absent`, `k.spawn`, `k.yield`) and a way to call a library function by identity. The printer uses a native spelling only where the front end lowers it back to the same kernel operation. Printing a document in a different dialect from the one it was written in is a conversion: the result is a new workflow in that dialect, whose manifest may still reference the first dialect's helpers.

## 5. Execution

- The engine compiles an admitted document to an internal executable. It is a cache: derived deterministically, never edited, rebuilt at will.
- Charges come from a cost table for kernel forms and from each function's charge formula; both are pinned (§6). Bounds are explicit, as today.
- **After a crash** a run resumes from its last saved state. Whatever ran after that save runs again. An effect whose outcome committed is never executed again: its `perform` is answered from the committed outcome.
- Worker processes, worker reset, effect admission before execution and the host boundary stay as they are.

## 6. Parked runs

A parked run is saved in the document's vocabulary. Its schema is derived from today's `VmContinuation` (`crates/lash-vm/src/runtime/vm/continuation.rs:216`) and the broker's snapshot (`crates/lash-vm-broker/src/snapshot.rs`). Every field of both gets a row or a stated reason for having none; the table below is the starting point, and [parked-state.md](parked-state.md) is the completed one, held by laws.

| Today | In the kernel schema |
| --- | --- |
| `executable`, `format_version` | Document identity, kernel version, and the manifest's function identities |
| (one line of control) | The set of tasks with their identities, the ready queue in order, each handle's state (ready, waiting on what, or ended with its result or error), whether its error has been observed, and which joins it was a member of |
| `instruction_pointer`, `resume` | Per task: the site of its pending statement, and whether that statement is to be issued again or continued |
| `operand_stack` | Nothing: the statement rule leaves no unnamed values |
| `frame_stack`, `active_function` | Per task, its active calls: function identity, the calling statement's site, bindings |
| `slots`, `globals` | Bindings per active call, and session bindings |
| `heap` | The reachable object graph with identity, including variables shared with closures |
| `iterator_stack` | Per active loop: the collection, the position, any values already taken |
| `loop_stack`, `loop_activations` | Per active loop: site, activation number, iteration and check counts |
| `handler_stack` | Derived from the site; not saved |
| `finally_stack` | Per `finally` being run: how it was entered (normal, throw with its value and origin, return with its value, break or continue with its target) |
| `pending_tools`, `execution_nonce`, broker pending request | Per waiting `perform`: effect name, arguments, its identity (site, occurrence and the loop context at issue), and whether it is requested, admitted or committed-and-undelivered |
| `Await { settled }`, the 1024 limit | Per waiting `join` on a list: which members have ended. A bound on members is kept |
| `occurrence_counters` | Per site: how many times it has run |
| `instructions_executed`, bounds | Meters, in the kernel's charge units |
| `mode`, `expired_functions`, `reference_semantics` | Stated again per session, or gone with the JavaScript runtime |
| `VmLoopPhase` (fuel-slice and cancel checkpoint) | The fuel-slice return of §6 |
| `last_value`, `profile`, `pending_error_span` | Not saved |

**Roots.** State is saved as fragments, one per root. Roots are: each active call's bindings, each session binding, each task handle, and each value held only by control state (a pending throw or return value, an iterator's taken values). An object reachable from several roots is owned by the first that reaches it in a fixed order, as between cells today (`crates/lash-vm/src/runtime/state/durable.rs`). Only changed fragments are rewritten. In flight this is new work.

**Law.** Resuming from saved state continues the computation and its effect ownership exactly, without re-running committed work. For each saved row the corpus holds a pair of states that differ only there and must resume differently; for each derived or discarded row it holds a pair that must resume the same.

**Upgrades.** A parked run pins: the kernel version (forms, values, evaluation order, the statement rule, task scheduling, site derivation, the cost table and this schema), its document identity, and every function identity in its manifest, charge formulas and guards included. Most change never touches the kernel version: a library function is fixed or added as a new identity, and a host adopts it by an edit.

A kernel version may break when a pinned rule is wrong, unsafe or unbounded. A breaking version ships, under lash's existing upgrade policies (ADR 0106):
1. a document migration, which rewrites a version N document into N+1;
2. a parked-run migration, which carries a version N saved state onto the rewritten document, mapping each saved site through the rewrite's correspondence;
3. a conformance case per migration: park under N, migrate, resume under N+1, and compare with a run under N throughout.

Both versions coexist for the one-release window (ADR 0115). Short runs drain; a long-parked run is migrated when a node of the new build first claims it, as session state is (ADR 0077). After the window the old interpreter is deleted, so an engine carries at most the current and the previous kernel version. A migration that cannot carry some state refuses that run with a typed result, and the release names what is refused before it ships.

*Where the two versions live.* A kernel version is a value, `KernelVersion` in `lash-kernel-doc` (`K-VER-003`). The forms, the machine and the parked-state schema are one set of Rust types for every version a build interprets; what two versions differ in is an arm of a `match` on that value: the stored spelling of a form (`lash-kernel-doc`), the price of an operation (`lash-kernel-vm`'s cost table), and the migration between them (`lash-kernel-migrate`). The previous interpreter is the sum of the arms for its variant. It is deleted in one place, by removing that variant from `KernelVersion`: every arm that served it then fails to compile and is removed with it, and the migration from it goes with them.

*The migration.* `lash-kernel-migrate` holds, per breaking version, one `Migration` from the version before it (`K-VER-004`, `K-VER-005`): a definition redeclared, a document rewritten with its correspondence, a parked run carried over that correspondence, each a total function with a typed refusal (`DocumentRefusal`, `ParkedRefusal`). The library of a build that interprets two versions holds each function twice, once per version, under two identities; a document's manifest moves from one to the other in the rewrite. `lash-kernel-conformance::check_migration` is the law's harness: it parks a case at each of its parks under N, migrates, resumes under N+1 and compares with the run that stayed.

*In lash.* The kernel version, the document schema and the parked-state format are version surfaces with the policy `migrate` (ADR 0131), and the kernel process engine's state format carries the kernel version its run is parked under. A node of the new build decodes both formats, so it claims a process the previous build parked (ADR 0106 §1). At that claim, before any transition and as the claimer's first commit under its claim epoch, it rewrites the document, publishes it under the process's own referrer, carries the parked run and every wait the engine holds onto it, and stamps the process with its own format set. It does this only once every live node that decodes the process also decodes the new set; while a node of the previous build is live the process stays as that build reads it and the new build advances it under the previous kernel version, so a rollback inside the window finds nothing it cannot read. A process the migration refuses is parked with `migration_refused` and the migration's typed reason, as the previous build left it. A process is migrated when it is claimed and not before: one that sleeps through the whole window would still be in the previous version when the window closes, and the build that deletes the previous interpreter cannot read it. `lashctl kernel-migration list` counts the processes and sessions still in the previous version (`unmigrated`), and `lashctl kernel-migration run` wakes each of them so a live node of the new build claims it and carries it; the sweep carries nothing itself, so running it again resumes it.

*Sessions.* A session holds kernel definitions in two places, and a node of the new build decodes the previous build's session set, so it claims a session that build left. The cell an open turn stopped in is carried when the turn is restored: the cell's snapshot holds its document, the run parked under it and the ledger of the waits the run stands on, and the restore rewrites the document, carries the run onto it under the same seal owner, identifies each wait in the rewritten document, and commits the carried snapshot with the session's format set, before the cell runs again and behind the same fleet gate as a process. A cell the migration refuses is a cell snapshot the build does not resume: the session parks with the typed refusal and its turn open, and a cancel of the turn is its way out. The function a session saved is a document of its own, so it is carried alone, by the document migration, when the session's state is restored or a session is seeded with it; the next capture stores what was carried. A saved function is declared in the document of each cell that uses it, so it is held in the version the build's dialects lower a cell in. One the migration refuses is not held: the session lists its name with the refusal, as it lists a binding that was not saved, and the model reads why. A session with no open turn still holds kernel-versioned state (its bindings and the functions it saved), and its actor stays in the previous build's format set until a turn is admitted. Woken by the sweep, a node of the new build that finds such a session with nothing to admit carries it outside a turn, behind the same fleet gate: it restores the session from its head, which carries its saved functions, recaptures what the restore carried, and commits that with the session's format set in one commit. It is not a session command: a command row is something a node of the previous build, still live inside the window, would have to read.

*The seal.* A holder reads a parked run's kernel version from its seal. The bytes state it too, and a restore, a process's decode and a carry each refuse a run whose bytes state another version than its seal, so no run is resumed or carried as a version it was not parked under.

*Before the upgrade.* `lashctl kernel-migration list`, run from the new build, reads every unfinished kernel process and every session still in the previous version, and reports each process the migration would refuse with that typed reason, each session with the cell its open turn stopped in when the migration would refuse that cell, and each library function with no counterpart in the new version with the processes whose documents list it.

*Closing the window.* The build that deletes the previous interpreter retires the format sets the previous build wrote: its nodes neither decode nor carry them, and a node does not start while a process or session is still in one (ADR 0115 §3.5, drain by release). It refuses with `DurableError::Unmigrated`, naming how many and `lashctl kernel-migration run`, which an operator runs with a node of the build before it serving. A corrected library function is adopted by `ReplaceFunctionIdentity` (`K-EDIT-009`), never by a migration; a function a new version retires is one its documents must stop listing before they can be carried.

**Safe points.** A run is saved when it parks. The machine also returns to the embedder on a fuel slice, where the embedder may save and where a run cancel is observed.

## 7. Stability

- The kernel, its library and the document schema have a written specification.
- A conformance corpus pins it: named cases for every library edge, every form, every row of §6, task interleavings and every edit. Each dialect adds cases for its helpers against its own language's behaviour, run against Node or CPython as the witness.
- A second, independent reader of the document (for example in TypeScript, for browser canvases) reads, edits, derives ids and sites, and runs kernel code, including helpers. It does not run native-only functions; those are answered by lash.

## 8. What is deleted, what stays, what is rewritten

**Deleted.** The ECMAScript operators, `Absent`-as-`undefined` and the f64-only number model in the IR; the JavaScript runtime and object kinds in the VM (RegExp, Date, Url, Map and Set as exotics, the Error family, prototype and receiver machinery); the separate pure declared-function kind and its link-time effect ban; pending tool handles as a VM object kind; process forms in the IR; the sequential async-map deviation (`TS_ASYNC_MAP_SEQUENTIAL_V1`); continuations bound to a bytecode position and an exact executable; the hand-written TypeScript printer and its round-trip laws; the "exact ECMA-262 subset" ruling and ADRs 0060, 0062, 0064, 0095 and 0096 as written; FIG-5654 and FIG-5655, which this supersedes.

**Unreadable at cutover.** Every stored definition, parked process and session snapshot. A host re-imports from source it kept. ADR 0115's one-release rolling upgrade does not span the cutover.

**Stays.** The total graph and its admission; content-addressed definitions; worker processes and reset; admission before execution, in one transaction with the saved state; the durable group contract (ADR 0065), stated again over tasks; explicit execution bounds; the host boundary.

**Rewritten.** The broker's ledger, from one pending operation per run to a set. The tool-result path and effect-value adapters, to carry number tokens (§2.1). Process start and await, as effects taking a function reference, and the host's process edits, as edits to document entries. The TypeScript front end, with a type analysis, against the kernel. Python is written second, as the proof that a dialect needs no engine change.

## 9. Crates and boundaries

The kernel is a set of small crates that depend on nothing else in lash, so the set can move to its own repository and serve other projects. Today `lash-vm` depends on `lash-core-execution`, `lash-sansio`, `lash-render` and `lash-vm-protocol`; that direction is reversed.

| Crate | Owns | Used by |
| --- | --- | --- |
| `lash-kernel-doc` | Values, types, forms, the document, function definitions and the native-function interface, the manifest, annotations, serialisation, structural validation, the kernel text notation | Everything; a host that only reads documents needs nothing else |
| `lash-kernel-check` | Linking, the statement rule, derived facts, admission against an environment's effect and function signatures | Editors, the engine |
| `lash-kernel-edit` | Drafts, typed edit transactions, correspondence | Editors |
| `lash-kernel-lib` | The kernel library: bodies and native implementations | The machine, any second reader |
| `lash-kernel-vm` | Compile to the executable, run tasks, charge, park, resume. The embedder assembles the function registry and hands it in | lash, other embedders |
| `lash-kernel-state` | The parked-run schema and its per-root encoding | The machine; a host that reads a run as data |
| `lash-kernel-dialect` | The front end and printer interfaces, diagnostics, the shared document-to-source walker | Dialect packages |
| `lash-kernel-conformance` | The corpus and the harness that runs it against a reader, a front end or a function | Everyone's tests |

Dialect packages sit beside them: `lash-dialect-typescript`, `lash-ext-regex-ecma`, `lash-ext-url-whatwg`.

**Rules.**
1. **One direction.** `lash-kernel-*`, `lash-dialect-*` and `lash-ext-*` crates depend only on each other and on third-party crates. A forked engine an extension is built on is in the set under its own name: `lash-regress`, the matcher under `lash-ext-regex-ecma`. A CI check enforces it.
2. **No I/O in the kernel.** No async runtime, store, network, threads or process management. The machine is a plain library. `run` executes until no task is ready, or a fuel slice ends, and returns the effects and sleeps requested since the last park; `deliver` takes one outcome. The embedder passes `run` a synchronous interface that answers host reads (clock, random, projection reads), takes `print` output and reports a pending cancel.
3. **No global state.** Everything guest-derived lives in an instance the embedder owns (ADR 0123).
4. **The machine parks; the embedder commits.** `lash-kernel-vm` produces and consumes parked state. Durable admission, transactions, ownership, leases, retries and processes stay in `lash-vm-broker` and the durable engine.
5. **Isolation is the embedder's choice.** The machine runs in-process. lash hosts it in resettable worker processes.
6. **The kernel owns its versions.** lash's fleet-format machinery wraps them.
7. **Small public surfaces.** Each crate exports a short, documented facade.
8. **Embedding is tested.** A minimal embedder outside lash (load a document, run it, answer effects from a table, park and resume) is part of the kernel's tests.

**What stays in lash.** The worker pool, protocol and client; the broker; the process engine integration; the RLM protocol; tools, plugins and tracing.

## 10. Rulings

None is open. The five items listed in §1 as written in on the reviewers' agreement stand unless Sam objects.

## 11. How it is built and proven

It is built directly, on an arc branch, in the crates of §9. Every lane that replaces something deletes what it replaces in the same change: the old code, its tests, its docs, its dependencies. Nothing is kept for compatibility: no aliases, shims, dual readers, feature flags or legacy tests, and no reader for any format written before the cutover. The branch may be red between lanes; it lands on main once, whole. The work and its order are the Linear arc; this section states what proves it.

**Oracles.**
1. **Test262.** lash vendors the tests its TypeScript dialect supports and records one outcome per test (`crates/lash-typescript/tests/test262/outcomes/`). The TypeScript dialect on the kernel must pass every test that record marks as passing, or carry a row in the deviation register (§4) that names the test. The ratchet (`scripts/check_test262_ratchet.py`) holds it. The kernel runner defaults to the complete record, partitioned into `selection::shard_00` through `selection::shard_39`; invoke exact selectors in small groups to stay inside each action’s deadline. Every case uses the same deterministic kernel bounds, and a bound trip remains a failure. Its stdout emits `outcome\t<path>\t<class>\t<qualifier>` for each case. Collect the last three fields into a TSV, then run `python3 scripts/check_test262_ratchet.py --base origin/main --kernel-outcomes <observations.tsv>`. This mode requires every main case exactly once and permits only exact cases in `crates/lash-dialect-typescript/deviations.md`; a diagnostic-wide refusal or a routed feature gap never exempts a regression. `TEST262_KERNEL_FILTER` accepts comma-separated path prefixes for focused local reruns; partial output cannot pass the complete ratchet.
2. **lash's own TypeScript laws** (`crates/lash-typescript/tests/`), ported where their subject survives and deleted where it does not.
3. **The kernel corpus** (`lash-kernel-conformance`): one named case per rule of §2, per row of §6 and per edit, written in kernel text and independent of any dialect.
4. **Python witnesses.** The Python dialect's cases carry values, errors and effect traces recorded from CPython.

**Gates at cutover.** The arc fails its gate if:
1. a Test262 test passing on main's record fails on the kernel without a deviation row;
2. the Python dialect needed a kernel form or value that §2 does not list (a dict keyed by tuple, `except KeyError`, a keyword argument, `nonlocal`, mutation of a dict during iteration and f-string number formatting are named now);
3. a generic host with no dialect code cannot change an effect argument, insert a statement and replace a condition using only typed edits;
4. parking at every park, discarding the executable, rebuilding it with a different layout and a native function swapped for its kernel body, then resuming, changes any value, error, effect identity or charge; or a crash injected before admission, after admission and after an outcome commits runs a committed effect again;
5. a regex's result, charge or point of failure differs between cold and warm caches;
6. the programs of `crates/lash-vm/benches/benchmark.rs`, lowered through the new TypeScript front end, run more than 3 times slower than today, VM time only.

## 12. Order

1. The seam: `lash-kernel-doc`, with the rules of §2 written out one by one and the machine interface fixed.
2. In parallel against the seam: the machine, the checker, the library, the conformance harness, the extension crates, the TypeScript front end, the broker ledger.
3. Parked runs, edits, the printer, the Python dialect.
4. lash on the kernel: workers, sessions and cells, processes and the host workflow API, upgrades.
5. Cutover and certification.

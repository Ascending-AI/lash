# Lash kernel: one dialect-free workflow language, many front ends

Recovery and external-effect guarantees follow [What survives a crash](../../README.md#what-survives-a-crash).

Status: accepted as [ADR 0139](../adr/0139-the-lash-vm-is-a-dialect-free-kernel.md); revision 4, 2026-10-09. This is the design the kernel was built to, as a clean cutover: no dual IR, no reader for the old VM's graph, continuation or snapshot formats, no transition plan. Where it says "today" it means the tree before the cutover, which is deleted. The rules themselves are in [the kernel semantics](semantics.md).

## 1. What this is for

1. A stable host-facing representation of a workflow that a host can read, edit and interpret.
2. Several source dialects (TypeScript, Python, others) running on one engine.
3. A dialect author can support any reasonable subset of their language.
4. The representation is dialect-free. A host supplies effects; Documents retain local function definitions and declare signature-only Imports; Links pin the fills. Nothing in a document's meaning names a dialect.

Out of scope, by standing ruling: running a stock runtime behind a sandbox; journal-and-replay of code (ADR 0132 §2); a wire protocol (ADR 0136); host event routing and scheduling (ADR 0137).

**Settled by Sam (2026-10-09).** Three tiers; an expansive kernel library; trusted dialect authors; regex outside the kernel; functions shipped by packages in their own registry, not mixed into tools, and any package may define them; one dialect per workflow; arbitrary-precision integers; every wait is its own statement; effects may appear in any function; one mechanism for library functions; a dialect can print any document as equivalent source, with no promise to reproduce the original text; hosts edit kernel statements directly, with no grouping mechanism; async functions run concurrently as the dialect's language would run them, through kernel tasks; a run that ends with an unjoined task is an error, and a dialect compiles in any other behaviour; kernel versions may break and each break ships a migration; front ends trust type annotations and document the deviation; crates named `lash-kernel-*`; build it directly and test the TypeScript dialect against the ECMAScript tests lash supports.

**Written in on the reviewers' agreement, open to Sam's objection.** Numbers compare by mathematical value; the statement rule is syntactic; no typed-binding form; a task's error is data on its handle; outcomes may be delivered in any order.

**Since withdrawn.** FIG-6402 withdrew the statement rule, and with it "every wait is its own statement": an action may nest in an expression, and a task may pause there (`K-EVAL-008`).

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

**The effect boundary.** Effect arguments and results are copied: a tool never shares identity with guest objects, and a result is a fresh graph. Aliases between guest objects survive a wait. Each `perform` states the result type it expects, and the result is decoded by that type: `Int` and `Float` fields decode exactly as declared, and a bare `Number` decodes a JSON number with no fraction or exponent as an integer and any other as a float. Most tool results are untyped, so the Document states its numeric policy for a bare number once (float for a TypeScript document, by spelling for a Python one), and a site overrides it only where the effect's signature says more. The effect-value adapters that today reduce every number to an f64 before the worker sees it are rewritten to carry the number token to this decoder.

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

**Nested actions.** A call (of a declared function, a closure or a library function), `perform`, `sleep` and `spawn` may sit inside an expression, with expressions as arguments; `join`, `yield` and `cancel` are statement forms only. Actions nest in a `while` condition, evaluated afresh at each iteration inside its loop, and in an assignment's place, evaluated in the order the assignment states: its value first (Python) or its place first (TypeScript, `set a[k] = v place first`). A library function called by name inside an expression states a native implementation; one with only a kernel-code body is called with `invoke`. A task pauses only at an action, and a frame stands either between statements or at one nested action in a phase (called, waiting or spawned). The values its statement evaluated before that action and has not consumed are the continuation's *operand roles*, a function of the document alone and named by their sites: a run resumed there loads them into whatever registers its layout chose and continues the statement, evaluating nothing of it twice (`K-EVAL-008`).

**Evaluation order** is left to right, operands before operation, and is part of the kernel version.

**Iteration.** A loop over a list reads live by index. A loop over a map or set visits in insertion order, sees entries added during the loop and skips entries removed before their turn. A dialect whose language differs compiles the difference in.

### 2.3 Tasks

A run is a set of tasks. `main` is the first.

- `spawn f(args)` creates a task that runs `f`, and returns its handle. The new task runs at once, up to its first wait or its end; then the spawning task continues. (This is how a JavaScript async function behaves. A dialect whose language queues a new task instead, as Python's `create_task` does, makes `yield` the task's first statement. A Python coroutine that is only awaited is an ordinary call.)
- One task runs at a time, until it reaches a wait or ends. Then the next ready task runs, first in first out. There is no parallelism inside a run.
- A wait that completes makes its task ready, at the back of the queue. A `perform` or `sleep` marked `then yield` takes one more turn once its outcome is in, before its result or raise reaches the code. A `join` on a handle that has already ended continues at once. When a task ends, every waiting join it decides, on its handle alone or on a list, becomes ready in join-start order.
- `join` on a list: all returns when every member has ended or at the first failure; all-settled when every member has ended; race at the first member to end; any at the first to succeed. Members not yet ended keep running.
- **Errors.** An error that ends a task is data on its handle, raised at each `join` that observes it, and nothing more.
- `cancel h` raises a cancellation error in the task at its current wait. Its cleanup blocks run and may wait. Cancelling an ended task does nothing.
- **Parks and delivery.** When no task is ready the run parks. Every effect requested since the last park is admitted together with the saved state, in one transaction. The embedder delivers committed outcomes in any order, and several may be delivered before the next save; a save is required only before effects are admitted. Because lash saves state and never replays, the delivery order need not be reproducible.
- **Ending.** A run ends when `main` ends, or at `finish` or `fail`. A task that was a member of a `join` the code has already passed is cancelled then. Any other task that is unfinished, or that ended in an error no `join` observed, makes the end a typed error naming those tasks. The library function `tasks.unfinished()` returns the unfinished handles, so a dialect whose language waits for them (Node) or cancels them (asyncio) compiles that in.
- **Identity.** A task is identified by its spawn site, that site's occurrence in the spawning task, and the spawning task. An effect is identified by its task, its site, the site's occurrence within that task, and the loop context. So a host can tell which element of a fan-out an effect belongs to.
- **Bounds.** The number of live tasks, of effects admitted at one park and of members of one `join` are execution bounds the embedder states, like instructions and memory.

The scheduling rules above are part of the kernel version.

How source maps: `await tool(x)` is one `perform` (TypeScript marks it `then yield`, so it continues in arrival order with a promise's). `const p = tool(x)` is a `spawn` of a function that performs it, and `await p` is `await p via ts.await`, whose policy joins the promise's task; Python's `await t` is `await t via py.await`, which takes no turn of its own. `Promise.all(xs.map(async x => …))` spawns one task per element and joins the list; `asyncio.gather` does the same.

### 2.4 Library functions (one mechanism)

A library function is a content-addressed definition with a name, a typed signature over kernel values, its error kinds, and either a kernel-code body, a native implementation, or both.

- **The kernel library** is the set the engine ships: arithmetic, comparison, text, slicing and negative indexing, `sum`/`any`/`all`/`map`/`filter`/`sort` (stable), membership, split/join/format, JSON, math.
- **A helper** is a function a dialect package ships with a kernel-code body, to carry a difference between its language and the kernel.
- **An extension function** is one a package ships with only a native implementation, for what kernel code cannot reasonably express: `regex.ecma.exec`, `url.whatwg.parse`.

A Document contains immutable definitions keyed by FunctionId. Same-document calls name those identities; cross-document calls use explicit Import slots. A Link pairs the DocumentId with a complete slot-to-Fill map. Fresh fulfilment checks kind, visibility, ownership, signature and convention; existing Links keep their exact operation contract pins. Different fills produce different Links while retaining the same DocumentId and FunctionIds.

A function with a kernel-code body is ordinary kernel code: it may nest actions (`K-EVAL-008`), may call a function argument, and so may wait if its callback does. Nothing about the registry makes a call effect-free. Every library call charges its declared formula at completion, plus its declared dynamic-loop tests. Guest callbacks and closures remain ordinarily charged (K-CHG-003/007).

**Admission test for the kernel library.** A function belongs in it only if its meaning fits in a sentence that names no language, two dialects would map to it directly or expansions need it as a building block, and every edge is pinned. Coercing addition, any language's truthiness and operand-returning logical operators fail the test and are helpers.

**Native implementations.**
1. **Charge.** A function's identity includes a charge formula over the sizes of its arguments and result. That formula is what the caller is charged, whichever implementation runs and whether engine caches are cold or warm.
2. **Guard.** A function that can run away (a backtracking regex) also states, in its identity, a work unit and a limit as a formula over its arguments. The implementation counts that unit deterministically, independent of caches, and a call that passes the limit ends with a typed bound error. Because the unit and limit are in the identity, the point of failure is pinned like any other behaviour. Memory for engine-side caches is the embedder's, bounded by its worker limits, and is never a guest-visible failure.
3. **Values in, values out.** A native function sees only its arguments and its counter, does no I/O and is deterministic. The conformance harness re-runs every native function and compares results. Native functions are registered by the embedder in Rust at startup, never loaded from a document.
4. **No callbacks.** A function that takes a function argument has a kernel-code body and no native implementation.
5. **No mutation.** Extension values are kernel data: a regex is a record with `__lash_brand: ("regex.ecma", "reg_exp")`, its constructor's identity in `__lash_built`, and `pattern`, `flags` and `lastIndex`. `regex.ecma.exec` returns the match and the next `lastIndex`; the helper that wraps it writes `lastIndex` back to the record, so aliases see the update.

## 3. The document

- The workflow document is a kernel program: `main`, declared functions (including those run as processes), private bindings. It is total (every construct is a typed node or a typed expression inside one) and it is the only authority.
- Derived, never written by a host: node ids, edges, scopes, type facets, effect sets, execution sites. lash recomputes them on every change.
- **Imports and Exports:** Imports are document-local slots declaring operation/code signatures or nominal type/error Brands. Exports are Function, Type or Error values. Main is optional. Host-startable Exports are Function kind with data-only parameters and settled result; synchronous functions qualify, and Export starts never run Main. The function's StartConvention supplies its existing ABI helpers, without generating an adapter. Runtime inputs and settled results are still fitted.
- **Identity.** A document's behavioural identity is the hash of its content. A source map from a dialect is a separate, optional value keyed to that identity; it never changes behaviour and may be dropped. Labels, layout and other presentation are the host's, keyed by site.
- **Edits.** Typed, transactional edits and re-admission keep their present shape. Every expression payload changes to kernel forms, and edits and transactions become serialisable data; both are new work. A host edits kernel statements directly. Hoisted temporaries are ordinary variables, and the scope check refuses an edit that uses one before it is bound.
- **Sessions.** Saved closures retain their defining Link, FunctionId, captured environment EntityId and nested reference membership. Relinking changes only the defining Link after fresh compatible fulfilment; it never copies or reidentifies code. Captured-only closures participate in saved reach. FunctionValue transport carries a CallableId, public signature, convention and frozen captures. Callable starts install that captured environment directly. Package values carry nominal Brands; changed representation raises `package.stale_value` through the ordinary member check.
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

**Shape.** A front end lowers a workflow to a document shaped like its source. Calls, awaits and effects nest in the expressions that use them (§2.2); each source loop is one kernel `for` through its dialect's protocol, with one body; an entry's body is the author's function, bound by its start convention, which also carries a run-root cleanup such as Python's task cancellation (`K-DOC-003`, `K-TASK-019`). A statement the source did not write is an operand pin that keeps an evaluation order the kernel's left-to-right order would change, a `let` that holds a value a later statement reuses, or a hoist past the nesting limit (`K-DOC-006`). Speed is the executable's business (§5): a front end never duplicates a body or tests a representation in the document to make it faster. The `examples/workflows` corpus pins this shape: statements at most the author's statements plus operand pins, one body per source loop (FIG-6402).

**Types.** A front end may use declared and inferred types to choose tier 1. If the type was wrong at run time, the strict operation raises a typed error (§2.1). Where no type is known the front end emits a helper that tests at run time, or refuses. Using types this way needs a type analysis per dialect; today's TypeScript front end erases types. Trusting types makes a dialect deliberately stricter than its language: where JavaScript would coerce a mistyped value, lash raises a typed error. Each dialect lists these in a deviation register, with a test per entry.

**Printer.** Kernel canonical text round-trips Imports, Exports, optional Main and immutable local definitions. Retained source maps and captions are sidecars. A dialect source printer uses verified authored source supported by its ordinary lowering; it does not encode a second hidden kernel format. Workflow views derive names from the retained Document; Package labels are caller annotations outside the derived view.

## 5. Execution

- The engine compiles an admitted document to an internal executable. It is a cache: derived deterministically, never edited, rebuilt at will. The library's part of it is compiled once per registry and shared by every run that uses the registry, keyed by function identity; each live immutable document is admitted and compiled once against that prepared library and layout, then reused at start and at every open of a checkpoint. Weak document references isolate replacement and copy-on-write edits, and unused preparation is reclaimed on the next admission. A checkpoint's header (kernel version, document, execution closure) is checked on every open, before the run executes; each page is checked when it is first read (§6).
- Charges come from a cost table for kernel forms and from each function's charge formula; both are pinned (§6). Bounds are explicit, as today.
- **After a crash** a run resumes from its last saved state. Computation after that save may run again, including clock, random, projection reads, declared `Read` tools, printing and logging. An admitted `Once` without an outcome becomes `Interrupted`; an unfinished `Repeatable` can run again under its policy. An effect whose outcome committed is never executed again: its `perform` is answered from the committed outcome.
- Worker processes, worker reset, effect admission before execution and the host boundary stay as they are.

## 6. Parked runs

A parked run is a checkpoint root over the run's persistent heap ([ADR 0145](../adr/0145-kernel-execution-and-its-checkpoints-share-one-persistent-heap.md)). The machine executes on that heap, so a park seals the pages the stretch since the last park changed and writes one root; there is no separate image of the run. The types are `lash-kernel-state`'s `paged` module; each page is a canonical tree page of the semantic kinds 1000 to 1008, and a root is complete: nothing else is needed to resume it.

| `CheckpointRoot` field | What it holds |
| --- | --- |
| Kernel version, document, execution closure and owner | The kernel version, document identity, exact function identities with their definitions and native-only requirements, and the execution's `OwnerBinding` node: a header the embedder reads without decoding the heap |
| `target`, `functions` | Whether the run is a session cell's `main` or an entry, and the saved functions it adopted |
| `status` | Parked, or ended with its frozen result or error and the session carry it ended with |
| `heap` | The heap header page: the object directory, lazy families, the next identities, the accounting certificate (an upper bound, `K-BND-003`) and the data-only population certificate |
| `tasks` | Per task, by handle number: its identity (a parent-linked descriptor), its state (ready, resuming with a value, a raise, a join or a committed outcome, waiting on a wait, joining one handle or a list, or ended), its joiners, whether its error was observed, its occurrence counts per action site, and its active calls |
| `ready` | Ready task numbers in the order they run (`K-TASK-003`) |
| `waits` | Per wait number: its task, its kind (effect or sleep) and its phase: issued with its frozen request, admitted, or committed with its frozen outcome |
| `joins` | Per list join: the joining task, its mode and its members |
| `withdrawn`, `unreported` | Waits withdrawn after they were handed out (`K-MACH-004`), and those the next park reports (`K-MACH-003`) |
| `reads` | The effects the run performs as reads (`K-EFF-012`) |
| `session`, `host` | A session cell's bindings by name, and the names its host projected into `main`, which its carry leaves out |
| `meters` | Charge, waits issued, the next join number and live tasks, exactly |

An active call (`FrameRecord`) holds semantic coordinates only: the site of its pending statement and, when it stands at a nested action, that action's continuation (its site under the statement's, its phase, and its operand roles, each the site suffix of an operand with its value); its bindings, each by name and declaring site with its value or shared cell; its loops (site, iterations started, and a map or set position as a table cursor); its `finally` blocks under way and how each was entered, an entry's run-root cleanup among them as a `finally` row at the body's own site; the values a loop iterates and a `finally` (or a cleanup) departs with; and a library call's arguments. Every unbounded record is itself a tree, so a resume reads only the paths it touches.

Not saved: registers, which belong to one step and are empty at every safe point, a continuation's roles being saved by site instead; compiled code, statement and slot numbers, which are rebuilt caches; the handlers around a statement, derived from its site; loop activation numbers and check counts, which the kernel does not have (`K-EFF-008`); and any trace state of the memory census.

A wait persists its identity and kind, never its admitted request's arguments: the embedder hands the admission of every wait in at open. A request issued since the last park is frozen once, so a crash before the park that reports it neither loses nor duplicates it, and an outcome delivered and not yet run with stays committed in the root until the task resumes and its wait is removed in the same edit.

**Law.** Resuming from saved state continues the computation and its effect ownership exactly, without re-executing effects whose outcomes committed; unsaved computation and diagnostics may repeat. The laws are `lash-kernel-state`'s `paged::laws::root` (every record round-trips canonically, and a root fits its 4096-byte bound), `lash-kernel-vm`'s `laws::paged` (a parked frame resumes from its sealed record; an issued request is reported once; a committed outcome survives a seal before semantic resume) and `laws::layout` (every layout and every implementation of a function runs a document the same way).

**Upgrades.** A parked run pins: the kernel version (forms, values, evaluation order, where an action may be nested, task scheduling, site derivation, the cost table and this schema), its document identity, and every function identity in its manifest, charge formulas and guards included. Most change never touches the kernel version: a library function is fixed or added as a new identity, and a host adopts it by an edit.

A kernel version may break when a pinned rule is wrong, unsafe or unbounded. A breaking version ships, under lash's existing upgrade policies (ADR 0106):
1. a document migration, which rewrites a version N document into N+1;
2. a parked-run migration, which carries a version N saved state onto the rewritten document, mapping each saved site through the rewrite's correspondence;
3. a conformance case per migration: park under N, migrate, resume under N+1, and compare with a run under N throughout.

Both versions coexist for the one-release window (ADR 0115). Short runs drain; a long-parked run is migrated when a node of the new build first claims it, as session state is (ADR 0077). After the window the old interpreter is deleted, so an engine carries at most the current and the previous kernel version. A migration that cannot carry some state refuses that run with a typed result, and the release names what is refused before it ships.

*Where the two versions live.* A kernel version is a value, `KernelVersion` in `lash-kernel-doc` (`K-VER-003`). The forms, the machine and the parked-state schema are one set of Rust types for every version a build interprets; what two versions differ in is an arm of a `match` on that value: the stored spelling of a form (`lash-kernel-doc`), the price of an operation (`lash-kernel-vm`'s cost table), and the migration between them (`lash-kernel-migrate`). The previous interpreter is the sum of the arms for its variant. It is deleted in one place, by removing that variant from `KernelVersion`: every arm that served it then fails to compile and is removed with it, and the migration from it goes with them.

*The migration.* `lash-kernel-migrate` holds, per breaking version, one `Migration` from the version before it (`K-VER-004`): a definition redeclared and a document rewritten with its correspondence, each a total function with a typed refusal (`DocumentRefusal`). The library of a build that interprets two versions holds each function twice, once per version, under two identities; a document's manifest moves from one to the other in the rewrite. Carrying a parked run over the correspondence (`K-VER-005`) is a paged migrator that rewrites sites, task lineage, closure sites and wait identities copy-on-write over the run's page roots, with a foreign-image validator. It preserves the exact pinned package snapshot and rewrites each held spawn origin. Missing code or an unmapped held coordinate refuses with a typed reason and leaves the original run intact.

*In lash.* The kernel version and the document schema are version surfaces with the policy `migrate` (ADR 0131); the parked-state format carries through a checked correspondence, and the kernel process engine's state format carries the kernel version its run is parked under. A node of the new build decodes both formats, so it claims a process the previous build parked (ADR 0106 §1). At that claim, before any transition and as the claimer's first commit under its claim epoch, it rewrites the document, publishes it under the process's own referrer, identifies every wait the engine holds in the rewritten document, and stamps the process with its own format set. The same fenced transaction publishes the carried pages, re-derived requirements and new closure roots. It does this only once every live node that decodes the process also decodes the new set; while a node of the previous build is live the process stays as that build reads it and the new build advances it under the previous kernel version, so a rollback inside the window finds nothing it cannot read. A process the migration refuses is parked with `migration_refused` and the migration's typed reason, as the previous build left it. A process is migrated when it is claimed and not before: one that sleeps through the whole window would still be in the previous version when the window closes, and the build that deletes the previous interpreter cannot read it. `lashctl kernel-migration list` counts the processes and sessions still in the previous version (`unmigrated`), and `lashctl kernel-migration run` wakes each of them so a live node of the new build claims it and carries it; the sweep carries nothing itself, so running it again resumes it.

*Sessions.* A session holds kernel definitions in two places, and a node of the new build decodes the previous build's session set, so it claims a session that build left. The cell an open turn stopped in carries its heap coordinates, typed Written table and admission identities together under the session fence. A cell the build does not carry is one it does not resume: the session parks with the typed refusal and its turn open, and a cancel of the turn is its way out. The function a session saved is a document of its own, so it is carried alone, by the document migration, when the session's state is restored or a session is seeded with it; the next capture stores what was carried. A saved function is declared in the document of each cell that uses it, so it is held in the version the build's dialects lower a cell in. One the migration refuses is not held: the session lists its name with the refusal, as it lists a binding that was not saved, and the model reads why. A session with no open turn still holds kernel-versioned state (its bindings and the functions it saved), and its actor stays in the previous build's format set until a turn is admitted. Woken by the sweep, a node of the new build that finds such a session with nothing to admit carries it outside a turn, behind the same fleet gate: it restores the session from its head, which carries its saved functions, recaptures what the restore carried, and commits that with the session's format set in one commit. It is not a session command: a command row is something a node of the previous build, still live inside the window, would have to read.

*The header.* A holder reads a parked run's kernel version, document and pinned execution closure from its checkpoint root's header, never from the heap. A resume refuses a root whose header states another kernel version or document than the run it resumes, so no run is resumed as a version it was not parked under.

*Before the upgrade.* `lashctl kernel-migration list`, run from the new build, reads every unfinished kernel process and every session still in the previous version, and reports each process the migration would refuse with that typed reason, each session with the cell its open turn stopped in when the migration would refuse that cell, and each library function with no counterpart in the new version with the processes whose documents list it.

*Closing the window.* The build that deletes the previous interpreter retires the format sets the previous build wrote: its nodes neither decode nor carry them, and a node does not start while a process or session is still in one (ADR 0115 §3.5, drain by release). It refuses with `DurableError::Unmigrated`, naming how many and `lashctl kernel-migration run`, which an operator runs with a node of the build before it serving. A corrected library function is adopted by source replacement and re-import, never by a canvas edit; a function a new version retires is one its documents must stop listing before they can be carried.

*Packages and adoption.* Library code is installed as dependency-closed
package sets. A parked run and a published workflow retain exact function
identities through an execution closure; a cell uses the fleet-selected
package set. The [host-package guide](../host-packages.md) covers the single
install path, runner contracts, retention and current session limitations.

- **Naming the pair.** The host names the new pair: the same entry and arguments over a document that lists, for each pinned helper, the build's function of the same name, and a published env.
- **The request.** Lash link-checks and holds the pair under the process's referrer. It plans the carry and records the request as `process.successor` mail.
- **The apply.** The process's owner moves the request into its driver. At the run's next rest, under its claim fence, it commits in one transaction the carried engine state and the `process.succeeded` event, whose fold rewrites the record's input and definition id. The transaction also stages the parked pages, closure roots and the successor requirements record after checking internal-operation authority. A refusal records nothing at request time, or `process.successor_refused` at the apply, and the process goes on as written.
- **The correspondence.** The carry is planned over a correspondence (`plan_carry`, over `lash_kernel_migrate::adopt`). The relink of the old document onto the successor's functions of the same names must be the named document. Admitted waits keep their recorded requests, matches and call ids; only their effect identities move with the rewrite.

*Adoption* (`K-LIB-009`). A published definition's document, or a parked process, is adopted onto a successor release of the packages it calls by name (`lash_kernel_migrate::relink`, `plan_adoption`): each function its own code calls that the earlier release holds is replaced, in every slot, by the successor's function of the same name, and its manifest lists what the result reaches. Adoption never moves code onto a changed operation contract: a result that performs an operation under another requirement than the document did is refused, `operation_contract_changed`, naming the operation. The correspondence places every node of the document's own code at its own site. Library-body frames move through unambiguous statement correspondence after identity substitution; any held coordinate without a successor refuses, leaving the original run intact. `survey_adoption` lists the processes that reach a relinked function, each with its reason. A session's open cell keeps its pinned snapshot during a kernel carry; explicit package adoption applies to process succession. Its saved functions are not adopted either; each cell relinks them to the set it is written under (`RELINK-CLOSURE`).

**Safe points.** A run is saved when it parks. The machine also returns to the embedder on a fuel slice, where the embedder may save and where a run cancel is observed.

## 7. Stability

- The kernel, its library and the document schema have a written specification.
- A conformance corpus pins it: named cases for every library edge, every form, every row of §6, task interleavings and every edit. Each dialect adds cases for its helpers against its own language's behaviour, run against Node or CPython as the witness.
- A second, independent reader of the document (for example in TypeScript, for browser canvases) reads, edits, derives ids and sites, and runs kernel code, including helpers. It does not run native-only functions; those are answered by lash.

## 8. What is deleted, what stays, what is rewritten

**Deleted.** The ECMAScript operators, `Absent`-as-`undefined` and the f64-only number model in the IR; the JavaScript runtime and object kinds in the VM (RegExp, Date, Url, Map and Set as exotics, the Error family, prototype and receiver machinery); the separate pure declared-function kind and its link-time effect ban; pending tool handles as a VM object kind; process forms in the IR; the sequential async-map deviation; continuations bound to a bytecode position and an exact executable; the hand-written TypeScript printer and its round-trip laws; the "exact ECMA-262 subset" ruling and ADRs 0060, 0062, 0064, 0095 and 0096 as written; FIG-5654 and FIG-5655, which this supersedes.

**Unreadable at cutover.** Every stored definition, parked process and session snapshot. A host re-imports from source it kept. ADR 0115's one-release rolling upgrade does not span the cutover.

**Stays.** The total graph and its admission; content-addressed definitions; worker processes and reset; admission before execution, in one transaction with the saved state; the durable group contract (ADR 0065), stated again over tasks; explicit execution bounds; the host boundary.

**Rewritten.** The broker's ledger, from one pending operation per run to a set. The tool-result path and effect-value adapters, to carry number tokens (§2.1). Process start and await, as effects taking a function reference, and the host's process edits, as edits to document entries. The TypeScript front end, with a type analysis, against the kernel. Python is written second, as the proof that a dialect needs no engine change.

## 9. Crates and boundaries

The kernel is a set of small crates that depend on nothing else in lash, so the set can move to its own repository and serve other projects. The old VM depended on `lash-core-execution`, `lash-sansio`, `lash-render` and `lash-vm-protocol`; the kernel reverses that direction.

| Crate | Owns | Used by |
| --- | --- | --- |
| `lash-kernel-doc` | Values, types, forms, the document, function definitions and the native-function interface, the manifest, source maps and correspondences, serialisation, structural validation, the kernel text notation | Everything; a host that only reads documents needs nothing else |
| `lash-kernel-check` | Linking, typing, derived facts, admission against an environment's effect and function signatures, the checked document holder (document, graph, entry and linked declarations) | Viewers, editors, the engine |
| `lash-kernel-view` | The canvas read of a checked document's entry and its host schema; no dialect, editor or runtime | Viewers, editors |
| `lash-kernel-edit` | Drafts, typed edit transactions, authoring operations | Editors |
| `lash-kernel-lib` | The kernel library: bodies and native implementations | The machine, any second reader |
| `lash-kernel-vm` | Compile to the executable, run tasks, charge, park, resume. The embedder assembles the function registry, prepares it once and hands it in | lash, other embedders |
| `lash-kernel-state` | The persistent heap: pages, the object directory, the checkpoint and session roots and their records, the memory census and frozen families | The machine; a host that reads a run as data |
| `lash-kernel-dialect` | The front end and printer interfaces, diagnostics, bounded final-text writing and optional spans | Dialect packages |
| `lash-kernel-conformance` | The corpus and the harness that runs it against a reader, a front end or a function | Everyone's tests |

Dialect packages sit beside them: `lash-dialect-typescript`, `lash-ext-regex-ecma`, `lash-ext-url-whatwg`.

**Rules.**
1. **One direction.** `lash-kernel-*`, `lash-dialect-*` and `lash-ext-*` crates depend only on each other and on third-party crates. A forked engine an extension is built on is in the set under its own name: `lash-regress`, the matcher under `lash-ext-regex-ecma`. A CI check enforces it.
2. **No I/O in the kernel.** No async runtime, store, network, threads or process management. The machine is a plain library. `run` executes until no task is ready, or a fuel slice ends, and returns the effects and sleeps requested since the last park; `deliver` takes one outcome. The embedder passes `run` a synchronous interface that answers host reads (clock, random, projection reads), takes `print` output and reports a pending cancel, and hands the heap a synchronous page source it reads and seals pages through (`K-MACH-001`). A page the source cannot supply is an infrastructure failure, never a guest outcome.
3. **No global state.** Everything guest-derived lives in an instance the embedder owns (ADR 0123).
4. **The machine parks; the embedder commits.** `lash-kernel-vm` seals checkpoint roots and opens them. Page staging, publication, durable admission, transactions, ownership, root leases, retirement, retries and processes stay in `lash-vm-broker` and the durable engine.
5. **Isolation is the embedder's choice.** The machine runs in-process. lash hosts it in resettable worker processes.
6. **The kernel owns its versions.** lash's fleet-format machinery wraps them.
7. **Small public surfaces.** Each crate exports a short, documented facade. Code outside `crates/lash-kernel-*` uses the machine's embedding API and the kernel's semantic readers (a session's pages, bound names and notes, the helper probe heap, a cell's saving context), never the paged heap's or the machine's internals: those are crate-private, and what one kernel crate builds on in another is exported only through its `#[doc(hidden)]` `kernel_internals` module, which `scripts/check-kernel-boundary.py` keeps inside the kernel (FIG-6688). The one diagnostic seam outside it, `lash_kernel_vm::perf_witness`, sits behind the `perf-witness` feature and never ships in production.
8. **Embedding is tested.** A minimal embedder outside lash (load a document, run it, answer effects from a table, park and resume) is part of the kernel's tests.

**What stays in lash.** The worker pool, protocol and client; the broker; the process engine integration; the RLM protocol; tools, plugins and tracing.

## 10. Rulings

None is open. The five items listed in §1 as written in on the reviewers' agreement stand unless Sam objects.

## 11. How it is built and proven

It is built directly, on an arc branch, in the crates of §9. Every lane that replaces something deletes what it replaces in the same change: the old code, its tests, its docs, its dependencies. Nothing is kept for compatibility: no aliases, shims, dual readers, feature flags or legacy tests, and no reader for any format written before the cutover. The branch may be red between lanes; it lands on main once, whole. The work and its order are the Linear arc; this section states what proves it.

**Oracles.**
1. **Test262.** lash vendors the tests its TypeScript dialect supports and records one outcome per test (`crates/lash-dialect-typescript/tests/test262/outcomes/`). The TypeScript dialect on the kernel must pass every test that record marks as passing, or carry a row in the deviation register (§4) that names the test. The ratchet (`scripts/check_test262_ratchet.py`) holds it. The kernel runner defaults to the complete record, partitioned into `selection::shard_00` through `selection::shard_39`; invoke exact selectors in small groups to stay inside each action’s deadline. Every case uses the same deterministic kernel bounds, and a bound trip remains a failure. Its stdout emits `outcome\t<path>\t<class>\t<qualifier>` for each case. Collect the last three fields into a TSV, then run `python3 scripts/check_test262_ratchet.py --base origin/main --kernel-outcomes <observations.tsv>`. This mode requires every main case exactly once and permits only exact cases in `crates/lash-dialect-typescript/deviations.md`; a diagnostic-wide refusal or a routed feature gap never exempts a regression. `TEST262_KERNEL_FILTER` accepts comma-separated path prefixes for focused local reruns; partial output cannot pass the complete ratchet.
2. **lash's own TypeScript laws**, ported to `crates/lash-dialect-typescript` where their subject survived and deleted where it did not.
3. **The kernel corpus** (`lash-kernel-conformance`): one named case per rule of §2, per row of §6 and per edit, written in kernel text and independent of any dialect.
4. **Python witnesses.** The Python dialect's cases carry values, errors and effect traces recorded from CPython.

**Gates at cutover.** The arc fails its gate if:
1. a Test262 test passing on main's record fails on the kernel without a deviation row;
2. the Python dialect needed a kernel form or value that §2 does not list (a dict keyed by tuple, `except KeyError`, a keyword argument, `nonlocal`, mutation of a dict during iteration and f-string number formatting are named now);
3. a generic host with no dialect code cannot change an effect argument, insert a statement and replace a condition using only typed edits;
4. parking at every park, discarding the executable, rebuilding it with a different layout and a native function swapped for its kernel body, then resuming, changes any value, error, effect identity or charge; or a crash injected before admission, after admission and after an outcome commits runs a committed effect again;
5. a regex's result, charge or point of failure differs between cold and warm caches;
6. At cutover, the old VM benchmark suite supplied a one-time 3× VM-time comparison. The maintained kernel comparison now uses recorded kernel baselines and advisory duration history; see [kernel gate 6 receipts](../perf/instruments/kernel-gate6.md). The retired VM is no longer a comparison producer.

## 12. Order

1. The seam: `lash-kernel-doc`, with the rules of §2 written out one by one and the machine interface fixed.
2. In parallel against the seam: the machine, the checker, the library, the conformance harness, the extension crates, the TypeScript front end, the broker ledger.
3. Parked runs, edits, the printer, the Python dialect.
4. lash on the kernel: workers, sessions and cells, processes and the host workflow API, upgrades.
5. Cutover and certification.

### Linked package execution and adoption

A selected Main, Export or callable derives its executable reach from the defining
Link. An unused Export contributes no operation uses to that process. Operation
identity is `(LinkId, ImportSlot)`, so one Document linked twice can invoke different
contracts without changing function identities. Full immutable DAG retention is
broader than executable reach. Publication records attest scoped provenance and
are retained intact with their imported publication closure under the owner's
existing claim; content hashes alone grant no visibility or ownership.

Adoption maps explicit predecessor uses to successor uses. It preserves resolved,
refused and unavailable facts, refuses incompatible pins and conflicting merges,
and never invokes a resolver. General durable-format migration remains a separate
port and must prove callable, frame, Site and Link correspondence before publication.
Guest authored bodies charge statements; shipped guest helpers carry an explicit
formula. Native and Both implementations carry their formula at the implementation.

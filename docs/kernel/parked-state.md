# Parked runs: the schema, row by row

A run that is not executing is data in the document's terms ([design](design.md) §6). This page is the schema's checked table: every field the old engine saved, in its `VmContinuation` and in the broker's snapshot (`crates/lash-vm-broker/src/snapshot.rs`, `ledger.rs`), with its counterpart in the kernel schema or the reason it has none. The schema is `lash-kernel-state`; the machine writes and reads it in `crates/lash-kernel-vm/src/machine/parked.rs`.

A row is one of three kinds:

- **Saved.** The kernel schema holds it. Two parked states that differ only there resume differently.
- **Derived.** The document, or another saved row, determines it. It is not written.
- **Discarded.** Nothing about how the run resumes depends on it. It is not written.

The laws in `crates/lash-kernel-vm/src/laws/parked.rs` hold the table: `every_saved_row_changes_how_a_run_resumes` has a pair for each saved row, `what_is_not_saved_leaves_no_trace_in_a_parked_run` covers what is derived or discarded and still exists in the machine, and `a_run_rebuilt_at_every_safe_point_runs_the_same` resumes whole programs against an executable rebuilt under another layout.

## The schema

`ParkedRun` is the whole state. Every coordinate in it is a site (`K-SITE-001` to `K-SITE-005`); every value is a kernel `Value`; every heap object is a kernel `Object` under the identity the run gave it.

| Part | Fields |
| --- | --- |
| `Run` | `kernel`, `document`, `functions` (what the run pins); `charged`; `objects_allocated`; `waits_issued`; `ready`; `withdrawn`; `unreported` |
| session | each binding's name and value |
| `Task` (a handle) | `identity`; `state`; `joiners`; `observed`; `passed`; `failed`; `occurrences` |
| `TaskState` | `ready`; `resuming` with a value, a raise or the task joined; `performing` with the wait; `joining` a handle; `joining_many` with `mode`, `members`; `ended` with the result or the error |
| `Perform` (a wait) | `wait`; `request` (effect name, arguments and identity, or a sleep's identity and duration); `state`: `requested`, `admitted`, or `committed` with the outcome |
| `Call` | `statement`; `bindings` (name, declaring node, value or shared cell); `loops` (site, iterations started, position); `finally` (site, how it was entered) |
| `Held` | `iterated` (what each `for` iterates); `departing` (the value each `finally` leaves with); `arguments` (of a library call) |
| objects | each live object: list, map, set, record, closure (its expression's site and the cells it shares), shared variable |

`joiners` holds every task waiting in a `join` on the handle, on it alone or on a list that names it, in the order the joins began; which kind each is, is the joining task's own `state`. A list join has no number of its own: when it wakes is its place in each running member's `joiners`.

## Stored form

A parked run is stored whole, as the one document `ParkedRun` is, or in parts: a header and one fragment per root, of which a save rewrites only those that changed. The parts hold exactly what the whole does.

- **Roots**, in the order ownership is decided: each session binding by name; then each task by handle number: its handle, then each of its active calls from the outermost, a call's bindings before the values its control state holds.
- **Header**: `Run` and the list of roots. It is written at every save.
- **Fragment**: the root's state and the objects the root owns. An object is owned by the first root, in that order, that reaches it; a reference to an object another root owns is its identity. A task handle in a value names that task's root and is not followed.
- **Change set**: a save against the baseline of the last one names the fragments whose content changed and the roots that are gone. A fragment changed when its root's state differs, when the set of objects it owns differs, or when one of them was written: the heap stamps an object at every write, through whichever alias.
- **Reading** rebuilds the run from the header and every fragment, then writes it again and requires the same bytes: a missing fragment, an object under the wrong root, an object no root reaches and a non-canonical spelling are refused.

The encoding is JSON, as the document's is, with every object closed to unknown members. `schemas/host/kernel-parked-run`, `kernel-parked-header` and `kernel-parked-fragment` are the checked-in schemas. A value nests 128 deep (`K-VAL-034`), two JSON levels to each of its own: a reader of the whole document lifts its JSON decoder's nesting limit, as `ParkedRun::load` does for the parts. The format has no version of its own: the kernel version pins it (`K-VER-001`), and a reader checks the header's `kernel` before it decodes anything else. `lash-kernel-state` commits nothing: the embedder stores the header and the changed fragments in the transaction that admits the park's effects, and declares the stored surface (ADR 0131) where it writes them.

This is the per-root scheme the old VM's sessions used between cells (first-discovery ownership, write stamps, a fixed-point read), applied to the kernel's roots, and written in `lash-kernel-state`, because a kernel crate depends on no other lash crate; there is one encoding of a heap object, `Object`'s own.

## `VmContinuation`

| Field | Kind | In the kernel schema |
| --- | --- | --- |
| `format_version` | Saved | `Run.kernel`. The kernel version owns the schema. |
| `executable` | Saved | `Run.document` and `Run.functions`. The executable is a cache; a run pins the document's identity and every function identity in its manifest, not a compiled form. |
| `reference_semantics` | Discarded | The kernel has one heap model: mutable objects have identity. |
| `instruction_pointer` | Saved | `Call.statement` of the task's innermost call. |
| `active_function` | Derived | The function body that holds `Call.statement`: its unit, or the closure body nearest above it. |
| `operand_stack` | Discarded | A task pauses only between statements (`K-STMT-004`); no unnamed value is live. |
| `pending_tools[..].site`, `.occurrence` | Saved | `Perform.request.identity`: task, site, occurrence and loop context (`K-EFF-008`). |
| `pending_tools[..].receiver`, `.args` | Saved | `Perform.request.effect` and `.args`, copied out of the run. A tool is an effect by name; there is no receiver value. |
| `pending_tools[..]` `Timer.duration` | Saved | `Perform.request.duration`. |
| `pending_tools` as lazy handles | Discarded | A wait is a statement, not a value. An effect started and awaited later is a `spawn` and a `join`. |
| `execution_nonce` | Discarded | It told one cell's handles from the next cell's. A wait's number is `Perform.wait`, and no handle value carries it. |
| `last_value` | Discarded | A run's result is its `return`; no expression value outlives its statement. |
| `slots` | Saved | `Call.bindings`: each bound variable by name and declaring node. A slot's index is a layout. |
| `globals` | Saved | The session's bindings. |
| `iterator_stack[..].cursor` `List`/`Live` | Saved | `Held.iterated` (the collection, or the tuple whose elements are taken) and `Loop.position`. |
| `iterator_stack[..].cursor` `Range` | Discarded | The kernel has no range loop; a dialect compiles one to `while`. |
| `iterator_stack[..].binding_slot` | Derived | The `for` statement's binding. |
| `iterator_stack[..].restore_value` | Discarded | A loop binding is a variable of the loop's body block and ends with each iteration. |
| `frame_stack[..].return_instruction_pointer` | Saved | `Call.statement` of the calling call. |
| `frame_stack[..].function` | Derived | As `active_function`. |
| `frame_stack[..].operand_stack_base` | Discarded | As `operand_stack`. |
| `frame_stack[..].slots`, `.globals`, `.iterator_stack` | Saved | The calling call's `bindings`, `loops` and `Held.iterated`. |
| `frame_stack[..].return_target` `Direct` | Derived | A call returns to its calling statement. |
| `frame_stack[..].return_target` `Callback` (`function`, `this_arg`, `calls`, `next_index`, `results`, `completion`, `allow_effects`, `live_url_search_params`, `array_like`) | Saved, as ordinary state | A function that takes a callback has a kernel-code body (`K-LIB-006`): its progress is that body's `Call`, with its loop and its variables. `Held.arguments` keeps the arguments its charge formula measures. |
| `handler_stack` (all fields) | Derived | The `try` statements around `Call.statement`, read from its site. |
| `finally_stack[..].completion` | Saved | `Finally.entered` and, for a throw or a return, `Held.departing`. |
| `finally_stack[..].completion.resume_instruction_pointer` | Derived | A `finally` entered normally goes on after its `try`. |
| `finally_stack[..].completion.origin` | Discarded | An error is a value with a kind (`K-ERR-001`); a raise carries nothing else. |
| `finally_stack[..]` depths and `frame_function` | Derived | The `try`'s site and the call it is in. |
| `occurrence_counters` | Saved | `Task.occurrences`, per task, at action sites. |
| `loop_stack[..].site`, `.iterations` | Saved | `Loop.site`, `Loop.started`. |
| `loop_stack[..].activation`, `.checks`, `.checking`; `loop_activations` | Discarded | An effect's loop context is site and iteration (`K-EFF-008`). The kernel counts no activations or checks. |
| `loop_stack[..].call_depth`, `.handler_depth` | Derived | The loop's site and the call it is in. |
| `mode` | Derived | A run of `main` is a session cell; a run of an entry is not. The outermost call's unit says which. |
| `profile` | Discarded | Profiling is not state. |
| `pending_error_span` | Discarded | As `origin`. |
| `instructions_executed` | Saved | `Run.charged`, in charge units. |
| `heap` objects | Saved | `ParkedRun.objects`, each live object once. |
| `heap` allocation counter | Saved | `Run.objects_allocated`. Object identities appear in a run's results (`K-SES-002`). |
| `heap` logical bytes | Derived | The machine accounts memory again when it imports, from what is live. |
| `resume` `NextInstruction` / `ReissueOperation` | Derived | The task's state: a task that is merely ready issues its statement; one that waits or resumes with a value continues it. |
| `resume.operation` `ResourceOperation`, `Sleep` | Saved | `Perform`. |
| `resume.operation` `ResourceOperationBatch`, `Await { settled }` | Saved | `TaskState::JoiningMany` with its members. Which members have ended is each member's own `state`; no results are copied into the waiting task, so the 1024-result limit is gone and the bound is `Bounds::join_members`. |
| `resume.loop_phase` (`yield_budget`, `announced_checkpoint`) | Discarded | A fuel slice is the embedder's argument to `run`; slices change no value, order or charge (`K-MACH-005`). |
| `expired_functions` | Discarded | A binding that reaches a closure is not carried to the next cell, and the run's end names it (`K-SES-003`). Nothing is remembered across cells. |

## The broker's snapshot

| Field | Kind | In the kernel schema |
| --- | --- | --- |
| `Checkpoint.vm` | Saved | The header and the fragments. |
| `Checkpoint.ledger.pending` (`run`, `members`, `kind`, `request`, `fingerprint`, `waits`) | Saved, as a set | One operation becomes every `Perform` of the run: `wait`, `request` and `state`. The admission's run, the members' identities, the fingerprint and the pinned waits are the embedder's rows for each wait, keyed by `Perform.wait` and `request.identity`. |
| `Checkpoint.ledger.operations` (`OperationId`, `OpenMember.call`, `.request`) | Embedder's | The ledger keeps the admitted members that have not settled. The machine names each by its wait and holds what it asked. |
| `Checkpoint.ledger.next_admission` | Embedder's | Admission sequence. The machine's own counter is `Run.waits_issued`. |
| `Checkpoint.ledger.frame_epoch` | Embedder's | Transport state between a worker and its parent. |
| `Checkpoint.ledger.grants` (`run`, `call_id`, `frame_epoch`) | Embedder's | A handle is a kernel value the host gave meaning to (`K-VAL-016`). Which admission granted it is the host's authority check. |
| `Checkpoint.host` | Embedder's | The host's own state for the execution. |
| `Checkpoint.end` | Discarded | A run that has ended has no parked state; `run` returns its end. |
| `QuietPoint.members`, `.waits`, `.with` | Embedder's | What commits beside the state. A park hands the machine's requests out (`K-MACH-003`); the embedder admits them in the transaction that stores the header and the changed fragments. |
| `Committed.rev`, `.waits` | Embedder's | The stored revision and the pinned waits. |

## A wait's three states

`requested`: the wait was issued since the last park and the embedder has not seen it. It exists only in a state saved at a fuel slice; the next park hands it out. `admitted`: a park handed it out, and the embedder admitted it with the state. `committed`: its outcome was delivered and its task has not yet run with it.

An outcome that committed in the ledger and never reached a machine is the ledger's to deliver: the state the machine resumes from holds the wait as `admitted`, and `deliver` makes it `committed`. A committed effect is never issued again, because nothing in a resumed run issues an `admitted` wait.

## Findings

1. **A withdrawn wait is remembered for the life of the run.** `Run.withdrawn` grows by one for every wait a `cancel` withdraws after it was handed out, so that a late outcome is dropped and not refused (`K-MACH-004`). A run that cancels without bound grows its header without bound. A fix is the embedder's acknowledgement of a withdrawal, after which the number can be forgotten; that is a change to the machine interface.
2. **The machine keeps what an admitted wait asked.** The embedder's ledger holds each admitted request too. The state holds it so that a host reads what every task waits on from the run alone; the cost is the arguments' bytes in the task's fragment until the outcome is consumed.
3. **A loop's position in a map or a set is a count.** Counting the entries at or before the one last visited costs time in the loop's progress at every save taken inside it.
4. **Section 6 of the design names two things the kernel does not have.** A `finally` entered by a throw has no origin beside its value, and a loop has no activation number or check count: `K-ERR-001` and `K-EFF-008` define errors and loop context without them.

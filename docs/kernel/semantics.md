# Kernel semantics

The lash kernel is one small language with one meaning. This document is that meaning, one behaviour per numbered rule. The design it implements is [design.md](design.md); the types the rules speak of are in `crates/lash-kernel-doc`, linking, derived facts and admission in `crates/lash-kernel-check`, edits in `crates/lash-kernel-edit`, and the machine interface in `crates/lash-kernel-vm`.

A rule id is permanent. A rule is never renumbered or reused; a rule that is withdrawn keeps its id and says so. Other crates, the conformance corpus and dialect deviation registers cite rules by id. Changing what a rule says is a new kernel version (`K-VER-001`).

Where a rule says an operation *raises* `kind`, it raises an error value of that kind (`K-ERR-002`), which a `catch` may take. Where it says a document *is refused*, structural validation or admission rejects it before anything runs.

Rules whose cases need a running machine are pinned by the conformance corpus (`lash-kernel-conformance`), one named case per rule.

## Versions (`K-VER`)

- **K-VER-001.** Kernel version 1 pins: the value kinds and their equality, ordering and text; the forms; evaluation order; the statement rule; task scheduling; site derivation; the cost table; the canonical form identities are taken over; the error kinds of `K-ERR-002`; and every rule in this document. A document and a function definition each state the version they are written for; a reader refuses any other.
- **K-VER-002.** A library function is not part of the kernel version. It is fixed or added as a new definition with a new identity (`K-ID-002`).
- **K-VER-003.** A kernel version is a value. A build interprets the newest version and, for the one release after a breaking version ships, the version before it; a document, a definition or a parked run that states any other is refused before it is decoded. Every library function a document lists is written for the document's version. A machine restores a parked run only under the version the run states, and writes that version into what it parks. Everything two versions differ in is an arm of a `match` on the version value, so the older interpreter is the sum of its arms and is deleted in one place: its variant.
- **K-VER-004.** A breaking version N+1 ships one migration from N. It is three total functions, each answering its result or a typed refusal and none panicking: a definition is redeclared for N+1; a document is rewritten, with the correspondence of every surviving node's site (`K-EDIT-001`) and the map from each listed library function to the one redeclared for it; a parked run is carried onto the rewritten document. A refusal names what it could not carry: the function with no counterpart, the node with no form, the site a run stands on, the state N+1 cannot express. The correspondence lists a node of a library function's body only where the migration vouches for where the redeclared body put it.
- **K-VER-005.** Carrying a parked run moves each coordinate it saves to its successor in the correspondence: a task's pending statement, a variable's declaration, a closure's expression, a loop or cleanup block under way, a task's spawn site, an effect's site and enclosing loops, and each action occurrence. Each pinned function identity becomes the redeclared function's, and each reference to a declared function its name in the rewritten document. Values, heap objects, waits and meters are carried as they are. A run parked under N at any park, carried and resumed under N+1, yields the values, errors and effect identities (mapped through the correspondence) of the same run continued under N; a difference N+1 makes on purpose shows only in what it was made to change.

## Values (`K-VAL`)

### Kinds

- **K-VAL-001.** A value is of exactly one kind: null, absent, bool, integer, float, text, bytes, timestamp, tuple, list, map, set, record, closure, error, task handle, function reference, handle, or ref. No value changes kind and nothing converts a value of one kind into another implicitly.
- **K-VAL-002.** Null and absent are distinct values. Absent is the value of a missing record field when read (`K-FORM-009`), of a parameter no argument was given for (`K-FN-004`), and of the literal `absent`. It is otherwise an ordinary value: it may be bound, stored in a collection, passed and returned. It is not a legal map key (`K-KEY-001`).
- **K-VAL-003.** A bool is `true` or `false`. Nothing else is true or false: a condition that is not a bool raises `type_error` (`K-FORM-013`).
- **K-VAL-004.** An integer is a mathematical integer of any size. There is no negative zero integer.
- **K-VAL-005.** A float is an IEEE 754 binary64 value. Every NaN is the same NaN: no operation distinguishes NaN payloads or signs.
- **K-VAL-006.** A text is a finite sequence of Unicode scalar values. It cannot hold a lone surrogate. The empty text is a text.
- **K-VAL-007.** A text is addressed two ways, by separate library functions. By code point, position `i` is the `i`-th scalar value. By UTF-16 unit, a scalar value below U+10000 is one unit and any other is two. Reading the unit at a UTF-16 position gives it as an integer, including one half of a pair. A UTF-16 slice whose boundary falls between the two units of a pair raises `text_boundary`, since the result could not be a text.
- **K-VAL-008.** Bytes are an immutable finite sequence of octets.
- **K-VAL-009.** A timestamp is an instant: an integer count of nanoseconds since 1970-01-01T00:00:00Z. It has no zone and no calendar. It is immutable.
- **K-VAL-010.** A tuple is an immutable sequence of fixed length. Its members may be of any kind, heap objects included; the tuple still has no identity.
- **K-VAL-011.** A list, a map, a set and a record are mutable heap objects with identity. A map and a set keep their entries in insertion order. A record keeps its fields in the order they were first written.
- **K-VAL-012.** A closure is a heap object with identity: a function body together with the variables it shares with its defining scope (`K-CLO-001`).
- **K-VAL-013.** An error is an immutable value with three parts: `kind` (text), `message` (text) and `data` (any value).
- **K-VAL-014.** A task handle names one task of the run (`K-TASK-001`). It holds the task's state and, once the task has ended, its result or its error.
- **K-VAL-015.** A function reference names a declared function of the document. It captures nothing and is data: it may cross the effect boundary (`K-EFF-002`).
- **K-VAL-016.** A handle is a typed reference to a host resource or projection: a host kind and a host identifier, both text. The kernel gives neither meaning.
- **K-VAL-017.** A ref is the identity of a heap object or a task, taken with `ref(x)` (`K-KEY-004`). It is immutable.

### Equality

- **K-VAL-020.** `eq(a, b)` is defined for every pair of values and raises nothing. Values of different kinds are not equal, with the one exception of an integer and a float (`K-VAL-021`). In particular null is not absent, `true` is not 1, and a tuple is not a list.
- **K-VAL-021.** Numbers are equal when they are the same mathematical value. An integer is compared with a float exactly, against the float's true value and without converting the integer first: `2^53 + 1` is not equal to `9007199254740992.0`. `-0.0` equals `0.0`. NaN equals nothing, itself included. Each infinity equals itself and no integer.
- **K-VAL-022.** Null equals null; absent equals absent; bools, texts, bytes and timestamps are equal when their contents are.
- **K-VAL-023.** Two tuples are equal when they have the same length and their members are pairwise equal in order. A tuple that holds NaN is therefore not equal to itself.
- **K-VAL-024.** Two lists are equal when they have the same length and their elements are pairwise equal in order. Two maps are equal when they have the same number of entries and each key of one is a key of the other (`K-KEY-002`) with an equal value. Two sets are equal when each has the other's members. Two records are equal when they have the same field names with equal values. Insertion order is not compared.
- **K-VAL-025.** Two errors are equal when their kinds, messages and data are. Two function references are equal when they name the same function. Two handles are equal when their kind and identifier are. Two closures are equal only when they are the same closure, and two task handles only when they name the same task. Two refs are equal when they name the same object or task.
- **K-VAL-026.** `eq` terminates on cyclic objects. While a pair of heap objects is being compared, meeting the same pair again counts as equal: two cyclic structures are equal when no finite path through them tells them apart.
- **K-VAL-027.** `same(a, b)` is identity. For two heap objects, closures or task handles it holds when they are the one object. For immutable values it holds when they are the same kind and the same datum: integer 1 is not the same as float `1.0`, `-0.0` is not the same as `0.0`, NaN is the same as NaN, and two tuples are the same when their members pairwise are.
- **K-VAL-028.** No operation exposes a hash. A map or a set is observable only through key equality (`K-KEY-002`) and insertion order, so however an implementation hashes, keys that are equal must hash alike: an integral float as its integer, `-0.0` as zero, every NaN as one key.

### Ordering

- **K-VAL-029.** Order is defined within these kinds and raises `type_error` for any other pairing: two numbers, by mathematical value, an integer against a float exactly; two texts; two byte strings, octet by octet, a prefix first; two timestamps; two bools, `false` first; two tuples or two lists, member by member, a prefix first. Null, absent, maps, sets, records, closures, errors, task handles, function references, handles and refs have no order.
- **K-VAL-030.** Texts are ordered by code point, a prefix first. This differs from UTF-16 unit order for scalar values at or above U+10000; a dialect that needs unit order calls the library function that gives it.
- **K-VAL-033.** NaN is unordered. Each of less-than, less-or-equal, greater-than and greater-or-equal is false when either operand is NaN. A three-way comparison raises `unordered`, and so does a sort that meets one.

### Nesting

- **K-VAL-034.** An immutable value nests tuples, and errors through their data, at most 128 levels deep. Constructing a deeper one raises `too_deep`. A list, map, set or record may nest to any depth, since each is an object of its own; a value copied out of the run (`K-EFF-002`) nests at most 128 levels of any kind.

### Text of numbers

- **K-VAL-031.** A float's text is: `nan`, `inf` or `-inf` for the non-finite values; `0.0` or `-0.0` for the zeros; otherwise the shortest decimal digits that read back as the same float (the closest to the true value where several do), laid out as a plain decimal with at least one digit after the point when `1e-4 <= |x| < 1e16`, and as `d[.ddd]e<exponent>` with no `+` and no leading zeros otherwise. So `1.0`, `0.1`, `0.0001`, `9.999e-5`, `1000000000000000.0`, `1e16`, `1.5e300`, `5e-324`. This is the spelling of a float literal in kernel text and in the JSON encoding, and what the library's number-to-text function returns.
- **K-VAL-032.** An integer's text is its decimal digits with a leading `-` when negative: no `+`, no leading zero, no separators.

## Numbers (`K-NUM`)

- **K-NUM-001.** Addition, subtraction, multiplication and negation of integers are exact and give an integer.
- **K-NUM-002.** Arithmetic on floats is IEEE 754 binary64, rounding to nearest, ties to even.
- **K-NUM-003.** Arithmetic on an integer and a float converts the integer to the nearest float (ties to even), then follows `K-NUM-002`. An integer that rounds to an infinity (magnitude at or above `2^1024 - 2^970`) raises `number_range`. Comparison is the exception: it never converts (`K-VAL-021`, `K-VAL-029`).
- **K-NUM-004.** `div(a, b)` gives a float. For two integers it is the exact quotient rounded once to the nearest float, not the quotient of two converted floats; a zero divisor raises `division_by_zero`, and a quotient that rounds to an infinity raises `number_range`. When either operand is a float it is IEEE division, so a zero divisor gives an infinity or NaN.
- **K-NUM-005.** `div_floor(a, b)` and `rem_floor(a, b)` on integers give integers `q` and `r` with `a = q*b + r`, `q` the quotient rounded toward negative infinity, and `r` zero or of the divisor's sign. A zero divisor raises `division_by_zero`.
- **K-NUM-006.** `div_trunc(a, b)` and `rem_trunc(a, b)` on integers give integers `q` and `r` with `a = q*b + r`, `q` the quotient rounded toward zero, and `r` zero or of the dividend's sign. A zero divisor raises `division_by_zero`.
- **K-NUM-007.** When either operand of `rem_trunc` is a float, the result is the IEEE remainder of truncating division (C `fmod`), and `div_trunc` is the IEEE quotient rounded toward zero. When either operand of `rem_floor` is a float, the result is `fmod(a, b)`, plus `b` when it is non-zero and its sign differs from `b`'s, and a zero result takes `b`'s sign; `div_floor` is `(a - rem_floor(a, b)) / b` rounded to the nearest integral float. A zero float divisor raises nothing: the quotients are what `div` gives and the remainders are NaN.
- **K-NUM-008.** A bool, a text and null are not numbers. An arithmetic or ordering operation given one raises `type_error`.

## Keys (`K-KEY`)

- **K-KEY-001.** A map key or a set member is a legal key: null, a bool, an integer, a float, a text, bytes, a timestamp, a function reference, a ref, or a tuple whose members are all legal keys. Any other value used as a key raises `invalid_key`. A heap object is used as a key through its ref (`K-KEY-004`).
- **K-KEY-002.** Two keys are the same key when they are `eq`, except that NaN is the same key as NaN, also inside a tuple. So integer 1 and float `1.0` are one key, `-0.0` and `0.0` are one key, and `true` and 1 are two.
- **K-KEY-003.** Writing under a key a map already holds replaces the value and keeps the entry's first key and its position. After `m[1] = "a"` and `m[1.0] = "b"`, the map has one entry whose key is the integer 1. Adding a member a set already holds changes nothing.
- **K-KEY-004.** `ref(x)` takes the identity of a list, map, set, record, closure or task handle, and raises `type_error` for any other kind. `deref(r)` gives the object or task handle back. A ref keeps what it names alive.
- **K-KEY-005.** An entry removed and written again takes a new position at the end.

## Forms (`K-FORM`)

- **K-FORM-001.** The forms are closed. Statements: `let`, assign, remove, do, `if`, `for`, `while`, `break`, `continue`, `return`, `try`, `throw`, `print`, `finish`, `fail`. Actions (`K-STMT-001`): call, `perform`, `sleep`, `join` on one handle, `join` on a list, `yield`, `spawn`, `cancel`. Expressions: literal, variable, tuple, list, map, set, record, field read, index read, closure, native library call, clock, random, projection read. A form is added only by a new kernel version. There is no operator: arithmetic, comparison and logic are library functions.
- **K-FORM-002.** A literal is null, absent, a bool, an integer, a float, a text, bytes, or a reference to a declared function.
- **K-FORM-003.** A block opens a scope. `let` declares a variable in the enclosing block, from the statement after it to the block's end: its own right-hand side does not see it, so `let x = f(x)` reads an outer `x`. An inner block may declare the same name again and shadows it. A function's parameters are declared in its body's scope, a `for` binding in the loop body's scope, fresh on each iteration, and a `catch` binding in the catch body's scope. Reading or assigning a variable that no enclosing scope has declared at that point is refused at admission; in a run it raises `unbound_variable`.
- **K-FORM-004.** `let x = v` binds `v` to a new variable. Assigning to a variable replaces what it holds; it changes no object.
- **K-FORM-005.** Assigning to a field requires a record and raises `type_error` otherwise. It replaces the field's value, or adds the field at the end.
- **K-FORM-006.** Assigning to an index: of a list, the index must be a number with an integral value `i`, `0 <= i <= length`; `i < length` replaces the element, `i == length` appends, anything else raises `index_out_of_range`. Of a record, the index must be text or it raises `type_error`; it replaces the named field or adds it at the end in insertion order (`K-VAL-011`). Of a map, the index must be a legal key; the entry is replaced or added (`K-KEY-003`). Of a set, the value assigned must be a bool: `true` adds the index as a member, `false` removes it. Any other target raises `type_error`.
- **K-FORM-007.** `remove` deletes a record's field, a map's entry or a set's member, and does nothing when it is not there. An indexed removal from a record requires a text index naming the field; any other index kind raises `type_error`. Of a list it deletes the element at an integral index `0 <= i < length`, moving later elements down one, and raises `index_out_of_range` otherwise. Any other target raises `type_error`.
- **K-FORM-008.** Constructing a tuple, list, map, set or record evaluates its parts left to right and gives a new value, for the four mutable kinds a new object each time the expression runs. A map literal that writes one key twice keeps the first key and position and the last value; a set literal keeps the first of equal members. A record literal that names a field twice is refused.
- **K-FORM-009.** Reading a field: of a record, its value, or absent when the record has no such field; of an error, its `kind`, `message` or `data`, and absent for any other name. Any other target raises `type_error`.
- **K-FORM-010.** Reading an index: of a list or a tuple, the element at an integral index `0 <= i < length`, raising `index_out_of_range` otherwise (there is no negative indexing in the form; the library has it); of a record, a text index gives the named field, or absent when the field is missing, and any other index kind raises `type_error`; of a map, the value under that key, raising `key_missing` when there is none; of a set, a bool: whether the index is a member. Any other target, text and bytes included, raises `type_error`.
- **K-FORM-011.** A closure expression evaluates to a new closure each time it runs. Creating it runs none of its body.
- **K-FORM-013.** `if` evaluates its condition, which must be a bool, and runs one block. `while` evaluates its condition before each iteration.
- **K-FORM-014.** `break` ends the innermost enclosing loop of the same function body; `continue` starts its next iteration. Either outside a loop is refused. A closure body does not see a loop around the closure.
- **K-FORM-015.** `return v` ends the current function with `v`. A function whose body ends without one returns null. In `main` it ends `main` (`K-TASK-018`).
- **K-FORM-016.** `throw v` raises `v`, which may be any value. `try` runs its body; if a value is raised inside it and there is a `catch`, the catch body runs with the value bound.
- **K-FORM-017.** A `finally` block runs whenever control leaves its `try`: on normal completion, on a raise the catch did not take or itself raised, on `return`, `break` and `continue`. When it completes normally the departure it interrupted resumes; when it leaves by its own raise, `return`, `break` or `continue`, that replaces the departure it interrupted.
- **K-FORM-018.** `print v` copies `v` out (`K-EFF-002`) and hands it to the host at once. It is not a wait.
- **K-FORM-019.** `finish v` ends the run with result `v`, and `fail v` ends it as failed with reason `v`, from any task and any call depth (`K-TASK-018`). The value is copied out (`K-EFF-002`). No `finally` runs.

## The statement rule (`K-STMT`)

- **K-STMT-001.** An action is the whole right-hand side of its own statement (`let x = <action>`, an assignment, or `do <action>`), and its arguments are atoms: variables or literals. The actions are a call of a declared function, of a closure or function reference held in a variable, or of a library function by identity; `perform`; `sleep`; `join`; `yield`; `spawn`; `cancel`. None can appear inside an expression: the tree has no node for it, kernel text refuses it at the form, and the JSON encoding has no spelling for it.
- **K-STMT-002.** A call inside an expression names a library function whose definition states a native implementation (`K-LIB-002`). A call inside an expression to a function with only a kernel-code body is refused, naming the call.
- **K-STMT-003.** The rule is checked on each statement alone, from the statement and the definitions of the library functions it names. Nothing is inferred about what may wait, and no edit to another statement, function or document can make an admitted statement invalid. It is checked the same way for documents, host edits and library bodies.
- **K-STMT-004.** A task therefore pauses only at an action, between statements, and when it does no value is live that a variable does not hold.
- **K-STMT-005.** A front end hoists to satisfy the rule, in its source's evaluation order: every operand the source evaluates before a statement-position form is bound to a temporary first, because another task may run while the form waits. Hoisted temporaries are ordinary variables.

## Evaluation (`K-EVAL`)

- **K-EVAL-001.** Evaluation is left to right, operands before operation. An expression's parts are evaluated in the order kernel text writes them.
- **K-EVAL-002.** `let` evaluates its right-hand side, then binds.
- **K-EVAL-003.** An assignment evaluates its right-hand side first (running the action, when it is one), then the place's target, then the place's index, then writes. No part of the place is evaluated before an action waits.
- **K-EVAL-004.** An action reads its atoms left to right when it starts. A callee held in a variable is read before the arguments.
- **K-EVAL-005.** A map literal evaluates each entry's key, then its value, before the next entry.
- **K-EVAL-006.** Every operation is strict: it is defined on the kinds its rule names and raises `type_error` on any other. Nothing coerces. A front end that wants an earlier or a different failure emits an explicit check.
- **K-EVAL-007.** An operation that raises has changed nothing: a failed assignment, removal or construction leaves every object as it was.

## Iteration (`K-ITER`)

- **K-ITER-001.** `for` evaluates its iterable once. It must be a list, a tuple, a map or a set; anything else raises `type_error`. A list or tuple gives its elements, a map its keys, a set its members.
- **K-ITER-002.** A loop over a list reads live by position. Before each iteration, if the position is below the list's current length it binds the element there and advances; otherwise the loop ends. Elements appended during the loop are visited; removing an element at or before the position makes the loop skip one.
- **K-ITER-003.** A loop over a map or a set visits entries in insertion order, sees entries added during the loop, and skips entries removed before their turn. An entry removed and added again is at the end (`K-KEY-005`) and is visited there. Replacing a value does not move the entry.
- **K-ITER-004.** A dialect whose language iterates differently compiles the difference in: by looping over a copy, or by a helper.

## Closures (`K-CLO`)

- **K-CLO-001.** A closure captures by reference. A variable of an enclosing scope that the closure's body names is shared: the closure and the scope read and write the one variable, and it lives as long as either does.
- **K-CLO-002.** Each iteration of a `for` has its own binding, so closures made in different iterations share different variables. A `while` loop's variables are those of the scopes around it.
- **K-CLO-003.** A closure's `return` returns from the closure. It cannot `break` or `continue` a loop outside itself.
- **K-CLO-004.** A closure is not data. It cannot cross the effect boundary (`K-EFF-002`), is not a legal key except through its ref, and is not carried from one session cell to the next (`K-SES-003`).

## Functions (`K-FN`)

- **K-FN-001.** There is one kind of function. A declared function, a closure and a library function with a kernel-code body are all kernel code and may contain any form. Whether a call is free of effects is derived, never declared.
- **K-FN-002.** A declared function is closed: its body sees its parameters, its own variables, the document's declared functions and the library, and none of `main`'s variables.
- **K-FN-003.** A call creates an activation, binds the arguments to the parameters in order, runs the body, and yields what it returns. A function may call itself; depth is bounded (`K-BND-002`).
- **K-FN-004.** A call may give fewer arguments than there are parameters; each parameter with no argument is absent. A call that gives more raises `arity`; for a declared or library callee it is refused before it runs. A library function's signature marks which parameters may be omitted, and a call that omits a required one is refused.
- **K-FN-005.** Calling through a variable requires a closure or a function reference and raises `type_error` otherwise.
- **K-FN-006.** A value raised in a callee and not caught there is raised at the calling statement.
- **K-FN-007.** A library function is total over the argument types its signature declares and raises `type_error` on any other value. The check is the function's own: the machine hands it the arguments as given, with `absent` for each one omitted.

## Errors (`K-ERR`)

- **K-ERR-001.** A raise comes from `throw`, from an operation's rule, from a library function, from a failed effect (`K-EFF-009`), from a `join` that observes a task's error (`K-TASK-016`), or from `cancel` (`K-TASK-017`). What `throw` raises is the value given, unchanged even when uncaught; a `join` raises the task's error value unchanged too. Every other raise is an error value.
- **K-ERR-002.** The kernel's own error kinds are: `type_error`, `unbound_variable`, `index_out_of_range`, `key_missing`, `invalid_key`, `division_by_zero`, `number_range`, `unordered`, `text_boundary`, `arity`, `cycle`, `not_data`, `effect_result`, `cancelled`, `join_self`, `empty_join`, `all_failed`, `too_deep`. A library function adds the kinds its definition declares. A program may raise any kind it likes.
- **K-ERR-003.** A value raised in a task and caught nowhere ends the task with that value as its error (`K-TASK-016`). In `main` it ends the run (`K-TASK-018`): with `RunError::Uncaught(Datum)` carrying the value unchanged, with no wrapping. An error is one kind of value.
- **K-ERR-004.** Passing an execution bound is not a raise. It ends the run, and no `catch` or `finally` sees it (`K-BND-001`).

## Tasks (`K-TASK`)

- **K-TASK-001.** A run is a set of tasks. `main` (or the entry the run was started on) is the first. Each task has its own active calls; all tasks share the heap. There is no parallelism: one task runs at a time.
- **K-TASK-002.** `spawn f(args)` creates a task that will run `f` and yields its handle. Handles are numbered in spawn order from 1; `main` is 0. The new task runs at once, until its first wait or its end. Then the spawning task continues, before any other ready task.
- **K-TASK-003.** A running task runs until it reaches a wait or ends. Then the task at the front of the ready queue runs. The queue is first in, first out.
- **K-TASK-004.** The waits are: `perform`; `sleep`, for any duration including zero; `join` on a handle whose task has not ended; `join` on a list that is not yet decided (`K-TASK-011` to `K-TASK-014`); and `yield`. Nothing else pauses a task: not a call, not a host read, not `print`, not `spawn`, not `cancel`.
- **K-TASK-005.** A wait that completes makes its task ready, at the back of the queue. Outcomes delivered between two runs of the machine make their tasks ready in the order they were delivered.
- **K-TASK-006.** `yield` puts the running task at the back of the ready queue. If the queue is otherwise empty the task continues at once. `yield` does not let the embedder deliver an outcome: outcomes arrive only when no task is ready (`K-TASK-024`).
- **K-TASK-007.** A `join` on a handle whose task has already ended continues at once: the task does not leave the front. A `join` on a list that is already decided when it starts does the same.
- **K-TASK-008.** When a task ends, every `join` waiting on it (on its handle alone, or a list `join` it decides) becomes ready in the order the joins began.
- **K-TASK-009.** A `join` on one handle yields the task's result, or raises the task's error. Joining a handle again, from any task, gives the same answer: the same result value, or a raise of the same error value.
- **K-TASK-010.** A `join` requires a task handle, and a list `join` a list or a tuple of task handles; anything else raises `type_error`. A task that joins its own handle, alone or in a list, raises `join_self`. A handle may appear in a list more than once. A list longer than the member bound ends the run (`K-BND-001`).
- **K-TASK-011.** `join all` returns when every member has ended, with a new list of their results in member order, or raises at the first failure. When it starts, if any member has already failed, it raises the error of the first such member in list order; otherwise, while it waits, it raises the error of the first member to fail. An empty list returns an empty list at once.
- **K-TASK-012.** `join settled` returns when every member has ended, with a new list in member order of new records: `{status: "ok", value: v}` for a member that returned `v`, and `{status: "error", error: e}` for one that ended in `e`. It never raises for a member. An empty list returns an empty list at once.
- **K-TASK-013.** `join race` returns or raises as the first member to end did. When it starts, if members have already ended, that is the first of them in list order. An empty list raises `empty_join`.
- **K-TASK-014.** `join any` returns the result of the first member to succeed; when it starts, the first already-succeeded member in list order. If every member has failed it raises `all_failed`, whose data is a new list of the members' errors in member order. An empty list raises `empty_join`.
- **K-TASK-015.** A list `join` cancels nothing. Members that have not ended when it returns keep running.
- **K-TASK-016.** A value that ends a task is data on its handle and nothing more: it stops no other task and does not end the run by itself. It is *observed* when a `join` on the handle raises it, and when the task is or has been a member of a list `join` that has returned or raised, whichever member decided it and whether the task failed before or after.
- **K-TASK-017.** `cancel h` requires a task handle and yields null without waiting. If the task has ended it does nothing. If the task is waiting, its wait is withdrawn and the task becomes ready, at the back of the queue; when it next runs, the waiting statement raises an error of kind `cancelled` in place of its result. If the task is ready, the raise replaces the result of the wait it was resuming from. If the task is the running task, the `cancel` statement itself raises. The raise is ordinary: `catch` may take it and `finally` blocks run, and they may wait. A task cancelled again while it cleans up is raised in again.
- **K-TASK-018.** A run ends when `main` returns, when `main` ends by an uncaught raise, or when any task runs `finish` or `fail`. At that point no further guest code runs, in any task. If `main` ended by a raise, the run ends in that error. Otherwise every task other than `main` and the one that ran the `finish` or `fail` is examined: one that ended and whose error, if any, was observed is ignored; one that has not ended but is or was a member of a list `join` that has returned or raised is cancelled; any other task that has not ended, and any task that ended in an unobserved error, makes the end a typed error naming those tasks (`RunError::TasksOutstanding`), in place of the result.
- **K-TASK-019.** A task cancelled by the run's end runs no cleanup: its waits are withdrawn and its `finally` blocks do not run. A dialect whose language cleans up, waits for or silently drops outstanding tasks compiles that in before `main` returns, using `tasks.unfinished()`, `cancel` and `join`.
- **K-TASK-020.** A task's identity is `main`, or the triple of the task that ran the `spawn`, the site of the `spawn`, and how many times that task had run that site before. The handle number of `K-TASK-002` is a name within one run; the identity is what a host and the parked state key on.
- **K-TASK-021.** `tasks.unfinished()` yields a new list of the handles of every task that has not ended, other than the caller's, in spawn order.
- **K-TASK-022.** The number of tasks that have not ended is bounded (`K-BND-001`); a `spawn` that would pass the bound ends the run.
- **K-TASK-023.** If no task is ready and no task waits on a `perform` or a `sleep`, nothing can wake the run. It ends with `RunError::Deadlock`, naming the waiting tasks.
- **K-TASK-024.** When no task is ready the run parks. Every `perform` and `sleep` requested since the last park is handed to the embedder then, in request order, and not before. The embedder admits them together with the saved state, in one transaction.
- **K-TASK-025.** The embedder may deliver committed outcomes in any order, and several before the machine runs again. The schedule is a function of the document, the start, the host's answers and the order of deliveries, and of nothing else.

## The effect boundary (`K-EFF`)

- **K-EFF-001.** `perform e(args) as T` requests one run of effect `e`, which the manifest must list, and waits. It yields the effect's result decoded as `T`.
- **K-EFF-002.** A value that leaves the run is copied out as a tree: an effect's arguments, the value of `print`, `finish` and `fail`, a projection read's request, and the run's result. Null, absent, bools, numbers, texts, bytes, timestamps, errors, function references and handles are copied as themselves; a tuple, list, map, set or record is copied member by member, an object reached twice being copied twice. A cycle raises `cycle`. A closure, a task handle or a ref raises `not_data`. Nesting deeper than 128 levels raises `too_deep` (`K-VAL-034`). All are raised at the statement, before anything is requested; for the run's result that statement is the `return` in `main`.
- **K-EFF-003.** A result is a fresh graph. Every list, map, set and record in it is a new object, and it shares no identity with the arguments or with any earlier result.
- **K-EFF-004.** A wait copies nothing inside the run. Two variables that held one object before a wait hold one object after it, and after a park and a resume.
- **K-EFF-005.** A result's numbers arrive as written, undecoded, and are decoded by the type the `perform` states. Under `Int` the number must have an integral value, however spelled (`1.0`, `1e3`), and is that integer exactly. Under `Float` it is the nearest float, ties to even. Under `Number`, and under `Any`, it is decoded by the document's policy (`K-EFF-006`).
- **K-EFF-006.** The manifest states one policy for a bare number. `float`: every bare number is the nearest float. `by_spelling`: a number written with no fraction and no exponent is an integer, exactly; any other is the nearest float.
- **K-EFF-007.** A result that does not fit the stated type raises `effect_result` at the `perform`: a wrong kind, a missing required field, an `Int` with a fractional value, a number whose nearest float is an infinity. Under a union the first member that fits, in order, decodes the value. Under `Any` a JSON array is a list and a JSON object is a record.
- **K-EFF-008.** An effect's identity is its task's identity (`K-TASK-020`), the site of the `perform` or `sleep`, how many times that task had run that site before, and the loops the task was inside, outermost first across its active calls, each with its iteration number.
- **K-EFF-009.** An effect that fails raises the error the embedder delivers, at the `perform`.
- **K-EFF-010.** `sleep d` requires a number of milliseconds that is finite and not negative, and raises `type_error` for another kind and `number_range` otherwise. It is always a wait.
- **K-EFF-011.** An effect whose outcome has committed is never executed again. After a crash a run resumes from its last saved state, and a `perform` that runs again there is answered from the committed outcome.

## Host reads (`K-HOST`)

- **K-HOST-001.** Clock, random and a projection read are answered by the embedder at once, inside the statement that asks. They are not waits and do not end the running task's turn. Their answers are not saved: after a crash, a read in a stretch that was not saved is drawn again.
- **K-HOST-002.** `clock` yields a timestamp.
- **K-HOST-003.** `random` yields a float `x` with `0 <= x < 1`: the top 53 of 64 uniformly random bits the embedder supplies, times `2^-53`.
- **K-HOST-004.** `read(h, request)` requires a handle and raises `type_error` otherwise. The request is copied out (`K-EFF-002`). The answer is kernel data decoded as `Any` (`K-EFF-005`), a fresh graph. An error the host answers with is raised at the read.

## Charges (`K-CHG`)

- **K-CHG-001.** A run is charged in charge units, deterministically: the charge of a run is a function of what it executed and of nothing else. It does not depend on the engine's layout, on caches, on whether a native implementation or a kernel body ran, or on parking.
- **K-CHG-002.** The cost table of kernel version 1: each statement executed costs 1; each expression node evaluated costs 1; a tuple, list, map, set or record construction costs 1 more per member written; each test of a loop's continuation costs 1; an action costs 1 per atom; copying a value out or decoding one in (`K-EFF-002`, `K-EFF-005`) costs its deep size (`K-CHG-005`).
- **K-CHG-003.** A call of a library function that states a native implementation is charged its definition's charge formula, on the call's arguments and result, when the call returns; if it raises, the formula is taken with a result of size 0. Formula arithmetic is unsigned 64-bit and saturating. An empty sum is 0 and an empty product is 1.
- **K-CHG-004.** A value's size: 1 for null, absent, a bool, a float, a timestamp, a closure, a task handle, a function reference, a handle and a ref; 1 plus its magnitude's length in 64-bit words for an integer (zero has none); 1 plus its length in UTF-8 bytes for a text; 1 plus its length for bytes; 1 plus its member count for a tuple, list, set or record; 1 plus twice its entry count for a map; 1 plus the UTF-8 lengths of its kind and message for an error.
- **K-CHG-005.** A value's deep size is its size plus the deep sizes of what it holds: members, keys and values, field values, and an error's data. A heap object reached more than once is counted the first time only.
- **K-CHG-006.** A value's magnitude is its value when it is a number that is finite and not negative, rounded down and saturating at `2^64 - 1`, and 0 otherwise.
- **K-CHG-007.** While the kernel body of a library function that states a native implementation runs, its forms and the calls it makes are not charged: the formula is the whole charge, whichever implementation runs (`K-CHG-001`). A function with only a kernel-code body is ordinary code: its forms and the calls it makes are charged as they run and its formula is not, so a call's charge is the work its body does. Code either reaches through a function argument is ordinary code and is charged as it runs.
- **K-CHG-008.** A charge that takes the run past its bound ends the run at that charge (`K-BND-001`).

## Bounds (`K-BND`)

- **K-BND-001.** The embedder states the run's bounds: total charge; memory; the depth of any task's active calls; the number of tasks that have not ended; the number of effects and sleeps requested at one park; the number of members of one list `join`. A native function's guard (`K-LIB-008`) is a bound too. Passing a bound ends the run with a typed bound error naming it. It is not a raise: no guest code runs afterwards.
- **K-BND-002.** Call depth is counted per task, one per active call of a declared function, a closure or a library body. `main`'s own body is not a call; an entry's and a spawned task's function is. Library bodies run inside expressions (`K-STMT-002`) nest at most 64 deep, whatever the bound.
- **K-BND-003.** How memory is counted belongs to the machine, not to the kernel version. A machine counts it deterministically: whether an allocation passes the bound depends on what the run can still reach and on nothing else, not on when the machine last freed what it cannot. The result of an ended task counts for as long as its handle can be reached.

## Library functions (`K-LIB`)

- **K-LIB-001.** A library function is a definition: its kernel version, its name, its typed signature, the error kinds it may raise, its charge formula, an optional guard, and its implementation. The kernel library, a dialect's helpers and extension functions are all definitions; there is one mechanism.
- **K-LIB-002.** A definition's implementation is one of: native only; a kernel-code body only; or both, which behave the same. Which one is part of the definition and so of its identity. Only a function whose definition states a native implementation may be called inside an expression (`K-STMT-002`).
- **K-LIB-003.** A function with a native implementation takes no function: no parameter's type mentions a function type. A function that takes a function argument has a kernel-code body only.
- **K-LIB-004.** A kernel-code body is ordinary kernel code and obeys the statement rule. It performs no effect itself: an effect reaches it only through a function argument. A body that stands beside a native implementation cannot wait: its only actions are calls of library functions that state a native implementation.
- **K-LIB-005.** A native implementation is registered by the embedder, in Rust, at startup, beside its definition. Nothing in a document loads one. Admission refuses a document whose manifest lists a function the registry does not hold.
- **K-LIB-006.** A native function is a function of its arguments. It sees its arguments, the objects they reach and its work counter. It does no I/O, reads no clock, keeps no state a later call could observe and calls no guest code. Called again with equal arguments it gives an equal result, raises the same error, or passes its guard at the same count.
- **K-LIB-007.** A native function changes no object that exists. It may allocate the objects of its result. Mutation is done by forms (`K-FORM-005` to `K-FORM-007`), in a kernel body or a caller.
- **K-LIB-008.** A guard states a unit of work and a limit, a formula over the arguments. The implementation counts that unit, the same for the same arguments whatever its caches hold. The call that would pass the limit ends the run with a bound error naming the function.
- **K-LIB-009.** A document and a body name library functions by identity and list them with the names their definitions carry. They do not contain bodies. Replacing identity A by B everywhere it is listed and called adopts a corrected function.
- **K-LIB-010.** `deref` and `tasks.unfinished` are functions of the kernel library that the machine implements itself, because they read an object's kind or the task set, which a native function cannot see. The machine supplies their definitions and recognises them by identity; they are native for the statement rule. `ref` and `same` read only their arguments and are ordinary native functions.
- **K-LIB-011.** A definition that states a native implementation states its native version, the first, 1, when it states none; it is part of the definition and so of its identity. A native implementation's code is not part of its definition, so one that answers otherwise (`K-LIB-006`) is stated under the next native version: a new function. A build that ships a function under an identity a released set holds answers as that set recorded, or does not build. Only a definition that states a native implementation states a version; the text form is `native <version>`.

## Documents (`K-DOC`)

- **K-DOC-001.** A document is a kernel program: `main`, the declared functions by name, the entries, the private bindings and the manifest. Every construct is a typed node. The document is the only authority: node ids, edges, scopes, type facets, effect sets and execution sites are derived from it and never written by a host.
- **K-DOC-002.** The manifest states the kernel version, every effect the document performs with the signature it expects, every library function it reaches, directly or through another function's body, by identity, and the bare-number policy (`K-EFF-006`).
- **K-DOC-003.** An entry names a declared function a host may start and gives the typed signature it is started under, with as many parameters as the function has.
- **K-DOC-004.** The JSON encoding is the stored form. An enum value is an object with one member named for the variant in snake case, or that name as a string when the variant has no content. Every object is closed: an unknown member is refused. A member that is an empty list, an empty map, `false` or missing by default is omitted. An integer and a float are strings in the texts of `K-VAL-032` and `K-VAL-031`; bytes and identities are lower-case hexadecimal.
- **K-DOC-005.** Structural validation refuses, naming the node: another kernel version; an empty name; a repeated parameter or record field; `break` or `continue` outside a loop; a statement-rule violation (`K-STMT-002`); a library function called but not listed; an effect performed but not listed; a call of an undeclared function; a call with too many, or too few required, arguments; an entry that names no function or disagrees with its arity; a malformed type; nesting past the limit (`K-DOC-006`).
- **K-DOC-006.** No node sits more than 64 levels below its function's body, a block and each statement in it counting as two. Kernel text, the JSON encoding and validation refuse a deeper tree.
- **K-DOC-007.** Annotations (labels, the dialect, authored source, anything a host keeps on a node) are a separate value keyed to the document's identity and attached to sites. They never change behaviour, are not part of the identity, and may be dropped.

## Identity (`K-ID`)

- **K-ID-001.** A document's identity is the SHA-256 of the bytes `lash-kernel-document`, one zero byte, and the document's canonical form. It covers all of the document's behaviour and none of its annotations.
- **K-ID-002.** A function's identity is the SHA-256 of the bytes `lash-kernel-function`, one zero byte, and the definition's canonical form. It covers the name, signature, error kinds, charge formula, guard and implementation.
- **K-ID-003.** The canonical form is the JSON encoding (`K-DOC-004`) with every object's members sorted by name as UTF-8 bytes, no white space, and strings written with only these escapes: `\"`, `\\`, and `\u00XX` (lower-case) for a character below U+0020.
- **K-ID-004.** A site is the unit a node belongs to (`main`, a declared function by name, or a library body by identity) and the chain of child indexes that reaches the node from that unit's body. A block's children are its statements. A statement's children are its expressions, its action and its blocks, in the order kernel text writes them; an absent `catch` or `finally` takes no index. An expression's children are its sub-expressions in order (a map entry's key before its value), and a closure's one child is its body. An action has none.

## Execution sites (`K-SITE`)

Every coordinate a machine reports and a parked run saves is a site of `K-ID-004`. These rules fix which node each one names. `lash-kernel-check` derives them from the document and publishes them; nothing stores them as authority.

- **K-SITE-001.** Where a task stands is the site of a statement: its pending statement, the calling statement of each of its active calls, and the `try` whose `finally` it is running. It is never the site of a block, an expression or an action.
- **K-SITE-002.** A wait and a spawn are named by the site of the action node: the site in an effect's identity (`K-EFF-008`), in a spawn's (`K-TASK-020`), and the one their occurrences are counted at. The action is a child of its statement, so its statement's site is the action's with the last index dropped. A statement holds at most one action, so the two name each other.
- **K-SITE-003.** A loop is named by the site of its `for` or `while` statement, in an effect's loop context and wherever a loop's state is saved.
- **K-SITE-004.** The loops that enclose a site inside its own function body, outermost first, are derived from the document. A closure body is a function body: the loops around a closure expression do not enclose the sites inside it. The loop context of `K-EFF-008` is these, joined across the task's active calls.
- **K-SITE-005.** Code written in a closure belongs to the unit that writes the closure, and its sites continue that unit's path through the closure expression. A library body's sites are in the unit of that function's identity. A derived view also numbers its nodes, from 0, in a pre-order walk of the units, `main` first and then the declared functions by name, children in the order of `K-ID-004`. A node id names a node within one derivation of one document; only a site is saved or reported.

## Admission (`K-ADM`)

Admission decides whether an environment can run a document. The environment is what an embedder provides: effects with their signatures, library functions by identity, and the session's bindings. A refusal names every fault of the first three rules, not the first one found.

- **K-ADM-001.** A document is refused when its manifest lists an effect the environment does not provide.
- **K-ADM-002.** A document is refused when the environment provides an effect with a signature that does not serve the one the manifest expects. A signature serves when it takes every argument list the expected one allows (as many parameters or more, no more of them required, each expected parameter type contained in the provided one) and its result type is contained in the expected result type. A type is contained in another when every value of the first is a value of the second.
- **K-ADM-003.** A document is refused when the environment holds no definition for a function identity the manifest lists or the code reaches, directly or through another function's body (`K-LIB-005`).
- **K-ADM-004.** A variable `main` reads or assigns where no enclosing scope has declared it is a session binding (`K-SES-001`), and the document is refused when the environment does not provide it. In a declared function, which is closed (`K-FN-002`), and in a library body, such a variable is refused whatever the environment (`K-FORM-003`). The refusal names the node.
- **K-ADM-005.** An argument of a library call or of a `perform` is refused when no value it can hold is of its parameter's type: a literal of another kind, a construction of another kind, the result of a call whose signature states another type. The check reads only what the document states. A variable is typed by the `let` that declares it when nothing assigns it, and then only as far as its kind, since the contents of a mutable object may change; every other variable may hold anything. An argument that might fit is admitted and checked when it runs (`K-FN-007`). `absent` given for an optional parameter is the omitted argument (`K-FN-004`).
- **K-ADM-006.** A `perform` is refused when the result type it states shares no value with the result type of the effect's signature in the manifest.
- **K-ADM-007.** The manifest a document carries is the one its code derives (`K-DOC-002`). A document is refused when its manifest lists an effect nothing performs, lists a function nothing reaches, omits a function reached through another function's body, or lists a function under a name its definition does not carry.
- **K-ADM-008.** The body of every library function a document reaches is held to the rules of a document: structural validation with the statement rule (`K-STMT-003`, `K-LIB-004`), and the scope rule of `K-FORM-003`, its parameters being its signature's.
- **K-ADM-009.** Every derived fact (node ids, edges, scopes, type facets, effect sets, execution sites) is a function of the document and of the definitions of the functions it reaches. Annotations change none of them.

## Edits (`K-EDIT`)

A host changes a document through `lash-kernel-edit`. These rules fix what an edit names, what a transaction publishes and what survives it.

- **K-EDIT-001.** A transaction is a list of edits and the identity of the document they were written against, its base. It is applied in order to a copy of the base and published whole, or refused with typed diagnostics, and a refused transaction changes nothing. A diagnostic of one edit names the edit by its position and the site the edit named.
- **K-EDIT-002.** An edit names a node by its site in the base (`K-ID-004`), wherever earlier edits of the same transaction have put the node. A transaction whose base is not the document it is applied to is refused. A site that names no node of the base, or a node an earlier edit removed, is refused; a node the transaction itself brought in has no site in the base and is named by a later transaction. A statement is placed in a block, ahead of a statement of that block or at the block's end; a statement moved within its block lands ahead of the statement named, and no statement moves into itself.
- **K-EDIT-003.** The edited document is admitted as any document is (`K-ADM`), against the environment the host gives, and a refusal names the node in the edited document. There is no rule for edits alone: the statement rule holds of each statement by itself (`K-STMT-003`), and the scope rule (`K-FORM-003`) refuses an edit that leaves a variable used before it is bound. A hoisted temporary (`K-STMT-005`) is such a variable and nothing more; no edit groups statements, and none is needed to keep a temporary with its use.
- **K-EDIT-004.** A published transaction answers a correspondence: for every node of the base that survives, its site in the result, and whether an edit wrote the node itself. Each surviving node has one successor and no two share one; a node that does not survive has none, and a node of the result no entry leads to is new. Inserting, removing, moving or copying a statement writes no other node. A moved statement survives with everything under it. A copy is new throughout. Replacing a statement, an expression, an action or a declared function's body keeps the node replaced, as edited, and nothing that was under it. Setting a condition keeps and edits the condition's expression; setting an argument edits the action; setting a `try`'s `catch` or `finally` edits the `try`, keeps its other clauses and replaces the one set. A rename edits each node that holds the name. Renaming a declared function moves its body's nodes to the unit of the new name. Adopting a function identity edits each node that calls it. Correspondences compose: the one from a draft's opening is the composition of its transactions'.
- **K-EDIT-005.** The manifest a transaction publishes is the one the edited code derives (`K-DOC-002`, `K-ADM-007`). The functions listed are those the code reaches. An effect no longer performed is dropped. An effect newly performed is listed with the signature the environment provides, unless the transaction set one. The kernel version is never edited.
- **K-EDIT-006.** Annotations move with their node (`K-DOC-007`): a published transaction carries each surviving node's annotations to its new site, copies them onto a copy, drops those of a removed node, and keys the layer to the result's identity. The dialect is kept. The authored source is kept only when the identity is unchanged, since it is not the source of a document that behaves differently. An edit of a label or of a node's data changes no behaviour and no identity.
- **K-EDIT-007.** Renaming a variable names its declaration: the `let`, the `for`, the `try` of its `catch`, the closure expression, or a declared function's body for a parameter. It rewrites the declaration and every use that resolves to it (`K-FORM-003`) and no other variable of that name; a private binding of `main` stays private under the new name. A rename after which any name of the document resolves to another declaration than before is refused.
- **K-EDIT-008.** An entry is a declared function's name and a signature (`K-DOC-003`), so an entry is inserted on a declared function, removed without removing the function, and renamed by renaming the function, which rewrites its declaration, its entry and every call, spawn and reference of it. Removing a function removes its entry.
- **K-EDIT-009.** Adopting a corrected library function (`K-VER-002`) replaces identity A with B in every call of A the document writes. It is refused when the document writes no call of A, and when the environment does not hold B, naming B.
- **K-EDIT-010.** Edits, transactions and correspondences are data in the JSON encoding of `K-DOC-004`: closed objects, variants by snake-case name, a cleared option omitted. Their payloads are the forms, types and sites of the kernel version, and nothing in them names a dialect.

## Kernel text (`K-TEXT`)

- **K-TEXT-001.** Kernel text is the one plain text notation for a document and for a definition. It writes behaviour only.
- **K-TEXT-002.** Printing is canonical and parsing inverts it: for every document and definition within the nesting limit, parsing the printed text gives an equal value, and printing that gives the same text.
- **K-TEXT-003.** Every statement begins with a keyword. A name spelled like a keyword, or not an identifier, is written between backticks. A library function is called by the name a `use name = @<identity>` line gives it, or as `@<identity>(…)` when no one line names it. A `#` begins a comment to the end of the line; white space is not significant.

## Sessions (`K-SES`)

- **K-SES-001.** Each session cell is its own document. The session's state is its bindings. A variable `main` declares at its top level, and does not list as private, is a session binding: the run starts with the session's bindings in scope in `main`, and ends with `main`'s top-level bindings as the session's.
- **K-SES-002.** Two bindings that share an object share it in the next cell.
- **K-SES-003.** A binding that reaches a closure or a task handle is not carried to the next cell; the run's end names it.

## The machine (`K-MACH`)

- **K-MACH-001.** The machine is a library. It does no I/O, starts no thread and keeps no global state; everything guest-derived lives in the instance the embedder owns.
- **K-MACH-002.** `run` executes ready tasks until no task is ready, the run ends, or the charge spent in this call reaches the slice, tested between statements. It returns which.
- **K-MACH-003.** A park returns the effects and sleeps requested since the last park and the waits withdrawn since then.
- **K-MACH-004.** `deliver` takes one outcome for one wait and makes its task ready (`K-TASK-005`). An outcome for a withdrawn wait is dropped. An outcome for a wait never handed out, or delivered twice, is an error to the embedder and changes nothing.
- **K-MACH-005.** A slice return leaves tasks ready. Running again continues as if there had been no return: slices change no value, no order and no charge.
- **K-MACH-006.** The machine asks the host whether the run is cancelled at each slice boundary and at each park. A cancelled run ends there; no cleanup block runs.
- **K-MACH-007.** Between two calls the run is at a safe point: every task is between statements. Its state may be exported then, and only then.
- **K-MACH-008.** Importing an exported state into a machine built from the same document and the same function identities continues the computation exactly: the same values, errors, effect identities and charges as if it had never been exported, whatever the engine's internal layout and whichever of a function's implementations it runs.

## Dialect printing (`K-DIALECT`)

- **K-DIALECT-001.** Every admitted document prints as source whose lowering preserves values, errors, mutations, effect order and task interleaving. Printing need not recover original source, grouping, parameter patterns or formatting. A dialect's reserved namespace names kernel operations directly, and library calls can name an identity even when several definitions have the same name.
- **K-DIALECT-002.** Printing retains statement boundaries and the order of their operands. A hoisted temporary is an ordinary binding; it is never folded across a side effect or a wait.

The shared imperative walker is `lash-kernel-dialect::imperative_source`. The TypeScript spelling uses `k.document` for manifest, entry signatures and private-binding names, `k.declare` for declared function bodies, and `k.main` for the main body. These are source declarations, not runtime calls. Executable code is printed as TypeScript statements, with `k` operations where JavaScript semantics differ. The `k` namespace is reserved in authored TypeScript. Source identifiers in a printed document are `v_` followed by the UTF-8 hexadecimal encoding of a kernel name; the envelope reader reverses that encoding, so keywords, punctuation and a kernel binding called `k` remain distinct.

`k.int` and `k.float` take canonical kernel number text, `k.bytes` takes hexadecimal, `k.absent` denotes absent, and `k.function` names a function reference. `k.tuple`, `k.list`, `k.map`, `k.set` and `k.record` construct the corresponding values. `k.invoke(identity, args)` is an expression call to a native library function; `k.call(k.library(identity), atoms)` is a statement call, and `k.declared(name)` and `k.value(name)` select declared and variable callees. `k.spawn`, `k.perform`, `k.sleep`, `k.join`, `k.joinMany`, `k.yield` and `k.cancel` obey the statement rule. Field and index writes use `k.field(target, field).value` and `k.index(target, index).value` as reserved lvalues; their reads and removals retain the field/index distinction. No source annotations are required to print or read this surface.

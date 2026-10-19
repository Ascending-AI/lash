# Python exported source, format v1

The Python codec implements the [shared exact-source contract](generated-source-format.md).
`print(Document, captions, PrintOptions)` only renders plain Python an agent could have
written: functions, then `def main():`, with no header, directive, stub block,
name or helper table, and no `result=` cast in the ordinary case.
`read(source, base, limits)` reconstructs the document directly against the
base Document it was printed from, without authored lowering, admission or an
installed library. The base supplies the manifest, entries, private bindings,
helper identities and renames. Every accepted document retains its complete
canonical bytes. The subset uses kernel semantics; stock Python execution is
not promised.

Original names use deterministic, injective aliases from `NamePlan`, computed
from the base by both printer and reader. Only NFKC-stable identifiers may
remain unaliased, so Python normalization cannot collapse two kernel names.
Python keywords, builtins and the generated namespaces are reserved; aliases
apply to declarations, parameters, references, function values, entries and
private names, including shadowing. Helper labels never resolve dependencies
against an installed library.

The real `deep_research` agent cell prints as:

```python
async def deep_research():
    search_input = record({"topic": "Reducing customer onboarding drop-off", "max_sources": 3})
    evidence = await tools.research.search(search_input)
    synthesis_input = record({"topic": evidence.topic, "sources": evidence.sources})
    report = await tools.research.synthesize(synthesis_input)
    display_input = record({"text": report.summary})
    await tools.display.show_message(display_input)
    return report
def main():
    return None
```

## Captions

Captions print as `#:` lines directly above what they caption, at its
indentation: a statement's first line; the `else:`, `except BaseException as
…:` or `finally:` line of an arm, or its `with k.block():` line in a generic
suite; and a declared function's `def`. A captioned else arm never folds
into `elif`, and an empty one prints `else:` with `pass`. A function whose
parameters or whose types' fields carry captions prints one parameter a line,
each below its `#:` lines. Each parameter or result with a captioned field is
annotated with its type from the entry's signature, and every record in such
an annotation becomes a `TypedDict` class above all functions, named from the
function, the root and the path, apart from every name the program writes.
Each field of the class sits below its caption. The reader takes the classes
and annotations only to place field captions: the types stay the base
document's. A record with a field no class body can name prints as
`dict[str, Any]`, and the captions below it are dropped, as are an entry's
parameter and field captions when its function takes other parameters than
its signature names. Caption Sites are tracked whether or not a map is
collected, and no other caption is dropped. The reader
numbers statements in the order it begins them, which is the order a walk of
the document meets them, and resolves each caption to its Site once the
document is whole; any other comment, and a `#:` run above anything else, is
ignored. The front end and the reader share the syntax module.

## Paired readable inventory

Each rule is a structural rendering predicate and a direct decoding inverse.
There is no compile-and-fallback loop, statement fusion, temporary removal,
closure lifting, runtime dependency conversion or alternative generated codec.
`InlinePlan` memoizes expression eligibility in one postorder pass; names use
ordered tables and output uses the shared append-only writer.

| Rule and applicability | Python spelling and exact inverse | Positive / boundary witness |
| --- | --- | --- |
| Document function or main | `async def` for a body containing native awaits, otherwise `def` with direct suites; aliases reconstruct original names and parameters | Declared worker / function originally named `main` receives a reversible alias |
| First binding in a Python function scope | `x = value` reconstructs Let; after a parameter, loop/catch binding or previous assignment, it reconstructs Assign | First Let, repeated Assign / repeated Let uses `x: Final = value`; an Assign before any binding uses `x: reassigned = value` |
| Bound closure RHS | `def nested(x):` reconstructs the closure RHS and its enclosing binding statement, with a fresh parameter scope | First Let and subsequent Assign / `-> Final` or `-> reassigned` carries an exceptional binding kind; an unbound block expression stays explicit |
| Literal variants | Native integers, finite canonical floats, `None`, booleans and quoted text; `float("nan")`, `float("inf")`, `bytes.fromhex("00ff")`, `declared.worker` preserve nonfinite floats, bytes and Function versus Variable | Signed zero, huge integers, UTF-8 escapes / Absent remains `k.absent` because None is Null |
| Inline List, Tuple and Set | `[a, b]`, `(a, b)`, `(a,)`, `()`, `{a, b}`, `set()` reconstruct distinct variants and ordered children | Empty and singleton variants / child block expression requires an explicit suite |
| Inline Map | `{key: value}` reconstructs Map entries in source order, including repeated keys | Authored Python dict / a key or value block expression requires a suite |
| Inline Record | `record({"field": value})` reconstructs Record field text and ordered values, including duplicates | Shared workflow records / child block expression requires a suite; unwrapped dict always means Map, which authored Python lowers to |
| Field and Index | `value.title`, `value[index]`; `getattr(value, "return")` for fields Python cannot spell as attributes | Builtin attribute names and keyword fields / a target or index block expression requires a suite |
| Member Assign and Remove | `value.title = rhs`, `value[index] = rhs`, `del value[index]`; `setattr` and `delattr` preserve nonidentifier Field places | Literal target is grouped / target, index or RHS block expression requires a suite |
| Expression helper call | `helper_name(expressions)` reconstructs Expr::Call with the full identity the base holds for that spelling | Foreign helper and empty args / a child block expression requires an invocation suite |
| Operators and comparisons | Native operators with minimal parentheses by precedence reconstruct exactly one full identity through an unambiguous base manifest label | `add` becomes `x + y`, `eq` becomes `x == y`, `bool.not` becomes `not x`, `(a + b) * c` and `a ** (b ** c)` group only where precedence or associativity needs it, a negated numeric literal is `-(7)` / chained comparisons, an ungrouped unary base of `**` and ambiguous identities refuse or keep the helper call; `python_operators_print_minimal_parentheses_with_an_exact_inverse` |
| Action Call | Builtins `len(x)`, `range(a, b)`, `abs`, `all`, `any`, `chr`, `dict`, `divmod`, `int`, `list`, `ord`, `repr`, `reversed`, `round`, `set`, `sum`, `tuple`; methods `xs.append(v)`, `"a b".split()`; slices `x[a:b:c]`; `type(x).__name__`; otherwise `await helper_name(atoms)`; `await declared.worker(atoms)`, `await closure_name(atoms)` retain Library, Declared and Value callees, distinct from expression calls. An authored spelling applies only when the base labels that helper once | All callee kinds remain distinct; computed arguments reconstruct a nested action and Spawn remains separate |
| Perform | `await tools.research.search(atoms)` preserves the effect name; unannotated it reads as `result: any`, which authored lowering stores; bracket names and `result=` cover other names or types | Ordinary tool, arbitrary effect and foreign result type / malformed metadata refuses; computed arguments retain their expression nodes |
| If / Elif / Else | Direct clauses preserve both Blocks; elif is exactly one If in an else Block | Singleton nested If / extra statement requires an explicit else and nested if |
| For / While | `for item in values:` is the loop through the base's `py.for_in` protocol, `for item in d.items():` (`.keys()`, `.values()`) the loop through that view's protocol over `d`, and `while ready:`; in a base that labels no `py.for_in` protocol, `for item in values:` is the kernel's own loop, as operator syntax binds the labels the base holds. Each retains the exact binding, iterable, condition and body, and a non-empty `else:` is the loop's `else` block | The loop the base's labels do not give `for` (the kernel's own loop beside `py.for_in`, or another protocol), or a header block expression, requires the `k.for_(item[, open, next, item, check])` suite, `None` for an omitted hook, with the `else` block after the body |
| Try / Except / Finally | `try:`, optional `except BaseException as error:`, optional `finally:` retain handler presence and empty blocks | Catch-only or finally-only / handlerless Try requires an explicit kernel form |
| Terminals | Main Finish uses `return value`; function Return uses `return value`; Throw uses `raise value`; Print uses `print(value)`; Break and Continue use their keywords | Ordinary terminals / unusual main Return uses `return returned(value)`, function Finish uses `return finish(value)`, and Fail uses `raise failure(value)` to preserve distinct terminal kinds |

Scope inference follows stable source traversal within each function, not
runtime branch execution. A block's declaration that shadows an earlier binding
uses the explicit annotation, including across conditional branches. Closure
parameters start a fresh scope; outer binding state is restored after its body.
Captures and original Names remain the kernel's own derived facts; decoding never
introduces Python coercions, temporaries, `global` or `nonlocal` semantics.

Operator aliases are generated syntax, not algebraic rewrites. The fixed label
inventory is `add`, `text.concat`, `py.add` (+), `sub`, `py.sub` (-), `mul`,
`py.mul` (*), `py.truediv` (/), `py.floordiv` (//), `py.mod` (%), `py.pow` (**),
`eq` (==), `same` (is), `lt` (<), `le` (<=), `gt` (>), `ge` (>=), `bool.not`
(not), and `neg`, `py.neg` (unary -). Operand facts from literal, binding and result types select numeric, text or dynamic requirements when they share a token. Printer and reader share the same operand inference. Distinct identities with identical operand facts, duplicate labels and unknown facts retain explicit helper calls. Integer-only labels (`int.add`, `int.sub`, `int.mul`, `int.lt`, `int.le`) also participate.
The decoder uses the preserved identity and exact stored argument order, never
assumptions about operator behavior or a live library.

## Remaining generic forms and reasons

Generic forms permanently express exact kernel distinctions that ordinary
Python syntax cannot carry. Their decoder consumes every child in kernel order.
They are not a printer mode or a compatibility path. A suite has four-space
indentation and uses exactly `pass` when empty.

| Rule | Generic forms | Why ordinary Python cannot carry the exact node |
| --- | --- | --- |
| `absent` | `k.absent`, including atomic action operands | Python None already represents kernel Null; an absent value is different |
| `block-expression` | `with k.closure([params]):`, followed by its block | Python has no arbitrary statement block in an expression position; bound closures instead print as def |
| `expression-child-suite` | `with k.list/tuple/set/map/record/invoke/field/index(...):` | A child block expression cannot be embedded in native expression syntax without lifting or changing stored boundaries |
| `statement-child-suite` | `with k.let/assign/assign_field/assign_index/remove_field/remove_index/if_/for_/while_/return_/throw/print/finish/fail(...):` | A child block expression needs visible child suites without introducing a temporary |
| `explicit-block` | `with k.block():` inside a generic parent | The generic parent's child boundaries must remain explicit, including empty blocks |
| `handlerless-try` | `with k.try_(None, False):` | Python requires an except or finally clause; adding one would change handler presence |
| `projection-read` | `with k.read():` with handle and request children | Kernel projection reads have no ordinary Python primitive and retain their dedicated expression kind |
| `clock` / `random` | `k.clock()` / `k.random()` | Kernel nondeterministic expressions are distinct from library calls and Python's time/random APIs |
| `task-action` | `spawn(callee, [args])`, `await sleep(value)`, `await join(value)`, `await join_all/join_all_settled/join_race/join_any(value)`, `k.cancel`, `k.yield_` | Kernel task actions, modes and timing units differ from Python coroutine/library operations; their exact primitives retain every action without spawn/join fusion |

The generic reader also accepts full explicit body-level variants for the
readable forms (`k.int`, `k.float`, `k.bytes`, `k.function`, `k.call`,
`k.perform`); the printer always chooses the eligible readable rule. The
`k.document` header and the `with k.declare`/`with k.main` wrappers are
deleted; there is no dual format.

Guard parity: Python lowering emits no operand-narrowing guards, so the reader
infers none. Operator selection uses only literal, binding and result-type
operand facts.

## Proof, limits and maps

Each paired witness runs print, public `read` against the printed Document
and complete canonical byte comparison. The independent finite
form/stored-field inventory, 64 Smith seeds, 28 authored Python cases, four
foreign TypeScript cases, the `deep_research` agent cell and the 560- and
672-document bounded domains remain in use. The shared harness independently
lexes every node's own mapped spelling, excludes descendant ranges and quoted
data, and counts `k.*`, every `helper_*` call and pseudo-intrinsics; any text
outside every node other than function declarations fails as a header,
directive or table. The seven-document corpus reports its named exactness
distinctions (Record versus Map, declared callees and foreign helper
identity). Authored-source acceptance reports any generic form against the
documented unavoidable list. A mutation of the first-binding inverse fails the
canonical-byte law.

The CPython compile law runs every export, `deep_research` included, through
`python3` `compile(..., 'exec')`, the `ast.parse` check. It is a hard
dependency: the test executors provide `python3`, and a missing interpreter
fails the law rather than skipping it.

Print and decode independently bound actual bytes and real node depth. Compressed
source does not bypass the encoded input budget. Postfix and collection parsing
track tree height before constructing deeper nodes, separately from syntax
nesting. Alias expansion counts original-name bytes before cloning. A `result=` type
is depth-preflighted and rejects duplicate fields before deserialization.

Maps default off and do no map-specific work in that mode. Requested maps cover
all real Nodes from `Node::children`, including direct root Blocks, closures,
actions and atomic-free calls; aliases and atomic operands invent no Sites.
Ranges use final UTF-8 byte offsets and maps never change output bytes. Every
empty block has its `pass` range; no new implicit-node category is necessary.

The authored corpus also reports pseudo-intrinsics: exceptional binding annotations
retain Let versus Assign, `returned`/`finish`/`failure` retain distinct terminal
kinds. Strict `list.len`, kernel `kind`, and `text.join` have different domains,
results or argument conventions from Python builtins. Integer-only arithmetic
and unknown text operands retain helpers when their full identities cannot be
selected from the source operand facts.

Library Actions with an authored spelling that is the node's exact inverse
print as that builtin, method, slice or `type(x).__name__` call. The ones
remaining as `await helper_*` calls, each counted by the harness with its
reason, have none:

| Helper | Reason |
| --- | --- |
| `helper_py_add`, `helper_py_mul`, `helper_py_neg`, `helper_py_mod`, `helper_py_floordiv`, `helper_py_pow` | The operator spelling is the expression operator; this is the action lowering emits for dynamic operands. |
| `helper_py_getitem`, `helper_py_setitem`, `helper_py_exc_args` | `x[i]`, `x[i] = v` and `error.args` are the kernel Index, member Assign and Field nodes. |
| `helper_py_iadd` | `x += y` also assigns; the stored node is only the call. |
| `helper_py_show` | Print's text is formatted before the Print statement, which has its own spelling. |
| `helper_py_arg_missing`, `helper_py_exc_matches`, `helper_py_exc_new`, `helper_py_fail`, `helper_py_name_check` | Missing-argument checks, except-clause tests, exception construction, typed failures and unbound-name checks that lowering inserts; no source spells them. |

A method call on a non-variable, non-text receiver, a slice of a non-variable
and `float(...)` (whose `nan`/`inf` spellings are literal forms) also keep the
helper call. A function whose body holds an authored builtin or method call
prints as `async def`, because those calls are stored Actions.

## Nested actions and child suites

Nested actions use parenthesized native calls and effects with expression
arguments: `(await declared.worker(record({"n": 1})))`,
`(await tools.echo(record({"text": "inline"})))`,
`(spawn(declared.worker, [record({"n": 1})]))`, and `(await sleep(value))`.
Calls preserve Library, Declared and Value callees. Effects preserve `result=`
and `then_yield=True`; sleep also retains `then_yield=True`. Joins retain their
exact mode. The base's unique `py.await` uses `await (expression)`; any other
policy uses `await_by(helper_policy, expression)`. Native task constructor
names are reserved and receive reversible aliases when a binding collides.
A complete RHS demotes to the statement Action only when every argument is an
atom; nested expression positions retain Expr::Act and each argument Site.

As for `with k.invoke` above, an argument without a native Python expression
inverse requires a child suite. This applies to arbitrary Closure and Read
nodes and the existing If/Let suites, including collections containing them.
Only that inverse test selects the envelope: `with k.call(callee):`,
`with k.spawn(callee):`, `with k.perform("effect", result_type, then_yield):`,
`with k.sleep(then_yield):`, `with k.join():`,
`with k.join_many("mode"):` or `with k.await_(helper_policy):`, followed by
exactly the argument expression suites in order. Callees retain their native
aliases and result types are structured literals. No argument is hoisted.
The common inline path has no `k.*` calls or effects. The shared acceptance
corpus proves that path; `nested_actions_use_child_suites_only_without_an_inline_inverse`
proves the necessary envelope's exact re-import and complete source map.
The old inline `k.spawn/sleep/join/join_many/await_` task decoder is removed.

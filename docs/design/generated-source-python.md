# Python generated source, format v1

The Python codec implements the [shared exact-source contract](generated-source-format.md).
`print(Document, PrintOptions)` only renders. `compile_generated(source, limits)`
reconstructs the document directly, without authored lowering, admission or an
installed library. Every accepted document retains its complete canonical bytes.
The generated subset uses kernel semantics; stock Python execution is not promised.

The shared seven-field envelope contains native Python dictionaries and lists,
including complete signatures and full helper identities. No body is hidden in
metadata and no metadata is escaped JSON. Original names use deterministic,
injective aliases from `NamePlan`. Identity aliases are omitted; native names reconstruct directly. Only NFKC-stable identifiers may remain unaliased, so Python normalization cannot collapse two kernel names. Python keywords, builtins and the generated
namespaces are reserved; aliases apply to declarations, parameters, references,
function values, entries and private names, including shadowing. Helper labels
never resolve dependencies against an installed library.

```python
k.document("lash.generated.python", "1", {"kernel": 1, "numbers": "by_spelling"}, {}, [], [], [])
def worker(items):
    return items[0]
async def main():
    items = [1, 2]
    answer = await declared.worker(items)
    return answer
```

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
| Expression helper call | `helper_name(expressions)` reconstructs Expr::Call with its full envelope identity | Foreign helper and empty args / a child block expression requires an invocation suite |
| Operators and comparisons | Parenthesized native operators reconstruct exactly one full identity through an unambiguous manifest requirement label | `add` becomes `(x + y)`, `eq` becomes `(x == y)`, `bool.not` becomes `not (x)` / ambiguous identity or wrong arity keeps the ordinary helper call |
| Action Call | `await helper_name(atoms)`, `await declared.worker(atoms)`, `await closure_name(atoms)` retain Library, Declared and Value callees, distinct from expression calls | All callee kinds / non-atomic operands refuse; Spawn remains a separate action |
| Perform | `await tools.research.search(atoms)` preserves the effect name, using the manifest's exact result type; bracket names and `result=` native metadata cover other names or types | Ordinary tool, arbitrary effect and foreign result type / malformed metadata or non-atomic arguments refuse |
| If / Elif / Else | Direct clauses preserve both Blocks; elif is exactly one If in an else Block | Singleton nested If / extra statement requires an explicit else and nested if |
| For / While | `for item in values:` and `while ready:` retain the exact binding, iterable, condition and body | List iteration and nested loops / a header block expression requires a suite |
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
| `task-action` | `k.spawn`, `k.sleep`, `k.join`, `k.join_many`, `k.cancel`, `k.yield_` | Kernel task actions, modes and timing units differ from Python coroutine/library operations; no spawn/join fusion is allowed |

The generic reader also accepts full explicit variants for the readable forms,
including `with k.declare`, `with k.main`, `k.int`, `k.float`, `k.bytes`,
`k.function`, `k.call`, and `k.perform`. The printer always chooses the eligible
readable rule. The envelope is exclusively the current structured v1 contract;
obsolete escaped envelopes refuse and require re-exporting the original Document.

## Proof, limits and maps

Each paired witness runs print, public compile_generated and complete canonical
byte comparison. The independent finite form/stored-field inventory, 64 Smith
seeds, 28 authored Python cases, four foreign TypeScript cases and the 560- and
672-document bounded domains remain in use. The shared harness independently lexes every node's own mapped spelling, excludes descendant ranges and quoted data, and counts `k.*`, every `helper_*` call and pseudo-intrinsics. The seven-document corpus reports its named exactness distinctions (Record versus Map, declared callees and foreign helper identity). Authored-source acceptance
reports any generic form against the documented unavoidable list. A mutation of
the first-binding inverse fails the canonical-byte law.

Print and decode independently bound actual bytes and real node depth. Compressed
source does not bypass the encoded input budget. Postfix and collection parsing
track tree height before constructing deeper nodes, separately from syntax
nesting. Alias expansion counts original-name bytes before cloning. Metadata
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

The following authored helper Actions remain explicit and are individually
counted by the harness. Each has atomic operands and a stored await boundary;
a synchronous Python expression would change Action to Expr and its execution
boundary. These are exact document exports, not a rewrite of authored lowering:
`helper_py_add`, `helper_py_arg_missing`, `helper_py_chr`, `helper_py_divmod`, `helper_py_exc_args`, `helper_py_exc_matches`, `helper_py_exc_new`, `helper_py_fail`, `helper_py_floordiv`, `helper_py_getitem`, `helper_py_iadd`, `helper_py_int`, `helper_py_iter`, `helper_py_iter_guard`, `helper_py_iter_size`, `helper_py_length`, `helper_py_method_append`, `helper_py_method_clear`, `helper_py_method_copy`, `helper_py_method_count`, `helper_py_method_extend`, `helper_py_method_get`, `helper_py_method_insert`, `helper_py_method_lower`, `helper_py_method_pop`, `helper_py_method_remove`, `helper_py_method_reverse`, `helper_py_method_setdefault`, `helper_py_method_split`, `helper_py_method_upper`, `helper_py_mod`, `helper_py_mul`, `helper_py_name_check`, `helper_py_neg`, `helper_py_pow`, `helper_py_range`, `helper_py_round`, `helper_py_setitem`, `helper_py_show`, `helper_py_slice`, `helper_py_to_list`, `helper_py_type`.

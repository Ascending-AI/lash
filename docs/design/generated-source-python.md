# Python generated source, format v1

This is the Python generic codec under the [shared exact-source contract](generated-source-format.md).
`lash_dialect_python::print(Document, PrintOptions)` renders only;
`compile_generated(source, GeneratedSourceLimits)` reconstructs an unadmitted
Document directly. Neither operation resolves a library. Python's authored
`lower` remains a separate cell operation. Host and worker integration, and
readable Python rules, belong to FIG-6205 and FIG-6204 respectively.

The source is a Python syntax subset with kernel semantics. Every compound
node is a `with k.form(operands):` suite. Children appear as separate lines,
in `Node::children` order, indented by four spaces. Empty blocks and empty
expression-child sequences contain exactly `pass`. No body is hidden in
metadata. Suites let the generic codec express a closure in any expression
position without lifting a declaration or inserting a temporary.

```python
k.document("lash.generated.python", "1", "{\"kernel\":1,\"numbers\":\"by_spelling\"}", "{}", [], "[[\"answer\",\"answer\"]]", "[]")
with k.main():
    with k.block():
        with k.let(answer):
            k.int("42")
        with k.finish():
            answer
```

The envelope has the same seven logical operands as TypeScript: format,
version, complete manifest JSON, entries JSON, original private names,
identifier alias pairs JSON and full-FunctionId helper alias pairs JSON.
Zero or more `with k.declare("original", [parameterAliases]):` items precede
exactly one `with k.main():`. Each wrapper contains exactly one explicit
block. Extra items, missing markers, unsupported versions, duplicate
metadata keys, duplicate declarations and ambiguous aliases refuse.

Naming uses the shared deterministic `NamePlan` traversal and allocation.
Legal Unicode XID identifiers retain their original spelling. Python keywords,
soft keywords, builtins (including exception classes), `k`, `helper_` and
`kernel_name_` are reserved. Other names get deterministic `kernel_name_N`
aliases. Repeated original Names use the same alias, including shadowed
bindings and closure parameters. Explicit string-bearing forms carry originals.
Helper aliases use the shared full-ID order and label collision suffixes;
they never resolve labels against installed helpers.

## Paired generic inventory

Each row is both the printer rule and its direct decoding inverse. `e`
is an expression line/suite, `a` is an inline atomic operand, `n` an identifier
alias, `h` a full-ID helper alias, and `b` an explicit block suite. Compound
forms below are `with k.form(...):`, followed by the listed children.

| Kernel form | Header / leaf | Children and inverse |
| --- | --- | --- |
| Block | `k.block()` | Ordered statements; `pass` means empty |
| Let / variable Assign | `k.let(n)` / `k.assign(n)` | One RHS, its outer form distinguishes Expr from Action |
| Member Assign | `k.assign_field("field")` / `k.assign_index()` | Target, optional index, RHS |
| Remove | `k.remove_field("field")` / `k.remove_index()` | Target, optional index |
| Do | `k.do()` | One Action |
| If | `k.if_()` | Condition, then block, else block, including empty else |
| For / While | `k.for_(n)` / `k.while_()` | Iterable / condition, body block |
| Break / Continue | `k.break_()` / `k.continue_()` | Leaf statements |
| Return / Throw / Print / Finish / Fail | `k.return_()` / `k.throw()` / `k.print()` / `k.finish()` / `k.fail()` | One expression |
| Try | `k.try_(n or None, True or False)` | Body, catch block if binding present, finally block if flag true |
| Tuple / List / Set | `k.tuple()` / `k.list()` / `k.set()` | Ordered expressions; `pass` means empty |
| Map | `k.map()` | Alternating key/value expressions, in entry order; odd counts refuse |
| Record | `k.record(["field", ...])` | One expression per field, in field order |
| Member | `k.field("field")` / `k.index()` | Target, optional index |
| Closure | `k.closure([n, ...])` | One block, captures remain derived |
| Expression Call | `k.invoke(h)` | Ordered expressions; `pass` means no arguments |
| Projection Read | `k.read()` | Handle, request |
| Clock / Random | `k.clock()` / `k.random()` | Leaf expressions |
| Variable | `n` | Original Name from the injective alias table |
| Null / Absent / Bool / Text | `None` / `k.absent` / `True` or `False` / JSON-quoted string | Exact literal variant |
| Int / Float / Bytes | `k.int("decimal")` / `k.float("canonical")` / `k.bytes("hex")` | Kernel parsers preserve variants, non-finite values and signed zero |
| Function literal | `k.function("original")` | Original declared Name |
| Action Call / Spawn | `k.call(callee, [a, ...])` / `k.spawn(callee, [a, ...])` | Leaf Action; atoms never invent expression Sites |
| Perform | `k.perform("effect", [a, ...], k.type("type JSON"))` | Complete effect, atoms and result type |
| Sleep / Join / Cancel | `k.sleep(a)` / `k.join(a)` / `k.cancel(a)` | Leaf Action |
| JoinMany | `k.join_many("mode", a)` | `all`, `all_settled`, `race`, or `any` |
| Yield | `k.yield_()` | No-operand Action |
| Callee | `k.declared("original")` / `k.value("original")` / `k.library(h)` | Declared, value and library call kinds remain distinct |

Action forms are ordinary expression-statement lines, never suites. Literal,
variable, clock and random forms are also leaves. Other forms use suites.
No automatic coercion, authored lowering, temporary renaming or closure
introduction occurs. Every expected child must be consumed exactly once.
Strings use JSON quoting/escapes with Unicode scalar values, including text
and metadata; raw numeric literals, triple strings, concatenation, imports,
operators and arbitrary Python statements are outside this generic subset.
Blank lines and whole-line comments are ignored. In-line comments and
non-four-space indentation are outside this canonical generic spelling.

## Limits, maps and proof

Both operations enforce the shared byte and node-depth limits, independently
of the authored cell budget. Print uses `check_document_depth` before naming,
encoding or rendering, and the shared append-only `SourceWriter`. The direct
reader has no recursive operand parser or intermediate source tree: inline
arrays contain only the allowed strings, aliases or atoms. Suite recursion
checks real node depth before descent. Indentation and metadata JSON depth
are preflighted, and parsing, conversion and destruction use a bounded
8 MiB reserved stack. The reconstructed Document and metadata types undergo
the same shared resource check as printing. Expanded original-name bytes are
counted before alias uses clone them, bounding compression-driven allocation.

`GeneratedSourceCode` distinguishes format, bytes, depth and malformed body
refusals. Missing/obsolete envelopes require re-exporting the original Document
with the current toolchain; ordinary authored lowering is never a fallback.
Decoding success grants no admission or execution authority.

Maps default off and use the shared optional collector. All real Nodes,
including empty blocks and no-operand actions, have nonempty final UTF-8
ranges. Member wrappers, atom operands, declarations and envelope fields
create no synthetic Sites. Enabling maps leaves source bytes identical.

The owning `tests::generated_source` laws compare canonical bytes after every
successful print. They cover an independently handwritten source, unequal
manifest/entry/private/declaration controls, an exhaustive stored-field and
kernel-variant inventory, the 64 fixed Smith inputs, the admitted Python
conformance batch, TypeScript-origin documents, a bounded 560-document
name/literal/control domain, all Try handler-presence combinations, malformed
sources and byte/depth boundaries. A separate `Node::children` walk checks
Site completeness and final UTF-8 slices. Dropping private bindings in the
printer makes the independent-envelope byte law fail.

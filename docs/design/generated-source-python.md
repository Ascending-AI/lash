# Python generated source, format v1

This is the Python exact codec under the [shared exact-source contract](generated-source-format.md).
`lash_dialect_python::print(Document, PrintOptions)` renders only;
`compile_generated(source, GeneratedSourceLimits)` reconstructs an unadmitted
Document directly. Neither operation resolves a library. Python's authored
`lower` remains a separate cell operation. Host and worker integration belongs to FIG-6205. The readable rules below
extend the landed generic codec without a second printer or decoder.

The source is a Python syntax subset with kernel semantics. In the complete generic spelling, every compound
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
The readable subset below additionally accepts exact native numbers and
its enumerated statement, member and helper-call spellings.
Blank lines and whole-line comments are ignored. In-line comments and
non-four-space indentation are outside this canonical generic spelling.

## Paired readable rule inventory

The printer chooses these spellings by structural predicates, without trying
compilation. `InlinePlan` computes eligibility once in postorder and remembers
eligible expression addresses for this print only. Its membership lookups never
control ordering or alias allocation. The extra pass and expected hash-table
cost are O(nodes); output remains append-only O(bytes). The decoder uses its
own bounded operand grammar, never Python authored lowering or a library.

An inline expression is a literal, variable, clock/random leaf, an eligible
member, or a helper expression call whose arguments are all inline. Collections,
closures and projection reads keep explicit suites. This preserves tuple/list/
set/map/record distinctions and lets closures remain at their exact tree position.
A generic suite can contain readable children, and a readable block can contain
generic statements. No statements or temporaries disappear.

Every conditional row has a complete-byte positive and nearest-fallback witness
in `tests::generated_source::readable::paired_readable_rules_pin_positive_and_near_miss_spellings`.
The existing bounded naming and stored-field inventories also cover the reused
unconditional leaf rules.

| Rule and applicability | Readable spelling and exact inverse | Positive / nearest fallback witness |
| --- | --- | --- |
| Original identifier: shared legal, injective name plan | `λ` reconstructs the exact Name through its envelope row | `λ` / keyword `return` becomes `kernel_name_1`; aliases apply equally to shadowed bindings |
| Native integer: Literal::Int, all canonical decimal integers | `-7` reconstructs Int regardless of number policy, including arbitrary precision | Int `-7` / Function named `-7` stays `k.function("-7")` |
| Native float: Literal::Float with finite value | `-0.0`, `1.0`, `1e20` reconstruct Float through its kernel canonical parser; a point or exponent distinguishes Int | signed zero / infinity stays `k.float("inf")`; nan and negative infinity also stay explicit |
| Existing scalar leaves: null, bool or text | `None`, `True`/`False`, JSON-quoted text preserve their variants | null / absent stays `k.absent`; text / function and bytes stay `k.function` and `k.bytes` |
| Attribute: inline target and field satisfying the codec's conservative binding-identifier predicate | `object_value.title` wraps the target in Member::Field, preserving field text; non-variable/call/member targets are parenthesized | legal `title` / keyword field or closure target keeps `k.field` suite |
| Subscript: target and index both inline | `items[-1]` wraps them in Member::Index, in target/index child order | literal index / closure index keeps `k.index` suite |
| Expression helper call: every argument inline | `helper_function(3, λ)` constructs Expr::Call with the envelope's full FunctionId; empty argument lists are allowed | scalar args / closure argument keeps `k.invoke` suite |
| Library Call or Spawn Action: callee is Library; operands remain atoms | `k.call(helper_function(3))`, `k.spawn(helper_function(3))` construct Action::Call/Spawn with Library callee; no expression call is introduced | Library / Value or Declared callee keeps explicit `k.value`/`k.declared` plus atom list |
| If: inline condition | `if ready:` and mandatory `else:` decode directly to If with both Blocks; empty bodies spell `pass` | variable condition / closure condition keeps `k.if_` suite |
| Elif: else Block has exactly one If whose condition is inline | `elif second:` reconstructs an else Block containing that one If; both Nodes keep their Sites | singleton nested If / extra statement preserves `else:` plus nested `if` |
| For: inline iterable | `for item in ready:` preserves binding, iterable and body, without iterator/coercion nodes | variable iterable / closure iterable keeps `k.for_` suite |
| While: inline condition | `while ready:` preserves condition and body exactly | variable condition / closure condition keeps `k.while_` suite |
| Try: at least one handler present | `try:`, optional `except BaseException as error:`, optional `finally:` preserve absent versus present empty Blocks; BaseException is reserved clause syntax, not an effect or lookup | finally-only / handlerless Try keeps `k.try_(None, False)` suite |
| Terminal expression statement: inline value | `return ready`, `raise ready`, `k.print(ready)`, `k.finish(ready)`, `k.fail(ready)` each reconstruct their distinct Stmt variant | variable value / closure value keeps the corresponding explicit suite |
| Break and Continue: unconditional Stmt leaves | `break`, `continue` preserve their variants | both positive leaves; a function literal named `break` remains an explicit expression, never a statement |

For example, the same generic envelope and main wrapper can contain:

```python
with k.main():
    with k.block():
        with k.let(answer):
            helper_math_add(input_value.total, 2)
        if ready:
            k.finish(answer)
        elif retry:
            while pending:
                with k.do():
                    k.call(helper_worker(answer))
        else:
            try:
                raise "unavailable"
            except BaseException as error:
                k.fail(error)
            finally:
                pass
```

Here every identifier and helper alias must be declared by the envelope. The
nested `while` does not insert a condition evaluation temporary. Let and Assign
remain distinct explicit suites, and action wrappers never fuse spawn/join.
Numeric selection ignores the manifest's authored number policy: the spelling
itself records the stored variant. Only canonical numeric text is accepted;
leading zeros, noncanonical floats and overflowing native floats refuse.

Readable blocks emit statements directly at one deeper indentation, or exactly
`pass`. Else is mandatory even when empty; it is never inferred from a missing
clause. A flattened elif creates exactly the same two kernel Nodes as an explicit
else containing one If. The reader accounts for both levels in its node-depth
budget. Postfix expression depth is checked independently of indentation;
parentheses introduce no Node and have a separate bounded syntax-depth guard.
Unexpected operators, calls through variable identifiers, non-atomic action
arguments, extra handlers or leftover clauses refuse.

Maps begin and end frames in `Node::children` order during final writing,
including inline arguments, member target/index, direct Blocks and both elif
frames. Clause labels create no synthetic Site. Even empty Blocks have their
`pass` range; no new implicit-node category is needed. Each inline invocation
has its own range, whereas the shared alias declaration has no Site.

The acceptance and independent handwritten-source laws pin the inverses rather
than execution observations. The additional bounded domain is exactly 6 names
× 7 literals × 8 control contexts × 2 empty/nonempty choices = 672 Documents,
with map-on/off canonical-byte checks, foreign full IDs, duplicate helper labels,
keyword/namespace aliases, closure shadowing and escaping. The native literal
law additionally covers arbitrary-precision integers, finite binary64 extrema,
subnormals, exponents and both signed zeroes under both number policies.
The independent finite form inventory, admitted Python corpus, TypeScript-origin
batch and 64 Smith seeds are reused. Dropping the else Block in the readable
inverse makes the independent reader fixture's complete-byte law fail.

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

# Workflow source and the general document codec

Saved workflows print as standalone source through
`HostArtifacts::print_workflow(definition, captions, format, options)`. It loads
retained content and uses the shared materializer also used by `open_workflow`.
Captured data, callable environments and initialization appear as ordinary
lexical declarations inside the selected entry. Reading executes no guest code
and publishes no artifacts. The caller retains the original definition while
loading; the resulting document and text own their content.

`publish_callable` materializes the frozen graph and verifies a complete
print/readback in its authored dialect under the default generated-source
limits before it retains or publishes the definition. An unspellable callable,
a callable without an authored dialect, or a readback mismatch refuses
publication with the materialization or source error. `inspect_callable` still
inspects executable reach without imposing this authoring rule. Crossing a
`$lash_function` value does not itself publish it. A later explicit print may
still refuse stricter caller-supplied limits or invalid caption overrides.

The host edits the text freely, selects B's entry, and calls `import_workflow`
with the host's current environment and source policy. It publishes the imported
DocumentBytes, map and source with `publish_workflow`, then creates and publishes
a Linked definition. No original definition ID, capture table, edit token or old
Link fills participate. Equivalent dependencies preserve the public contract and
observable behavior; another dependency choice can change behavior or refuse.
Definition identity, formatting, private names, internal Sites and charge need
not remain equal. A remains independently executable.

## Ordinary lowering and verification

Printers emit supported ordinary TypeScript or Python. They lower candidates
through the ordinary front end. Exact canonical equality remains the first
proof. Module verification additionally permits private alpha-renaming,
reidentification of local functions and exact import-slot renumbering, while
comparing executable node shapes, helper identities, public contracts and the
checker's resolved binding identities. Rendering may retain explicit ordered
locals when folding temporaries would change evaluation or failure order.
This is a structural proof, not sampled execution or a general equivalence solver.

In TypeScript, `k` is an ordinary identifier. Generated names use collision
handling rather than a reserved kernel namespace.

Typed data constructors preserve value kinds: `error.new`,
`timestamp.from_nanoseconds`, `integer.from_decimal`, `bytes.from_hex`,
`tuple.of`, `map.from_entries`, `set.of`, `record.new`, `value.absent` and
`value.null`. `value.null()` is the null value as an operand no source type
describes.
`tuple.at` expresses strict kernel indexing. `value.same` retains exact native
equality, including default-argument checks, without Python's `is` adapter. TypeScript prints a strict
numeric operation over checked `number` operands as the operator it lowers
from (`n < 1`, `n !== 0`): reading the operator lowers its checks again. Where
that does not read back, it spells the operation by its `num.*` helper, and
`num.checked_float` preserves the TypeScript `ts.number.checked` guard without
coercing an integer to Number. A primitive text method over constant bounds
(`slice`, `trim`, `split`, `indexOf`, `includes`, `startsWith`, `endsWith`)
prints as the method call its natives lower from, receiver check included.
Strict text concatenation first tries plain `+`. Its fallback spells the
operands with erased `as string` assertions, preserving `text.concat` when a
materialized capture cell initially holds `null`. The assertions introduce no
runtime conversion or check.
The conservative bigint form keeps the exact tuple representation and spells
its input normalization with an `as bigint` assertion, so the normalization
stays at its original read even when a frozen cell's initializer loses the
original type fact. Python nested signatures retain `JsonValue` annotations.
Known-token calls use `callable.invoke_token`; they lower normally and need no
retained document. `callable.invoke(closure, ...operands)` in either dialect
calls a bare closure with exactly the supplied operands. A qualified code
member, such as `callable.invoke(views.heading, digits)`, calls that linked
Import with exactly those operands. Neither form adds argument snapshots,
adaptation, packing, a receiver, an exception operand or an await adapter.
This preserves calls over frozen capture cells even when ordinary package
call lowering would snapshot a mutable argument. TypeScript has more exact operation spellings, used only when the
convenient spelling does not read back, as when a materialized capture cell
replaces the binding whose facts the original lowering took.
`list.element(xs, i)` is the element read of a list the source built: its
bounds test, its element, otherwise the numeric read. It has no kind test of
its own, so a receiver that is no list fails that bounds test's kind check.
`callable.method(m, "name", ...args)` is `m.name(args)`. It reads the
receiver binding, and each argument that names a binding, where the
method's dispatch reads them, with no hold before later operands run.
`list.method(xs, "name", ...args)` is that call on a list the source built:
the list's own row (`map` over its slots, say) with no kind test, its
receiver and named arguments read as `callable.method` reads them. It
spells every row of the list's own table (`push`, `indexOf`, `sort`, ...); a
receiver that is no list gets that row, with its kind check where it has one.
`record.method(r, "name", ...args)` is that call on a record the source built:
its own field by that name where it is unbranded and holds one, else the
built-in row's helper read, applied to `r`, receiver and named arguments read
as `callable.method` reads them. A receiver that is no record takes the same
ladder, so a list fails its own-field test's kind check.
`value.method(t, "name", ...args)` is that call on a text the source built:
the text's own row (`charAt`, `trim`, `split`, ...) after the text receiver
check a text binding's method takes, so a receiver that is no text fails that
check. `list.field(xs, "name")` and `value.field(t, "name")` are a built
list's or text's own member read (`size`, `flags`, ...), without the record
ladder an unknown receiver takes.
`list.set(xs, i, v)` is a built list's element write: `i`'s position test and
the bound that admits an append, then the write in place, else `ts.set`, so
`xs[i] += v` on a capture reads `list.element` and writes `list.set`.
`value.initialized(k)` is the read of a binding an enclosing function declares
later: its temporal-dead-zone check, failing where the read runs.
`list.length(xs)` is a built list's length as a float, with no kind test, so
a receiver that is no list fails the length read's own kind check.
`record.field(r, "name")` is a built record's field: its own field where it is
unbranded, else the read helper. `value.length(t)` is a built text's
`length`: the length helper's read, a number (`text` is no reserved root).
`value.operand(k)` is a binding as an operator's operand, read where the
operator reads it rather than held first: `"a" in value.operand(k)`,
`delete value.operand(k)[f]`, `new (value.operand(k))(...)`, `for (const p in
value.operand(k))`, `value.operand(k)?.(...)`. The operator, its `?.` test and
branch included, stays in the source, so an operand the original skips stays
skipped. `list.operand(xs)`, `record.operand(r)` and `value.text_operand(t)`
are that read of a built list, record or text: the operator applied to it takes
that kind's lowering (`list.operand(xs)?.[i]`, `record.operand(r)?.name`,
`value.text_operand(t)?.[i]`, `const {a} = list.operand(xs)`). None checks
the operand's kind; a value of another kind fails where the kind's operation
fails, or gets the generic helper the kind's lowering calls.
A materialized capture's factory declares its cell with the declared type the
captured binding's evidence carries, so an annotated binding (`xs: unknown[]`)
keeps the lowering its annotation chose. Python prints an in-place update of
an item, `xs[i] op= v`, from its pinned target and key, item read and
operator, for a list, a dict or any target.
Each lowers to the nodes, helpers and evaluation order of the lowering it
names, and keeps that operation's failures at its own site. Python's
`@callable.direct` on a nested function preserves that bare closure ABI and its
annotated signature. Parameters name positional operands; `= ...` marks an
optional operand without adding a default-value prologue or a binder. This
lets lexical factories and captured helpers retain their own calling
conventions even when the enclosing saved entry uses packed Python tokens.
Python capture setters use this direct ABI too, preserving captured `Absent`
as data instead of passing it through the source argument binder.
Native calls still select the installed helper by name and must reproduce its
exact identity for printing to succeed. Constructor roots are reserved package
roots. Bound or opaque function tokens without a faithful source spelling
refuse with typed `PrintError::Unprintable`, never plausible fragments.

Conflicting requirements with one qualified source name refuse as
`NamespaceRefusal::ImportNameConflict`, carrying both declarations through the
worker boundary. No source namespace silently overwrites a contract. Missing
requirements and fresh helper admission retain their ordinary typed refusals.
A Main-only definition without an annotated module entry refuses standalone
printing rather than inventing a public result contract.

Source-backed module publication compiles under the supplied namespace and
checks both the document and source map against that import. A permissive read
or print grants no authority to a stricter publication or a removed helper.

## Captions, maps and limits

Captions are sidecars, outside executable identity. Overrides validate against
the original document before materialization maps them into owned content.
The printer spells supported captions and reports explicit dropped targets.
A print map names the materialized document, not the original cell. Imports
answer a new map and sidecar for the source actually compiled. Hosts own their
versions, layout and any historical relationship between A and B.

Source evidence is a second sidecar, also outside executable identity: a
document compares equal exactly when its canonical bytes do. Besides a
function's admitted implicit text conversions, it holds the declared types of
its locals (`let cursor: string | null`), keyed by the binding's kernel name.
That name is unique along a lexical scope chain, so a shadowing `cursor` keeps
its own type, and a name that two bindings of a function and the functions it
contains declare differently records none. Lowering records it in the written
notes. A captured function value carries it (as an optional last field, so
values without evidence keep their encoding), and materialization attaches it
to each copied function. A printer spells the declaration's annotation from it
only when the printed source still lowers to the same document. Evidence
proves no construction, and a printer never guesses missing evidence from a
current value: without evidence, a local whose type decided its lowering
refuses to print.

Python trusts a simple local annotation (`total: int`, `label: str`) for every
read and write of the binding, including reads from a nested function, and
checks an assigned value of another type with `py.name_check`. Its evidence is
each annotation the lowering of a function and the functions it contains used,
keyed by the binding's kernel name. The printer spells the annotation on the
binding's first write in the scope that binds it: the scope's own evidence, or
for `main` and a materialized factory, which have none, what the functions
they contain agree on. Under the annotation, a checked value is written as the
value it checks, since the annotation writes the check again. A materialized
capture cell starts as an unchecked null, which it writes as
`label: str = value.null()`.

GeneratedSourceLimits independently bounds source bytes and node depth.
Printing refuses oversized or unrepresentable content with typed errors and
never returns truncated source. Optional SourceMaps describe final UTF-8 spans
and complete implicit locations; maps validate against their own document and
text. Printing and import do not select execution authority.

## General document codec

The pre-existing `Dialect::read`, `ReadSource`, worker `Read` request,
`WorkerService::read_exported` and `HostArtifacts::import_exported_workflow`
remain for general document/cell codec consumers. They accept a Document as
source-name metadata and carry no ProcessDefinitionId, capture environment or
saved Link fills. Their existing exact document laws remain applicable to
that supported subset. They are not the workflow author's source-edit path
and are not a fallback when standalone printing refuses.

`WorkerService::print` also prints supported cell Documents, which may have no
module entry. This generic codec contract is distinct from definition-level
standalone workflow-module printing. Hosts use ordinary module import when
independently publishing printed workflows.

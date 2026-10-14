# Exact generated workflow source, format v1

This is the shared contract for FIG-6202, FIG-6203 and FIG-6199/FIG-6204.
A dialect changes spelling only. A supported, structurally valid Document
prints and decodes to identical `Document::canonical_bytes()`: manifest,
entries, private names, all declarations (including unused ones), and main.
There is no alpha-equivalence or normalization. Annotations and maps are
outside the Document. Source is a generated language subset, with kernel
semantics; execution by a stock language runtime is not promised.

## Independent operations

`print(document, PrintOptions { source_map, limits }) -> PrintedSource`
performs naming and structural rendering only. It does not compile, compare,
resolve libraries, admit, execute, or select a helper release.
`PrintedSource` holds `source`, `SourceFormat`, and an optional `SourceMap`.
Maps default off. `compile_generated(source, GeneratedSourceLimits)` returns
an unadmitted Document and needs no library or name catalogue.
`Document::canonical_bytes()` exposes the existing identity encoder.

Tests must compose all three operations on every successful print. Hosts
may compose them too; there is no verifier service or verification flag:

```rust,ignore
let printed = lash::typescript::print(&document, Default::default())?;
let decoded = lash::typescript::compile_generated(&printed.source, Default::default())?;
assert_eq!(document.canonical_bytes()?, decoded.canonical_bytes()?);
```

The trusted whole-workflow entry recognizes a leading reserved `k.document`
envelope (and incomplete `k.declare`/`k.main` modules), including trivia between its tokens. It routes to this same decoder
and never falls back to authored lowering after a generated refusal. Arbitrary
`k.` tokens are not dispatch markers. Cell lowering remains authored lowering
and grants no privilege to an envelope. Hosts still perform their existing
structural/link/effect admission before use. Decoding neither discovers a
manifest closure nor checks dependencies against an installed library.

## Envelope and aliases

TypeScript's module is exactly one envelope, zero or more declarations, then
one main. Additional module items, duplicate declarations, missing main, old
headers and unsupported formats refuse. The seven envelope operands are:

```typescript
k.document("lash.generated.typescript", "1",
  "<manifest JSON>", "<entries JSON>", ["<private original name>"],
  "<identifier alias pairs JSON>", "<helper alias pairs JSON>");
k.declare("<original function name>", function(<parameter aliases>) { /* body */ });
k.main(function() { /* body */ });
```

Python uses the same seven logical operands and the format identifier
`lash.generated.python`, version `1`; its codec documents its function/body
syntax alongside its paired rule inventory. The version belongs to generated
source, not the kernel's stored/wire version. `SourceFormat::TypeScriptV1` and
`PythonV1` identify the selected syntax.

The manifest preserves kernel version, number policy, effects with complete
signatures, and full FunctionId-to-FunctionName requirements. Entries preserve
original keys and complete signatures, including parameter names, optionality,
and nested types. Private bindings are original names. Metadata uses the
kernel's existing JSON shapes; duplicate object keys at any level refuse.
No program body is hidden in metadata or in an opaque Document payload.

Identifier aliases are a JSON array of `[sourceIdentifier, originalName]`.
The writer includes legal names as self mappings too. Each source identifier
and each original name occurs at most once. Parameters, variables, loop and
catch bindings use this table. Explicit string-bearing forms (`declare`,
`declared`, `value`, `function`, entries and private names) carry original
names directly. Thus shadowing reconstructs the same Name without capture.
The reader refuses undeclared identifiers and conflicting/reserved aliases.

Helper aliases are a JSON array of `[sourceIdentifier, fullFunctionId]`.
Each alias and identity occurs once. Helper labels never resolve dependencies.
Aliases live in the reserved `helper_` namespace. `invoke` and `library` refer
to an alias, so even helpers from the other dialect decode without being
installed. Malformed IDs, duplicate identities or contradictory aliases refuse.
Extra unused metadata remains data; admission decides whether it is valid.

## Deterministic naming plan

`NamePlan` first collects declaration names and parameters in the Document's
BTreeMap order. It then walks main and declaration bodies in kernel
`Node::children` preorder, collecting bindings, variables, closure parameters,
callee names and function literals on first encounter. Entry/private names
follow in their BTree order. It reserves every legal original identifier
before assigning any alias. Equal original Names share one spelling everywhere.

TypeScript legality uses SWC's Unicode identifier character predicates, excludes
language keywords, strict bindings `eval`/`arguments`, and `k`, and reserves `kernel_name_` and `helper_`. Legal
original names retain their spelling. Other names receive `kernel_name_1`,
`kernel_name_2`, etc., in first-encounter order, skipping used spellings. The
mapping is injective. The Python codec supplies its language legality predicate
and the same allocation plan. Alias rows are emitted in original-name order.

Helper IDs are collected from the complete manifest and actual call nodes and
sorted by full identity. A manifest label becomes `helper_` plus its qualified
name with dots replaced by underscores. An identity with no manifest label uses
`helper_function`. Collisions get `_2`, `_3`, etc., in ID order, including
collisions with naturally suffixed labels. Helper rows are emitted in ID order.
Names in either reserved namespace are aliased, so they cannot capture a helper.
Ordered table operations cost O((names + helpers) log(names + helpers));
structural walks and final writing are linear in nodes and bytes, apart from
optional Site path copies, whose cost is the total collected path length.

## Complete generic spelling inventory

The TypeScript codec and its independent inventory law exhaustively match the
kernel variants and stored fields. These are direct reconstruction rules, never
ordinary authored lowering. `e` denotes an expression, `a` an atom, `n` an
identifier alias, `original` an original-name string, and `h` a helper alias.
Every block is explicitly spelled, including empty else/catch/finally blocks.

| Kernel form | TypeScript generic spelling |
| --- | --- |
| Block | `{ statement... }` (order and boundaries retained) |
| Let / Assign variable | `let n = rhs;` / `n = rhs;` |
| Assign member / Remove | `member.value = rhs;` / `k.remove(member);` |
| Do | `action;` |
| If | `if (e) { ... } else { ... }` |
| For / While | `for (let n of e) { ... }` / `while (e) { ... }` |
| Break / Continue | `break;` / `continue;` |
| Return / Throw | `return e;` / `throw e;` |
| Print / Finish / Fail | `k.print(e);` / `k.finish(e);` / `k.fail(e);` |
| Try with a handler | `try { ... }` with optional `catch (n) { ... }` and `finally { ... }`; absent differs from present empty |
| Try without either handler | `k.try(function() { ... });`; the parameterless wrapper reconstructs a Block, without introducing a Closure |
| RHS | An expression or an action, distinguished by its outer form |
| Null / Absent / Bool / Text | `null` / `k.absent` / `true` or `false` / JSON-quoted text |
| Int / Float / Bytes | `k.int("canonical integer")` / `k.float("canonical float")` / `k.bytes("hex")` |
| Function literal | `k.function("original")` |
| Variable | `n` |
| Tuple / List / Set | `k.tuple(e, ...)` / `k.list(e, ...)` / `k.set(e, ...)` |
| Map | `k.map([key, value], ...)` (entry order retained) |
| Record | `k.record(["field", value], ...)` (field order retained) |
| Field / Index member | `k.field(target, "field")` / `k.index(target, index)` |
| Closure | `function(parameterAliases) { ... }` (captures remain derived) |
| Expression Call | `k.invoke(h, [e, ...])` |
| Clock / Random / Read | `k.clock()` / `k.random()` / `k.read(handle, request)` |
| Callee Declared / Value / Library | `k.declared("original")` / `k.value("original")` / `k.library(h)` |
| Action Call / Spawn | `k.call(callee, [a, ...])` / `k.spawn(callee, [a, ...])` |
| Perform | `k.perform("effect", [a, ...], k.type("complete type JSON"))` |
| Sleep / Join / Cancel | `k.sleep(a)` / `k.join(a)` / `k.cancel(a)` |
| JoinMany | `k.joinMany("all" or "all_settled" or "race" or "any", a)` |
| Yield | `k.yield()` |
| Atom | Variable or literal only; never an invented expression Site |

Float spelling is kernel canonical spelling, including `nan`, `inf`, `-inf`
and signed zero `-0.0`. No helper is rewritten into a native operator. Numeric
variants, absent/null, expression/action calls and collection variants remain
distinct. The codec rejects syntax it does not reconstruct, including imports,
spread operands, numeric literals, const/var declarations, typed parameters,
async/generator function wrappers and trailing module text.

Readable successors extend a paired rule inventory next to the codec: each
rule states its applicability predicate, exact inverse, positive spelling
witness and nearest generic fallback witness. Generic forms remain permanent.
No statement fusion, temporary erasure, closure introduction or spawn/join
fusion is authorized by this format.

## Resource policy and optional final-text spans

`GeneratedSourceLimits` defaults to 16 MiB and node depth 64 (root block at
zero), independently of ordinary cell limits. Hosts can narrow these limits;
the native depth ceiling remains 64. Print checks node and metadata-type depth
iteratively before recursive encoding or rendering, bounds custom-serializer leaf allocations, then counts actual encoded
Document input bytes with a bounded sink. Input and output both obey the byte
budget. A single append-only writer checks each addition; metadata temporaries
are bounded too. No repeated recursive subtree strings or second source tree
are constructed. Byte arithmetic is checked. Exhaustion yields `PrintError::Bytes`
or `Depth`, never successful truncated source; encoding errors remain typed.

Generated compilation bounds source bytes and lexical/parser nesting before
SWC, keeps parsing, decoding and AST destruction on the reserved stack, checks metadata JSON depth before decoding, then checks the reconstructed
Document and metadata types under the same host policy. Parser stack reservation
uses checked arithmetic. Generated refusals use typed dialect codes
`GeneratedSourceFormat`, `GeneratedSourceBytes` and `GeneratedSourceDepth`;
malformed body syntax uses the dialect's existing `InvalidAst` diagnostic.

`SourceWriter` records final UTF-8 offsets while nodes emit their final layout;
no subsequent pretty-printing may invalidate them. Maps identify SHA-256 of
source bytes, DocumentId and SourceFormat. Reusing a map requires checking all
three. Nothing stores maps automatically. With collection disabled, no Site,
frame, range, source hash or document-identity calculation is performed for maps.
An instrumented shared-writer law proves zero map-specific work when disabled.

Map completeness is exactly `Node::children` from main and declared Function
roots. Each Site occurs once, with nonempty half-open ranges at valid UTF-8
boundaries. Parent/child ranges may overlap. Generic forms spell every block,
so they need no implicit anchors. `SourceSite::implicit` is reserved for a
successor rule explicitly enumerating an unspelled empty block; executable
statements, expressions and actions always have ranges. Metadata and alias
rows have no synthetic Sites; actions have no child Sites for their atoms.
Library bodies and graph IDs are outside this map.

## Cutover and old source

Only generated-source v1 is emitted and accepted. Unversioned generated
modules are refused, not routed through authored lowering or a compatibility
mode. Stored Documents and their JSON remain the same 1.0 baseline and can be
re-exported directly. A host holding only old generated text must decode it
with the previous toolchain, retain that Document, and export it with the new
one. This change does not add stored migrations or change kernel/worker wire
version constants. The existing worker Print request and string response retain
their shape; optional map transport is owned by FIG-6205.

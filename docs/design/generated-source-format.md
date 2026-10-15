# Exported workflow source, format v1

This is the shared contract for exporting a kernel Document as TypeScript or
Python and reading it back (FIG-6202, FIG-6205, FIG-6250, FIG-6251, FIG-6265,
FIG-6272). A dialect changes spelling only.

Ruling (2026-10-14): exported source contains only code an agent could have
written. The environment (effect manifest, number mode, entry signatures,
private bindings, helper identities, name renames) stays where it normally
lives, in the tool definitions, host configuration and the stored Document.
It is never serialized into dialect source.

A supported, structurally valid Document prints, and reading the printed
source against that Document gives identical `Document::canonical_bytes()` and
the same `DocumentId`: entries, private names, every declaration (including
unused ones) and main. There is no alpha-equivalence or normalization.
Annotations and maps are outside the Document. Source is a generated language
subset with kernel semantics; a stock language runtime is not promised to run it.

## Independent operations

`print(document, PrintOptions { source_map, limits }) -> PrintedSource`
performs naming and structural rendering only. It does not compile, compare,
resolve libraries, admit, execute, or select a helper release. `PrintedSource`
holds `source`, `SourceFormat` and an optional `SourceMap`; maps default off.

`read(source, base, GeneratedSourceLimits) -> Document` reads exported source
back against `base`, the Document it was printed from (the host holds it, as
JSON or, per FIG-6258, as its bytes or id). The source supplies the code: the
functions and main. The base supplies the rest: manifest, entries, private
bindings, the full identity behind each helper spelling and each renamed
identifier. `read` needs no library or name catalogue and does not admit.
`Document::canonical_bytes()` exposes the identity encoder.

Tests compose all three on every successful print; hosts may too. There is no
verifier service or verification flag:

```rust,ignore
let printed = lash::typescript::print(&document, Default::default())?;
let read = lash::typescript::read(&printed.source, &document, Default::default())?;
assert_eq!(document.canonical_bytes()?, read.canonical_bytes()?);
```

A host that edits the exported text reads the edit against the same base. An
edit can change any function body or main. It cannot add, drop or retype an
entry (in TypeScript, `export` must mark exactly the base's entries), add a
helper the base does not hold, or rename the environment. A construct the
reader has no exact inverse for is a typed refusal naming it
(`GeneratedSourceFormat` for module structure, the dialect's `InvalidAst` for
body syntax), never a silent change. Nothing falls through to authored
lowering, and authored lowering is not the reader: lowering is not injective
(boundary calls, guarded reads, iterator expansion and temporaries), so its
output has no unique authored source. New agent-written cells still lower
through the authored front end.

## Module shape

TypeScript: zero or more function declarations, then exactly one
`async function main()`. An entry is `export async function name(...)`; every
other function is `async function name(...)`. Python: zero or more
`def`/`async def` functions, then one `def main():` (`async def` wherever a body
awaits). Additional module items, duplicate declarations, a missing main and
trailing text refuse.

```typescript
export async function deep_research() {
  let search_input = {topic: "Reducing customer onboarding drop-off", max_sources: 3};
  let evidence = await tools.research.search(search_input);
  let synthesis_input = {topic: evidence.topic, sources: evidence.sources};
  let report = await tools.research.synthesize(synthesis_input);
  let display_input = {text: report.summary};
  await tools.display.show_message(display_input);
  return report;
}
async function main() {
  finish(null);
}
```

There is no header, directive, declaration or stub block, no name or helper
table and, in the ordinary case, no cast. An effect call without an annotation
reads as `result: any`, which is what authored lowering stores. Only a
document whose effect result is another type spells it
(`as KernelType<...>` in TypeScript, `result=...` in Python). The previous
`k.document(...)` envelope and its reader are deleted; there is no dual format.
`SourceFormat::TypeScriptV1` and `PythonV1` identify the selected syntax.

## Deterministic naming plan

`NamePlan::new(document, legal)` is computed from the Document by the printer
and from the base by the reader, so both sides agree without printing a table.
It first collects declaration names and parameters in the Document's BTreeMap
order, then walks main and declaration bodies in kernel `Node::children`
preorder, collecting bindings, variables, closure parameters, callee names and
function literals on first encounter. Entry and private names follow in their
BTree order. It reserves every legal original identifier before assigning any
alias. Equal original Names share one spelling everywhere.

Legal original names keep their spelling. Other names (keywords, reserved
generated APIs, `main`, names in the `kernel_name_` or `helper_` namespaces,
and for Python names that are not NFKC-stable) receive `kernel_name_1`,
`kernel_name_2`, ..., in first-encounter order, skipping used spellings. The
mapping is injective. A name an edit introduces reads as itself if it is legal
and is not the original of a renamed name.

Helper identities are collected from the manifest and the call nodes and
sorted by full identity. A manifest label becomes `helper_` plus its qualified
name with dots replaced by underscores; an identity with no label uses
`helper_function`. Collisions get `_2`, `_3`, ... in ID order. The reader
resolves a helper spelling to the base's identity; a spelling the base does
not hold refuses. Ordered table operations cost O((names + helpers) log(names +
helpers)); structural walks and writing are linear in nodes and bytes.

## Authored spellings and remaining fallbacks

Every modeled ordinary construct uses ordinary syntax with a direct inverse:
functions, calls, awaited effects, bindings, literals, records/objects,
collections, members, operators by precedence with minimal parentheses, and
structured controls. A library action whose authored spelling is the node's
exact inverse prints that way: TypeScript `console.log(...args)` and
`target.name(...args)`; Python builtins such as `len(x)` and `range(a, b)`,
methods such as `xs.append(v)` and `"a b".split()`, `x[a:b]` slices and
`type(x).__name__`. The base document must hold the helper label once.
Everything else keeps a body-level fallback, each listed per dialect with a
corpus witness and reason in
[`generated_rules.md`](../../crates/lash-dialect-typescript/src/generated_rules.md)
and [`generated-source-python.md`](generated-source-python.md). Examples are
the handlerless Try, kernel task primitives, and helper actions lowering
inserts that no source spells.

## Resource policy and optional final-text spans

`GeneratedSourceLimits` defaults to 16 MiB and node depth 64 (root block at
zero), independently of ordinary cell limits. Hosts can narrow these limits;
the native depth ceiling remains 64. Print checks node and type depth
iteratively before recursive rendering, then counts actual encoded Document
input bytes with a bounded sink. Input and output both obey the byte budget.
A single append-only writer checks each addition. Byte arithmetic is checked.
Exhaustion yields `PrintError::Bytes` or `Depth`, never truncated source.

Reading bounds source bytes and lexical/parser nesting before parsing, keeps
parsing, decoding and AST destruction on a reserved stack, checks result-type
nesting before decoding it, then checks the reconstructed Document under the
same policy. Reconstructed renamed names are counted against the byte budget
before they are cloned. Refusals use the typed dialect codes
`GeneratedSourceFormat`, `GeneratedSourceBytes` and `GeneratedSourceDepth`;
malformed body syntax uses the dialect's `InvalidAst` diagnostic.

`SourceWriter` records final UTF-8 offsets while nodes emit their final layout.
Maps identify the SHA-256 of the source bytes, the DocumentId and the
SourceFormat; reusing a map requires checking all three. Nothing stores maps
automatically. With collection disabled, no Site, range, hash or identity is
computed for maps (an instrumented writer law proves it).

Map completeness is exactly `Node::children` from main and the declared
Function roots. Each Site occurs once, with nonempty half-open ranges at valid
UTF-8 boundaries. TypeScript's omitted empty else records its Block with
`SourceSite::implicit` immediately after the then block. Actions have no child
Sites for their atoms. Library bodies and graph IDs are outside the map.

## Host and worker operations

`WorkflowDocument::typescript(options)` and `::python(options)` return the
complete `PrintedSource`. The facade exposes `lash::typescript::read` and
`lash::python::read`; their results are unadmitted Documents.
`HostArtifacts::import_exported_workflow(source, format, base, environment)`
reads and admits in one step, against the same environment as publication.
`HostArtifacts::import_typescript_workflow` is authored lowering only.
Authored TypeScript and Python lowering refuse unresolved printed helper names
(`helper_*`), renamed bindings (`kernel_name_*`) and the `declared`
namespace (also `k` attribute and method references in Python) as
`PrintedSourceNeedsBase` (`TS_PRINTED_SOURCE_NEEDS_BASE` or
`PY_PRINTED_SOURCE_NEEDS_BASE`). The diagnostic names
`typescript::read(source, base)` or `python::read(source, base)` so a host can
route the source with its
base document. Ordinary authored bindings with those spellings still resolve;
quoted data and unrelated unknown names do not trigger this refusal. Source
without these unresolved references carries no provenance marker and is not
guaranteed to be distinguishable from newly authored source.

`lash::vm::WorkerService::print(document, format, options)` and
`::read_exported(source, base, format, limits)` run the same operations in the
worker crash domain. The read request carries the base Document's JSON; the
response is a Document, without a library, helper-release choice or admission.
The installed dialect package owns a `read` function pointer alongside its
authored front end and printer. The worker wire-v1 snapshots change in place
before the 1.0 cut; no stored shape changes.

To execute a read document, admit it through `WorkflowDocument::admit` or
publish it through the host's workflow API. Unavailable requirements and
mismatched signatures remain admission refusals.

## Readability acceptance

The shared `lash_kernel_dialect::readability` harness owns seven realistic
documents (three effects with record inputs, recursion with a foreign helper,
nested controls, loops over records, error handling, multiple entries and an
RLM tool cell) and the real `deep_research` agent cell. Each dialect registers
its printer, its `read`, a node-based generic detector and its documented
unavoidable rules. The harness reads each print against the case's Document,
checks complete canonical bytes and the DocumentId, and requires unique
real-Site coverage. It lexes each Site's own source (descendant ranges and
quoted data excluded) and counts every `k.` form, `helper_*` call,
pseudo-intrinsic and cast. Outside every node, the source may hold only
function declarations (names, keywords and parameter lists): any header,
directive, table or data there fails. A form without a matching named rule
and reason fails.

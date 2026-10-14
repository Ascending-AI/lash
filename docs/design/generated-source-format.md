# Exact generated workflow source, format v1

This is the shared contract for FIG-6202, FIG-6203, FIG-6199/FIG-6204 and FIG-6205.
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
  {kernel: 1, numbers: "float"}, {entry: {result: "any"}}, ["privateName"],
  [["entry", "entry"], ["privateName", "privateName"]], [["helper_alias", "fullFunctionId"]]);
async function entry() { return null; }
async function main() { finish(null); }
```

Python uses the same seven logical operands and the format identifier
`lash.generated.python`, version `1`; its codec documents its function/body
syntax in the [Python generic and readable inventory](generated-source-python.md). The version belongs to generated
source, not the kernel's stored/wire version. `SourceFormat::TypeScriptV1` and
`PythonV1` identify the selected syntax.

The manifest preserves kernel version, number policy, effects with complete
signatures, and full FunctionId-to-FunctionName requirements. Entries preserve
original keys and complete signatures, including parameter names, optionality,
and nested types. Private bindings are original names. Metadata uses the
kernel's existing field/type shapes in native literal syntax; duplicate object keys at any level refuse.
No program body is hidden in metadata or in an opaque Document payload.

Identifier aliases are a JSON array of `[sourceIdentifier, originalName]`.
The writer includes legal names as self mappings too. Each source identifier
and each original name occurs at most once. Parameters, variables, loop and
catch bindings use this table. TypeScript declaration names and `declared.alias`
function references also resolve through it; entries and private names retain
original names. Thus shadowing reconstructs the same Name without capture.
The reader refuses undeclared identifiers and conflicting/reserved aliases.

Helper aliases are a JSON array of `[sourceIdentifier, fullFunctionId]`.
Each alias and identity occurs once. Helper labels never resolve dependencies.
Aliases live in the reserved `helper_` namespace. Ordinary TypeScript helper
calls and awaited calls refer to the alias, so even helpers from the other
dialect decode without being installed. Malformed IDs, duplicate identities or contradictory aliases refuse.
Extra unused metadata remains data; admission decides whether it is valid.

## Deterministic naming plan

`NamePlan` first collects declaration names and parameters in the Document's
BTreeMap order. It then walks main and declaration bodies in kernel
`Node::children` preorder, collecting bindings, variables, closure parameters,
callee names and function literals on first encounter. Entry/private names
follow in their BTree order. It reserves every legal original identifier
before assigning any alias. Equal original Names share one spelling everywhere.

TypeScript legality uses SWC's Unicode identifier character predicates, excludes
language keywords, strict bindings `eval`/`arguments`, `main`, the documented
global generated APIs (tools, declared, console, task/terminal APIs and native
constructors), and `k`; it reserves `kernel_name_` and `helper_`. Legal
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

## Native TypeScript and necessary explicit forms

Every modeled ordinary construct uses ordinary TypeScript syntax with a direct
inverse: declarations/anonymous functions, calls, awaited effects, bindings,
literals, object/array expressions, members, operators and structured controls.
The paired inventory in
[`generated_rules.md`](../../crates/lash-dialect-typescript/src/generated_rules.md)
names applicability, inverses and positive/boundary witnesses. The only
remaining TypeScript body `k.*` form is handlerless Try, because native TypeScript
requires catch or finally. The header marker is the sole allowed generic form
in ordinary programs. Exactness and readability are both acceptance criteria.

Full-ID helper aliases distinguish unawaited Expression Calls from awaited
Action Calls. Bare, parenthesized and helper callees retain Declared/Value/Library
kinds. Effects are `await tools.effect.name(atoms...)`; result decoding types are
preserved by `as KernelType<structuredType>` only when the complete manifest
cannot express them. Every tool path and arity has this inverse without host
catalogues. Finish/Fail/Print are `finish`, `failure` and `console.log` calls;
Return/Throw stay native statements. Main is its own native function declaration.
Task APIs retain every spawn/join/action boundary.

A unique manifest label for a known `ts.*` operator explicitly binds its full
identity to that operator's generated syntax. The inverse uses this preserved
binding, without installing the helper or assuming equivalence to JavaScript.
Duplicate labels and mismatched arities keep ordinary full-ID alias calls.
Numeric/collection syntax carries exact variants: fractional/exponent tokens
for Float, canonical integer/bigint tokens for Int, undefined for Absent,
const-asserted arrays for Tuple, arrays for List and constructors for Set/Map/Bytes.
Quoted const-asserted text in an index selects Field; const-asserted tuple keys
remain Index. All native syntax decodes directly without inserting temporaries.
The generated async marker enables await syntax and has no stored node of its own.
No statement fusion or spawn/join fusion is authorized.

Python's native literal envelope/type encoding uses the same shared writer and
schema. Its body inventory remains in
[generated-source-python.md](generated-source-python.md); full native body
parity is owned by FIG-6251.

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
boundaries. Parent/child ranges may overlap. Spelled blocks receive ranges, including empty function/main bodies. TypeScript's readable empty-else rule records
the omitted Block with `SourceSite::implicit` at the byte boundary immediately
after its then block. This is the only unspelled TypeScript node; executable
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
version constants. Before the 1.0 cut, worker service shapes and both wire-v1
snapshots change in place.

## Host and worker operations

`WorkflowDocument::typescript(options)` and `::python(options)` return the
complete `PrintedSource`. Features `typescript` and `python` enable the local
codecs without selecting helper releases; `dialect-typescript` and
`dialect-python` include the matching local feature and helper release.
The facade exposes `lash::typescript::compile_generated` and
`lash::python::compile_generated`; their results are unadmitted Documents.
A host compares `document.canonical_bytes()` directly.

`lash::vm::WorkerService::print(document, format, options)` and
`::compile_generated(source, format, limits)` expose the same independent
operations asynchronously in the worker crash domain. Print requests carry
options and the selected format; responses retain source, format and the
requested map. A map-off response omits the map field entirely. Generated
compilation is a distinct request/response returning a Document, without a
library, helper-release choice or admission. The installed dialect package
owns a `compile_generated` function pointer alongside its authored front end
and printer; dispatch never chooses a second codec. The shared options,
limits, result, format, map and span types now have Serde representations,
covered by the worker schema snapshots. This adds no stored shape.

Whole-workflow source limits apply to these operations; the ordinary cell
source cap does not. Pool frame, allocation, queue and deadline bounds still
apply. Source-service errors retain dialect codes or the typed worker cause.
Nothing automatically stores a source or map.

To execute an imported document, admit it through `WorkflowDocument::admit`
or publish it through the host's existing workflow API. TypeScript's trusted
host import recognizes the reserved generated envelope before constructing a
name catalogue, decodes full identities, then performs normal admission.
Unavailable requirements and mismatched signatures remain admission refusals.
Ordinary model cells still use authored lowering; the marker grants no authority.
The [facade example and measured costs](generated-source-cost.md) show the
three operations composed explicitly, both locally and through a worker.


## Shared native envelope and readability acceptance

Both dialects use the same ordered seven-field envelope:
`document(format, version, manifest, entries, privateBindings, names, helpers)`.
The marker is dialect-specific, version is `"1"`, and the remaining fields
are structured native literals, never JSON encoded inside a string. Manifest,
entries and type objects retain the kernel's field names and type tags. Name
and helper tables are ordered arrays of `[alias, original]` and `[alias, fullId]`.
TypeScript uses object/array literals and `true`/`false`/`null`; Python uses
dictionaries/lists and `True`/`False`/`None`. Primitive type metadata is a single
string such as `"any"`, not a string containing JSON quotation marks.
Each decoder accepts only literal metadata, rejects duplicate object fields
before deserialization, and preserves unused requirements, entries and aliases.
The shared bounded writer and native-token conversion own this encoding rule.

The shared `lash_kernel_dialect::readability` harness owns seven realistic
kernel documents: three effects with record inputs, recursion with a foreign
helper, nested controls, loops over records, error handling, multiple entries,
and an RLM tool cell. Register every dialect's printer, public generated decoder,
node-based generic detector and documented unavoidable rules. The harness
checks complete canonical bytes and reports every generic occurrence with
dialect, case, kernel Site, node kind and reason. Generic forms without a
matching named rule and reason fail. Exactness wins over native spelling;
ordinary modeled constructs must use their direct native inverse.


The common readability harness independently walks `Node::children` to require
complete, unique real-Site coverage before asking registered dialect detectors
about generic forms. `corpus()` returns a typed `GENERATED_READABILITY_CORPUS`
diagnostic for malformed shared fixtures; it does not panic. Test registrations
supply every dialect's printer, decoder, detector and named unavoidable list.
The TypeScript registration permits no generic forms in the seven ordinary
cases. Python's registration is maintained by its owning body-codec lane.

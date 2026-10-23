# Workflow source and the general document codec

Saved workflows print as standalone source through
`HostArtifacts::print_workflow(definition, captions, format, options)`. It loads
retained content and uses the shared materializer also used by `open_workflow`.
Captured data, callable environments and initialization appear as ordinary
lexical declarations inside the selected entry. Reading executes no guest code
and publishes no artifacts. The caller retains the original definition while
loading; the resulting document and text own their content.

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

Typed data constructors preserve value kinds: `error.new`,
`timestamp.from_nanoseconds`, `integer.from_decimal`, `bytes.from_hex`,
`tuple.of`, `map.from_entries`, `set.of`, `record.new` and `value.absent`.
`tuple.at` expresses strict kernel indexing. TypeScript can spell strict
numeric operations by their `num.*` helpers and known-token calls by
`callable.invoke_token`; they lower normally and need no retained document.
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

# Opaque host documents

`lash::workflow::document::DocumentBytes` is the default host retention and
execution form. `HostArtifacts::import_workflow` and
`import_exported_workflow` return it; `publish_workflow` accepts it and
`WorkflowPublication` returns the admission and a `WorkflowDocumentRef` selecting
the entry. Read the published document with `HostArtifacts::execution_document(reference)`
while its publication pin is alive; the acknowledgment carries no document body. Starting a published definition uses its id, already held by Lash.
A host that only keeps and runs documents never needs their typed tree or JSON.

Keep all three parts together: `grammar_version()`, the Lash-issued `id()`,
and `as_bytes()`. Treat the bytes as an opaque blob. Reassemble stored parts
with `DocumentBytes::from_parts`; this constructor trusts nothing and performs
no admission. Lash validates receipt before publication: the grammar tag,
canonical token spelling and recursion/allocation bounds, and agreement of the
body's digest with the supplied id. Semantic admission then checks the
program against the chosen execution environment. Neither validation nor an
id confers retention: publish under a live host pin.

`DocumentBytesRefusal::GrammarVersion { found, supported }` names an unsupported
grammar before inspecting its tokens. `Tokens(TokenError)` retains malformed,
noncanonical or over-limit token causes; `Identity { supplied, actual }`
retains a digest mismatch. Publication returns them as
`WorkflowPublish::Refused(WorkflowAdmissionRefusal::Bytes(..))`, before any
artifact is held. A host can narrow recursive decoding policy through
`DocumentBytes::decode(lash::persistence::KernelDataLimits)` when reading a blob itself.

The blob is **not a public durable format**. A grammar change refuses old
blobs; there is no fallback decoder, upcaster or migration. The current grammar
is `DOCUMENT_GRAMMAR_VERSION = 1`, unchanged throughout the 1.0 baseline.
Hosts needing longevity across grammar changes keep source to re-lower, or
typed JSON to read under the target build's readable contract and re-encode.
That readable contract has its own compatibility boundary; JSON does not make
an unsupported program executable.

Reading and editing are explicit:

```rust,ignore
let document = bytes.to_document()?;                 // validated typed tree
let json = document.to_json()?;                      // readable host form
let edited = Document::from_json(&json)?;             // typed JSON reader
let runnable = DocumentBytes::from(edited);           // Lash derives the id
```

JSON never feeds identity. Generated kernel schemas (and any generated
TypeScript for them) describe only this typed readable form. Typed graph
reads, dialect lowering, worker source operations, the checker, editor and
function registry are read/edit or integrator seams: they intentionally keep
trees so implementing a frontend or editor adds no binary decode round trips.
They are not the default retained runnable host contract.

Internally, the opaque bytes are the existing canonical logical token body;
its digest is the document id (`K-ID-001..003`). The host wrapper changes no
checkpoint graph node, parked image, helper release or stored engine format.

# Editing, publishing and showing workflows

Recovery and external-effect guarantees follow [What survives a crash](../README.md#what-survives-a-crash).

A host imports or constructs a kernel Document, edits it, publishes a linked
start, and observes the process that runs it. Code identity, execution authority
and display data have different owners:

- A Document contains local functions, typed Imports, Exports, an optional Main,
  exact built-in library dependencies and node requirements. Its `DocumentId`
  identifies its executable content.
- A Link records the fills of that Document's Imports. A `LinkedStart` carries
  that `LinkId` and a `StartSelector::Main` or `StartSelector::Export(name)`.
- Packages advertise user- and host-installed operations and linked Exports.
  Accepted publications, consumer context and explicit attachments determine
  what unlinked code may import. A Package does not expose a model tool;
  model-facing providers are registered separately.
- Source maps, captions and package display labels are sidecars. They describe
  code without granting execution authority or changing executable identity.

Built-in TypeScript and Python helpers belong to the dialect binary, addressed
by exact `FunctionId`. Hosts do not install or publish helper Packages. The
[host-package guide](host-packages.md) describes publication ownership, visibility,
retention and deferred resolution for installed code.

The Rust blocks below are exact quotations from
[`host_guide.rs`](../examples/workflow-graph-roundtrip/src/host_guide.rs), part of the
compiled [workflow round-trip example](../examples/workflow-graph-roundtrip/README.md).
The `host_guide::tests::every_host_guide_snippet_is_compiled` law checks every Rust
fence against its source region, including the imports and function signatures.
Update the compiled source and its quotations together; ignored Rust fences
are rejected. The example's [`runtime.rs`](../examples/workflow-graph-roundtrip/src/runtime.rs)
shows the complete application, component registration and observation feed.

## Prepare the authoring namespace

Enable `codemode` and the dialect support the host needs; this example also uses
`workflow-authoring` and `workflow-view`. Register the kernel process plugin and
any process components before preparing a namespace. The base `ProcessEnv`
selects those components and their configuration.

Mint a `HostArtifactPin` for the saved version before publishing anything. Its
owner must release it on a failed save, or when the version is no longer needed
for starts. Hold the namespace publications for as long as the saved version
needs them. A released pin cannot be reused.

Choose a `PublicationContext` with the backend's binding identity as its domain,
`PublicationScope::Host`, the host's stable publisher instance and its owned root.
Supply an `Attachment` listing the packages, individual members and deferred
paths the workflow may import. These are host choices; do not infer them from
untrusted workflow content. `process_namespace` records and retains the chosen
components' advertisements under the pin. The resulting
`lash::vm::WorkflowEnvironmentRequest` keeps the accepted snapshot, context and
attachment together:

The attachment filters the accepted namespace before dialect lowering. An
unattached, private, out-of-domain or out-of-scope member is unavailable to the
workflow source and cannot be imported. Publication fulfils the selected start's
Imports against that same snapshot, context and attachment, then retains the
resulting immutable Link and its exact package releases. A later advertisement
does not widen or replace that Link. The host chooses the attachment policy;
code cannot grant itself additional packages or operations.

<!-- host-guide: namespace -->
```rust
use std::collections::BTreeMap;

use lash::LashCore;
use lash::dialect::workflow::WorkflowSourceLimits;
use lash::package::{
    Attachment, CallableOrigin, LinkedStart, PublicationContext, RecordedPackages, StartSelector,
};
use lash::process::{
    EngineLaunch, HostArtifactPin, Lifetime, ProcessDefinition, ProcessEnv, ProcessOriginator,
    ProcessStartRequest,
};
use lash::schema::Name;
use lash::vm::WorkflowEnvironmentRequest;
use lash::workflow::document::{Captions, FunctionValue, SourceLanguage, SourceMapId};
use lash::workflow::{SourceMapRead, WorkflowImport, WorkflowRead};

pub async fn namespace(
    core: &LashCore,
    pin: &HostArtifactPin,
    base: &ProcessEnv,
    context: PublicationContext,
    attachment: Attachment,
) -> lash::Result<(RecordedPackages, WorkflowEnvironmentRequest)> {
    let packages = core
        .host_artifacts()
        .process_namespace(pin, base, context.clone())
        .await?;
    let request = WorkflowEnvironmentRequest {
        snapshot: packages.snapshot.clone(),
        context,
        attachment,
    };
    Ok((packages, request))
}
```

`host_artifacts().workflow_environment(&request)` returns an optional
`WorkflowEnvironment` for authoring and checking. It exposes `namespaces()`,
`functions()` and `checker()`. An absent environment means the core has no
workflow-document engine. The function registry includes the build's helpers;
package Imports are fulfilled against the explicitly accepted namespace.

## Import, publish, link and start

`import_workflow` lowers source and returns `WorkflowImport::Imported` with
opaque `DocumentBytes`, the exact environment request, source map, source and
captions. Import publishes nothing. Handle `WorkflowImport::Refused` using its
typed diagnostics and original byte spans; `Unsupported` distinguishes unavailable
engines and dialects. The compact example below reports a failed import to its
caller; a source editor should present the returned diagnostics.

`publish_workflow` accepts those bytes, a selector, the environment request,
optional map-and-source and optional source limits. It returns `LinkedStart`.
A function Export needs a complete, data-only argument and settled-result
signature. To publish a kernel-authored document, construct `DocumentBytes` from
its typed `Document` and omit the source pair.

`link_process_env` derives the operations performed by the selected start and
selects their routes from the recorded packages. If several providers can serve
an `OperationUse`, pass an explicit selection keyed by that identity. Code that
only passes a child callable does not thereby perform the child's operations.
The linked environment retains the execution's exact publications and authored
Links; recovery uses those facts.

Publish a process definition from the linked start, then publish the linked
process environment and pass its reference and engine settings in `EngineLaunch`.
A stable host start key makes an exact retry return the same process. Different
content, arguments or launch under the same key is a `StartKeyConflict`.
Registration hands execution and recovery to the engine. Hosts submit session
turns through `send()`; they do not drive a turn themselves.

<!-- host-guide: publish -->
```rust
pub struct SavedWorkflow {
    pub start: LinkedStart,
    pub definition: ProcessDefinition,
    pub environment: ProcessEnv,
    pub source_map: SourceMapId,
    pub captions: Captions,
}

pub async fn publish(
    core: &LashCore,
    pin: &HostArtifactPin,
    base: &ProcessEnv,
    namespace: &(RecordedPackages, WorkflowEnvironmentRequest),
    source: &str,
    entry: Name,
    limits: Option<WorkflowSourceLimits>,
) -> anyhow::Result<SavedWorkflow> {
    let artifacts = core.host_artifacts();
    let WorkflowImport::Imported {
        environment,
        document,
        source_map,
        source,
        captions,
    } = artifacts
        .import_workflow(SourceLanguage::TypeScript, source, &namespace.1, limits)
        .await?
    else {
        anyhow::bail!("source was refused or its dialect is unavailable");
    };
    let start = artifacts
        .publish_workflow(
            pin,
            &document,
            &StartSelector::Export(entry),
            &environment,
            Some((&source_map, &source)),
            limits,
        )
        .await?;
    // Supply selections by OperationUse here if providers are ambiguous.
    let environment = artifacts
        .link_process_env(base, &start, &namespace.0, &BTreeMap::new())
        .await?;
    let draft = lash::vm::definition_draft(&lash::process::KernelProcessDefinition {
        code: lash::vm::KernelInlineRequest::Linked {
            start: start.clone(),
        },
    })?;
    let definition = artifacts.publish_definition(pin, &draft).await?;
    Ok(SavedWorkflow {
        start,
        definition,
        environment,
        source_map: source_map.identity(),
        captions,
    })
}

pub async fn start(
    core: &LashCore,
    pin: &HostArtifactPin,
    saved: &SavedWorkflow,
    key: &str,
    args: serde_json::Map<String, serde_json::Value>,
) -> lash::Result<lash::persistence::ProcessStartReceipt> {
    let env = core
        .host_artifacts()
        .publish_process_env(pin, &saved.environment)
        .await?;
    let settings = core
        .processes()
        .kernel_engine_defaults()?
        .into_engine_settings();
    let request = ProcessStartRequest::definition(
        saved.definition.id.clone(),
        Some(saved.definition.signature.clone()),
        args,
        EngineLaunch::new(env, Some(settings)),
        ProcessOriginator::host(),
        Lifetime::Detached,
    )
    .with_host_start_key(key);
    core.processes().start(request, core.effect_host()).await
}
```

A definition is immutable and content addressed. Editing creates a new version;
a process already started retains what it was admitted under. An id alone holds
nothing and proves no host-level permission. Keep the version's pin while it
may be started; a started process holds its own executable closure.

Keep readable source or JSON if a document must be reconstructed across
unsupported grammar changes. `DocumentBytes` is an opaque transport, not a public
long-term storage format. Store its grammar tag, `DocumentId` and body verbatim;
`DocumentBytes::from_parts` reassembles them and admission validates them. Use
`to_document()` when an editor needs a typed tree. Receipt checks the grammar, canonical tokens, decoding bounds and supplied
identity before semantic admission.

## Read the code and its sidecars

`definition_graph(&definition_id)` and `core.processes().graph(&process_id)`
return `WorkflowRead`. `Inspected` contains a `WorkflowDocument`; `Unavailable`
retains the typed reason the read cannot be made, and `Unsupported` identifies
an engine without workflow documents.

A `WorkflowDocument` exposes `document()`, `identity()`, `reference()`,
`selector()`, `entry_unit()`, `entry_signature()` and `graph()`. Its reference
is the `LinkedStart`, so the selector and defining Link travel with the code.
`host_artifacts().execution_document(&start)` reads that reference directly as
`WorkflowDocumentRead::Read`, `Unavailable` or `Unsupported`.

Enable `workflow-view` to derive a `WorkflowView`. Load `ViewContext` with
`workflow_view_context` from the same start and its recorded publications. It
retrieves the exact Link graph and package display sidecars, and uses the build's
helper display facts. Never substitute the current advertised namespace for a
retained execution's provenance.

The host keeps the `SourceMapId` from publication. `read_source_map` returns
`SourceMapRead::Read { map, source }` or a typed unavailability; reading does not
acquire a lasting pin. Keep captions beside the saved version and join them with
`view.captions`:

<!-- host-guide: read -->
```rust
pub async fn read(
    core: &LashCore,
    saved: &SavedWorkflow,
) -> anyhow::Result<(lash::workflow::view::WorkflowView, SourceMapRead)> {
    let artifacts = core.host_artifacts();
    let WorkflowRead::Inspected(inspection) =
        artifacts.definition_graph(&saved.definition.id).await?
    else {
        anyhow::bail!("definition is unavailable or its engine has no document");
    };
    let context = artifacts
        .workflow_view_context(&saved.start, &saved.environment.execution().publications)
        .await?;
    let view = lash::workflow::view::derive(&inspection.document, &context)?;
    let _caption_view = view.captions(&inspection.document, &saved.captions)?;
    let source = artifacts.read_source_map(&saved.source_map).await?;
    Ok((view, source))
}
```

### What is derived

The view derives statements, scopes, dataflow, callables and execution sites.
Draw a statement as one node with its actions as parts. An action's `role`
identifies an effect, wait, sub-workflow, library call or task operation; use it
instead of matching helper names. `applies` identifies callable bodies the view
can derive. An empty list does not prove that a dynamically supplied callable
will do no work.

The entry's declared parameter and result types come from its signature;
`result_facet` describes the returns the view can derive. A control header's
optional `comparison` is derived evidence, including package display
declarations. If it is absent, show the expression instead of guessing.
`lash::workflow::view::TYPESCRIPT_DECLARATION` exposes the checked-in view wire
types for hosts that vendor them. Host wording, icons and layout remain host data.

## Save a callable received by a tool

A tool accepts a function only where its input schema marks
`{"x-lash": {"kind": "function"}}`, optionally with a `signature`. The marker can
appear inside a composed schema or a declared object field. A function in an
ordinary data slot is refused as `not_data` with its value path. The transport
wraps a typed `FunctionValue` as `{"$lash_function": <FunctionValue>}`; the slot
schema is the marker, not a hand-written schema for the payload.

`FunctionValue` carries `callable` (the defining Link and function identity),
`captures`, `signature`, `convention`, `representation`, `captions`, `creation`
and `token`. It also retains optional `signature_origin` for diagnostics and
`implicit_text_conversions` for saved-workflow admission. Captured callables keep
their own defining Links. Pass the complete value to Lash rather than extracting
or rebuilding a document from it.

At the receiving tool attempt, obtain `CallableOrigin` from
`AttemptContext::callable_origin()`. Preserve this origin with the value if
publication is deferred, and retain the resources it names while needed. It
records the accepted publications and authored Links; a callable hash alone is
not execution provenance. `publish_callable` validates and retains that origin
and the captured callable under the pin, checks the host's source policy and
returns a process definition:

<!-- host-guide: callable -->
```rust
pub async fn save_callable(
    core: &LashCore,
    pin: &HostArtifactPin,
    function: &FunctionValue,
    environment: &WorkflowEnvironmentRequest,
    attempt: &lash::tools::AttemptContext<'_>,
    limits: Option<WorkflowSourceLimits>,
) -> anyhow::Result<ProcessDefinition> {
    let origin: CallableOrigin = attempt.callable_origin()?;
    Ok(core
        .host_artifacts()
        .publish_callable(pin, function, environment, &origin, limits)
        .await?)
}
```

To run already-linked code inline within an attempt, use
`AttemptContext::retained_inline_request` and the normal process-start APIs.
For a saved definition, derive its selected start through `definition_graph`,
link its process environment from the retained packages, and start it as above.
Newly uploaded unlinked source instead needs its explicit namespace and attachment
for fulfilment. Display captions stay separate from executable identity.

## Implicit text conversion policy

`lash::dialect::workflow::WorkflowSourceLimits::implicit_text_conversion`
selects `ImplicitTextConversionPolicy`. Every saved-workflow path accepts
`Option<WorkflowSourceLimits>`: `import_workflow`, `import_exported_workflow`,
`publish_callable`, and source-backed `publish_workflow`. The default when limits
are `None` is **`Refuse`**. Pass the same policy on every path that creates or
replaces a saved version. `Some(limits)` also sets source byte and nesting limits
where source is read.

| Policy | Implicit text conversion in template interpolation and string `+` |
| --- | --- |
| `Refuse` (default) | Reject known records, collections, functions and package objects; admit unconstrained operands. |
| `Allow` | Admit every implicit conversion. |
| `Strict` | Also reject unconstrained operands. |

Explicit `String(x)` and `JSON.stringify(x)` are available under every policy.
Refusals carry `TS_IMPLICIT_TEXT_CONVERSION`, the operand's original byte span
and `operand_type`; Lash supplies no repair hints, so the host owns any guidance
shown with them. Text, numbers, booleans, nullish values and selected primitive
fields pass even under `Strict`.

Cell execution keeps JavaScript semantics. Its `Written` metadata retains
conversion facts, which saved and crossing callables carry. `publish_callable`
checks those facts, including captured callables, without reconstructing source.
A callable without retained TypeScript authoring facts has no facts for this
source policy to check.

Direct dialect callers select the same policy with
`SourceLimits::with_implicit_text_conversion` before `lower_with_limits`.

`import_exported_workflow` checks edited source before returning
`WorkflowImport::Read`; pass the policy there whenever edited printed source replaces a saved
version. `publish_workflow` checks supplied map-and-source text. A raw
source-less `DocumentBytes` publication is kernel-authored: kernel conversions
are explicit, so this TypeScript authoring policy does not apply. Printing
applies no admission policy. Generated-source limits and ordinary source-byte
limits are independent bounds. The in-process dialect readers and
`WorkerService::read_exported` take the same optional `WorkflowSourceLimits`
after generated-source limits; their generated-source byte bound is independent
of the ordinary cell source-byte bound.

## Helper availability on restore

The default restore policy requires **every helper needed by live code to exist
by exact identity** in the receiving build. Helpers are compiled into the dialect
binary; retaining package publications cannot replace a missing binary helper.

A checkpoint records its helper-library identity. If it matches the receiving
build, compatibility requires no live-helper walk. When the identity differs,
Lash checks the exact helper identities retained by live state, including their
needed closure. Additional unrelated helpers in the receiving build are harmless.
An absent required helper refuses restore as
`KernelRestoreRefusal::MissingHelper { function }`; parked-state carry exposes
`lash::vm::KernelParkedRefusal::MissingHelper { function }`. There is no automatic replacement
by name, relinking to a newer helper or permissive default. Keep a capable build
available for retained work. Surface the typed refusal and its function identity.

## Edit documents and retain identity

With `workflow-authoring`, open `lash::workflow::edit::Draft` on the typed
Document. `Draft::apply` takes a `Transaction` and checker environment. Every
operation carries the base `DocumentId` and exact `Site` in that base. Supported
canvas operations are `SetValue`, `InsertStep`, `RemoveStep`, `MoveStep`,
`WrapSteps`, `UnwrapSteps` and `ChangeCondition` in `lash::workflow::authoring`.
Use source replacement for arbitrary declarations, handlers or terminal structure.

A transaction applies atomically after checker admission or changes neither the
draft nor its correspondence. Structural operations require separable effect
steps. `EditDiagnosticKind::NonSeparableStep` supplies optional
`enclosing_statement` and `separable_step` base sites, so a host can offer an
appropriate source or structural edit. A statement with several effects is not
one separable effect step. Wrapping supports if/for/try; while loops support
condition changes.

A Site names a unit and a path in `Node::children` order. Its root omits the
serialized `path`; children have a nonempty path. It is an address within one
Document, so an insertion can move the sites after it. Use the returned
`Correspondence` to carry layout, comments and selection to the new Document.
`successor`, `predecessor` and `then` follow and compose surviving nodes;
`Draft::correspondence_since_open` spans all applied transactions. A wholesale
replacement has no edit history. Editing never moves a parked execution.

Use `extract_entry` with the checker to obtain an admitted document for a
selected entry. A missing entry is a typed `NoSuchEntry`. Keep the returned
document and its correspondence where provided; publish the resulting bytes
against an explicit workflow environment.

## Captions and source editing

A caption is a step's plain-language title and optional description:
`lash::workflow::document::Caption`. `Captions` holds a document's captions
by target, keyed to its `DocumentId` like a source map but separate from it, and
serialises as the `kernel-captions` host schema. Your host holds it; lash
stores none, and no edit, draft, admission, checkpoint or identity reads it
(`K-CAP-001`).

A caption names a `CaptionTarget`: a site, a parameter or a field.
`CaptionKind::captionable(&document)` lists every target a document has.

- **A site:** a statement, a clause arm (an `if`'s else block, a `try`'s
  catch or finally block, a loop's non-empty else block) or a declared
  function's body. A then-arm or loop body is named by its statement, and
  the entry function's caption is the workflow's title.
- **A parameter** (`ParamTarget { function, param }`): a declared function's
  parameter, by name. An entry's parameters are its signature's, the names a
  start passes its arguments by.
- **A field** (`FieldTarget { function, path }`): a named field of a record
  type an entry's parameter or result has. `FieldPath { root, fields }`
  starts at a parameter (`FieldRoot::Param(name)`) or the result
  (`FieldRoot::Result`) and names one record field a step, so
  `event.answer.choice` is root `event`, fields `["answer", "choice"]`. A
  list is walked to its element and an optional value (a union whose
  members other than null and absent are one type) to that type, so
  `items.title` names the title of each record in a list `items`. A map, a
  set, a tuple, a record's rest and a union of two records are not walked.

Use parameter and field captions to say in plain words what a workflow takes
and what it gives back. In JSON an entry names its target by exactly one of
`site`, `param` and `field`:

```json
{ "param": { "function": "triage", "param": "event" },
  "caption": { "title": "The card that was answered" } }
{ "field": { "function": "triage",
             "path": { "root": { "param": "event" }, "fields": ["choice"] } },
  "caption": { "title": "What the reviewer picked" } }
```

Build a value with `Caption::new(title, description)` and
`Captions::new(&document, entries)`. Both refuse text outside the one domain
every dialect can spell (`CaptionRefusal`: one trimmed title line of at most
200 bytes, at most 2 KiB of description, no `*/`, no `@` line, no control
characters or line separators other than `\n`). `Captions::new` also refuses
a site that is not captionable, a parameter the function does not have and a
path its types do not resolve (`CaptionsRefusal`). There is no edit operation
for a caption: to change one, build a new `Captions` and keep it.
`captions.get(&target)` reads one caption, and `captions.at(&site)` a site's.

**Authority.** The sidecar you hold is authoritative. Importing a source
(`WorkflowImport::Imported { captions, .. }`) or reading back printed source
(`WorkflowImport::Read { captions, .. }`) answers the captions that source
spells; replace the sidecar with them wholesale. Once a version has any
change that did not come from its source, a canvas edit or a caption change,
show `print(document, Some(&captions), options)` in your source editor rather
than the stored authored source, so the next save cannot revert captions.
Captions a dialect cannot spell are listed, by target, in
`PrintedSource::dropped` (`K-CAP-006`): the source read back will not have
them, so tell the author before a source edit replaces them. A printer drops
an entry's parameter and field captions when the printed function takes other
parameters than its signature names (a TypeScript entry that reads `this` or
`arguments`), and Python drops a field caption below a record with a field name no
`TypedDict` class body can declare.

**Through edits.** After an applied transaction,
`captions.rekey(&applied.correspondence, draft.document())` answers
`Rekeyed { captions, touched }`: the captions whose step survived, at their
new sites, and the sites of those whose step, or anything beneath it, the
edit wrote or made new (`K-CAP-004`). Mark the `touched` captions for review:
"Email the team" may no longer describe a step whose recipient changed. A
parameter's or field's caption is keyed by name, not position, so moving
statements never moves it. It survives while its function still has the
parameter, or still resolves the path, and is dropped once the parameter or
field is gone. It is never `touched` (`K-CAP-007`).

**Showing them.** `view.captions(&workflow_document, &captions)` joins them
with a `WorkflowView` by view location, as the `kernel-caption-view` schema.
`workflow_document` is the `WorkflowDocument` the view was derived from: its
entry's signature names and orders the parameters.

- An action or control node shows its statement's caption. A statement
  with several actions shows it on its first outermost action, the one it
  performs or awaits. A helper nested in that action's arguments gets none.
- A code node shows every caption among its `members` and their arms.
- A control shows its `arms`' captions, by `ControlArm.block`.
- A callable shows its function's caption, on `CallableView.block`.
- `params` lists the entry's captioned parameters as
  `ParamCaption { index, name, caption }`, in signature order.
- `fields` lists the captioned fields of its parameter and result types as
  `FieldCaption { path, caption }`, by path.
- `bindings` lists each `let` of the entry whose declaring statement has a
  caption as `BindingCaption { binding, name, site, caption }`, by binding
  id: that caption names the value the variable holds.

A Start/End panel reads `params` and `fields` and shows any parameter
without a caption by its name. The view itself is unchanged by captions, so
a caption-only change keeps its identity. Wording for a step without a
caption is yours.

**Naming values.** A binding has no caption syntax of its own: the caption
a source writes on a declaration is the caption of the statement that
declares the variable, so `/** The open orders */ const orders = …` and
`#: The open orders` above `orders = …` name `orders`, and printing and
reading back keep it there (`K-CAP-002`). For the value a variable holds,
read its `BindingCaption`. For one that holds a tool's reply, read
`BindingView.produced_by` too: the site of the `perform` it awaited, in
`dataflow.actions`, so "what Fetch the orders returned" can name the step
by its node's caption. It is set for TypeScript's
`const r = await tools.x(…)`, Python's `r = await tools.x(…)` for an
operation `tools.x` (through its
reply helper, `py.reply`) and an `await` of a promise or task that only
performs the call, for a `let` nothing reassigns. A value computed from a
reply, such as `(await inbox.lookup()).urgent`, has none: it comes from the code.
`produced_by` is derived from the document; nothing stores it. A condition
operand that is the `result` of an action joins to the binding whose
`value_site` is that action's site.

Lowered temporaries the source never named stay unnamed: lash makes up no
plain name for a variable a lowering declared (`t28`, `v2`), so show such a
value by what produced it or as "a value from the code", never by its
kernel name. A temporary declared inside an expression never has a
caption. When a lowering splits one source declaration over several
statements, as TypeScript does for a field read of an awaited value or a
destructuring, the declaration's caption stays the step's, on the statement
that holds its first action (`K-CAP-003`): the temporary that statement
declares lists it, and the source variable has no `BindingCaption`.

**Telling a model.** Each dialect declares the caption kinds it spells and a
short guide in its own syntax (`lash::typescript::CAPTIONS`,
`lash::python::CAPTIONS`, a `CaptionSupport`). Put the `guide` in a model's
prompt rather than hard-coding a syntax. Both dialects spell every kind.

- **TypeScript** uses a JSDoc block, `/** Send the weekly report */`, right
  before the statement, function or `else`/`catch`/`finally` keyword it
  names. Put one before an entry's parameter to caption it, and before a
  property of an object type written inline in a parameter's or the
  result's (`Promise<{ ... }>`) annotation to caption that field. A named
  `type` or `interface` is not resolved into an entry's signature, so its
  properties take no caption. `@param` tags are not captions.
- **Python** uses `#:` lines directly above what they caption, at its
  indentation: `#: Send the weekly report`. Write an entry's parameters one
  a line to caption them. To caption fields, annotate a parameter or the
  result with a `TypedDict` class the module declares and put `#:` lines
  above its fields. Field annotations may use `NotRequired[...]`.

Source printing is a lens over the Document. Use the worker service's `print`
with the Document, captions, source format and print options. A printer can
return typed `Unprintable` for code the chosen dialect cannot spell; the
Document remains inspectable and executable. Read edited printed source with
`import_exported_workflow` against the base Document and the same host source
policy.

Once a canvas or caption edit changes a version, show freshly printed source so
saving old authored source cannot undo those edits. Draft storage, revision
naming, undo, collaboration and permissions belong to the host.

## Showing a run

Follow one recovering observation feed per process through
`core.processes().observe(&process_id)`. Its snapshot's retained read view holds
the process's `ProcessWorkflowOverlay`, combining committed facts and live
provisional evidence. Subscribe from the snapshot cursor and recover gaps as
[Observing processes](observing-processes.md) describes. The example's runtime
shows this flow, including terminal settlement and replay.

`ProcessWorkflowOverlayAccumulator` folds the process feed; the document-level
`WorkflowExecutionOverlayAccumulator` checks execution facts against a document.
Committed events alone do not establish live coverage. Use the process feed to
show live state, retain missing-evidence distinctions, and let committed facts
settle provisional observations. `RunObservation` in `lash::process` selects the
recorded detail through engine settings; it is fixed when the start is admitted.

Draw action states from the overlay, statement and scope counts from
`workflow::view::coverage`, and wording and nesting from the static view.
Omitted occurrences have no individual outcome evidence; absence is not success.
Recorded paths can be read through process replay and `workflow::view::path`.
A run's evidence always belongs to the Document and defining Links it ran. After
an edit, a new run reports against the new Document; compare revisions only
through an explicit correspondence.

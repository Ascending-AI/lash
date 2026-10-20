# Host packages

A package gives programs a namespace containing operations and code. An
operation is a host effect served by a runner. Code is a set of
content-addressed kernel functions, with exported functions, sealed value
types and errors. A package can contain either or both. An operation-only
package describes the effects a tool provider or MCP server serves; lash's own libraries are
code-only packages installed through the same path.

The host chooses the installed packages. Programs use their paths directly,
without imports. A package declares what an operation means; the host chooses
the runner that implements it. The same program can therefore run against a
production service or a test fake serving the same contract.

[Plugin authoring](plugins.md) covers registering a runner with session
plugins. [Workflow hosting](workflow-hosts.md) covers importing, publishing,
editing and observing the documents that call packages.

## A dataframe pipeline in two dialects

The [dataframe fixture](../crates/lash-kernel-package/src/testing/dataframe.surface)
supplies `col`, `lit`, `count`, expression values and `DataQuanta` pipeline
values. A static `DataQuanta.readText` / `read_text` call starts a plan.
Collecting rows performs `dataframe.run`; explaining it performs
`dataframe.explain`. The fixture declares `run` public and `explain`
internal. Its fake runner supplies deterministic results for both.

The TypeScript workflow is:

```ts
export async function wordcount(text: string) {
  const { col, lit, count } = dataframe;
  const viaExprs = dataframe.DataQuanta.readText(text)
    .flatMap((line: string) => line.toLowerCase().split(/\W+/), { output: "utf8" })
    .filter(col("value").neq(lit("")))
    .reduceByKey([col("value")], [count().alias("n")])
    .sort(col("n").sort({ ascending: false }), col("value"))
    .limit(5);
  const lines: string[] = [];
  for (const [word, n] of (await viaExprs.collectRows()) as [string, number][]) lines.push(`${word} ${n}`);
  lines.push(String(await viaExprs.explain()));
  return lines;
}
```

The Python workflow is:

```python
async def wordcount(text: str):
    col, lit, count = dataframe.col, dataframe.lit, dataframe.count
    via = (dataframe.DataQuanta.read_text(text).flat_map(lambda l: l.lower().split(), output="utf8")
           .filter(col("value").neq(lit(""))).reduce_by_key([col("value")], [count().alias("n")])
           .sort(col("n").sort(ascending=False), col("value")).limit(5))
    return [f"{w} {n}" for w, n in await via.collect_rows()] + [str(await via.explain())]
```

These are the programs tested by the TypeScript and Python acceptance
laws. Their lowered goldens are
[TypeScript](../crates/lash-dialect-typescript/src/tests/wordcount.kernel) and
[Python](../crates/lash-dialect-python/src/tests/wordcount.kernel).

Both use canonical definitions such as
`dataframe.data_quanta.reduce_by_key` and
`dataframe.data_quanta.collect_rows`. Surface names are canonical snake_case
segments. TypeScript spells methods in camelCase and types in PascalCase;
Python keeps snake_case methods and PascalCase types. Thus the surface type
`data_quanta` is `dataframe.DataQuanta` in both dialects. The journal and host
views use canonical names regardless of the source dialect.

A canonical segment matches `[a-z][a-z0-9]*(_[a-z][a-z0-9]*)*`. A package path
is one or more such segments joined by dots, and an operation is that path
plus one export segment. For example, `inbox.send_item` is called as
`inbox.sendItem(...)` in TypeScript and `inbox.send_item(...)` in Python.
Language-library definition names follow the language they implement:
`ts.array.indexOf` keeps its spelling.

Programs can bind code members to short names, as both examples do. A whole
namespace is not a value: `const df = dataframe` is refused. Operations
cannot be taken as function values. Ordinary lexical bindings shadow package
roots; a dotted call that would shadow an offered operation is refused with
the binding and operation named, so the author can rename the binding.

## Declare the package

A `Package` has a path, a human-readable release label, exact dependency
artifacts, definitions and an exported surface. The release label helps a
host explain a change; the artifact digest identifies the artifact. Display
labels and comparison descriptions stay outside execution identity.

Author definitions in a `.kernel` source, the surface in a `.surface` source
and display labels in a `.labels.tsv` source. The surface has one `exports`
map, keyed by canonical segment. Each entry is one `Export` variant:

| Export | What the author declares |
| --- | --- |
| `Operation` | An `OperationRequirement` containing the kernel signature and `ContractId`, declared intents and public or internal visibility. |
| `Function` | A `Member` naming its definition, signature and whether it is asynchronous. |
| `Type` | A `ValueType` with its representation and a member map containing statics, methods and properties. Each member names its definition. |
| `Error` | The error name; its kernel error kind is derived from the package path and export, such as `dataframe.execution_error`. |

For `dataframe`, the code surface includes `col`, `lit`, `count`,
`data_quanta.read_text`, the `data_quanta` pipeline methods
and the expression members. `data_quanta.sort` and `expr.sort` are separate
members with their own signatures. A property such as `expr.name` names a
getter function. An exported error is spelled `dataframe.ExecutionError`
in the two source dialects.

The calling convention has fixed slots. A method's receiver is first;
optional slots include `Absent`, rest arguments occupy one list slot and
options occupy one record slot. Python's keyword spelling fills an options
record only when the mapping is unambiguous. A callback slot declares a
function type. The dataframe value keeps the user's `flat_map` callback as
given; a closure created by a library body cannot be saved or cross an
operation boundary.

A body-only definition can perform its package's declared operations. Its
body records each operation's signature and contract id; both contribute to
the function's identity. A native is pure, and a body paired with a native
cannot wait or perform. Declare a member asynchronous when its body can
wait. Package code never selects a runner.

`Package::build` validates and freezes the artifact. It refuses missing
member definitions, signature mismatches, duplicate exports or members,
noncanonical surface names, undeclared operations and synchronous members
that can wait. A host package's definitions must live under its own path.
Lash's packages declare their definition roots, and host packages cannot
claim those roots. Two packages may contribute disjoint children beneath a
namespace prefix; an owned export path cannot also be another namespace.

### Contract meaning and visibility

Choose a declared contract id for each package operation and change it when
its meaning changes, even if the signature does not. An operation's declared
intents must include the runtime work it can request, such as starting a
process. Execution and retry policy are chosen by the runner and captured
on the operation's link.

The surface spells those declarations explicitly. For example, the
fixture contains:

```text
operation run(plan: List(Record{step: Text, exprs: List(Json), callback?: Fn(value: Any) -> Any, n?: Int})) -> List(Any) contract "dataframe.run/1"
internal operation explain(plan: List(Record{step: Text, exprs: List(Json), callback?: Fn(value: Any) -> Any, n?: Int})) -> Text contract "dataframe.explain/1"
```

Declare an operation's parameters with the types its runner can serve. A
runner's tool takes its input as JSON, so a `Json` slot is plain schema
and a function slot is marked `{"x-lash": {"kind": "function"}}`. The
fixture's runner states the plan as an array of objects whose `callback`
property is such a slot, which serves `run`'s parameter. An `Any` slot, or
a package value type, has no JSON schema that serves it.

The operation requirement is part of a performing body's identity: the
code pins each operation's contract id beside its signature. A runner
serves a contract through `ToolManifest.contract`, a
`lash::tools::ContractSource`. `ContractSource::Declared { id }` states the
id; build it with `ToolDraft::declared_contract`. `ContractSource::Derived
{ tag }` derives the id from the input and output schemas, output contract,
declared intents and the host's `HostTag`, and from nothing else; set the
tag with `ToolDraft::host_tag` when two meanings share one set of schemas.
`ToolDefinition::contract_id()` computes the result; it is never stored.

On a package-owned path, a runner must declare the package's contract id
and intents and serve the operation's signature. Otherwise a process whose
code performs the operation is refused when its environment is checked
against it, with the `runner_not_packaged` refusal naming the tool and the
package, and the cell's surface setup refuses the runner the same way. A
program performs an owned operation under the package's declared
requirement, whether it calls the operation directly or through a member
body, so both state one requirement. A tool advertised under another
contract than the package declares, or at the segment of one of the
package's functions, types or errors, is refused when the namespaces are
composed, naming both; a tool never shadows package code.

An `internal` operation is callable only from bodies installed by its declaring
package. It is absent from Standard catalogs, discovery and prompts. A direct
perform from document code is refused with its unit; deferred and dynamic
attempts are refused before any operation link is recorded.

Installed package namespaces are closed to deferred providers. Their surface
links public members and operations, refuses internal operations, and reports
unknown paths as “no such member.” Neither cell discovery nor dynamic dispatch
sends a package path to a deferred provider. Computed strings cannot call package
operations, including public ones. Paths outside installed package namespaces
continue to resolve through the recorded deferred provider. The selected cell or
definition surface supplies this decision; a provider cannot override it.
At process start its linked package roots are stored in `ProcessEnv`.
A granted tool’s dynamic dispatch reads those roots on every activation, including resume,
without reconstructing a package view from the deployment's current set.

## Write and replace a runner

A runner implements `ToolProvider`. Its `tool_manifests` and
`resolve_contract` describe the operations it serves, `prepare_tool_call`
prepares a call and `execute` returns its outcome. Register the provider
through the host's session plugins or process components. The
[compiled plugin specimens](../crates/lash/examples/plugin_authoring.rs)
show the provider registration path; the
[plugin guide](plugins.md) describes configuration and certification.

For durable dataframe execution, configure `dataframe.run` as recorded
`Repeatable` work and make repeats safe using the stable `ToolCallId`.
A read may execute again after a crash; use it only where recomputation is
acceptable. The acceptance laws run both operations through a fake host;
they do not certify a production provider's retry behavior.

Replace a production runner with a fake by registering the fake to serve
the same contracts. Each `lash::tools::OperationLink` records the tool,
provider, execution policy, argument projection and the runner's codec
payload, and whether the tool was advertised, resolved through a deferred
match or reached dynamically. It carries no contract: contract identity
stays in the code's requirement. A replacement for already-linked work must
remain resolvable under the recorded tool and provider keys and must read
any retained codec payload. One judge decides every pinned call. When the
live definition's contract id equals the pinned one, the call runs under
the link's policy. When it differs, the call raises
`operation_contract_changed`, which the program may catch, and no runner
body runs. A missing tool parks. A description, transport, server-version
or retry-bound change is not a contract change. A committed outcome is
consumed without executing the runner again. An MCP tool derives its
contract from its durable tool id and a digest of its schemas, so a
redeployed server that keeps the contract receives an admitted call again,
`Once` included. The [plugin guide](plugins.md) covers provider readiness,
configuration and certification.

## Install once, choose a complete set

Build a package against the exact artifacts it requires. `PackageSources`
borrows its definition, surface and label text; `Package::build` takes those
sources by reference and an array of dependency references. Given your
three source strings, the held kernel artifact `kernel: &Package`, and the
built-in artifacts `builtins: Vec<Package>`, assembly is:

```rust
use std::sync::Arc;
use lash::package::{HeldPackages, Package, PackageSources};

let sources = PackageSources {
    kernel: kernel_source,
    surface: surface_source,
    labels: label_source,
};
let dataframe = Package::build("dataframe", "0.3.0", &sources, &[kernel])?;
let held = Arc::new(HeldPackages::install(builtins.into_iter().chain([dataframe]))?);
let workers = workers.with_packages(Arc::clone(&held));
let protocol = rlm_factory.with_packages(held);
```

Here `workers` is a `lash::vm::WorkerService` and `rlm_factory` is a
`lash::rlm::RlmProtocolPluginFactory` already configured for your protocol.
`RlmProtocolPluginFactory::with_packages` gives its executor and pools the same
holdings. A standalone process host uses
`KernelProcessPluginFactory::with_packages` with that same `Arc`.
Select packages before starting or cloning the worker service.
`lash::package::lash_packages()` supplies the built-in holdings; its
`in_order()` iterates dependency-first artifacts for your assembly.
`HeldPackages::install` assembles an own set in memory and takes no store
argument. Use `HeldPackages::new` when reconstructing own and retained sets
from stored rows.

A node installs its sets into the store when it starts. For each set, the
store keeps its surface snapshot under the `PackageSetDigest`: every
package's exports (errors, types, member conventions and operation
declarations included), roots, origin, dependency closure and definition
identities, helpers included. With the snapshot it keeps what running the
set requires: its native-only functions and kernel versions. Every
definition is stored as a kind-14 node under a host pin for the set. The
definitions are written before the set's row, so any set a node can read
already has all its code stored. The store also indexes which packages
declare each definition and each internal operation. Release labels are
kept per artifact and appended on each install, so a second release with
identical content keeps both labels. Install is the only writer of
snapshots, their definitions, labels and the index, and none of them is
ever deleted. So a node that does not hold a set's artifacts still reads
its whole surface and code from the store after the installer has
stopped.

Each serving node advertises its capabilities for as long as it is live:
its native implementations, kernel versions, registered provider keys,
readable codec versions and its own set's digest. It advertises no
credentials or grants.

Prefer body-backed functions for portable packages: any node can run them
from the stored definitions. A native-only function runs only on a node
whose build supplies its implementation. For deploys it behaves like a
tool runner: a set that adds one becomes writable only once every live
node has the native, and rolling back to a build without it can strand
runs that need it.

Lash's `kernel`, `typescript`, `python`, `regex.ecma`, `date.ecma` and
`url.whatwg` libraries use this same install path. They export no operations.
The language packages have no user surface: dialect lowering calls their
definitions directly. Hosts still choose which dialects their workers build
and serve.

`HeldPackages` contains the node's own and retained artifacts. The parent
builds the declaration registry from them, and every worker receives its
registry from the parent. A single-binary worker supplies native
implementations by `FunctionId`; it takes no second package list. A shipped
worker receives the definitions in one release-image frame at pool start.
Its registry stays fixed for its lifetime. Pool start refuses a native-only
function whose implementation is missing; a function with both a body and
an optional native can run its body.

An `ArtifactKey` is a package path and artifact digest. A `PackageSet` has one
artifact digest per package path and must be dependency-closed: every
artifact's exact requirements must name the set's own dependency artifacts.
If `dataframe` depends on a new kernel artifact, rebuild `dataframe` against
it even when its source and release label are unchanged. The result is a new
artifact digest. Selecting a newer dependency independently would let one
cell execute two artifacts of the same package.

`PackageSetDigest` identifies the complete set. The fleet gate selects a
set every live serving node holds, considering the node's own set first,
then retained sets in store install order, newest first. The result is the
cell's `CellPackages`, passed to lowering, namespace resolution, discovery
and workflow import. Each reader takes that set's surface snapshot: the
node's own when it holds the set, else the stored one. A node's own set is only a gate
candidate. The gate may fall back to an older set during a rollback.

## Parked runs, publication and adoption

A parked run pins exact function identities in its execution closure. The
closure records the definitions the document reaches and which require a
native implementation. Stored definitions remain reachable while their
owners need them. Published workflow documents also keep the identities
they were admitted against; installing a new package does not relink them.

Resume needs those exact identities and any required natives. The pending
and future operations of parked code remain bound to its recorded
requirements. Retain artifacts needed by parked roots, unfinished processes
including ones not started, saved functions and published definitions.
Install retained artifacts before a deployment begins writing new work.
Rolling back to a build that lacks needed identities can strand that work.

Adoption relinks a published document or an unparked process to the
successor's functions of the same names. It refuses a changed operation
requirement with `operation_contract_changed`. Parked adoption currently
refuses `state_not_carried`: no migrator carries its heap pages. That run
stays on its pinned identities. Publishing a replacement document alone
does not adopt an already-running process. The
[workflow host guide](workflow-hosts.md) describes the successor request.

## Sessions use the cell's current set

A new cell lowers against the package set selected for its round. The fleet
gate is asked once per round: the prompt shows that set's packages, and the
round's cell is written under the same set while the node holds it. A gate
that cannot answer yet fails the round's prompt with its typed retryable
refusal, and the turn defers the call. A parked cell keeps its exact
execution closure until it completes. Its envelope records its set, and its
first park on each node stamps the session with it, so only a node holding
that set claims the session while the cell stands parked. The turn's commit
after the cell clears the stamp.

A top-level function binding is saved as a self-contained document when
its cell succeeds, with its captures frozen at cell end. So is every
function a carried binding holds inside data, such as a dataframe plan
retaining its user callback, or a lambda inside a record that another
function captures (`save`, `Saved`). A function no binding holds as its own
value is saved under a generated name, `saved#n` (`Name::saved`), which no
dialect can spell and the prompt never lists, so a later cell declaring a
function of any name never meets it. Every place that held the function
holds a reference to that one saved function, so two bindings that shared a
function or a record still share it in the next cell. At the start of
each later cell, the session relinks every saved function by name to the
cell's package set (`relink_saved`). Each package function it calls takes
the identity that set defines under the same name, even while the node
still holds an earlier release. Each package operation it performs takes
the set's requirement, so a changed contract under an unchanged signature is
taken. A saved function whose callee the set no longer defines, or defines
or offers under another signature, is dropped at the cell's start. The
session lists it as not carried with `Unusable::Relink`, naming the function,
the callee and the `RelinkGap`; one saved under a generated name is named by
the binding path that held it (`plan.map`), and that binding is dropped
with it. The signature a callee must keep is the one the function was saved,
or last relinked, against, which the saved function holds
(`SavedFunction::against`); a callee whose old signature is not known is
refused, never taken unchecked. The session records the set it is linked to
(`linked`); a cell under that set rewrites nothing. Installing a saved
function into a cell checks that every function it lists is the cell's
set's.

Tasks, functions made by library code and functions performing control
operations still prevent saving the binding that reaches them
(`NotSaved::Task`, `NotSaved::MadeByLibrary`,
`NotSaved::PerformsControl`). So does a function held where no reference can
stand: as a map key or set member, behind a ref, or inside an error
(`NotSaved::Keyed`).

A package value crosses to a runner, or into any tool input, as its own
fields: its provenance fields `__lash_brand` and `__lash_built` are left out
of the JSON whatever their encoding, since no runner reads them. A record
without `__lash_brand` keeps every field.

Package values have the brand `(package, type)` and, in `__lash_built`, the
identity of the constructor that built them, which a constructor body writes
with `brand.built()`. A constructor's identity never depends on its
artifact's digest, so an unrelated release re-identifies no constructor and
no code that calls one. Only declared members are accessible. A member
checks its arguments recursively against its own artifact's declared
representation, including package values inside lists, options and plans.
A different brand raises `type_error`. The same brand with an incompatible
shape raises `package.stale_value`, naming the type, first mismatched path,
the constructor that built the value and the check that refused it, with the
repair to rebuild the value. `StaleValue::of` decodes those facts and
`StaleValue::message` finds the held artifacts of the type's package whose
code holds each function and renders their release labels, adding short
digests when labels are equal or unknown. A constructor that several held
releases share unchanged is named by every one of them. The diagnosis
applies in either upgrade direction.

There is no automatic representation conversion. Rebuild a stale dataframe
plan by running its construction code under the cell's selected set. A brand
is not an authority grant, and a matching brand does not bypass validation.

Functions cross into an operation only where its declared type admits them:
a function slot, a union containing one or `Any`. JSON-schema slots are data
unless explicitly marked with `x-lash` function metadata. A stray lambda in
a concrete data slot raises `not_data` with its argument path before a
request reaches the runner.

## Typed facts and storage responsibilities

A host gets the selected package view, canonical operation requirements,
export kinds, signatures, visibility, member paths and display labels.
Workflow views derive the operations an action performs and the package
members its callees implement. Execution overlays attribute work inside
package code to the authored statement that entered it; effect identity
still uses the actual performing site. Hosts choose how to present those
facts and what model-facing guidance to supply.

| Stored fact | Responsibility |
| --- | --- |
| Package artifacts and exact dependency keys | Lash stores artifacts by `(path, digest)`. Each install appends its release labels, and per-line ordinals support listings. Keep artifacts reachable while dependents remain. |
| Package sets | Lash stores each member map, surface snapshot and execution requirements under the set's computed digest, with every definition, and assigns install order. Snapshots and their definitions are never deleted. Node advertisements list held artifacts and the node's capabilities; holding a set is derived from them. |
| Workflow documents and execution closures | Preserve the document's exact identities and its referrer's retention. Parked state remains opaque to the host. |
| Operation links and committed outcomes | Lash records the chosen provider, policy and codec payload before a park. Supply a runner that can read its retained payload. |
| Session bindings and saved functions | Lash retains data and saved function documents with frozen captures, including functions nested in carried data. |
| External runner resources and credentials | The host owns them. Package artifacts contain declarations and code, with no live handles or credentials. |

Hosts submit session work through `send()` and observe the engine's drive.
Installing packages, reading views and preparing a runner do not drive a
turn. External-effect guarantees and the limits of retries follow
[What survives a crash](../README.md#what-survives-a-crash).

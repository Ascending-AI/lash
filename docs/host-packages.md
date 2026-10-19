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
operation run(plan: Any) -> List(Any) contract "dataframe.run/1"
internal operation explain(plan: Any) -> Text contract "dataframe.explain/1"
```

The operation requirement is part of a performing body's identity. The
shared runner contract-id dispatch check is still in progress; current
provider registration uses the runner's schemas and execution declarations.

The `internal` declaration records own-package-only visibility on the
export. End-to-end enforcement across direct calls, discovery, Standard
mode and dynamic resolution is still in progress. Treat it as declaration
metadata until that enforcement lands; it is not an authorization boundary.
Keep host authorization at the runner boundary.

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

Replace a production runner with a fake by registering it under the same
operation paths and compatible schemas. A replacement for already-linked
work must remain resolvable under the recorded tool and provider keys and
read any retained execution-binding payload. Keep the operation's meaning
unchanged. A committed outcome is consumed without executing the runner
again. The [plugin guide](plugins.md) covers provider readiness,
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
and workflow import. Each reader uses
`HeldPackages::view` for that same digest. A node's own set is only a gate
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

A new cell lowers against the package set selected for that cell. A parked
cell keeps its exact execution closure until it completes.

Today a top-level function binding is saved as a self-contained document
when its cell succeeds. Captures freeze at cell end; installation in the
next cell checks that its library identities and operation signatures are
available. An unusable saved function is reported with its typed reason.
Functions nested inside carried data are refused as
`NotSaved::FunctionInData`. In particular, a dataframe plan retaining its
user callback cannot yet carry between cells. Tasks, functions made by
library code and functions performing control operations also prevent
saving. Relinking saved functions to the selected package set and saving
functions nested in data are in progress.

Package values have the brand `(package, type)` and the digest of the
artifact that built them. Only declared members are accessible. A member
checks its arguments recursively against its own artifact's declared
representation, including package values inside lists, options and plans.
A different brand raises `type_error`. The same brand with an incompatible
shape raises `package.stale_value`, naming the type, first mismatched path,
built artifact and current artifact, with the repair to rebuild the value.
`StaleValue::of` decodes those facts and `StaleValue::message` renders the
release labels, adding short digests when labels are equal or unknown. The
diagnosis applies in either upgrade direction.

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
| Package artifacts and exact dependency keys | Lash stores artifacts by `(path, digest)`; release labels and per-line ordinals support listings. Keep artifacts reachable while dependents remain. |
| Package sets | Lash stores each member map under its computed digest and assigns install order. Node advertisements list held artifacts; holding a set is derived from them. |
| Workflow documents and execution closures | Preserve the document's exact identities and its referrer's retention. Parked state remains opaque to the host. |
| Operation links and committed outcomes | Lash records the chosen provider, policy and codec payload before a park. Supply a runner that can read its retained payload. |
| Session bindings and saved functions | Lash retains data and saved function documents with frozen captures. Nested functions in data are currently refused. |
| External runner resources and credentials | The host owns them. Package artifacts contain declarations and code, with no live handles or credentials. |

Hosts submit session work through `send()` and observe the engine's drive.
Installing packages, reading views and preparing a runner do not drive a
turn. External-effect guarantees and the limits of retries follow
[What survives a crash](../README.md#what-survives-a-crash).

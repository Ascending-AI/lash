# Host packages

A Package publishes members at one canonical path. Each member is an operation
or an Export of a linked Document, with public or internal visibility. A Document
contains code and typed Imports; its immutable Link records every fill. Publishing
another Package never rewrites an existing Document or Link.

Plugins register a `PackageProvider` under a declared owner/local key. At round
start it advertises under committed owner config. Core supplies the durable domain,
host or session scope, configured publisher instance and owned root. Sessions add
disjoint paths. A member is a leaf: a descendant package cannot occupy it. Only
code published into the exact owning package by that publisher may import its
internal members. Function Exports may alias; type and error Brands keep their
canonical defining path.

`ScopedPackagePublisher` binds trusted publication context, accepted namespace,
runtime document provider and package store. Publication validates all Exports,
Brands and internal fills, holds the full Link/Document/function/provenance DAG,
then publishes the immutable record. Hosts retain its exact publication key and
member LinkIds in config, under their ordinary artifact referrer. A released pin
cannot be reused. Startup reads those exact records without relinking.

A round records its accepted advertisements and route candidates beside its
provider catalog in the existing round write. It uses the node's own coherent
advertisement, with no fleet-wide latest selection. A committed config that cannot
rebuild refuses new work with its typed package cause; old runners are never paired
with the new config. Already running work retains its recorded environment.

Owner configuration commits are a supported environment announcement path.
After a config transaction commits, Lash compares each touched owner's complete
recorded namespace by content, including its format stamp. An identical
replacement dirties no source: the next clean model round asks no tool or
package provider anything. A changed namespace re-collects sources belonging to
that owner and sources declaring it through `environment_config_dependencies`
at the next model boundary. No separate `refresh_tool_catalog` is needed for
that config change; already running work keeps its admitted configuration.

Run options sent through `send().run(spec)` also announce their owner's
configuration. Within one acknowledged session claim, an equal effective owner
configuration reuses collected snapshots and the validated catalog. A different
configuration re-collects its dependent sources. These results are metadata:
each override run still prepares its own lifetime owner and holds its package
closure, and never replaces the base environment head. A cold claim or an epoch
change reconciles sources again.

For mutable provider data outside owner configuration, publish changes through
`subscribe_environment`, or submit `refresh_tool_catalog` when the source does
not notify. Re-applying identical configuration does not announce an external
data change. Applied transactions still advance the config revision even when
the owner namespace is identical.

The model's Tool Catalog comes from explicit model-facing ToolProvider
registration and selection. A Package never registers a model tool. A provider
may register an executor with no model manifests and advertise service operations
separately. Standard keeps its discovery, names, inverse maps and batch syntax.
RLM code-package documentation is a separate projection.

Every Main, function Export and inline start pins a LinkId. Hosts supply explicit
attachments, including deferred paths. Every known machine, provider, callback,
codec and component requirement is recorded before first claim. A missing capable
node leaves the work parked, potentially indefinitely.

Machine requirements name the exact build-owned helper library once. Its immutable
manifest owns the native-only helper requirements; ordinary natives retain their
individual tokens. The worker pool starts on first use, so a node that only reads
starts no worker; that start installs the manifest and binds its mandatory natives
before any guest runs. Optional native accelerators with guest bodies add no debt. A different library identity does not serve the claim.

Deferred packages pin resolver scope and config, not a future target. Hosts
register session and process resolvers separately. `validate_grant` certifies the
exact recorded origin/config; matching an owner name is insufficient. The first
validated result or negative outcome is committed before execution. Later calls,
recovery and adoption use that outcome. MCP's retained-id executor is not a
resolver. A host that wants deferred processes supplies an explicit process-capable
resolver.

Process-controls owns environment capture and restriction policy. Its default
helpers copy recorded routes and pins by OperationUse and translate providers and
codecs only to explicitly registered process twins. Core validates the chosen
start independently. Worker native declarations travel in WorkerLibrary; each run
carries its reached linked code independently of that library.

A fresh code-mode cell prepares one `CellCode` bundle for its saved declarations
and new document. `KernelDocuments::fulfil_cell` adds identified documents and
their Links to that bundle; inspection and worker setup consume the same
canonical bodies. `hold_cell_code` acquires selected roots and their dependency
closure atomically under the caller's claim. Each activation prepares its own
bundle. Independent `load_linked_identified` reads still validate current stored
records, and preparation never grants a storage lifetime.

An answering cell passes the original archival call-body references from its
acknowledged checkpoints through `CellEndHandoff`. The consuming turn merges
outcomes committed after the last checkpoint from the run ledger. Cold and
stopped-cell reports still read their stored snapshot. Fixed operation bindings
already carry their route and contract; only deferred bindings load resolution
winners during cell setup.

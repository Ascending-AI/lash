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

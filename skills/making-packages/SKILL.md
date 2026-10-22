---
name: making-packages
description: Author and publish host-owned operation and code packages.
---

Read `docs/host-packages.md` and the package vocabulary before changing a host.

1. Define Document Imports and function/type/error Exports. Keep pure native
   declarations separate from operation runners. A function Export must carry
   its calling convention; a process start requires data-only inputs/result.
2. Advertise Package members with explicit Visibility through a registered
   PackageProvider. Root ownership belongs to the configured publisher instance.
   Sessions add disjoint paths; internal imports require the exact path owner.
3. Fulfil code bottom-up through ProcessDocumentProvider, using the accepted
   namespace and PackageCode purpose. Publish through ScopedPackagePublisher,
   retaining its complete code and publication provenance under an ordinary pin.
4. Store exact publication keys, PackageIds and member LinkIds in host/session
   config. Rebuild those references without relinking. Record advertisement and
   routes in the round's existing write before execution.
5. Register model-facing tool definitions separately when the model should see
   them. Ordinary service Packages do not enter the Tool Catalog.
6. Supply explicit workflow attachments and initial pins. A deferred path needs
   an explicitly registered process resolver that validates its pinned scope and
   config. MCP's retained-id executor does not provide discovery.
7. Validate the named package laws once on SQLite, plus clippy and applicable
   facade/schema/crash gates. Keep local claim and referrer-fence laws.

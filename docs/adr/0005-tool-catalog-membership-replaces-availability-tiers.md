# Tool Catalog Membership Replaces Availability Tiers

## Status

accepted

## Decision

The `ToolAvailability` ladder (`Off < Searchable < Callable < Showcased`) is removed. The Tool Catalog becomes a flat set of callable tools: **membership is the only availability fact** — a tool is in the catalog and therefore callable, or it does not exist to the model. Plugins are trusted and assemble the catalog by freely adding and removing members; suppression (authority hiding, plan-mode gating) is expressed as non-membership, not as a tier. Prompt presentation and tool discovery are no longer properties of the catalog.

## Why

The ladder compressed three independent concerns onto one ordered scale: whether a tool's schema is in the request (callable), whether it is documented in the prompt (showcased), and whether some out-of-band mechanism may surface it (searchable). Only the first two are kernel facts, and they are independent rather than ordered. `Searchable` in particular encoded a host-owned discovery mechanism as a core availability value — its meaning even differed by protocol (auto-armed under RLM, inert under standard) and collapsed to `Off` when the discovery plugin was absent. A resident tool the kernel already holds gains nothing from being "searchable" rather than callable except a per-request token saving, which is a host budgeting decision, not an availability state.

## Consequences

**Presentation is protocol-owned, not core.** Standard protocol presents every catalog member through the LLM Provider's native tools array and renders no prompt-side tool docs (already true in code). RLM, which has no native tools array, renders all catalog members as full prompt docs in its own idiom (Lashlang call-paths). The former "resident-but-not-showcased" middle tier is eliminated: a tool is either a documented catalog member or it is deferred.

**Deferred resolution is RLM-only and link-scoped.** A host-provided `DeferredToolResolver` (Lashlang layer) resolves the deterministic batch of call-paths absent from the link-time Lashlang Host Environment into per-path Tool Grants or unavailable outcomes. Linking does a gather → resolve → link pass: collect unresolved paths, exclude outcomes already recorded for this link, resolve the remaining batch in one non-transactional call, fold successful results into the host environment, then link. Missing batch results become `NotAvailable`. Each outcome is frozen in a record keyed by the stable `ExecCode` invocation; replay reuses that record and never re-authorizes, while a different code effect starts a fresh record. Recorded grants carry their **Tool Execution Binding**, and a replay-only host hook can reinstall process-local routing before the grant is folded. The flat catalog never mutates from resolution — resolution is scoped to the linked program, not promoted to session-resident state, so the Execution Environment does not drift. Standard protocol has no deferral because the API cannot express a call to an unlisted tool; large tool sets there are handled purely by host windowing of the callable set.

**lash ships no turnkey tool discovery.** Discovery, ranking, and catalogue-preview formatting are host policy, not Lash primitives. The `lash-plugin-tool-discovery` crate is removed. `examples/agent-workbench` is the in-repository production reference: it keeps a deterministic utility family outside the resident catalog, advertises a capped preview, exposes `tools.search`, persists returned grants in SQLite, and resolves granted call paths through a `DeferredToolResolver`. `search_tools` is still host-authored, and RLM does not special-case its name.

## Considered Alternatives

- **Keep the ladder, rename `Searchable` → `Deferred`.** Rejected: treats the symptom (a mechanism-coupled name) without fixing the cause (one enum doing three jobs).
- **Resident-but-searchable tier with on-demand promotion.** Rejected: if the kernel already holds the tool it is effectively callable; "searchable" only suppresses request tokens, which is host budgeting, not availability.
- **`discover`/`catalogue` methods on the resolver.** Rejected: enumeration and previews are host concerns delivered through ordinary prompt contributions and host tools; the resolver stays resolve-only.

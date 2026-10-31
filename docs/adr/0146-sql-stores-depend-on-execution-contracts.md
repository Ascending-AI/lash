# 0146: SQL stores depend on execution contracts

## Status

Accepted (FIG-6578).

## Context

SQLite and PostgreSQL implement the same process registry and persistence
ports, but importing their records from execution made both stores wait for
actor drivers, plugin composition and tool dispatch to compile. Moving a
runtime directory wholesale would retain reverse references through process
operation scopes, engine registration, waits and typed plugin errors.

## Decision

`lash-internal-execution-contracts` owns the shared execution interface. Arrows
below mean prerequisite to consumer:

```text
core-store + durable + protocol-types + core-ids + core-llm
                         |
                 execution-contracts
                  /       |       \
          execution     SQLite    PostgreSQL
                  \       |       /
                         facade
```

The crossing closure comprises:

- process inputs, identities, environments, scopes, starts, records, lifecycle,
  events, validation/folds, definitions, references, roster cursors and registry
  interfaces (including their decorators and testing interfaces);
- the pure process engine interface, admission/identity values, engine state,
  action and step vocabulary;
- module-artifact ports, graph-node codecs and their typed errors;
- retained tool-intent commands, ledger outcomes and state commands;
- session specifications and create/handle records, staged registrations,
  run references and scope projections;
- `StoreSet`, `ProcessAuthorityReads`, `BackendParts`, `Backend`,
  `StoreBindingId`, `DurableBuildError`, build format sets,
  and the projection-provider interface;
- wait keys/deadlines and wire values used by engine state;
- `PluginError`, its recorded intent causes and classification/conversions,
  configuration/declaration refusal values and process successor refusals.

Settled material, session store ports, runtime error vocabulary and policy
already belong to core-store. Consumers import those owners directly. The
contracts crate does not re-export another crate's vocabulary as a replacement
facade. The stable `lash` facade exposes each item at its natural existing
path, unless a method needs its receiving runtime type as described below.

Cycles break by ownership, without erasing typed causes:

1. The error/registry/process knot moves together. Conversions into the lower
   runtime error and plugin operation failure stay with the now-local
   `PluginError`. Pure declaration/configuration refusal values move with it;
   plugin composition and configuration registrars stay in execution.
2. `ProcessEngine` moves, while engine registration/catalog composition stays
   with the concrete plugin components in execution. Backend assembly depends
   on engine interfaces, never actor drivers.
3. `ProcessStartOptions::execution_context(&ProcessOpScope)` receives a runtime
   scope. Its implementation belongs to `ProcessOpScope::execution_context()`;
   the options were not read. This removes the records-to-actor reference.
4. Wait vocabulary moves below the runtime. Functions that drive an
   `ActorContext` stay with actors. Process park records and their store-only
   observation are contracts; committing a park remains actor implementation.
5. `ProcessStartRequest::keyed_in(&ActorContext)` becomes
   `ActorContext::key_host_start(request)`. `TurnPreludeRef::read(store)` becomes
   `TurnPrelude::read(&reference, store)`. `CoreConfigOwner::surface_refusal`
   becomes `CoreConfigRefusal::surface_refusal`. The pure `DeclaredStart`
   record owns its private decoding forms in contracts, while
   `DeclaredStart::new(context, start)` becomes `AttemptContext::processes().start(start)` per the FIG-7574 host-context ruling in ADR 0051.
   Each body moves to its
   receiving runtime type or local refusal, retaining the same data and order.
6. Successor refusal values move; scheduling/carry operations stay with the
   process runtime. No new conversion adapter or dynamic erasure is added.

Concrete actor drivers, plugin composition, direct clients, effect execution,
process service operations and dispatch remain in core-execution. No moved
item is re-exported from its old internal crate. All workspace consumers use
the owning crate directly. SQL stores retain an execution **dev dependency**
for their existing actor/conformance laws; production libraries have no such
edge. Existing projection and plugin-option type-erased catalogues keep their
original meaning; none is introduced to bridge this cut.

## Consequences

SQL store library compilation can overlap execution. An implementation-only
execution edit does not invalidate SQL stores through that dependency.
Records keep their serde/schemars names and bytes, errors retain all typed
fields, and execution/effect order and charges are unchanged. This introduces
no migration or fixture recording. Build measurements, rather than source
line counts, determine the elapsed effect.

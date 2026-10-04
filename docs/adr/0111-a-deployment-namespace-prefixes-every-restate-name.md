# 0111: A deployment namespace prefixes every Restate name lash binds or calls

## Status

Accepted.

## Context

Restate addresses services by name. Deployments that bind the same names
share those addresses, so independent Lash cores need distinct names when
they share a server. The namespace must apply to calls and administration as
well as registration.

## Decision

### 1. The namespace

`RestateConfig::with_namespace` sets a `RestateNamespace`. A nonempty
namespace is 1 to 63 ASCII bytes of lowercase letters, digits and `-`, starts
with a lowercase letter, and does not start with the reserved `restate`
prefix. Invalid values return `RestateNamespaceError`.

The empty string selects the default namespace and bare service names.
A dot separates a namespace from a base name. An underscore introduces a
generation suffix and is a wildcard in admin SQL, so neither belongs in a
namespace.

Evidence: `crates/lash-restate/src/services.rs`.

### 2. Every name carries it

Stable names are `{namespace}.{Base}` and generation lanes are
`{namespace}.{Base}_g{generation}`. The default namespace omits the prefix.
`LashDurableWaitIndex` and `LashSession` are base names in this scheme.

Host ingress, typed handler clients and admin queries use these qualified
names. Namespace-aware route parsing and service-lane filters keep queries
within the configured namespace. [ADR 0106](0106-durable-formats-upgrade-by-migration-or-drain.md)
and [ADR 0115](0115-the-1-0-binary-carries-its-half-of-every-upgrade.md)
own generation routing and compatibility.

Evidence: `crates/lash-restate/src/services.rs`.

### 3. Keys stay scoped to their services

Object keys, workflow keys, idempotency keys and awakeable ids retain their
own spelling. Qualifying the service name separates service-scoped keys.
Moving to a different namespace addresses different Restate state; it does
not migrate the state under the source namespace.

Process start keys are store identities, as specified in ADR 0107 §2.
Sharing a SQL store shares that key space regardless of Restate namespaces.

### 4. A colliding registration is refused

Bound services carry `lash.authority` metadata with the engine authority's
binding id. `RestateEngine::register_deployment(uri)` checks stable service
registrations through the admin API. A name is accepted when unregistered,
when its registration has no authority claim, when the claim matches, or
when the holding deployment uses the same URI. A different holder returns
`RestateRegistrationError::NameTaken` before registration.

The endpoint-generation guard runs first. A fresh URI registers without
force. A URI serving this build's generation can redeploy with force; a URI
serving another generation returns `EndpointServesAnotherGeneration`.
Namespace separation does not bypass this guard.

The checks and registration are separate admin operations. They detect
misconfiguration and do not serialize racing registrations. They perform
no authentication or host security policy.

Evidence: `crates/lash-restate/src/engine.rs` and
`crates/lash-restate/src/engine.rs`.

### 5. Hosts share a server

`LocalRestateServer::shared` supplies a process-wide server. Each core takes
a namespace through `LocalRestateServer::core(label)` and registers through
the guard. Toolbench and Slack-clone's live cores use this arrangement.
The workbench's valid-empty fixture uses `agent-workbench-valid-empty` on
the workbench's server. Launch-script hosts can register their default
namespace endpoint through their scripts.

Evidence: `examples/shared/local_restate.rs`.

Executable evidence is the namespace suite in
`crates/lash-restate-test/tests/namespaces.rs`, which runs on the Restate
double and the live backend. It checks independent deployments on one
server and colliding registration refusals.

## Consequences

Independent cores can share a Restate server. The namespace is part of the
deployment's identity; changing it starts a deployment under different
addresses. A private server per core also separates names, but duplicates
server resources and startup work. A qualified name makes the separation
explicit while permitting a shared server.

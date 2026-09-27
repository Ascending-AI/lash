# 0111: A deployment namespace prefixes every Restate name lash binds or calls

## Status

Accepted and implemented 2026-09-27 (FIG-3898).

Amends [ADR 0104](0104-restate-is-the-only-effect-engine-sql-stores-are-storage.md)
§4: one `restate-server` serves any number of lash deployments, each in a
namespace of its own. Extends the lane naming of
[ADR 0106](0106-durable-formats-upgrade-by-migration-or-drain.md) (FIG-3795):
the stable and generation names carry the namespace. The FIG-3814 ruling
stands: the registered names `LashDurableWaitIndex` and `EffectGroupIndex`
keep their base names.

## Context

The engine bound lash's services under fixed names (`LashSession`, `LashTurn`,
`EffectGroupDispatch`, ...). Restate keys a service by its name alone. On the
pinned server (1.7.12), a second deployment that registers the same names
takes them over without complaint: the services move to revision 2, and new
calls go to the new deployment. Two cores on one server therefore sent each
other's turns, processes and effect groups to whichever deployment
registered last.

The example hosts worked around this by starting a private server for each
core: every toolbench run, each of the slack-clone live E2E cores, and the
agent-workbench valid-empty fixture.

## Decision

### 1. The namespace

A deployment's namespace is set on the engine's configuration with
`RestateConfig::with_namespace(RestateNamespace)`. A namespace is 1 to 63
bytes of `[a-z0-9-]`, it starts with a lowercase letter, and it does not start
with `restate`, which Restate reserves for its own names. `RestateNamespace`
refuses any other value with a typed `RestateNamespaceError`.

- `.` is not allowed because it separates the namespace from the name.
- `_` is not allowed because it starts a generation suffix, and because the
  admin SQL filters treat it as a `LIKE` wildcard.

The empty string gives the **default namespace**, which keeps the bare names
of every build before this ADR.

### 2. Every name carries it

A service is bound as `{namespace}.{Base}`, and a generation lane as
`{namespace}.{Base}_g{generation}`. In the default namespace both keep their
bare form. The prefix applies to every name lash **calls** as well as every
name it binds:

- ingress calls from hosts;
- handler-to-handler calls, made through namespaced request clients, each
  pinned at compile time to the SDK's typed client for the same handler;
- the admin-API queries: the `target_service_name` filters over
  `sys_invocation` (paused session drives, park reconcile, the lost-run scan).

No namespace reads another namespace's names. The default namespace matches
only names that contain no `.`.

### 3. Keys stay

Object keys, workflow keys, idempotency keys and awakeable ids do not change.
Restate scopes keys and idempotency keys to their service, so the prefix
scopes them too. Existing data in the default namespace is untouched.

Moving a deployment to another namespace makes it a new deployment. Its
Restate state lives under other names, and nothing migrates it.

### 4. A colliding registration is refused

Every service lash binds carries the metadata `lash.authority`, set to the
engine authority's binding id. `RestateEngine::register_deployment(uri)` reads
each stable name from the admin API before it registers `uri` with
`force: true`. It lets the registration through when the name is:

- unregistered;
- unclaimed, meaning a build from before this ADR holds it;
- claimed by the same authority, as in a redeploy or a new build;
- held by a deployment at the same `uri`.

Any other holder refuses the registration with
`RestateRegistrationError::NameTaken`, and nothing is registered.

The check and the registration are two admin calls. The guard catches
misconfiguration; it is not a lock between two registrations that race.

### 5. Hosts share a server

- **Example hosts** get their server from `LocalRestateServer::shared`. It
  starts one server for the whole process and stops it when the last core lets
  go. Each core takes its own namespace with `LocalRestateServer::core(label)`
  and registers through the guard. This covers toolbench runs and the
  slack-clone live cores.
- **The valid-empty fixture** registers on the agent-workbench's own server,
  in the namespace `agent-workbench-valid-empty`.
- **Launch-script hosts** (agent-service, the workers runbook, the workbench
  itself) still register their endpoint from their launch scripts in the
  default namespace. They can adopt `register_deployment` whenever they need
  to share a server.

## Consequences

- One `restate-server` serves every core a process runs. The per-core server
  start (about 0.3 s and 200 MB RSS each, ADR 0104 §4) is gone.
- The `lash-restate-test` double and the live backend take a namespace too.
  `RestateTestBackend::beside` registers a second deployment on the same
  double. The laws in `crates/lash-restate-test/tests/namespaces.rs` run two
  namespaced cores side by side on one server, and prove that a colliding
  registration is refused. The `namespaces` Restate suite runs the same laws
  against a live server.
- A namespace is part of a deployment's identity, in the same way as its
  authority. An operator who changes it starts a new deployment.

# One promised package: the facade owns the API; internals are lash-internal-*

## Status

Accepted. Ratified on FIG-2088 on 2026-08-25; the six sections of the decision
are the six rulings recorded there. This ADR supersedes ADR 0051 only where
that ADR promises individual internal package paths, retires them in waves, or
enforces the promise through the API example-coverage inventory. ADR 0051's
four integrator classes and transitive signature-closure rule remain normative.

## Context

Lash has one deliberate host facade and a release family of twenty-nine
publishable packages. The package topology currently makes both look promised:
`lash-runtime` exposes the `lash` library, while the other twenty-eight Lash
packages retain ordinary names on crates.io and public Rust paths that a host
can depend on directly. Documentation can call those paths internal, but their
package names and per-path compatibility machinery communicate a second
contract.

ADR 0051 correctly established that the facade is the host API and identified
the four classes of integrators that must implement lower-level contracts. Its
wave model nevertheless preserves a promise for each surviving internal path,
then asks a hand-maintained inventory to decide and explain that promise item by
item. That is the wrong unit of ownership. A public item may remain physically
reachable so internal packages can compose and the facade can re-export it
without becoming a separately supported API.

The facade therefore owns the external promise. Internal packages remain
publishable implementation artifacts because Cargo requires them, but their
names, compatibility policy, examples, and release gates all point back to one
supported package.

## Decision

### 1. `lash-runtime` is the single promised package

The crates.io package `lash-runtime`, whose library crate is `lash`, is the only
supported package. The two-tier alternative — a supported facade plus a
supported family of lower-level packages — is rejected.

The facade gains optional, feature-gated dependencies and re-export modules for
host-wired extensions. The extension set is approximately twelve to fifteen
features and includes SQLite, Postgres, S3, Restate, OpenAI, Anthropic, Google,
MCP, subagents, TypeScript, and HTTP transport. A host selects those backends
and extensions through `lash-runtime`; it does not assemble a supported Lash
family from companion packages.

All other twenty-eight publishable Lash packages are renamed to
`lash-internal-*`. Cargo dependency aliases preserve their current crate names
and source paths, so the package classification does not require source-level
crate renames. The facade's dev-dependency-only edges to `lash-restate`,
`lash-postgres-store`, and `lash-provider-openai` do not form dependency cycles.

`lash-remote-protocol` remains internal. Its implementation may continue to use
internal packages where the facade's dependency direction requires it; hosts
name its public vocabulary through `lash::remote`.

### 2. Integrator contracts deepen existing facade modules

ADR 0051's four integrator classes remain the supported lower-level contracts:

1. Store and durable-substrate implementors.
2. Effect-host implementors.
3. Protocol and process-engine implementors.
4. Conformance-suite embedders.

Their membership continues to be determined by transitive signature closure,
including the produce-side and consume-side member rules in ADR 0051. This ADR
does not narrow or reopen that closure.

Those contracts acquire complete homes by deepening existing facade modules:

* Stores live under `lash::persistence`.
* Effect-host contracts live under `lash::durability` and `lash::runtime`.
* Engine extension points live under `lash::plugins`, including the three
  currently absent protocol-plugin traits: `ProtocolSessionPlugin`,
  `CodeExecutorPlugin`, and `ProtocolDriverPlugin`.
* Conformance suites live under `lash::testing::conformance`.

There is no new `lash::integrate` namespace. An internal path found to be
required by one of the four classes is a facade gap to close in the appropriate
existing domain module, not a second supported package.

### 3. Base conformance requires only `testing`

The store, registry, effect-host, trigger, attachment, and recovery conformance
suites are available when the facade's `testing` feature is enabled. They do
not also require `rlm`. Only RLM-specific suites remain gated by `rlm`.

This separates the executable contracts for the four integrator classes from a
specific process mode. The current placement of the entire conformance module
behind `rlm` is an implementation state to remove, not part of the supported
feature contract.

### 4. Features are additive and opt-in

`default = []` remains unchanged. The existing `testing`, `otel-trace`, and
`rlm` features remain unchanged except for the conformance-gating correction in
section 3. The facade adds only the optional backend and host-wired extension
features required by section 1. This decision does not create a bundled default
runtime or a second feature tier for internal packages.

### 5. The package cutover is one breaking alpha release

`lash-runtime` keeps its package name. The other twenty-eight package renames
ship together in one declared breaking alpha release, under the existing
lockstep publisher. There are no compatibility package aliases, staggered
rename waves, or overlapping supported names.

That release carries `Release-Notes` and updates the README and relevant docs so
existing direct-package consumers can move to `lash-runtime`, its features, and
the facade module that owns their contract.

### 6. Facade evidence is compiled and mechanically gated

The FIG-861 successor doctrine is:

> Every facade API is exercised by a compiled example or doctest. Enforced
> mechanically wherever FIG-2090's compiler derivation reaches; the remainder
> is a review-time expectation recorded in this ADR — never a prose ledger.

The hand ledger — `docs/api-example-coverage.toml` and its prose disposition,
alias, evidence, and tombstone machinery — is retired unconditionally by
FIG-2094. It is not narrowed to facade items or recreated under another name.

The replacement enforcement is a generated facade snapshot diff,
`cargo-semver-checks` (advisory until the cutover release ships, so it cannot
block the intentionally breaking release this ADR mandates), an external-type
allowlist, a facade-only import scan, and `deny(missing_docs)`. Together these gates answer which facade paths exist,
whether a release breaks them, whether internal dependency types leak through
them, whether repository consumers bypass them, and whether they are
documented. Compiled examples and doctests answer whether the facade is
exercised; review covers only the part FIG-2090 cannot yet derive mechanically,
without creating handwritten inventory rows.

The restored mechanical backstop compiler-scrapes direct function and method
calls from `agent-service`, `agent-workbench`, and `slack-clone` against the
default-feature `lash` facade and the public, non-hidden `lash_restate`
choreography surface. The Restate scrape enables `agent-service/restate`; the
upstream `restate_sdk` re-export is not adapter-owned surface. These callable
identities are the blocking scope once the mechanically reported residual gap
set reaches zero. Until then the pull-request job remains explicitly advisory
and publishes every gap rather than silently waiving it. Fields, variants,
types, concrete trait implementations, doctests, and other uses the stock
scraper cannot derive remain a review-time expectation. Including the adapter
in this evidence scope does not make its internal package a second promised
package.

## Alternatives considered

* **Keep a two-tier supported family.** Rejected. It preserves the ambiguity
  between deliberate facade contracts and physically public implementation
  paths, and makes every internal package another compatibility surface.
* **Create `lash::integrate`.** Rejected. Stores, effect hosts, engines, and
  conformance already have domain homes in the facade; another namespace would
  give integrators two plausible paths and leave those modules incomplete.
* **Retire internal paths and packages in waves.** Rejected. The package promise
  is singular, so the cutover and its release communication are singular too.
  Waves prolong the obsolete contract and charge consumers repeated breaking
  migrations.
* **Keep a facade-only prose inventory.** Rejected. It retains the recurring
  manual judgment and alias bookkeeping this decision replaces. Generated
  surface, compatibility, leakage, import, documentation, and compiled-usage
  facts are the enforcement model.

## Consequences

* Hosts have one package to select, one feature surface to configure, and one
  namespace in which compatibility is promised.
* Internal packages remain public enough for Cargo composition and facade
  re-export, but their `lash-internal-*` names explicitly exclude them from the
  supported-package contract. Physical reachability is not compatibility.
* Integrator contracts remain supported in full through the facade. ADR 0051's
  class definitions and signature closure continue to prevent a facade move
  from stranding types or members an implementor must construct or read.
* Direct consumers of any renamed package must migrate in the cutover release.
  This is intentionally breaking and has no shim period.
* Optional facade features add dependency and compile-time cost only when a host
  selects the corresponding backend or extension; the default remains empty.
* Surface review moves from handwritten per-item explanations to generated
  diffs and compiler-backed facts. FIG-861's compiled-example doctrine remains,
  with review-time judgment only where derivation is not yet mechanically
  complete.

### Migration

Implementation is carried by FIG-2087's children, FIG-2089 through FIG-2094:
the package and feature cutover, compiler-derived example evidence, facade
surface and compatibility gates, integrator-home completion, documentation and
release migration, and unconditional ledger retirement. Those tickets own the
mechanics and validation. This ADR records the end-state contract and makes no
code, manifest, release-gate, or source change itself.

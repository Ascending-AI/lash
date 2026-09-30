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

Amendment (FIG-3189): the extension set is realized as the eleven features
`sqlite`, `postgres`, `s3`, `restate`, `openai`, `anthropic`, `google`, `mcp`,
`subagents`, `typescript`, and `http-transport`, each gating one
`lash::<name>` module that re-exports the matching internal crate's root. With
the three features the facade already carried — `rlm`, `testing`, `otel-trace`
— the set is fourteen, inside the range this section names. No promised
extension lacked a crate to wire, and none was added or renamed.

All other twenty-eight publishable Lash packages are renamed to
`lash-internal-*`. Cargo dependency aliases preserve their current crate names
and source paths, so the package classification does not require source-level
crate renames.

Amendment (FIG-3189): the edges to `lash-restate`, `lash-postgres-store`, and
`lash-provider-openai` are optional normal dependencies now, not
dev-dependency-only edges, because the `restate`, `postgres`, and `openai`
features gate re-export modules in the library. They still form no publish
cycle: the four internal crates that depend back on the facade do so through
version-less path dev-dependencies, which the publish ordering excludes.

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
* Conformance suites live under `lash::testing::conformance`. *(Superseded on
  this point on 2026-09-23: the durable-store laws left the facade so the
  80k-line law crate is off `lash-runtime`'s dependency edge. A host depends on
  `lash-internal-conformance` directly.)*

There is no new `lash::integrate` namespace. An internal path found to be
required by one of the four classes is a facade gap to close in the appropriate
existing domain module, not a second supported package.

### 3. Base conformance requires only `testing`

The store, registry, effect-host, trigger, attachment, and recovery conformance
suites are available when the facade's `testing` feature is enabled. They do
not also require `rlm`. Only RLM-specific suites remain gated by `rlm`.
*(Superseded on this point on 2026-09-23: the store suites are no longer
reached through the facade; see section 2. The RLM rebuild suite remains
gated by `rlm`.)*

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

### 6. Facade evidence and current enforcement

The FIG-861 successor doctrine is:

> Every facade API should be exercised by a compiled example or doctest. This
> is review doctrine, not a universal compiler-derived coverage gate; it does
> not require a handwritten prose ledger.

The hand ledger — `docs/api-example-coverage.toml` and its prose disposition,
alias, evidence, and tombstone machinery — is retired unconditionally by
FIG-2094. It is not narrowed to facade items or recreated under another name.

Historical enforcement, removed under FIG-2933 (`8698820631`), comprised the
generated facade snapshot diff, semver checks, an external-type allowlist,
missing-docs enforcement and compiler scraping. Their original blocking or
advisory plans no longer describe current release or pull-request gates.

Current enforced scope is the facade-only import scan, the feature-plan check
and compile-fail fixtures. They check consumer imports, the declared feature
plan and forbidden API access respectively; none certifies universal semantic
example coverage. Reviewers retain the compiled-example doctrine above.

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
  facts were the original enforcement model; the removed gates are historical
  under FIG-2933, and section 6 states the current scope.

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
* Surface review no longer uses a handwritten per-item ledger. Section 6
  states the current bounded enforcement, and FIG-861's universal
  compiled-example expectation remains review doctrine.

### Migration

Implementation is carried by FIG-2087's children, FIG-2089 through FIG-2094:
the package and feature cutover, compiler-derived example evidence, facade
surface and compatibility gates, integrator-home completion, documentation and
release migration, and unconditional ledger retirement. Those tickets own the
mechanics and validation. This ADR records the end-state contract and makes no
code, manifest, release-gate, or source change itself.

## Amendment (FIG-4163, 2026-09-30)

The FIG-2933 deletion supersedes the former facade snapshot, semver, external-type, missing-docs and compiler-scraping gate promises; universal-example coverage remains review doctrine.
Current enforcement is [import scanning](../../scripts/check_facade_only_examples.py),
[feature-plan validation](../../scripts/check_feature_coverage.py), and
[compile-fail fixtures](../../crates/lash/tests/ui.rs).

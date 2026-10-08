# One promised package: the facade owns the API

## Context

Internal packages need public Rust items to compose and to support facade
re-exports. Physical reachability alone must not communicate a separate
supported API for each implementation package.

## Decision

### 1. `lash-runtime` is the single promised package

The crates.io package `lash-runtime`, imported as `lash`, owns the supported
host API. Other publishable packages use `lash-internal-*` names. Workspace
dependency aliases keep internal Rust crate names readable without promising
those package paths to hosts.

Optional facade features expose host-wired extensions: `sqlite`, `postgres`,
`s3`, `openai`, `anthropic`, `google`, `mcp`, `typescript`, and
`http-transport`. Each selects its domain module and
internal dependency. Hosts own their wire contracts ([ADR 0136](0136-hosts-own-their-wire-contracts.md)).

### 2. Integrator contracts deepen existing facade modules

ADR 0051 defines the supported integrator classes and transitive signature
closure. Store contracts have a home in `lash::persistence`; durable-engine
configuration in `lash::durability`; protocol, process-engine and
projection-provider extensions in `lash::plugins`. A required signature member missing from these
modules is a facade gap, rather than another promised internal package.

Durable-backend conformance laws are an explicit tooling dependency on
`lash-internal-conformance`. They do not sit on the facade dependency edge.

### 3. Testing and conformance have separate dependency paths

The `testing` feature exposes facade test support, including test providers.
It does not pull in the durable-backend certification laws. Integrators select
the law crate and the store matrix they need directly. RLM test support
is available when the optional RLM dependencies are selected.

### 4. Features are additive and opt-in

`default = []`. Backend features and `rlm`, `testing`, and `otel-trace` add their
respective dependencies. The `synthetic-next` feature forwards upgrade-proof
formats to participating dependencies for the upgrade harness. It is not a
production default or a bundled runtime.

### 5. Package names express the compatibility boundary

Internal package publication supports Cargo composition. It does not extend
the facade's compatibility promise. A host consuming internal implementation
paths accepts that boundary directly; no compatibility alias or second supported
package family is offered. Release policy remains the lockstep publisher's
contract.

### 6. Facade evidence and current enforcement

Every facade API should have a compiled example or doctest. This is review
doctrine, not a claim of universal compiler-derived coverage.

Executable enforcement comprises the facade-only consumer-import scan,
feature-plan validation, and compile-fail fixtures. They check import paths,
feature coverage, and forbidden access respectively. None certifies complete
semantic example coverage, and no handwritten per-item coverage ledger is
required.

## Alternatives considered

A supported lower-level package family makes every physically public path a
second compatibility commitment. A separate `lash::integrate` namespace gives
integrators competing homes instead of complete domain modules. A prose API
inventory adds manual dispositions and alias bookkeeping without proving the
behavior of those APIs. Domain re-exports and bounded executable gates keep
those responsibilities explicit.

## Consequences

Hosts select one supported package and its opt-in features. Implementation
packages remain usable by the workspace and tooling. Integrator obligations
still include the signature members required to construct and consume their
contracts. Review supplies example evidence beyond the bounded gates.

## Code references

- `crates/lash/Cargo.toml:1-19,60-105` defines package, features, and test-support edges.
- `crates/lash/src/lib.rs` defines the public domain modules and feature-gated extensions.
- `scripts/check_facade_only_examples.py` checks consumer imports.
- `scripts/check_feature_coverage.py` validates the feature plan.
- `crates/lash/tests/ui.rs` runs compile-fail fixtures.

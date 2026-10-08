# 0136: Hosts own their wire contracts

## Status

Accepted. Supersedes the engine transport decisions in
[ADR 0002](0002-session-observation-uses-cursors-and-bounded-live-replay.md),
[ADR 0079](0079-one-promised-package-facade-owns-the-api.md),
[ADR 0100](0100-the-run-observation-contract.md), and
[ADR 0115, section 4](0115-the-1-0-binary-carries-its-half-of-every-upgrade.md#4-host-wire-contracts).

## Context

Lash is an embedded engine. Its Rust API gives hosts local operations and
values, while each host chooses how clients reach those operations. An engine
transport vocabulary couples Rust changes to contracts owned by host products.

## Decision

Lash ships no engine wire protocol, transport DTOs, negotiation, or engine
wire schemas. Hosts define their own DTOs around the Rust API and own their
transport, authentication, version skew, and compatibility policy. Lash core
types carry no wire-stability promise; deriving serialization does not create
one. A host may serialize core values when its own contract permits it.

The workbench owns its small NDJSON observation envelope in the example. It
continues the local recoverable observation feed with cursors, replacement
commits, and replay gaps. It performs no engine protocol negotiation.

The six independent schema documents remain: workflow graph and type facets,
trace record and Lashlang graph, and process effect outcome and omissions.
Their owners define their stored or projection contracts. The VM IPC contract
also belongs to its own execution boundary.

## Consequences

Engine changes update local callers together. Hosts decide whether their
clients require stable JSON and supply that contract themselves. Local
observation, process administration, turn execution, and typed refusals retain
their Rust API semantics.

An optional universal engine adapter would impose a compatibility commitment
without a Lash-owned server product. Hosts that need a deployed API keep its
contract with that product.

Evidence: `examples/agent-workbench/src/main_sections/routes/observation_envelope.rs`,
`crates/lash/src/session.rs`, and `schemas/host/README.md`.

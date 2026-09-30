# Harness evolution lives outside the runtime repository

## Context

Candidate optimization needs mutable code and an evaluation loop. Keeping
that code outside the runtime makes the runtime contracts an external
dependency of each candidate rather than part of its mutation space.

## Decision

Lash owns reusable runtime, plugin and protocol contracts and its reference
Execution Modes. Generic harness optimization, evaluation, candidate worktrees
and mutable Harness Packages belong to the separate `lash-evolve` repository.
Interactive runtime features may live in Lash; generic candidate evolution
does not.

## Consequences

Evolution can use an experimental release cycle. Independently developed
Execution Modes exercise published contracts rather than private workspace
paths. Co-locating candidate code with the runtime is rejected because it
allows candidate mutation to change the dependency it is meant to evaluate.

## Implementation

The [workspace manifest](../../Cargo.toml) lists the runtime, reference
protocols, providers, conformance and examples; it contains no candidate
evolution package. The [host facade](../../crates/lash/src/lib.rs) is the
external embedding boundary.

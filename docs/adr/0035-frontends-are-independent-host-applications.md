# Frontends are independent Host Applications

A terminal or other user-facing frontend for Lash lives in its own repository,
outside the Lash runtime repository. Such a repository owns its UI, UI
extensions, file index, transcript exporter, applied research integrations,
benchmark support, operator harness, installer, self-update policy, and binary
releases.

Lash owns reusable runtime, protocol, provider, persistence, plugin, tooling,
and performance contracts. A frontend consumes those contracts at one reviewed,
exact Lash revision and advances that revision through an explicit compatibility
change. The blanket prohibition on binaries is historical, superseded by
[ADR 0115](0115-the-1-0-binary-carries-its-half-of-every-upgrade.md): Lash
publishes the SDK crates and the `lashctl` operator binary. Frontend binaries
and installers remain the frontend host's responsibility.

This boundary makes every frontend an honest external embedder: it can choose
plugin composition and Execution Modes without forcing runtime releases, while
changes to Lash must remain usable without private workspace paths. A private
support crate stays in the Host Application repository while that host is its
only real consumer; it moves into Lash only when it becomes a stable,
frontend-independent contract with credible use by another host.

## Amendment (FIG-4125, 2026-09-29)

Item 23: [ADR 0079](0079-one-promised-package-facade-owns-the-api.md) governs
the promised Lash package API. Independent frontend ownership and host policy
remain as stated here.

## Amendment (FIG-4163, 2026-09-30)

ADR 0115 admits the `lashctl` operator binary without moving frontend ownership or host policy into Lash.
[Its package manifest](../../crates/lashctl/Cargo.toml) declares the binary.

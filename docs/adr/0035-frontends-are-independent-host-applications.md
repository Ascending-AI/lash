# Frontends are independent Host Applications

A terminal or other user-facing frontend for Lash lives in its own repository,
outside the Lash runtime repository. Such a repository owns its UI, UI
extensions, file index, transcript exporter, applied research integrations,
benchmark support, operator harness, installer, self-update policy, and binary
releases.

Lash owns reusable runtime, protocol, provider, persistence, plugin, tooling,
and performance contracts. A frontend consumes those contracts at one reviewed,
exact Lash revision and advances that revision through an explicit compatibility
change. Lash releases publish the SDK crates and no binaries or installer
assets.

This boundary makes every frontend an honest external embedder: it can choose
plugin composition and Execution Modes without forcing runtime releases, while
changes to Lash must remain usable without private workspace paths. A private
support crate stays in the Host Application repository while that host is its
only real consumer; it moves into Lash only when it becomes a stable,
frontend-independent contract with credible use by another host.

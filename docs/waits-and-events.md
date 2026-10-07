# Waits and events

A durable wait is a row with a kind, an owner and a deadline written once at
creation
([ADR 0132 §6](adr/0132-durability-is-state-first-over-the-lash-store.md#6-waits-and-timers-are-rows)).
A host-resolvable wait (`tool_completion` or `custom`) has a completion key,
`wk1.<wait_id>.<mac>`, signed with the deployment's completion secret. The key
is a credential: whoever holds it can resolve its wait.

`LashCore::completions()` lists a session's outstanding keys
(`outstanding(session_id)`) and settles one (`resolve(key, resolution)`),
first writer wins. A second resolution answers `AlreadyResolved` (same
digest) or `Conflict` (another digest). A key that does not verify, or whose
wait is unknown, revoked or timed out, answers `UnknownOrRevoked`. A key of
any other kind answers `ReservedKind`. Neither writes anything.

Lash applies no authorization to wait resolution. The host authenticates a
caller before it resolves a wait on the caller's behalf — see
[ADR 0014](adr/0014-operational-policy-stays-with-the-host.md) and
[ADR 0046 §3](adr/0046-process-transitions-are-events-record-is-a-fold.md).

The [host guide](operations/durable-hosting.md#5-completion-keys) covers
provisioning and rotating the completion secret, and every resolve answer.

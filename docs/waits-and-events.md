# Waits and events

A durable wait is a row with a kind, an owner and a deadline written once at
creation
([ADR 0132 §6](adr/0132-durability-is-state-first-over-the-lash-store.md#6-waits-and-timers-are-rows)).
A host-resolvable wait (`tool_completion` or `custom`) has a completion key:
its wait id, 128 random bits from the operating system's CSPRNG, as 32 hex
digits. The key is a bearer capability: whoever holds it can resolve its wait.
Lash keeps no completion secret.

`LashCore::completions().parked(owner)` lists a session's or process's pending
admitted tool calls. `lash::admin::CallOwner` selects `Session(SessionId)` or
`Process(ProcessId)`. Each `ParkedCall` carries its completion `key`, `owner`,
stable `call_id`, `tool_id` and `deadline`, read from existing wait and admission
rows. This is a snapshot: a returned key may be settled concurrently.

A deferred process call records `process.waiting` with
`WaitKind::Call { call_id, tool_id }`, without the bearer key; settling the call
records `process.resumed`.

`resolve(key, resolution)` settles a wait, first writer wins.
A second resolution answers `AlreadyResolved` (same
digest) or `Conflict` (another digest). A key that names no wait answers
`Unknown`; one whose wait was revoked or timed out answers `Revoked`. A key of
any other kind answers `ReservedKind`. None of them writes anything.

Lash applies no authorization to wait resolution. Who may finish a pending
wait is the host's decision: it authenticates and authorizes a caller (its API
authentication, its webhook signatures) before it resolves a wait on the
caller's behalf, and hands a key only to callers it has authorized — see
[ADR 0014](adr/0014-operational-policy-stays-with-the-host.md) and
[ADR 0046 §3](adr/0046-process-transitions-are-events-record-is-a-fold.md).

The [host guide](operations/durable-hosting.md#5-completion-keys) covers
completion keys and every resolve answer.

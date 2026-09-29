# Waits and events

A durable wait is a one-shot promise an execution scope parks on; its key is a
deterministic identity derived from the scope and the wait, not a credential.
`LashCore::completions()` lists a session's outstanding keys
(`outstanding(session_id)`) and settles one (`resolve(key, resolution)`), first
writer wins; a late resolution reports the already-recorded outcome.

Lash applies no authorization to wait resolution. The host authenticates a
caller before it resolves a wait on the caller's behalf — see
[ADR 0014](adr/0014-operational-policy-stays-with-the-host.md) and
[ADR 0046 §3](adr/0046-process-transitions-are-events-record-is-a-fold.md).

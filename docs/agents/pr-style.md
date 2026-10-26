# Change descriptions

Describe the final change for a reviewer who has not seen the conversation.
Lead with the concrete problem and resulting behaviour. Link the ticket for
context, then explain the change's shape and any trade-off the reviewer needs
to judge it. Put lasting architectural decisions in an ADR.

Report the actual validation scope and unresolved risks. A focused regression
is not a full-suite result. Apply the proof requirements in
[AGENTS.md](../../AGENTS.md) and the test admission rules in
[way-of-working.md](way-of-working.md#which-tests-get-admitted).

Commit descriptions carry durable evidence. For host-facing changes, include
the host impact required by
[way-of-working.md](way-of-working.md#host-changes-in-commit-bodies).
Keep follow-ups on tickets and update the description when the diff changes.

Land committed work through `scripts/land`; see [AGENTS.md](../../AGENTS.md).

# Triage labels

States carry the lifecycle; labels carry only what a state can't express: **who picks a ticket up**. See the "Lifecycle: states and labels" section of [way-of-working.md](way-of-working.md) for the full model. This file records the label vocabulary.

## The two dispatch labels

Only two triage labels exist, and both live on **Todo** tickets:

| Label | Meaning |
| ----- | ------- |
| `ready-for-agent` | Fully specified: success condition stated, no open decisions, Agent spec filled in; an AFK agent can take it without asking anything. |
| `ready-for-human` | Needs judgment or access an agent doesn't have. |

## Labels we deliberately don't use

The `mattpocock/skills` triage vocabulary also names `needs-triage`, `needs-info`, and `wontfix`. We don't create these; each duplicates a workflow state:

- `needs-triage` → the **Triage** state.
- `wontfix` → the **Canceled** state.
- `needs-info` → keep the ticket in **Triage** (or comment the question and leave it there); no label needed.

When a skill mentions one of those roles, map it to the state above, not a new label.

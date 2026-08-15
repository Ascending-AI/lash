# Host prompt presence controls reopen authority

## Status

Accepted.

## Context

A durable session head must distinguish a prompt field that was absent because
an older writer did not persist prompts from a prompt field that is present and
contains an intentionally empty `PromptLayer`. Collapsing both cases to an
empty layer makes a compatibility default indistinguishable from durable host
intent. Reopening also has a live authority input: the host may explicitly
supply a new session prompt, or it may leave prompt selection unspecified.

The authority decision belongs at facade reconciliation. Stores preserve wire
presence, while core runtime state continues to hold a concrete effective
prompt. The model-facing result is pinned by the in-memory and SQLite laws in
`core_session_builder/session_lifecycle.rs`, including the literal historical
head fixture used by
`legacy_promptless_head_with_host_prompt_renders_host_prompt_in_memory` and
`legacy_promptless_head_with_host_prompt_renders_host_prompt_sqlite`.

## Decision

`PersistedSessionConfig.prompt` is `Option<PromptLayer>`. `None` means the field
was absent from a legacy head; `Some(PromptLayer::new())` is an explicitly empty
committed prompt. New writes always use `Some` through one shared projection
from `SessionPolicy`.

Reopen authority is presence-based:

| Persisted prompt | Explicit host session prompt | Effective prompt |
|---|---|---|
| absent | present | host prompt |
| absent | absent | ordinary live host/core reconstruction |
| present, including empty | absent | persisted prompt |
| present | present | host prompt, committed at the next boundary |

The same matrix is asserted against the next fully rendered provider request,
not merely an intermediate policy object. A resident graph refresh reconciles
durable graph and checkpoint progress without reverting a live prompt mutation
that has not yet committed.

## Consequences

- Historical prompt-less bytes retain mainline behavior.
- Explicit emptiness survives a cold reopen and can suppress an inherited host
  default.
- A host can deliberately replace an old committed prompt at reopen without a
  compatibility shim or migration.
- Store implementations and fixtures must preserve prompt presence exactly;
  constructing durable configuration ad hoc is forbidden.
- The complete persisted session configuration, including its prompt, counts
  toward the Runtime Commit byte budget.

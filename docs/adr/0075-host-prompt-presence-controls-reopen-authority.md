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
session prompt. The live core prompt remains a separate, always-rendered base
layer in `RuntimeHostConfig`; it is neither persisted nor subject to reopen
reconciliation. The model-facing result is pinned by the in-memory and SQLite
laws in `core_session_builder/prompt_reopen_authority.rs`, including the
literal historical head fixture used by
`legacy_promptless_head_with_host_prompt_renders_host_prompt_in_memory` and
`legacy_promptless_head_with_host_prompt_renders_host_prompt_sqlite`.

This ADR explicitly supersedes ADR 0074's sentence that host configuration
wins on reopen "for the model and the prompt" for the session-prompt field.
The rule is refined by presence: an explicit host session prompt wins, while a
present persisted session prompt wins when the host leaves that field absent.
ADR 0074's model and generation authority is unchanged.

## Decision

`PersistedSessionConfig.prompt` is `Option<PromptLayer>`. `None` means the field
was absent from a legacy head; `Some(PromptLayer::new())` is an explicitly empty
committed prompt. New writes always use `Some` through one shared projection
from `SessionPolicy`.

Reopen authority is presence-based:

| Persisted session prompt | Explicit host session prompt | Effective session prompt layer |
|---|---|---|
| absent | present | host prompt |
| absent | absent | ordinary live session reconstruction |
| present, including empty | absent | persisted prompt |
| present | present | host prompt, committed at the next boundary |

The same matrix is asserted against the next fully rendered provider request,
not merely an intermediate policy object. The rendered request always adds the
current live core prompt beneath that session layer. A resident graph refresh
reconciles durable graph and checkpoint progress without reverting live prompt,
model, or provider mutations that have not yet committed.

## Consequences

- Historical prompt-less bytes retain mainline behavior.
- Explicit emptiness survives a cold reopen as an empty session layer without
  erasing the live core prompt.
- Redeploying a Host Application with a new core prompt updates the next
  rendered request for an existing session; the old core prompt is never
  frozen into that session's durable configuration.
- A host can deliberately replace an old committed prompt at reopen without a
  compatibility shim or migration.
- Store implementations and fixtures must preserve prompt presence exactly;
  constructing durable configuration ad hoc is forbidden.
- The complete persisted session configuration, including its prompt, counts
  toward the Runtime Commit byte budget.

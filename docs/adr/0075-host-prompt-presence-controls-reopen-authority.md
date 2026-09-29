# Host prompt presence controls reopen authority

## Status

Accepted. Amended 2026-09-29 (FIG-4099): the reopen authority matrix is
superseded — a reopen runs with the recorded prompt and writes nothing. See
the amendment at the end.

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
ADR 0074's model and generation authority is unchanged by this ADR. *(Since
FIG-1875 a persisted model is kept when the host supplies none, and the host's
generation overlay merges over the persisted options; see the ADR 0074
amendment.)*

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

## Amendment (FIG-4099, 2026-09-29): the recorded prompt is authoritative

Prompt presence no longer controls reopen authority, because a reopen has no
authority over config at all. The session prompt is baked into the session's
config when it is created; a reopen runs with the recorded prompt, and a host
session prompt stated at the reopen is ignored. Every creating path — `open()`
of a new id, `create()`, `open_with_state()`/`observe_with_state()` and the
engine's own drive-open — passes the creator's config to the catalog as
`SessionStoreCreateRequest::config`, and the store writes it as the session's
initial config head in the same transaction as the catalog row
(`SessionHeadMeta::created`). The request says which head it carries:
host-facing creation states `SessionCreationHead::Config`, so the head is on
disk before the session is first materialized, and it includes the protocol turn
options the session's protocol resolves at creation (the RLM session config
among them). A core runtime binding state it was handed states
`SessionCreationHead::CommittedByCreator`, and its first commit writes the head,
so only the row is written at admission. Admitting an id that already exists
writes no config either way. The facade forms that config in one place,
`SessionBuilder::creation_config`. A reopen reads the recorded head and writes
nothing: there is no reconciliation, no seed write and no report, and builder
config stated on a reopen is ignored. Only live policy follows an open (the
session binding, turn budget, autonomy, no-progress budget and charge safety),
and a builder provider that cannot serve the recorded pin is still refused typed
(`ProviderMismatch`) without a write. Every later change is the one durable
command, `update(SessionConfigPatch)`, which covers provider, model, prompt,
generation, attachment acceptance and plugin session config.

The matrix above is superseded by one rule: the effective session prompt layer
is the recorded one. A legacy head with no prompt field reopens with an empty
session layer, which renders like a fresh session's ordinary reconstruction;
an explicitly empty committed layer stays empty; and "committed at the next
boundary" no longer happens, because the host prompt is not applied. The live
core prompt is unchanged: it is not session config, it is always rendered
beneath the session layer, and a redeployed core prompt still reaches existing
sessions.

The laws in `core_session_builder/prompt_reopen_authority.rs` now pin this:
`legacy_promptless_head_ignores_a_reopen_host_prompt_{in_memory,sqlite}`,
`open_with_state_runs_the_supplied_snapshot_prompt_not_the_builders` and
`a_reopen_host_prompt_is_ignored_and_update_recommits_the_prompt_{in_memory,sqlite}`.
The test names cited in the Context above describe the state before this
amendment.

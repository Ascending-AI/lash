# Durable session facts are a typed read and a guarded set-if-unset write

## Status

Accepted.

## Context

A durable session fact and a live open-time binding have different authority.
The recorded fact governs a reopened session. A provider resolver or plugin
factory supplies process-local wiring. Confusing the two lets defaults overwrite
recorded facts or encourages hosts to detect conflicts by matching error prose.

## Decision

A durable per-session fact has a typed read and a guarded set-if-unset write
with a typed conflict. Creation records the initial facts; opening reads them;
later changes use the durable session-config command.

### Read the recorded facts

`RlmSessionConfig` carries `final_answer_format` and `termination`, both as
`Option`. `None` means the session records no statement for that fact, which is
different from explicitly recording its default value. Read it through
`RlmSessionReadViewExt::rlm_config` or `RlmSessionExt::rlm_config`.

Reads return `Result`. Malformed recorded options refuse instead of defaulting.
The session's dialect is not an RLM fact: the host selects it where it
constructs the protocol, and the session records its language id beside the
channel in the protocol-turn options (ADR 0096).

### Creation and opening

`SessionBuilder::create(SessionCreation)` is the facade's creating operation.
`SessionCreation` carries the session spec, parent relation and plugin-keyed
creation options. Its `plugin_options` states RLM facts under
`RLM_PROTOCOL_PLUGIN_ID`. The protocol resolves these options before admission,
and the catalog writes the row and `SessionCreationHead::Config` in one
transaction. An existing id refuses with `SessionAlreadyExists`.

`SessionBuilder::open` opens an existing id and reads its recorded head.
`UnknownSession` and `SessionDeleted` are typed refusals. The builder carries
provider resolution, process-local plugin factories and tool-source policy;
it carries no replacement session config. A resolver that cannot serve the
recorded provider pin refuses with `ProviderMismatch`.

The protocol fills a missing final-answer format at creation: `Markdown` for a
root and `RawFinalValue` for a child. Termination has no default fill, so its
absence survives. Recorded options are read strictly and carried through
rematerialization without defaulting again. The kernel's creation path for state
bound by its creator uses `SessionCreationHead::CommittedByCreator`; its creator
commits the head.

### One guarded write

A host forms a patch with `lash::rlm::rlm_session_config_patch` and applies it
with `SessionConfigAdmin::update(SessionConfigPatch)`. The protocol's
`apply_session_config_patch` hook calls `apply_rlm_session_config_if_unset`:

1. An unstated field is carried through unchanged.
2. A stated field fills an absent recorded fact.
3. Restating the recorded value is a no-op.
4. Stating a different value refuses with `RlmSessionConfigConflict`, carrying
   the recorded and requested values and naming the fact.

The command settles a durable config write. Successful return means the patch
is durable. Conflicts travel as `SessionError::SessionConfigRefused` and are
read with `lash::rlm::rlm_session_config_conflict`; error prose is presentation.
The RLM patch preserves the recorded channel and dialect and refuses plugin
keys it does not accept.

Because creation fills the final-answer format, a later statement normally
agrees or refuses. Termination can be introduced later where absent. RLM
per-turn options have their own type and do not carry a dialect fact.

Assertion remains host code: read the fact, compare it with the host's
requirement, and fail if it differs. A host preference uses the guarded patch.
The protocol does not interpret environment variables or decide host policy.

## Alternatives considered

A request-shaped open parameter makes a durable fact appear replaceable and
requires conflict handling during ordinary reads. Explicit creation and a
separate durable update identify the write authority.

Writing the whole bag for one stated fact overwrites fields the caller never
mentions. Field-by-field set-if-unset preserves them.

`assert` and `prefer` modes in the open operation duplicate host decisions
already expressible with the typed read and guarded patch. Matching a refusal's
message makes wording part of the contract; the typed conflict carries the
information directly.

## Consequences

Creation facts exist before the first facade runtime opens. A reopen cannot
change them through builder defaults. A patch records an absent fact or confirms
an existing one; it cannot change an RLM fact already recorded. Hosts that need a
particular presentation format state it at creation.

## Executable evidence

- [Recorded facts and conflicts](../../crates/lash-rlm-types/src/lib.rs#L911)
  define the two fields and typed conflict values.
- [Strict decode](../../crates/lash-protocol-rlm/src/plugin/protocol_session.rs#L161),
  [guard](../../crates/lash-protocol-rlm/src/plugin/protocol_session.rs#L222),
  [materialization](../../crates/lash-protocol-rlm/src/plugin/protocol_session.rs#L275)
  and [patch hook](../../crates/lash-protocol-rlm/src/plugin/protocol_session.rs#L309)
  implement the recorded-options rules.
- [Facade creation](../../crates/lash/src/session.rs#L250) records the config head;
  [opening](../../crates/lash/src/session.rs#L164) reads an existing session.
- [Patch construction and conflict read](../../crates/lash/src/rlm.rs#L131) and
  [durable update](../../crates/lash/src/admin.rs#L1077) define the host API.
- [Session-fact laws](../../crates/lash/src/tests/core_session_builder/rlm_session_facts.rs#L1)
  cover creation, reopen, idempotence and conflicts.

# Durable session facts are a typed read and a guarded set-if-unset write

## Status

Accepted. The only RLM write after creation is the typed render command of
[ADR 0126](0126-session-config-changes-are-typed-owner-commands.md):
`SetRlmRender`.

## Context

A durable session fact and a live open-time binding have different authority.
The recorded fact governs a reopened session. A provider resolver or plugin
factory supplies process-local wiring. Confusing the two lets defaults overwrite
recorded facts or encourages hosts to detect conflicts by matching error prose.

## Decision

A durable per-session fact has a typed read. Creation records the initial
facts; opening reads them; a later change is a config command the fact's owner
registered (ADR 0126), and a fact with no command never changes.

### Read the recorded facts

`RlmSessionConfig` carries `termination` as an `Option`. `None` means the session records no statement for that fact, which is
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
the tool-source policy; it carries no replacement
session config. A registry that cannot bind the recorded model key refuses
its requests with `LlmProfileUnavailable`.

Termination has no default fill, so its absence survives creation.
`RlmConfigOwner::create` records the RLM namespace in the session's plugin config
(FIG-4379). Recorded options are read strictly and
delivered unchanged on every open, which never defaults again. The kernel's creation path
admits with `SessionCreationHead::Config` too: the catalog records the complete
config its first commit writes with the row, so a creator that dies before
that commit leaves a session that opens with what its creation recorded.

### No write after creation

The RLM owner registers `SetRlmRender`, which replaces the recorded print and
preview render and clears it when empty. The prompt is not recorded config:
the protocol contributes keyed sections that the host's `SetPromptPlan`
orders and places, and a host adds or wraps sections of its own
([ADR 0133](0133-prompt-sections-are-keyed-trusted-and-placed-by-the-host.md),
FIG-5257). Termination, the
channel and the dialect have no command, so a
transaction cannot name them: a host that states one is refused
`UnknownCommand` at submission. The owner's validation refuses a candidate
that moves the recorded channel or dialect with `RlmConfigRefusal::PinChanged`.
A run cannot name them either: its options are the owner's typed
`RlmTurnOptions`, which the owner applies over the recorded namespace, and a
payload that states a pin or the prompt does not decode (FIG-4652).
Refusals travel as data in the transaction's `Refused` outcome, where only the
`owner` reason carries the owner's refusal type; error prose is presentation.

Assertion remains host code: read the fact, compare it with the host's
requirement, and fail if it differs. A host that needs presentation guidance supplies its own prompt section. The protocol does not interpret
environment variables or decide host policy.

## Alternatives considered

A request-shaped open parameter makes a durable fact appear replaceable and
requires conflict handling during ordinary reads. Explicit creation and a
separate durable update identify the write authority.

A set-if-unset write on RLM facts would let a later statement fill a fact
creation left absent. Fixing the facts at creation is simpler: a fact with no
command cannot drift, and a host states what it needs when it creates.

`assert` and `prefer` modes in the open operation duplicate host decisions
already expressible with the typed read. Matching a refusal's
message makes wording part of the contract; the typed conflict carries the
information directly.

## Consequences

Creation facts exist before the first facade runtime opens. A reopen cannot
change them through builder defaults, and no config command changes them after.
Hosts supply presentation guidance through prompt sections.

## Executable evidence

- [Recorded facts](../../crates/lash-rlm-types/src/lib.rs#L960) define the recorded
  termination.
- [The RLM owner](../../crates/lash-protocol-rlm/src/plugin/config_owner.rs)
  creates, validates and registers `SetRlmRender`.
- [Facade creation](../../crates/lash/src/session.rs#L257) records the config head;
  [opening](../../crates/lash/src/session.rs#L167) reads an existing session.
- [Session-fact laws](../../crates/lash/src/tests/core_session_builder/rlm_session_facts.rs#L1)
  cover creation, reopen, the typed read and the render command.

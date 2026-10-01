# 0126: Session config changes are typed owner commands

## Status

Accepted.

## Context

A session records its config with its head: the core model, prompt,
generation, attachment acceptance, tool access, the execution controls
(turn budget, autonomy, no-progress budget and charge safety; FIG-4376), and
one namespace per installed plugin, the protocol's included (FIG-4379,
[ADR 0013](0013-protocol-capabilities-enter-through-the-plugin-contract.md)).
Changing that record through an open patch bag had two flaws. The bag named
fields rather than allowed changes, so every owner had to decide from a raw
JSON value which of its facts could move. And the patch was checked when it
was admitted, while the drain applied it later under whatever code was then
running.

## Decision

Every change to recorded session config is a typed command that the owner of
the namespace registered.

**Owners.** A plugin factory registers config through
`PluginFactory::register_config` before any session exists. It registers one
`ConfigOwner` for the namespace keyed by its plugin id, and the
`ConfigCommand`s that change that namespace. The owner:

- creates the namespace at session creation from the creator's typed input,
  its defaults and, for a child, its parent's recorded value;
- validates a final candidate, including a run override against the
  namespace it was derived from;
- names an implementation identity for its reducers.

A reducer is a pure function from the recorded namespace and the command to
the next namespace and the command's output. It sees immutable facts only. A
setting with no command cannot change after creation. Core config is owned
by the reserved `core` owner, whose commands include `SetModel`,
`SetReasoning`, `SetAttachmentAcceptance`, the prompt commands,
`SetGeneration`, `SetToolAccess`, `SetTurnBudget`, `SetAutonomy`,
`SetNoProgressBudget` and `SetChargeSafety`. `SetChargeSafety` refuses a
policy accepting more unsafe retries than the provider handle ever buys. The
RLM and standard protocols register one render command each
(`SetRlmRender`, `SetStandardRender`).

`SetModel` carries an opaque `ModelKey` (FIG-4374). Its reducer is the one
core reducer that reads more than the recorded namespace: it asks the host's
`RuntimeModels` catalog to mint a `RecordedModel` (the key and its metadata)
for the key, even when the key is the one already recorded. The resolution
records that binding, so a redrive or replay reuses it and never re-derives it
from a catalog that may since have changed. `SetReasoning` changes the
reasoning selection on the recorded model and keeps the key.

Each protocol namespace also records the session's behaviour at creation
(FIG-4398), from the creating deployment's factory configuration:
`RlmRecordedBehaviour` (execution bounds, Lashlang abilities and language
features, prompt features, output limit, soft-warning threshold, discovery
operation, configured render) and `StandardRecordedBehaviour` (discovery
operation, `batch` choice and maximum, configured render). A child session
records its parent's behaviour, not that of the host creating it (FIG-4527).
No command changes it, and each owner refuses a candidate, a run override
included, that changes its recorded behaviour. A plugin and its hooks run
under the recorded behaviour, never under the opening deployment's factory
configuration. A root's render is resolved over the recorded render, under
the session's own render options.

**Transactions.** A `ConfigTransaction` is an ordered list of
`{owner, command, args}` entries and applies all or none. A host submits it
under a `ConfigWrite { id, expected_revision }`:

- the id is stable, and a resubmission reuses it;
- the expected revision is the config revision the host read.

**Ingress** decodes every entry against the installed registrations. An
unknown owner or command, or arguments that do not decode, is refused as a
typed `ConfigSubmitError` and nothing is enqueued. An accepted transaction
records the implementation identity of each named owner and a digest of its
request, and rides `SessionCommand::ApplyConfigTransaction`. A resubmission
under the same id with other content is refused as `ChangedContent`.

**Resolution.** The drain applies each session command alone. It resolves
the transaction in one journaled `ResolveConfigTransaction` effect:

1. If an owner's installed implementation differs from the recorded one, the
   effect records nothing and the command root parks as `RetiredGeneration`
   until a build that runs the recorded reducers drives it.
2. If the config revision moved past the expected revision, the transaction
   resolves `Stale` without running a reducer.
3. Otherwise the entries reduce in order over a private candidate. Every
   owner with a recorded namespace then validates the final candidate; when
   core changed, core validates the reasoning selection against the final
   recorded model.
4. The result is `Applied`, with the replacement namespaces and the outputs,
   or `Refused`, with the refusing entry, the owner and the owner's typed
   refusal.

A redrive replays the recorded resolution and never re-runs a reducer under
new code.

**Publication.** One fenced host-command commit publishes an applied
resolution's replacements, advances `config_revision` by exactly one, and
records the outcome as the command's settlement. A restatement advances the
revision too. A stale or refused transaction publishes no config, and its
outcome is still durable.

**Pending while a root owns the head.** A transaction submitted while a root
owns the session's head stays pending. It applies after that root releases
the head and is first visible to the next root. A running root never sees
config change under it.

**Discovery and transport.** `SessionConfigAdmin::commands` returns a catalog
generated from the registrations: each command's owner, name and
input/output/refusal schemas, and the config revision it describes. The
remote envelope (`RemoteConfigTransactionRequest`, `RemoteConfigTransactionOutcome`,
`RemoteConfigCommandCatalog`) carries the same commands and outcomes, never a
recorded namespace or a caller-minted replacement.

## Why and alternatives

- **An open patch bag with owner merge functions.** Rejected. It leaves every
  owner parsing an untyped value and cannot publish what may change.
- **Validation at submission only.** Rejected. The drain runs later, possibly
  under other code, against a config that may have moved.
- **Re-running reducers on redrive.** Rejected. A resolution is a recorded
  fact, and a new build must not rewrite it.
- **Coalescing adjacent config commands into one admission.** Rejected. Each
  transaction is its own run, so its revision check, resolution and receipt
  stay its own.

## Consequences

- `SessionConfigPatch`, `ApplyConfigPatch`, `SessionConfigAdmin::update`, the
  prompt and tool-access setters, the raw protocol-options setters and
  `PluginFactory::{resolve_session_config, patch_session_config}` are gone.
- An unknown model key and a reasoning selection the recorded model does not
  support are refused at resolution and settle as a typed `Refused` outcome
  (`CoreConfigRefusal::UnknownModel`, `ReasoningRefused`), not as a send-time
  error.
- A config command added later, such as a new core execution control,
  registers on its owner and joins the same resolver, catalog and envelope.

Sources: `crates/lash-core-execution/src/plugin/config/mod.rs`,
`crates/lash-core-execution/src/plugin/config/core.rs`,
`crates/lash-core-store/src/config_transaction.rs`,
`crates/lash-core/src/runtime/config_transaction.rs`,
`crates/lash/src/admin/config_transactions.rs` and
`crates/lash-remote-protocol/src/config.rs`.

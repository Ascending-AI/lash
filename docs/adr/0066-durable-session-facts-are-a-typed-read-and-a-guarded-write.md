# Durable session facts are a typed read and a guarded set-if-unset write

## Status

Accepted.

## Context

A session carries two different kinds of configuration and they had one shape.

ADR 0030 configuration — model, turn budget, generation options, the prompt
layer — is *host-wins*: the host resolves it when it constructs or reopens a
session, and the value it supplies is the value the session runs with. Passing
that at open is right. Re-passing it is right. A host that changes its mind
changes the value.

A *durable per-session fact* is the opposite. The RLM source dialect, the
final-answer format, the session-wide termination requirement: these are pinned
for the session's life. The host does not decide them on reopen; the session
already did. But they were expressed in the same shape as host-wins config —
open-time builder parameters, `.rlm_dialect(X)` and `.final_answer_format(F)` —
and a request-shaped API for a fact the caller cannot actually change produces a
predictable set of defects, all of which we shipped:

* **Prose refusals, matched as prose.** The pin conflict was a
  `SessionError::Protocol(String)`. Two reference hosts needed to tell it apart
  from every other protocol failure, so both wrote
  `error.to_string().contains("RLM dialect is durably pinned")` — with a test
  pinning the exact sentence, in each host. Matching a typed refusal on its
  message is itself the defect shape: the refusal's wording became load-bearing
  API, and the check silently answers "no" the moment anyone rewords it.

* **Catch-and-fallback smoothing.** Because the request could be refused and the
  host had a way to detect the refusal, both hosts did the natural thing: catch
  it and reopen *without* the dialect. A session whose recorded dialect
  disagreed with the operator's `LASH_RUNBOOK_DIALECT` opened anyway, in its old
  dialect, with every route green — and the operator's configuration quietly
  ignored. `runbooks/RULES.md` documented this as expected behaviour, which is
  how we know it was load-bearing rather than accidental.

* **Absence that could not be read.** The typed read returned
  `RlmDialect`, not `Option<RlmDialect>`, so "this session pinned Lashlang" and
  "this session pinned nothing" were the same answer. A host that needed to tell
  them apart — the workbench, to badge a session honestly — peeked at the raw
  option payload for the presence of a `"dialect"` key.

* **Silent clobbers.** Applying a request over a durable bag means writing the
  whole bag. Two facts got restated by callers that never mentioned them: every
  reopen that did not name a final-answer format reset it to the root default,
  and a bare `.rlm_dialect(X)` constructed full options carrying the *default*
  termination, resetting a recorded `FinishRequired` to `Natural`. Neither was
  reachable through a refusal. They just happened.

The in-tree counter-example already existed. `lash-subagents` reads the parent
session's recorded dialect and writes it forward onto the child through the
plugin-agnostic options seam. No refusal is possible, so no fallback exists, so
nothing smooths anything over.

## Decision

**A durable per-session fact is exposed as a typed config read plus a guarded
set-if-unset write with a typed conflict refusal. It is never a request.**

Open-time parameters are reserved for ADR 0030 host-wins configuration (model,
turn budget, generation) and live wiring (store, provider handle, plugin
factories).

Concretely, for the RLM bag — all three facts, because they share one durable
bag and one materialization hook:

1. **Read.** `RlmSessionConfig` carries `dialect`, `final_answer_format` and
   `termination`, each `Option`-shaped, read as recorded. `None` means the
   session has stated nothing, which is a different answer from the value its
   default resolves to. It is available on `SessionReadView`
   (`RlmSessionReadViewExt::rlm_config`) and on an opened session
   (`RlmSessionExt::rlm_config`).

2. **Guarded write.** `apply_rlm_session_config_if_unset` is the single engine:
   each fact the request states is written only where the session recorded
   nothing, restating a recorded fact is a no-op, and stating a *different*
   value refuses. Facts the request leaves unstated are carried through
   untouched — which is what closes both clobbers. Durability is unchanged: the
   pin lands with the session's next commit.

3. **Typed refusal.** `RlmSessionConfigConflict` names the fact and carries both
   the `recorded` and the `requested` value. Its `Display` is the one place any
   prose for a refused pin is produced. No host matches a message to tell a pin
   conflict from anything else.

4. **Assertion is host code.** `assert` is comparing the read against what you
   require and failing loudly. `prefer` is the guarded write. Both are
   one-liners in the host, and where the host sources its answer — an
   environment variable, a roster row, a create form — is host policy that stays
   out of core. `LASH_RUNBOOK_DIALECT` lives in the reference hosts, not in the
   protocol.

The RLM-specific request sugar is removed wholehog. A host that states a durable
fact when a session opens does so through the plugin-agnostic options seam every
plugin shares (`SessionBuilder::plugin_option` keyed by
`RLM_PROTOCOL_PLUGIN_ID`) — the same seam `lash-subagents` already writes a
parent's dialect forward through — and that statement is applied by the guarded
engine above, refusing with the same typed conflict.

### The one boundary this decision does not cross

The protocol plugin selects its dialect implementation when a session's plugins
are built, which happens before any post-open write can reach it: the active
dialect is captured by the prompt projector, the protocol driver, the prose
projector, the control-tool vocabulary and the stream mask at registration. So a
dialect a session has never recorded cannot be *introduced* after that session is
open — `set_rlm_config_if_unset` confirms the dialect the session resolved and
refuses a different one, and a session states its dialect when it opens. That is
a deliberate boundary, not an oversight: a live dialect re-selection would make
the one fact this layer exists to pin mutable mid-life, and it is not needed
once a host can read the recorded dialect *before* opening. That pre-open read is
FIG-1556's preflight surface.

The guarded write on an open session is therefore narrower than the engine
underneath it, and the boundary is worth stating exactly, because two of the
three facts are already decided by the time a host can call it:

* **Dialect** — compared, never written. A session that recorded no dialect is
  still *running* one (the plugin resolved the default), so the comparison is
  against the running dialect rather than against the recorded `Option`.
  `apply_rlm_session_config_post_open` is the one place that holds the field
  back; writing it would leave the recorded fact disagreeing with the plugin
  that is executing.
* **Final-answer format** — default-filled at the same first open, so post-open
  a statement can only agree (no-op) or disagree (refuse). It is writable in the
  engine, and it is what a session's *first* open states; it is not a fact a
  host adds later.
* **Termination** — genuinely settable after the fact. It has no default fill,
  so `None` survives the first open and the guarded write is the way a host
  records it.

Defaults are filled only on a session that has recorded nothing. That is what
pins a session's dialect at its first open, exactly as before, while making a
reopen incapable of re-defaulting — the mechanism behind both clobber fixes.

## Alternatives considered

* **Pass the dialect only when creating.** Rejected: there is no create/resume
  split in the API, and the pin lands at the first *commit*, not at open. This
  was already tried and reverted — the two call sites that "create" both open and
  drop without running a turn, so the pin evaporated with the handle and the
  first real turn committed the default permanently. A workbench told to serve
  TypeScript served Lashlang.

* **Assert the configured dialect on every open.** Rejected: it conflates
  *stating* configuration with *asserting* a requirement, and would refuse every
  route against a legitimately mixed store — a service holding sessions from
  before a configuration change could not open any of them. Asserting is a
  choice a host makes per requirement, not a property of the open call.

* **`assert` / `prefer` intent modes baked into the open call.** Rejected as
  unnecessary machinery: once a typed read and a guarded write exist, both modes
  collapse to one line of host code each, and the host can compose them — assert
  on one fact, prefer on another — without core knowing the difference.

* **Keeping the prose refusal alongside the typed one.** Rejected: a message a
  caller is expected to match is API, and two copies of an API drift. The
  message survives only as the typed error's rendering, for the operator.

## Consequences

* `.rlm_dialect(X)` and `.final_answer_format(F)` are gone. Callers state
  durable facts through the plugin options seam, or write them post-open through
  the guarded write.
* `RlmCreateExtras::termination` becomes `Option<RlmTermination>`. Absence and an
  explicit `Natural` are now different statements; only a stated value
  participates in the guard. Existing durable state decodes unchanged, and a bag
  that omits the key reads as absent.
* Both reference hosts lose their `is_dialect_pin_conflict` string match, their
  message-string tests and their catch-and-reopen fallbacks. A genuine mismatch
  now reaches the operator.
* `runbooks/RULES.md` no longer documents a fallback, because there is none: a
  carried-over store opened under the other row's `LASH_RUNBOOK_DIALECT` fails
  the open instead of serving the recorded dialect behind green routes. A fresh
  data directory per row is still required, now for evidence purity rather than
  to avoid a silent mislabel.
* Default-filling at the first open is now a pin, honestly: a dialect and a
  final-answer format the host never chose become unchangeable for the life of
  the session, and a reopen stating a *different* format is refused where it
  previously won by silently overwriting. That is the price of killing the
  clobber — the last writer no longer wins — and it is why a host that cares
  states its facts at the first open rather than later.
* Sibling facts follow the same shape rather than growing their own request
  parameters: the provider pin is FIG-1558 and the parent-relation rebind is
  FIG-1559.

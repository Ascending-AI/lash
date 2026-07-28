# Generation intent is session policy, and its fate on the wire is reported

`GenerationOptions` is caller intent, but only the one-shot direct path had a caller-owned
slot for it. Agent sessions synthesized their options from provider configuration —
copying `ProviderOptions.max_output_tokens` onto the request, where the adapter's own
resolver layered the same provider cap a second time, and hard-coding `temperature: None,
seed: None` no matter what the caller wanted. A host benchmarking agent behavior could not
make a turn repeatable and had no way to learn that from lash.

We decided that **generation intent belongs to the session policy**. `SessionSpec.generation`
is the public overlay and it resolves into `SessionPolicy.generation`, the value every LLM
call in the session carries. The policy is the right home rather than the spec alone,
because subagent specs resolve against the parent's *live* policy, and because every
carrier of a whole session policy — the current agent frame's assignment, the persisted
state, `RemoteProcessExecutionPolicy` on the wire — then carries the sampling intent with
it instead of silently dropping it at each boundary. Session-wide means session-wide: the
direct requests plugins issue on the session's behalf (the observational-memory workers,
the `llm_query` tool) read it from the policy they already read their model from, rather
than passing `GenerationOptions::default()` and running at provider defaults inside a
session that asked for repeatability. Provider configuration keeps its own
layer underneath the request, applied once, by the per-adapter resolver that already knows
which wire has a seed field and which model pins sampling. There is no per-turn override:
no caller in this workspace expresses sampling per turn, and true per-call intent is
already served by a direct request.

The overlay **merges per option**, and discarding what is inherited has to be asked for
(`GenerationOverlay::Replace`, spelled `.replace_generation(...)` / `.clear_generation()`).
`GenerationOptions` is three independently optional options rather than one value, so
wholesale replacement would let a subagent spec that sets only an output-token cap drop a
parent's pinned temperature and seed — and the disposition below could not report that,
because the child never *requested* a temperature. Per-option layering is also what the
layer directly underneath already does: `resolve_generation_policy` resolves a request's
cap against provider configuration with `.or(...)`.

Reopen follows ADR 0030 rather than inventing its own authority: **the host's configuration
wins, for generation exactly as for the model and the prompt.** The facade reconciles the
policy it resolves from the host's spec over loaded state, so a mid-run
`update_session_config` change lasts until the host reopens with a spec that says otherwise
— the same lifetime a mid-run `set_model` has. Pairing the host's new model with the
store's old temperature would be the anomaly. The persisted copy is what a session resumes
with wherever no live host reconciles it: the process/remote path, and core embedders that
hand `LashRuntime` a loaded state directly. A facade host that wants the recorded options
back reads them from the store and supplies them, as it does for any other policy field.

Session-wide defaults make the adapter's silent omissions matter. Anthropic drops a
caller-set temperature when the model's host-declared capability pins sampling or extended
thinking does, Messages and Responses have no seed field, and Codex sends none of the three.
Erroring instead would make a session-wide default unusable on any mixed-model session — a
host would need every model's capability before setting one — but silence is how a
repeatability request goes unhonored without anyone noticing. **Omission stays silent and
becomes observable**: `GenerationDisposition` records, per option, whether the caller
requested it and whether the assembled body carries it, with the reason when it does not.
Adapters derive it from the body they just built, so the report cannot drift from what was
sent. It rides the response, the per-attempt ledger of ADR 0032, the durable effect journal,
the trace record, and the remote mirror, so a host asserts "nothing was dropped" instead of
trusting that one temperature survived every model a run touched.

`output_token_cap` **clamps rather than fails**, for the same reason. It is the one option
a model can refuse arithmetically, and it was validated hard: a cap above the model's
`output_token_capacity` failed the call non-retryably. As per-turn intent that was a loud,
local error; as durable session policy it is a session that fails *every remaining turn*
after a `set_model` to a smaller model, and the only fail-closed field in an otherwise
fail-silent struct. The cap is a bound, not a demand — a request for at most 32k is
satisfied by a model that can only produce 8k — so the turn sends the capacity and reports
`ClampedToCapacity`. The runtime is the only layer that saw both numbers, so it narrows the
adapter's `Applied` on the response and on every attempt of the ledger together, and
`nothing_omitted()` (nothing was dropped) is joined by `fully_honored()` (nothing was
dropped *or* reduced) for a host that needs the number it named.

The disposition is deliberately *not* part of `ExecutionEvidence`. ADR 0031 binds that type
to facts the provider reported about the execution; this is a fact about the request lash
built, and folding request-side bookkeeping into provider-reported evidence would dissolve
the distinction that makes evidence falsifiable. `None` on either type keeps meaning
unreported: a third-party provider that does not report is distinguishable from one
reporting that nothing was requested.

We rejected a typed pre-flight check for the unsatisfiable combination that motivated this
(a seed against a routing pool whose backends have no seed field). `SamplingCapability` is
temperature-only, so a check built on today's capability model would silently narrow to
temperature and miss exactly the failure it was written for. That needs a typed seed
capability first, and until then the disposition report tells a host what happened after the
call rather than guessing before it.

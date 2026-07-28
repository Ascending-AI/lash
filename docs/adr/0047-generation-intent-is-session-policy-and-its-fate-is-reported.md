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
carrier of a whole session policy — `SessionCreateRequest` on the child-session wire,
`RemoteProcessExecutionPolicy` on the process/remote wire — then carries the sampling
intent with it instead of silently dropping it at each boundary. Session-wide means session-wide: the
direct requests plugins issue on the session's behalf (the observational-memory workers,
the `llm_query` tool) read it from the policy they already read their model from, bounded by
that model's capacity, rather than passing `GenerationOptions::default()` and running at
provider defaults inside a session that asked for repeatability. Provider configuration keeps its own
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
cap against provider configuration with `.or(...)`. `GenerationOverlay` is the vocabulary
for *every* surface that layers these options, not just the spec: `SessionConfigPatch`
carries one too, so a mid-run patch naming only a cap cannot drop a pinned temperature that
the spec's overlay would have kept. Two surfaces for one field, disagreeing about whether
naming one option discards the others, is the trap dressed as an API.

We considered a per-option clear — a `GenerationOverlay` of three `Option<Option<_>>`
fields, able to drop an inherited seed while keeping an inherited temperature — and
accepted the coarser gesture. No caller expresses that, none of the references offer it
(pydantic-ai, the Agents SDK and flue all merge with no per-field clear at all), and the
cost of being wrong is one additive field change on a type whose two variants would become
three. What could not be added later is the *default*: merge-by-default is the behavior
callers write against, and flipping it after release breaks silently rather than at compile
time.

Reopen follows ADR 0030 rather than inventing its own authority: **the host's configuration
wins, for generation exactly as for the model and the prompt.** The facade reconciles the
policy it resolves from the host's spec over loaded state, so a mid-run
`update_session_config` change lasts until the host reopens with a spec that says otherwise
— the same lifetime a mid-run `set_model` has. Pairing the host's new model with the
store's old temperature would be the anomaly.

That is the whole story on the durable side, because the session store is not a carrier of
generation intent at all: what it records of a session's configuration is
`PersistedSessionConfig` — provider id and model, the two facts that identify what produced
the history. Generation options travel on the policy through the seams that hand a *whole*
policy across a boundary and find no host on the far side: a process/remote execution
environment, and a `RuntimeSessionState` a core embedder holds and hands back to
`LashRuntime`. Forking is the case that makes the rule visible. A fork creates a session
head at a retained point, not a second authority over configuration, so a branch resolves
the host's spec when it opens exactly as a reopen of its source would; only `provider_id`
comes from the record, naming the provider that produced the history the branch continues.

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

Clamping belongs to the **policy**, not to the turn, so it lives on `ModelSpec` and every
path that takes generation options *out of a session policy* applies it. The turn driver is
not the only such path: the direct requests plugins issue on the session's behalf carry the
policy's options too, and `DirectRequest` has no `ModelLimits` to check a cap against. Left
to the turn path alone, the fix would have produced the worse version of the same failure —
after a `set_model` to a smaller model, turns clamp and proceed while every maintenance call
fails at the provider. So the observational-memory workers and `ToolSessionAdmin` hand out
options already bounded by the model the same policy names, and a tool that substitutes its
own model owns that pairing. Clamping is per request, against the model the request runs on;
the session's stored intent is never rewritten.

One case does lose a named local error. Provider configuration reaches the wire through
`resolve_generation_policy`, underneath the request, and is not caller intent, so nothing
checks it: a `ProviderOptions.max_output_tokens` above the model's capacity now fails at the
provider with its own 400 rather than as `output_token_cap_exceeds_model_capacity`. Before
this change, agent-session requests synthesized their cap *from* that provider config, which
is what put it in front of the check. Host configuration validated against host-declared
capability is a check worth having, but it belongs where provider options are configured,
not smuggled in through a request field that no longer carries them.

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

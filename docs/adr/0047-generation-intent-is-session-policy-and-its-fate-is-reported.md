# Generation intent is session policy, and its fate on the wire is reported

`GenerationOptions` is caller intent, but only the one-shot direct path had a caller-owned
slot for it. Agent sessions synthesized their options from provider configuration —
copying `ProviderOptions.max_output_tokens` onto the request, where the adapter's own
resolver layered the same provider cap a second time, and hard-coding `temperature: None,
seed: None` no matter what the caller wanted. A host benchmarking agent behavior could not
make a turn repeatable and had no way to learn that from lash.

We decided that **generation intent belongs to the session policy**. `SessionSpec.generation`
is the public overlay — inherit when absent, replace when present — and it resolves into
`SessionPolicy.generation`, the durable value every LLM call in the session carries. The
policy is the right home rather than the spec alone, because subagent specs resolve against
the parent's live policy and resume stamps live policy over loaded state: a spec-only seam
would leak uncontrolled sampling into child sessions and lose the options on recovery.
Provider configuration keeps its own layer underneath the request, applied once, by the
per-adapter resolver that already knows which wire has a seed field and which model pins
sampling. There is no per-turn override: no caller in this workspace expresses sampling per
turn, and true per-call intent is already served by a direct request.

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

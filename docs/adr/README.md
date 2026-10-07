# Architecture decisions

ADRs describe the design on main and the durable end state of
[ADR 0132](0132-durability-is-state-first-over-the-lash-store.md), which the
substrate lanes implement. History lives in git. Keep one numbered
file per decision and rewrite that file in place. The convention applies to
every ADR, including decisions maintained by implementation tickets.

## Convention

- State the problem the design solves, the decision, its rules and guarantees, its consequences, and the reasons for rejecting alternatives that remain plausible today. Use present tense.
- Do not add amendment sections or blocks, superseded-by or supersedes notes, previously/formerly/used-to/no-longer/was-retired/originally narration, dated change notes, or ticket-by-ticket history.
- Ticket IDs may appear only as pointers to executable evidence such as tests and gates, or to open work that the ADR explicitly depends on.
- Delete an ADR when its decision is entirely retired. Never reuse its number. Update every citation in code, tests, scripts and docs to the ADR that owns the behaviour, or remove the citation when no ADR owns it.
- While code on main still cites a replaced or retired ADR, keep its file until the lane that removes that code deletes it. Its `## Status` states, in present tense, either `Replaced by` its successor and section or `Retired:` with the reason. Trim its body to a short note, and keep each heading that code cites as a one-line pointer to the owning section.
- Preserve section headings and section numbers that code cites when their content survives. Update every affected citation in the same change when a cited section is removed or renumbered.
- Code on main is the truth, except where ADR 0132 governs durability: there the ADRs state the end state, and the gap between it and code on main is the substrate lanes' open work. Verify every other retained claim against the code. If a claim and the code disagree, record both file:line references in the lane report and state which you think is right. Do not silently choose a design or mark the ADR. The orchestrator decides whether the code needs a bug ticket or the text needs a correction.
- Keep this convention and the live index in this README. Each index row gives a decision's number and title.

## Repository check

Run `python3 scripts/check_adr_current.py` and its self-tests with
`python3 scripts/test_check_adr_current.py`. CI's repository script gates run
both commands. `scripts/ci/repository-gates.sh` reads the same command list
for local validation. These commands need only Python's standard library.

The check scans text in `crates/`, `scripts/`, `runbooks/`, `examples/`, `docs/`
and `tests/`. It resolves numbered ADR citations, linked citations and their
section numbers. Within an ADR, a bare section reference names that ADR's own
heading; an explicit "its" or "there" can refer to a nearby cited ADR in the
same paragraph. Section numbers must appear in Markdown headings outside code fences.

Narration, citation and index findings all fail the check. The check's small
allowlist names exact exceptions with reasons: the convention's list of
prohibited words and a present-tense size comparison. It does not exempt
entire ADRs.

Run `python3 scripts/check_adr_current.py --write-index` after changing a title,
adding a decision or deleting one. Commit the README change with the ADR change.
The generated region below is checked against the live filenames and headings.

## Live decisions

<!-- adr-index:start -->
| Number | Decision |
| --- | --- |
| 0001 | [Context management uses views or frames](0001-context-management-uses-views-or-frames.md) |
| 0002 | [Session observation uses cursors and bounded live replay](0002-session-observation-uses-cursors-and-bounded-live-replay.md) |
| 0003 | [Durable waits are scoped wait rows with a first winner](0003-keyed-promise-is-scope-agnostic.md) |
| 0004 | [Process environments carry plugin options, not product metadata](0004-process-environments-carry-plugin-options-not-product-metadata.md) |
| 0005 | [Tool Catalog membership defines availability](0005-tool-catalog-membership-replaces-availability-tiers.md) |
| 0006 | [RLM history renders in the emission format](0006-rlm-history-renders-in-emission-format.md) |
| 0007 | [Four layer scenario harnesses](0007-four-layer-scenario-harnesses.md) |
| 0008 | [Confidence gate](0008-confidence-gate.md) |
| 0009 | [Randomized simulation harness](0009-deterministic-simulation-harness.md) |
| 0011 | [Self-contained processes capture their environment at creation](0011-self-contained-processes.md) |
| 0013 | [Protocol capabilities enter through the plugin contract](0013-protocol-capabilities-enter-through-the-plugin-contract.md) |
| 0014 | [Operational policy stays with the host and Lash exposes levers](0014-operational-policy-stays-with-the-host.md) |
| 0015 | [Admission control lives in provider decorators](0015-admission-control-lives-in-provider-decorators.md) |
| 0016 | [Process waits live on the work-driver seam](0016-process-waits-live-on-the-work-driver-seam.md) |
| 0017 | [Process observation is best-effort push over state truth](0017-process-observation-is-best-effort-push-over-state-truth.md) |
| 0018 | [Per-tool telemetry emits from one shared seam](0018-per-tool-telemetry-emits-from-one-shared-seam.md) |
| 0020 | [Process change feed is a record-level cursor read](0020-process-change-feed-is-a-record-cursor-read.md) |
| 0021 | [Trigger deliveries are first-class and recoverable](0021-trigger-deliveries-are-first-class-and-recoverable.md) |
| 0022 | [Host originators carry named scopes](0022-host-originators-carry-named-scopes.md) |
| 0023 | [Retention stays a parameterized host lever](0023-retention-stays-a-parameterized-host-lever.md) |
| 0024 | [Drainage reads over artifact references](0024-drainage-reads-over-artifact-refcounts.md) |
| 0026 | [Model capability is host-supplied data and providers are executors](0026-model-capability-is-host-supplied-data.md) |
| 0027 | [Process completion carries explicit authority](0027-unleased-completion-carries-explicit-authority.md) |
| 0028 | [Attachments have blob storage, reference tracking and host lifecycle policy](0028-attachments-are-three-layers-blob-reference-lifecycle.md) |
| 0030 | [The session model is recorded at creation](0030-the-session-profile-is-resolved-once-at-open.md) |
| 0031 | [Execution evidence is provider-reported fact](0031-execution-evidence-is-provider-reported-fact.md) |
| 0032 | [Attempt history rides inside the result](0032-attempt-history-rides-inside-the-result.md) |
| 0033 | [Final-output attribution is host policy](0033-final-output-attribution-is-host-policy.md) |
| 0034 | [Harness evolution lives outside the runtime repository](0034-harness-evolution-lives-outside-the-runtime-repository.md) |
| 0035 | [Frontends are independent host applications](0035-frontends-are-independent-host-applications.md) |
| 0036 | [Stream termination is explicit dialect policy](0036-stream-termination-is-explicit-dialect-policy.md) |
| 0037 | [Lashlang workflows use a code-graph-code lens](0037-lashlang-workflows-use-a-code-graph-code-lens.md) |
| 0038 | [Response metadata is allowlisted host-supplied capture](0038-response-metadata-is-allowlisted-host-supplied-capture.md) |
| 0039 | [Turn cancellation is a first-party work-driver primitive](0039-turn-cancellation-is-a-first-party-work-driver-primitive.md) |
| 0040 | [Retried model attempts retract live text by correlation](0040-retried-model-attempts-retract-live-text-by-correlation.md) |
| 0041 | [Child-turn and driver stack growth have canonical seams](0041-child-turn-and-driver-stack-growth-have-canonical-seams.md) |
| 0042 | [Tool attempts are atomic](0042-tool-attempts-are-atomic.md) |
| 0044 | [Tests must be independent of what they test](0044-tests-must-be-independent-of-what-they-test.md) |
| 0045 | [Services are stateless; the store owns continuation](0045-services-are-stateless-substrates-own-continuation.md) |
| 0046 | [Process transitions are events; the record is a fold](0046-process-transitions-are-events-record-is-a-fold.md) |
| 0047 | [History is shared; branches are sessions](0047-history-is-shared-branches-are-sessions.md) |
| 0048 | [Checkpoint component identity is a backend contract](0048-checkpoint-component-identity-is-a-backend-contract.md) |
| 0049 | [Session ids are used once](0049-session-ids-are-used-once.md) |
| 0050 | [Behavior transcripts are one normalized vocabulary](0050-behavior-transcripts-are-one-normalized-vocabulary.md) |
| 0051 | [The facade is the host API; core exposes integrator seams](0051-the-facade-is-the-host-api-core-is-integrator-seams.md) |
| 0052 | [The Postgres schema is a published artifact lash verifies at open](0052-the-postgres-schema-is-a-published-artifact-lash-verifies.md) |
| 0054 | [Host panics are contained and standard-lock poison is recovered](0054-host-panics-are-contained-and-lock-poison-is-recovered.md) |
| 0055 | [Lashlang execution bounds span durable process lifetimes](0055-lashlang-execution-bounds-span-durable-process-lifetimes.md) |
| 0056 | [Checkpoint components generalize to a keyed set](0056-checkpoint-components-generalize-to-a-keyed-set.md) |
| 0057 | [History generations accelerate edge-authoritative reads](0057-history-generations-accelerate-edge-authoritative-reads.md) |
| 0058 | [Runtime commit budgets are explicit host policy](0058-runtime-commit-budgets-are-explicit-host-policy.md) |
| 0059 | [Tool-call directives compose monotonically](0059-before-tool-call-directives-compose-monotonically.md) |
| 0060 | [The lashlang VM is a heap substrate with dialect-lowered value semantics](0060-the-lashlang-vm-is-a-heap-substrate-with-dialect-lowered-value-semantics.md) |
| 0061 | [RLM dialects share one IR and VM](0061-two-first-class-rlm-dialects-with-full-parity-and-session-pinning.md) |
| 0062 | [The TypeScript dialect is an exact ECMA-262 subset](0062-the-typescript-dialect-is-an-exact-ecma-262-subset.md) |
| 0063 | [One RLM turn is prompted in its dialect](0063-one-rlm-turn-is-prompted-in-one-dialect.md) |
| 0064 | [The TypeScript dialect is broad, and every gap is an explicit ruling](0064-the-typescript-dialect-is-broad-and-every-gap-is-an-explicit-ruling.md) |
| 0065 | [Concurrent settlement is recorded by the logical Run](0065-concurrent-settlement-is-a-durable-group-at-the-effect-host-seam.md) |
| 0066 | [Durable session facts are a typed read and a guarded set-if-unset write](0066-durable-session-facts-are-a-typed-read-and-a-guarded-write.md) |
| 0067 | [Every durable row names one owner and one reclaim trigger](0067-durable-rows-name-one-owner-and-one-reclaim-trigger.md) |
| 0068 | [One meaning per outcome-type suffix](0068-one-meaning-per-outcome-suffix.md) |
| 0069 | [Durable acceptance is the sole turn ingress](0069-durable-acceptance-is-the-sole-turn-ingress.md) |
| 0070 | [Cache capabilities are host-supplied data](0070-cache-capabilities-are-host-supplied-data.md) |
| 0071 | [Engines emit unified tool-call accounting outside model projection](0071-engines-emit-unified-tool-call-accounting.md) |
| 0073 | [Gradual value types through to the workflow editor](0073-gradual-value-types-through-to-the-workflow-editor.md) |
| 0074 | [Generation intent is session policy, and its fate on the wire is reported](0074-generation-intent-is-session-policy-and-its-fate-is-reported.md) |
| 0076 | [Durable VM state preserves shared references and owns its roots](0076-lashlang-durable-stores-hold-exclusively-owned-copies.md) |
| 0077 | [Session state admits one compatible continuation generation](0077-session-state-migrates-totally-at-admission.md) |
| 0078 | [Plugin state is a lash-mediated per-plugin store](0078-plugin-state-is-a-lash-mediated-per-plugin-store.md) |
| 0079 | [One promised package: the facade owns the API](0079-one-promised-package-facade-owns-the-api.md) |
| 0081 | [SQL stores refuse unsupported schemas and report the writing release](0081-destructive-schema-changes-are-currently-reject-and-recreate.md) |
| 0082 | [The process registry is composed from narrow concern traits](0082-process-registry-is-composed-from-narrow-concern-traits.md) |
| 0083 | [RLM channels are pinned when a session materializes](0083-rlm-native-tool-channel.md) |
| 0084 | [Separate initial instructions from positional runtime feedback](0084-runtime-feedback-position.md) |
| 0085 | [RLM prompts teach only enabled capabilities](0085-rlm-prompt-teaches-only-enabled-capabilities.md) |
| 0086 | [Aggregate await operates on handles](0086-aggregate-await-shapes-and-question-placement.md) |
| 0087 | [TypeScript aggregates evaluate runtime arrays](0087-typescript-runtime-promise-arrays.md) |
| 0088 | [Facade sessions bind storage and lifecycle owners](0088-facade-sessions-bind-storage-and-lifecycle-owners.md) |
| 0089 | [Parent relationships do not define a second session model](0089-parent-relationships-do-not-define-a-second-session-model.md) |
| 0090 | [Named process signatures are authoritative](0090-named-process-signatures-are-authoritative.md) |
| 0091 | [One lowering walk owns expression semantics](0091-one-lowering-walk-owns-expression-semantics.md) |
| 0092 | [Agent frame scope is explicit and resolvable](0092-explicit-agent-frame-scope.md) |
| 0093 | [Artifact lifetimes use exact owner edges](0093-artifact-lifetimes-use-exact-owner-edges.md) |
| 0094 | [Child lifecycle is a registration fact settled by scope end](0094-child-lifecycle-is-a-registration-fact-settled-by-scope-end.md) |
| 0095 | [Processes are values, process controls are tools, one handle kind](0095-processes-are-values-and-process-controls-are-tools.md) |
| 0096 | [One IR and VM, extensible dialects, TypeScript today](0096-typescript-is-the-sole-rlm-dialect.md) |
| 0097 | [Commit-identity families mint frozen unframed preimages](0097-commit-identity-families-mint-frozen-unframed-preimages.md) |
| 0098 | [One owner per SQL table across both stores](0098-one-owner-per-sql-table-across-both-stores.md) |
| 0099 | [Tool calls and aggregates belong to the logical Run](0099-tool-children-of-effect-groups-are-live-closing-settled.md) |
| 0100 | [The run-observation contract](0100-the-run-observation-contract.md) |
| 0101 | [One session ingress carries every admitted item](0101-one-session-ingress-carries-every-admitted-item.md) |
| 0102 | [Every backend binds one durable engine to one store set](0102-zero-infra-is-a-sqlite-in-memory-backend.md) |
| 0103 | [Code cells replay by re-execution on every host](0103-code-cells-replay-by-re-execution-on-every-host.md) |
| 0104 | [Restate is the only effect engine; SQL stores are storage](0104-restate-is-the-only-effect-engine-sql-stores-are-storage.md) |
| 0105 | [The session actor decides from committed state](0105-the-shift-is-deterministic-workflow-code.md) |
| 0106 | [Durable formats use migration, drain or coexistence](0106-durable-formats-upgrade-by-migration-or-drain.md) |
| 0107 | [A process is named by a minted id, a start by its key](0107-a-process-is-named-by-a-minted-id-a-start-by-its-key.md) |
| 0108 | [A process lives until a scope its start could reach](0108-a-process-lives-until-a-scope-its-start-could-reach.md) |
| 0109 | [Work that outlives its transaction is an outbox of obligations](0109-store-to-engine-delivery-is-an-outbox-of-obligations.md) |
| 0110 | [The engine owns process recovery; lash never re-runs started work](0110-the-engine-owns-process-recovery.md) |
| 0111 | [A deployment namespace prefixes every Restate name lash binds or calls](0111-a-deployment-namespace-prefixes-every-restate-name.md) |
| 0112 | [The store is multi-session, and a session is resident from its current frame](0112-the-store-is-multi-session-and-a-session-is-resident-from-its-current-frame.md) |
| 0113 | [Artifacts are kept alive only by their referrers](0113-artifacts-are-kept-alive-only-by-their-referrers.md) |
| 0115 | [The 1.0 binary carries its half of every upgrade](0115-the-1-0-binary-carries-its-half-of-every-upgrade.md) |
| 0116 | [Tools are opaque, batch is sugar, and a spawn is a declared start](0116-tools-are-opaque.md) |
| 0117 | [Lash names every tool call](0117-lash-names-every-tool-call.md) |
| 0118 | [Native reasoning retention is frame-scoped](0118-native-reasoning-retention-is-frame-scoped.md) |
| 0119 | [Durable Session and live session are two authorities](0119-durable-session-and-live-session-are-two-authorities.md) |
| 0120 | [Tool presentation is a recorded, composable step](0120-tool-presentation-is-a-recorded-composable-step.md) |
| 0121 | [Host generation settings are sent or refused](0121-host-generation-settings-are-sent-or-refused.md) |
| 0122 | [A stopped turn's uncommitted tail lives only on the live stream](0122-a-stopped-turns-uncommitted-tail-lives-only-on-the-live-stream.md) |
| 0123 | [Model code runs in resettable worker processes](0123-model-code-runs-in-resettable-worker-processes.md) |
| 0124 | [Attachments are kept alive only by their referrers](0124-attachments-are-kept-alive-only-by-their-referrers.md) |
| 0125 | [Model usage is engine-owned accounting delivered per call](0125-model-usage-is-engine-owned-accounting-delivered-per-call.md) |
| 0126 | [Session config changes are typed owner commands](0126-session-config-changes-are-typed-owner-commands.md) |
| 0127 | [Usage is result data; hosts meter spend](0127-usage-is-result-data-hosts-meter-spend.md) |
| 0128 | [Tool hooks compose as transforms, then checks](0128-tool-hooks-compose-as-transforms-then-checks.md) |
| 0129 | [The transcript row stream is the only chat projection](0129-the-transcript-row-stream-is-the-only-chat-projection.md) |
| 0131 | [Durable types declare their version surface](0131-durable-types-declare-their-version-surface.md) |
| 0132 | [Durability is state-first over the lash store: actors, epoch fences, no replay](0132-durability-is-state-first-over-the-lash-store.md) |
<!-- adr-index:end -->

## Replaced and retired decisions

These files stay until the lane that removes the code citing them deletes them.
[The 2026-10 reset table](../architecture/adr-reset-2026-10.md) classifies
every decision from 0001 to 0131.

| Number | Status | Owner |
| --- | --- | --- |
| 0059 | Replaced | ADR 0128 |
| 0103 | Replaced | ADR 0132 §8 |
| 0104 | Replaced | ADR 0132 |
| 0111 | Retired: engine service names | ADR 0102 D2 owns deployment separation |
| 0125 | Replaced | ADR 0127 |

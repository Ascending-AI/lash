# ADR reset, 2026-10

This table classifies every decision from 0001 to 0131 against
[ADR 0132](../adr/0132-durability-is-state-first-over-the-lash-store.md),
state-first durability. Numbers without a row have no file and are not reused.

- **KEEP**: the decision names no durability mechanics that ADR 0132 changes.
- **AMEND**: the text is rewritten to state the ADR 0132 end state.
- **SUPERSEDE**: another decision owns the behaviour; the file is a short
  pointer until the lane that removes the code citing it deletes it.
- **RETIRE**: the decision goes with Restate and nothing replaces it; the file
  is a short note until the same lane deletes it.
- **NEW-ADR-NEEDED**: none. ADR 0132 §2, §9, §10, §12 and §13 cover the
  candidates.

| Class | Count |
| --- | --- |
| KEEP | 35 |
| AMEND | 79 |
| SUPERSEDE | 6 |
| RETIRE | 3 |
| NEW-ADR-NEEDED | 0 |

| ADR | Title | Class | Reason | Successor |
| --- | --- | --- | --- | --- |
| [0001](../adr/0001-context-management-uses-views-or-frames.md) | Context management uses views or frames | AMEND | Compaction records its base and frame under the session actor's epoch; the summarizer is a `Repeatable` model call. | — |
| [0002](../adr/0002-session-observation-uses-cursors-and-bounded-live-replay.md) | Session observation uses cursors and bounded live replay | AMEND | A re-sent model call opens a new live incarnation; activity keys are not replay keys. | — |
| [0003](../adr/0003-keyed-promise-is-scope-agnostic.md) | Durable waits are scoped wait rows with a first winner | AMEND | Durable waits are wait rows with a first winner and random-id completion keys; absorbs the surviving rules of 0012. | — |
| [0004](../adr/0004-process-environments-carry-plugin-options-not-product-metadata.md) | Process environments carry plugin options, not product metadata | AMEND | Resume refusals use stored formats, and process actors resume from committed state. | — |
| [0005](../adr/0005-tool-catalog-membership-replaces-availability-tiers.md) | Tool Catalog membership defines availability | AMEND | Frozen catalog outcomes commit with the VM snapshot instead of a journaled `ExecCode` address. | — |
| [0006](../adr/0006-rlm-history-renders-in-emission-format.md) | RLM history renders in the emission format | KEEP | No durability mechanics; the decision stands as written. | — |
| [0007](../adr/0007-four-layer-scenario-harnesses.md) | Four layer scenario harnesses | AMEND | Host-law tier is the production runtime under the fault store, virtual clock and `SimNodes`. | — |
| [0008](../adr/0008-confidence-gate.md) | Confidence gate | AMEND | Folds its dated amendment into present tense (main-red narration) and replaces the Restate host tier. | — |
| [0009](../adr/0009-deterministic-simulation-harness.md) | Randomized simulation harness | AMEND | lash-sim runs the production runtime with commit-label cuts instead of `SimEngine` on the server double. | — |
| [0011](../adr/0011-self-contained-processes.md) | Self-contained processes capture their environment at creation | AMEND | Process recovery is the actor's committed state, not an engine journal. | — |
| 0012 | Durable waits use effect-host engines and engine-owned journals | SUPERSEDE | Engine-owned keyed promises are replaced by wait rows. | ADR 0132 §6, ADR 0003 |
| [0013](../adr/0013-protocol-capabilities-enter-through-the-plugin-contract.md) | Protocol capabilities enter through the plugin contract | AMEND | Durability claims belong to the host's store topology; nodes reconstruct plugin capabilities. | — |
| [0014](../adr/0014-operational-policy-stays-with-the-host.md) | Operational policy stays with the host and Lash exposes levers | AMEND | Shutdown releases actors; wait revocation is on wait rows; recovery leadership is gone. | — |
| [0015](../adr/0015-admission-control-lives-in-provider-decorators.md) | Admission control lives in provider decorators | KEEP | No durability mechanics; the decision stands as written. | — |
| [0016](../adr/0016-process-waits-live-on-the-work-driver-seam.md) | Process waits live on the work-driver seam | AMEND | In-actor process awaits are `process_terminal` wait rows; host awaits keep the point-read path. | — |
| [0017](../adr/0017-process-observation-is-best-effort-push-over-state-truth.md) | Process observation is best-effort push over state truth | AMEND | Retention covers readers that still await a process, not replayable waiters. | — |
| [0018](../adr/0018-per-tool-telemetry-emits-from-one-shared-seam.md) | Per-tool telemetry emits from one shared seam | KEEP | No durability mechanics; the decision stands as written. | — |
| [0020](../adr/0020-process-change-feed-is-a-record-cursor-read.md) | Process change feed is a record-level cursor read | AMEND | Idempotent receipt commits replace receipt replay; drops the Restate-double law label. | — |
| [0021](../adr/0021-trigger-deliveries-are-first-class-and-recoverable.md) | Trigger deliveries are first-class and recoverable | AMEND | Emission is a store-local effect: reservation, registration and binding commit in one transaction; the trigger relay is gone. | — |
| [0022](../adr/0022-host-originators-carry-named-scopes.md) | Host originators carry named scopes | KEEP | No durability mechanics; the decision stands as written. | — |
| [0023](../adr/0023-retention-stays-a-parameterized-host-lever.md) | Retention stays a parameterized host lever | AMEND | Drops segment and Restate journal retention; SQLite sweeps run in one database. | — |
| [0024](../adr/0024-drainage-reads-over-artifact-refcounts.md) | Drainage reads over artifact references | KEEP | No durability mechanics; the decision stands as written. | — |
| 0025 | Bounded journals are an effect-controller obligation | RETIRE | Segment journal budgets and handovers go with Restate; the intent budget moves to ADR 0116 §1.7. | ADR 0116 §1.7, ADR 0132 §5 and §8 |
| [0026](../adr/0026-model-capability-is-host-supplied-data.md) | Model capability is host-supplied data and providers are executors | KEEP | No durability mechanics; the decision stands as written. | — |
| [0027](../adr/0027-unleased-completion-carries-explicit-authority.md) | Process completion carries explicit authority | AMEND | Completion authority is the actor's epoch fence or a forced cancel terminal; `ExternalOwner` is gone. | — |
| [0028](../adr/0028-attachments-are-three-layers-blob-reference-lifecycle.md) | Attachments have blob storage, reference tracking and host lifecycle policy | AMEND | Turn puts are held by the turn's execution, not a journal. | — |
| [0030](../adr/0030-the-session-profile-is-resolved-once-at-open.md) | The session model is recorded at creation | AMEND | The run's config snapshot commits with admission; model-bind faults re-send under `model_total` and park typed. | — |
| [0031](../adr/0031-execution-evidence-is-provider-reported-fact.md) | Execution evidence is provider-reported fact | KEEP | No durability mechanics; the decision stands as written. | — |
| [0032](../adr/0032-attempt-history-rides-inside-the-result.md) | Attempt history rides inside the result | AMEND | Attempt history rides in the model call's committed phase. | — |
| [0033](../adr/0033-final-output-attribution-is-host-policy.md) | Final-output attribution is host policy | KEEP | No durability mechanics; the decision stands as written. | — |
| [0034](../adr/0034-harness-evolution-lives-outside-the-runtime-repository.md) | Harness evolution lives outside the runtime repository | KEEP | No durability mechanics; the decision stands as written. | — |
| [0035](../adr/0035-frontends-are-independent-host-applications.md) | Frontends are independent host applications | KEEP | No durability mechanics; the decision stands as written. | — |
| [0036](../adr/0036-stream-termination-is-explicit-dialect-policy.md) | Stream termination is explicit dialect policy | KEEP | No durability mechanics; the decision stands as written. | — |
| [0037](../adr/0037-lashlang-workflows-use-a-code-graph-code-lens.md) | Lashlang workflows use a code-graph-code lens | KEEP | No durability mechanics; the decision stands as written. | — |
| [0038](../adr/0038-response-metadata-is-allowlisted-host-supplied-capture.md) | Response metadata is allowlisted host-supplied capture | KEEP | No durability mechanics; the decision stands as written. | — |
| [0039](../adr/0039-turn-cancellation-is-a-first-party-work-driver-primitive.md) | Turn cancellation is a first-party work-driver primitive | AMEND | Cancel requests are mailbox rows decided by the turn commit; the closure-authorization protocol is deleted. | — |
| [0040](../adr/0040-retried-model-attempts-retract-live-text-by-correlation.md) | Retried model attempts retract live text by correlation | KEEP | No durability mechanics; the decision stands as written. | — |
| [0041](../adr/0041-child-turn-and-driver-stack-growth-have-canonical-seams.md) | Child-turn and driver stack growth have canonical seams | AMEND | Drops the handler controller proxy; effect tasks commit nothing. | — |
| [0042](../adr/0042-tool-attempts-are-atomic.md) | Tool attempts are atomic | AMEND | Attempt recovery follows `Once`/`Repeatable` execution policy. | — |
| 0043 | Hosts register immutable deployments | RETIRE | Deployment pinning for journal replay is deleted; only formats drain. | ADR 0106 §1, ADR 0132 §2 |
| [0044](../adr/0044-tests-must-be-independent-of-what-they-test.md) | Tests must be independent of what they test | AMEND | Durability tests cut at labelled commits and count executions; the Restate host matrix is gone. | — |
| [0045](../adr/0045-services-are-stateless-substrates-own-continuation.md) | Services are stateless; the store owns continuation | AMEND | The store owns continuation through actors, epochs and phase rows; segment rebuild is deleted. | — |
| [0046](../adr/0046-process-transitions-are-events-record-is-a-fold.md) | Process transitions are events; the record is a fold | AMEND | Observer mutations use idempotency keys; VM continuation is the snapshot. | — |
| [0047](../adr/0047-history-is-shared-branches-are-sessions.md) | History is shared; branches are sessions | AMEND | Phase records and session history commit in one store and one transaction. | — |
| [0048](../adr/0048-checkpoint-component-identity-is-a-backend-contract.md) | Checkpoint component identity is a backend contract | KEEP | No durability mechanics; the decision stands as written. | — |
| [0049](../adr/0049-session-ids-are-used-once.md) | Session ids are used once | AMEND | Scope fences revoke wait rows; segment pins and the engine registration probe are gone. | — |
| [0050](../adr/0050-behavior-transcripts-are-one-normalized-vocabulary.md) | Behavior transcripts are one normalized vocabulary | KEEP | No durability mechanics; the decision stands as written. | — |
| [0051](../adr/0051-the-facade-is-the-host-api-core-is-integrator-seams.md) | 0051. The facade is the host API; core exposes integrator seams | AMEND | §2 becomes projection-provider implementors; `ProcessEngine` is an `advance` state machine. | — |
| [0052](../adr/0052-the-postgres-schema-is-a-published-artifact-lash-verifies.md) | The Postgres schema is a published artifact lash verifies at open | KEEP | No durability mechanics; the decision stands as written. | — |
| [0054](../adr/0054-host-panics-are-contained-and-lock-poison-is-recovered.md) | Host panics are contained and standard-lock poison is recovered | KEEP | No durability mechanics; the decision stands as written. | — |
| [0055](../adr/0055-lashlang-execution-bounds-span-durable-process-lifetimes.md) | Lashlang execution bounds span durable process lifetimes | AMEND | VM accounting persists in snapshots, not across segment handovers. | — |
| [0056](../adr/0056-checkpoint-components-generalize-to-a-keyed-set.md) | Checkpoint components generalize to a keyed set | KEEP | No durability mechanics; the decision stands as written. | — |
| [0057](../adr/0057-history-generations-accelerate-edge-authoritative-reads.md) | 0057. History generations accelerate edge-authoritative reads | AMEND | Receipt retries replace receipt replay wording. | — |
| [0058](../adr/0058-runtime-commit-budgets-are-explicit-host-policy.md) | Runtime commit budgets are explicit host policy | AMEND | Retry adoption replaces replay adoption wording. | — |
| [0059](../adr/0059-before-tool-call-directives-compose-monotonically.md) | Tool-call directives compose monotonically | SUPERSEDE | Mixed before/after directives are replaced by transforms then checks (main-red narration). | ADR 0128 |
| [0060](../adr/0060-the-lashlang-vm-is-a-heap-substrate-with-dialect-lowered-value-semantics.md) | The lashlang VM is a heap substrate with dialect-lowered value semantics | AMEND | Heap objects persist in VM snapshots. | — |
| [0061](../adr/0061-two-first-class-rlm-dialects-with-full-parity-and-session-pinning.md) | RLM dialects share one IR and VM | KEEP | No durability mechanics; the decision stands as written. | — |
| [0062](../adr/0062-the-typescript-dialect-is-an-exact-ecma-262-subset.md) | The TypeScript dialect is an exact ECMA-262 subset | AMEND | Aggregates settle on recorded order; resume reads the committed decision. | — |
| [0063](../adr/0063-one-rlm-turn-is-prompted-in-one-dialect.md) | One RLM turn is prompted in its dialect | AMEND | Durable identity wording replaces replay identity. | — |
| [0064](../adr/0064-the-typescript-dialect-is-broad-and-every-gap-is-an-explicit-ruling.md) | The TypeScript dialect is broad, and every gap is an explicit ruling | AMEND | Clock and random reads are redrawn in an uncommitted stretch instead of journaled. | — |
| [0065](../adr/0065-concurrent-settlement-is-a-durable-group-at-the-effect-host-seam.md) | Concurrent settlement is recorded by the logical Run | AMEND | Concurrent settlement is Run rows; ready futures never race again on resume. | — |
| [0066](../adr/0066-durable-session-facts-are-a-typed-read-and-a-guarded-write.md) | Durable session facts are a typed read and a guarded set-if-unset write | KEEP | No durability mechanics; the decision stands as written. | — |
| [0067](../adr/0067-durable-rows-name-one-owner-and-one-reclaim-trigger.md) | Every durable row names one owner and one reclaim trigger | AMEND | Execution retirement happens in owner transactions; snapshot and Run-row reclaim replace segment rows. | — |
| [0068](../adr/0068-one-meaning-per-outcome-suffix.md) | One meaning per outcome-type suffix | AMEND | Drops the vendored Restate protocol name from the gate's exclusion. | — |
| [0069](../adr/0069-durable-acceptance-is-the-sole-turn-ingress.md) | Durable acceptance is the sole turn ingress | AMEND | Child acceptance is a mailbox write plus wake; the child is its own actor; admission is recorded. | — |
| [0070](../adr/0070-cache-capabilities-are-host-supplied-data.md) | Cache capabilities are host-supplied data | KEEP | No durability mechanics; the decision stands as written. | — |
| [0071](../adr/0071-engines-emit-unified-tool-call-accounting.md) | Engines emit unified tool-call accounting outside model projection | AMEND | Resume wording; execution hold replaces journal settlement. | — |
| [0073](../adr/0073-gradual-value-types-through-to-the-workflow-editor.md) | Gradual value types through to the workflow editor | KEEP | No durability mechanics; the decision stands as written. | — |
| [0074](../adr/0074-generation-intent-is-session-policy-and-its-fate-is-reported.md) | Generation intent is session policy, and its fate on the wire is reported | AMEND | A re-sent call sends the recorded settings. | — |
| [0076](../adr/0076-lashlang-durable-stores-hold-exclusively-owned-copies.md) | Durable VM state preserves shared references and owns its roots | KEEP | No durability mechanics; the decision stands as written. | — |
| [0077](../adr/0077-session-state-migrates-totally-at-admission.md) | Session state admits one compatible continuation generation | AMEND | Admission validates the actor epoch; the format set replaces drain generation `G`. | — |
| [0078](../adr/0078-plugin-state-is-a-lash-mediated-per-plugin-store.md) | Plugin state is a lash-mediated per-plugin store | AMEND | Resolutions commit with their outcome; hooks recompute when their phase did not commit. | — |
| [0079](../adr/0079-one-promised-package-facade-owns-the-api.md) | One promised package: the facade owns the API | AMEND | Drops the `restate` facade feature and effect-host contracts. | — |
| [0081](../adr/0081-destructive-schema-changes-are-currently-reject-and-recreate.md) | SQL stores refuse unsupported schemas and report the writing release | AMEND | Drops the Restate state reset. | — |
| [0082](../adr/0082-process-registry-is-composed-from-narrow-concern-traits.md) | The process registry is composed from narrow concern traits | AMEND | Process recovery points at ADR 0132; wake claim tokens belong to wake delivery. | — |
| [0083](../adr/0083-rlm-native-tool-channel.md) | RLM channels are pinned when a session materializes | AMEND | Dated note restated in present tense (main-red narration). | — |
| [0084](../adr/0084-runtime-feedback-position.md) | Separate initial instructions from positional runtime feedback | KEEP | No durability mechanics; the decision stands as written. | — |
| [0085](../adr/0085-rlm-prompt-teaches-only-enabled-capabilities.md) | RLM prompts teach only enabled capabilities | KEEP | No durability mechanics; the decision stands as written. | — |
| [0086](../adr/0086-aggregate-await-shapes-and-question-placement.md) | Aggregate await operates on handles | KEEP | No durability mechanics; the decision stands as written. | — |
| [0087](../adr/0087-typescript-runtime-promise-arrays.md) | TypeScript aggregates evaluate runtime arrays | AMEND | Snapshot resume replaces replay re-execution. | — |
| [0088](../adr/0088-facade-sessions-bind-storage-and-lifecycle-owners.md) | Facade sessions bind storage and lifecycle owners | AMEND | Sessions bind the durable engine; plugin-set separation is by store set, not Restate namespace. | — |
| [0089](../adr/0089-parent-relationships-do-not-define-a-second-session-model.md) | Parent relationships do not define a second session model | KEEP | No durability mechanics; the decision stands as written. | — |
| [0090](../adr/0090-named-process-signatures-are-authoritative.md) | Named process signatures are authoritative | KEEP | No durability mechanics; the decision stands as written. | — |
| [0091](../adr/0091-one-lowering-walk-owns-expression-semantics.md) | One lowering walk owns expression semantics | KEEP | No durability mechanics; the decision stands as written. | — |
| [0092](../adr/0092-explicit-agent-frame-scope.md) | Agent frame scope is explicit and resolvable | KEEP | No durability mechanics; the decision stands as written. | — |
| [0093](../adr/0093-artifact-lifetimes-use-exact-owner-edges.md) | Artifact lifetimes use exact owner edges | AMEND | Durable reader wording replaces replay. | — |
| [0094](../adr/0094-child-lifecycle-is-a-registration-fact-settled-by-scope-end.md) | Child lifecycle is a registration fact settled by scope end | AMEND | Parent-end is a batched mailbox cascade with a cursor; start compensation needs no Restate submission. | — |
| [0095](../adr/0095-processes-are-values-and-process-controls-are-tools.md) | Processes are values, process controls are tools, one handle kind | AMEND | Code-call identity is the admitted operation; `processes.await` is a wait row. | — |
| [0096](../adr/0096-typescript-is-the-sole-rlm-dialect.md) | One IR and VM, extensible dialects, TypeScript today | AMEND | Clock/random module is not journaled. | — |
| [0097](../adr/0097-commit-identity-families-mint-frozen-unframed-preimages.md) | Commit-identity families mint frozen unframed preimages | AMEND | Idempotency wording replaces replay. | — |
| [0098](../adr/0098-one-owner-per-sql-table-across-both-stores.md) | One owner per SQL table across both stores | KEEP | No durability mechanics; the decision stands as written. | — |
| [0099](../adr/0099-tool-children-of-effect-groups-are-live-closing-settled.md) | Tool calls and aggregates belong to the logical Run | AMEND | Run rows replace the opener journal; deadlines follow ADR 0132 §7; §8, §9, §14 and §15 restated. | — |
| [0100](../adr/0100-the-run-observation-contract.md) | The run-observation contract | AMEND | R0/R4/R5 use admitted identities and snapshots instead of journal positions. | — |
| [0101](../adr/0101-one-session-ingress-carries-every-admitted-item.md) | One session ingress carries every admitted item | AMEND | Owner epoch replaces the shift fence; ingress needs no relay; command cancel is a mailbox row. | — |
| [0102](../adr/0102-zero-infra-is-a-sqlite-in-memory-backend.md) | Every backend binds one durable engine to one store set | AMEND | Every backend binds the one durable engine; SQLite is one database; zero-infra needs no server. | — |
| [0103](../adr/0103-code-cells-replay-by-re-execution-on-every-host.md) | Code cells replay by re-execution on every host | SUPERSEDE | Code cells resume from VM snapshots, not re-execution. | ADR 0132 §8 |
| [0104](../adr/0104-restate-is-the-only-effect-engine-sql-stores-are-storage.md) | Restate is the only effect engine; SQL stores are storage | SUPERSEDE | Restate and the `EffectEngine` seam are deleted. | ADR 0132 |
| [0105](../adr/0105-the-shift-is-deterministic-workflow-code.md) | The session actor decides from committed state | AMEND | Restated as decision rules over committed state; §3, §5, §7, §8, §10 to §12 point at ADR 0132. | — |
| [0106](../adr/0106-durable-formats-upgrade-by-migration-or-drain.md) | Durable formats use migration, drain or coexistence | AMEND | Drain by release and the format claim filter replace generation lanes; Restate object state retired. | — |
| [0107](../adr/0107-a-process-is-named-by-a-minted-id-a-start-by-its-key.md) | A process is named by a minted id, a start by its key | AMEND | Starts and trigger bindings commit in one transaction. | — |
| [0108](../adr/0108-a-process-lives-until-a-scope-its-start-could-reach.md) | A process lives until a scope its start could reach | AMEND | Scope close commits with the terminal; parent-end cascades; `SessionDelete` is deferred work. | — |
| [0109](../adr/0109-store-to-engine-delivery-is-an-outbox-of-obligations.md) | Work that outlives its transaction is an outbox of obligations | AMEND | The outbox keeps `SessionDelete` and `ArtifactCleanup`; leader lease gone; per-kind table restated. | — |
| [0110](../adr/0110-the-engine-owns-process-recovery.md) | The engine owns process recovery; lash never re-runs started work | AMEND | Recovery loads committed state; `Once`/`Repeatable`; `ActivationLoop`; `ExternalOwner` retired. | — |
| [0111](../adr/0111-a-deployment-namespace-prefixes-every-restate-name.md) | A deployment namespace prefixes every Restate name lash binds or calls | RETIRE | Restate service names and namespaces go with Restate. | ADR 0102 D2, ADR 0132 |
| [0112](../adr/0112-the-store-is-multi-session-and-a-session-is-resident-from-its-current-frame.md) | The store is multi-session, and a session is resident from its current frame | AMEND | Epoch fence and resume wording; drops Restate execution. | — |
| [0113](../adr/0113-artifacts-are-kept-alive-only-by-their-referrers.md) | Artifacts are kept alive only by their referrers | AMEND | Execution settlement by durable facts replaces `journal_replay`; SQLite has one ledger table. | — |
| [0115](../adr/0115-the-1-0-binary-carries-its-half-of-every-upgrade.md) | The 1.0 binary carries its half of every upgrade | AMEND | Drops `G`, Restate wire and object compat; §3 is nodes of two builds sharing the store; SQLite finalize is one transaction. | — |
| [0116](../adr/0116-tools-are-opaque.md) | Tools are opaque, batch is sugar, and a spawn is a declared start | AMEND | Execution policy and `ExecutionLimit`; adds §1.7, the intent admission budget. | — |
| [0117](../adr/0117-lash-names-every-tool-call.md) | Lash names every tool call | AMEND | §9 is format ownership; code identities persist in VM snapshots. | — |
| [0118](../adr/0118-native-reasoning-retention-is-frame-scoped.md) | Native reasoning retention is frame-scoped | KEEP | No durability mechanics; the decision stands as written. | — |
| [0119](../adr/0119-durable-session-and-live-session-are-two-authorities.md) | Durable Session and live session are two authorities | AMEND | Actor ownership replaces Restate shift serialization; removes a history sentence. | — |
| [0120](../adr/0120-tool-presentation-is-a-recorded-composable-step.md) | Tool presentation is a recorded, composable step | AMEND | §B presentation is a recorded phase. | — |
| [0121](../adr/0121-host-generation-settings-are-sent-or-refused.md) | Host generation settings are sent or refused | AMEND | Recorded calls replace journals. | — |
| [0122](../adr/0122-a-stopped-turns-uncommitted-tail-lives-only-on-the-live-stream.md) | A stopped turn's uncommitted tail lives only on the live stream | KEEP | No durability mechanics; the decision stands as written. | — |
| [0123](../adr/0123-model-code-runs-in-resettable-worker-processes.md) | Model code runs in resettable worker processes | AMEND | Worker durability uses snapshots and admitted identities; accounting rides the snapshot. | — |
| [0124](../adr/0124-attachments-are-kept-alive-only-by-their-referrers.md) | Attachments are kept alive only by their referrers | AMEND | Execution referrers settle by durable facts; process-terminal delivery is a wait row. | — |
| [0125](../adr/0125-model-usage-is-engine-owned-accounting-delivered-per-call.md) | Model usage is engine-owned accounting delivered per call | SUPERSEDE | Engine-owned usage accounting is replaced by result data (main-red narration). | ADR 0127 |
| [0126](../adr/0126-session-config-changes-are-typed-owner-commands.md) | Session config changes are typed owner commands | AMEND | Config resolution commits before publication; unreadable owners park typed. | — |
| [0127](../adr/0127-usage-is-result-data-hosts-meter-spend.md) | Usage is result data; hosts meter spend | AMEND | Removes the supersedes narration (main-red); resume and `Repeatable` guarantees. | — |
| [0128](../adr/0128-tool-hooks-compose-as-transforms-then-checks.md) | Tool hooks compose as transforms, then checks | AMEND | Resume wording replaces recorded replay. | — |
| [0129](../adr/0129-the-transcript-row-stream-is-the-only-chat-projection.md) | The transcript row stream is the only chat projection | KEEP | No durability mechanics; the decision stands as written. | — |
| 0130 | Protected realization runs in its own invocation | SUPERSEDE | The store half of realization commits with the tool result. | ADR 0132 §5 |
| [0131](../adr/0131-durable-types-declare-their-version-surface.md) | Durable types declare their version surface | AMEND | Record kinds declare surfaces for the format set; the journal-version section is retired. | — |

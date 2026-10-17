# Context-Overflow Recovery

> **Read [../RULES.md](../RULES.md) first.** This is a `deterministic_only`
> runbook. Its evidence is seven executable laws, selected by full test path
> and scored on the executed and passed counts. It has no paid model run.

**Purpose.** Establish that context overflow has its own turn outcome, the
public report exposes it, and an RLM session accepts a subsequent turn on the
same durable identity. Also establish that the standard-compaction plugin
records a pending recovery marker with the overflow turn's commit.

A settled overflow ends that turn. The host reads
`TurnOutcome::is_context_overflow` and chooses whether to compact, summarize,
or send another input. It submits the next turn through `send()`; Lash's
engine owns execution. See [ADR 0101](../../docs/adr/0101-one-session-ingress-carries-every-admitted-item.md).
The continuation law sends directly after the overflow to isolate session
continuity from a host's compaction policy.

## Companion laws

Run these five commands sequentially in an isolated Kiln fork, after sourcing
`env.sh`. The SQLite memory variants use real SQL stores and Lash's in-process
durable engine. The classifier and turn-machine laws need no store. All
provider behavior is scripted; none of these commands calls a live provider.

```sh
kiln test //crates/lash-sansio:lash-sansio__unit_test \
  --test_arg=--exact \
  --test_arg=sansio::tests::context_overflow_response_stops_as_its_own_outcome \
  --test_arg=sansio::tests::context_overflow_llm_error_stops_as_its_own_outcome

kiln test //crates/lash-core-llm:lash-core-llm__unit_test \
  --test_arg=--exact \
  --test_arg=provider::tests::classifier_tests::default_failure_classifier_uses_context_overflow_text_for_unclassified_failures \
  --test_arg=provider::tests::classifier_tests::default_failure_classifier_does_not_treat_rate_limits_as_context_overflow

kiln test //crates/lash-plugin-standard-compaction:lash-plugin-standard-compaction__unit_test \
  --test_arg=--exact \
  --test_arg=tests::overflow_after_turn_queues_marker_for_context_overflow_outcome_only

kiln test //crates/lash-durable-test:after_turn_crash_laws__test \
  --test_arg=--exact \
  --test_arg=sqlite_memory::compaction_recovery_records_a_context_overflow_with_its_turn

kiln test //crates/lash:lash__unit_test \
  --test_arg=--exact \
  --test_arg=tests::agent_scenarios::context_overflow_mid_turn_continues_on_the_same_session
```

Expected executed/passed counts, in command order: **2/2, 2/2, 1/1, 1/1,
1/1** (seven laws total). Read the final `kiln: test` receipt line and retain
the command logs and receipts. A green result with zero executed tests is an
Abort: resolve the selector before scoring. An observed failure is a product
finding; retain it and stop, without weakening the law.

Sources:

- `crates/lash-sansio/src/sansio/tests.rs`: the accepted-response and
  classified-failure paths preserve the context-overflow outcome.
- `crates/lash-core-llm/src/provider/tests/classifier_tests.rs`: overflow text
  becomes a non-retryable context overflow; rate-limit text stays distinct.
- `crates/lash-plugin-standard-compaction/src/tests.rs`: only the overflow
  outcome queues a pending marker; provider error and frame handoff do not.
- `crates/lash-durable-test/tests/after_turn_crash_laws.rs`: the pending marker
  is read from the committed history of the overflow turn.
- `crates/lash/src/tests/agent_scenarios.rs`: the mid-turn continuation law.
- `crates/lash/src/turn.rs`: the public report's overflow and success accessors.

## Mid-turn continuation evidence

The facade law runs two arms: one accepted response explicitly states context
overflow, and one failed call carries context-window text that Lash classifies.
Each arm:

1. Creates one RLM session over SQLite memory and requests a real registered
   tool through a TypeScript cell. The tool returns a 256 KiB string, bound in
   the cell; the cell prints its length.
2. Makes the second provider call only after that tool has executed. That
   request contains the observed length. The scripted provider overflows here,
   inside the first turn, and the public report reads overflow and unsuccessful.
3. Reopens the existing session id and sends a new input. Its cell finishes
   with the retained binding's length. The public report reads successful and
   no overflow; the committed turn index advances from one to two.
4. Checks all three provider requests name the original session, and the tool
   executed once. The continuing answer therefore uses committed state from
   before the overflow, without a replacement session or a repeated tool call.

The fixture does not force a live model's context window to shrink. The
oversized value is an actual tool result; the overflow is a deterministic
provider response or error. Printing the length establishes ordering without
claiming that all 256 KiB reached the model's prompt.

TypeScript is the dialect selected by this law's fixture. Lash also ships
Python; both lower to the dialect-free kernel described in
[ADR 0139](../../docs/adr/0139-the-lash-vm-is-a-dialect-free-kernel.md).
These gates certify this TypeScript continuation fixture. Compaction
completion, summaries, frame movement, PostgreSQL and Python continuation
are outside these seven gates.

For a deliberate regression witness, temporarily make the facade's send
admission refuse continuation after a settled turn. Select only the facade
law, require one executed failure at the continuing send after the overflow
has settled, retain the patch and failure log, then remove the mutation and
run the ordinary companion gates. Never commit the mutation.

## Scorecard

Fill this from the retained command results. Each law needs one executed pass;
the facade law includes both overflow arms in that one execution.

| Claim | Law | Verdict | Executed/passed count and receipt |
| --- | --- | --- | --- |
| Explicit overflow has its own stop | `context_overflow_response_stops_as_its_own_outcome` | | |
| Classified failed call has the same stop | `context_overflow_llm_error_stops_as_its_own_outcome` | | |
| Overflow text classifies as non-retryable overflow | `default_failure_classifier_uses_context_overflow_text_for_unclassified_failures` | | |
| Rate-limit text remains distinct | `default_failure_classifier_does_not_treat_rate_limits_as_context_overflow` | | |
| Only overflow queues the pending marker | `overflow_after_turn_queues_marker_for_context_overflow_outcome_only` | | |
| The pending marker commits with the overflow turn | `compaction_recovery_records_a_context_overflow_with_its_turn` | | |
| The public overflow report permits same-session continuation with retained state, both arms | `context_overflow_mid_turn_continues_on_the_same_session` | | |

Record the source revision, artifact directory, all five receipt paths and any
findings. A complete passing scorecard has seven executed passes and paid
provider cost **$0**.

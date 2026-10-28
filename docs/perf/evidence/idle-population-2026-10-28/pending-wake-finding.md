# Pending wake markers repeatedly reclaim suspended sessions

This is a measured runtime finding, left unchanged for a separate fix. It is
not a failed memory threshold. Evidence comes from the fixed-history idle
population, using zero idle owner retention, 20 completed turns, one pending
tool completion and a 1,024-byte payload per session.

The first 25-session Standard pending diagnostic did not reach released owners
within its 120-second state check. Its final durable snapshot had 19 waiting
actors and these six owned actors:

| Session suffix | Epoch | Wake sequence | Acknowledged sequence |
| --- | ---: | ---: | ---: |
| 00000002 | 13,629 | 21 | 20 |
| 00000014 | 12,791 | 21 | 20 |
| 00000016 | 12,667 | 21 | 20 |
| 00000019 | 12,459 | 21 | 20 |
| 00000023 | 12,165 | 21 | 20 |
| 00000025 | 12,053 | 21 | 20 |

The full IDs are `idlepop-` followed by the suffix. Every session had one pending
`tool_completion` wait, with a one-hour deadline. There were no retained
`actor_mail` rows. The high epochs show repeated release and claim activity,
not six immortal tasks that never release. No populated RSS point was accepted
by that diagnostic, so its missing 25-session RSS is not reconstructed.

One bounded rerun used the exact same optimized executable SHA-256
`0d45695dbe61eb77d7e4dbe2dd7e85ea771c64383cff87cf20e9afe33153c990`, the same
25 sessions, history, payload and wave size. Its sampled endpoint had 25 waiting
actors and zero owners, and it resolved the sample completion and reopened the
session successfully. The split was not deterministic across this one pair.
No further identical rerun was used to estimate frequency.

The retaining path is visible in the measured source revision:

- [round/runner.rs](../../../../crates/lash-core-execution/src/runtime/actor/round/runner.rs#L338)
  suspends a round waiting only on rows once its zero idle interval passes.
- [session/run.rs](../../../../crates/lash-core/src/runtime/durable/session/run.rs#L111)
  releases `PhaseExit::Suspended` as `Waiting` without acknowledging the observed
  wake marker in that transaction.
- [tx.rs](../../../../crates/lash-durable/src/tx.rs#L721) documents that `give_up`
  does not acknowledge mail or wake markers.
- [durable/mod.rs](../../../../crates/lash-store-sql/src/durable/mod.rs#L170)
  converts release to immediately claimable `ready` when `mail_seq > acked_seq`.
  Its [wake statement](../../../../crates/lash-store-sql/src/durable/mod.rs#L201)
  increments that sequence without adding a mail payload row.

Together, the observed marker gap and release SQL explain why these sessions
remain repeatedly claimable while their tool completion is still pending. The
inspection did not trace which producer's wake won the original race, and no
runtime change or regression test is included in this measurement lane.

An active claim holds its actor context, session projection and restored turn
and round lifecycle. Those owners and task state are rebuilt as epochs advance.
Standard mode creates no VM worker. The tool deadline is a durable wait, not
proof of a resident per-session OS timer. With zero idle retention, the
suspendable path returns before the hot polling branch at `runner.rs:353`.
The node's wake listener is shared; no per-session durable subscription or mail
payload was found. Post-exit rows cannot provide exact live task, timer or
subscription counts, and none are invented here.

The final pending populations are measured separately as **owners retained while
waiting**, with owner counts, epochs, wake backlog, pending calls, worker state
and RSS in the raw points. These endpoints permit actor churn and do not claim
CPU quiescence or universal owner release. The other residency modes retain
strict owner-release checks. The main instrument document gives the measured
per-session resident cost for both protocols.

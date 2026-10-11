# Recorded kernel migration sweep — Pass

Date: 2026-10-11. Baseline: `26f00498dd871f9f60cda9a6a266ac43a6f29849` plus the FIG-5987 changes in this commit. Evidence root: `.kiln/FIG-5987/`. Cost: **$0**, deterministic scripted provider. Judged build: `judged-build.json`; final fixture build log: `judged-build-reply.log`.

Store: owned SQLite file. Feature target:
`//crates/lash:kernel_two_build_claim__test__fv_0c6870c9`.

| Gate | Observed | Verdict / evidence |
| --- | --- | --- |
| Idle process/session carry | executed=1, passed=1 | Pass; migration-idle-report.json |
| Closing refusal then startup | executed=1, passed=1; named typed Unmigrated | Pass; migration-closing-report.json |
| Refused-cell listing | executed=1, passed=1; cell/reason asserted | Pass; migration-refusal-report.json |
| Owned state | idle session; one waiting process actor, future next_due_ms | Pass; 01-actors/processes.json |
| CLI envelopes | five exit-zero calls, no error, same owned SQLite path | Pass; 02 through 06 JSON receipts |
| Survey | checked=1, unmigrated=0, empty sessions/refused | Pass; 03-list-before / 05-list-after.json |
| Both sweeps | woken={processes:0,sessions:0} | Pass; 04-run / 06-run-again.json |
| Restart read | same session/process handle; double(21)=42 | Pass; 07-binding-proof.json |
| Reload / teardown | rendered resumed marker; owned stack stopped | Pass; 07-resumed.png / 08-teardown.log |

Operator artifacts: `run-root/kernel-migration-sweep/sqlite/`; recorder logs:
`migration-record.log`, `migration-finish.log`. The current-build empty survey
proves CLI addressing and idempotency; **the three synthetic-next laws supply
the actual two-build carry, closing refusal and refused-cell evidence**.

The first feature-target invocation failed to compile (executed=0), because
the refused-cell law called removed DurableStore.cell_snapshots at lines
937/948. The caller now reads cell_roots, retaining the same nonempty parked
root check and every refusal assertion. Four feature-target selectors then
executed once and passed; compile-red receipts are retained. No test deleted.

Recorder corrections are retained: a process sleep parks its actor using
next_due_ms, not the session waits table; woken is a processes/sessions object,
not an integer. The seeded store and five CLI calls were retained; the
continuation performed the restart read once. A missing sqlite3 import was
fixed before scoring the already committed read, without resending it.

The host was capped at 8 GiB virtual memory; Chromium was uncapped. Its
separate timed read-only reload peak RSS was 103120 KiB (`09-browser-rss.log`).
The full Chrome measurement preflight hit its Unix socket path-length limit;
the existing headless shell produced the successful measurements. No kernel
format stamp was changed, and no released upgrade/window is claimed.

# Recorded saved-functions laws — Pass

Date: 2026-10-11. Baseline: `26f00498dd871f9f60cda9a6a266ac43a6f29849` plus the FIG-5987 changes in this commit. Evidence root: `.kiln/FIG-5987/`. Cost: **$0**, deterministic scripted provider. Judged build: `judged-build.json`; final fixture build log: `judged-build-reply.log`.

Deterministic-only; no judged browser row, no save/seed UI, no screenshot.

| Law/property | Executed | Passed | Observed decision | Verdict / receipt |
| --- | --- | --- | --- | --- |
| New seeded session | 1 | 1 | hello, lash / hello, world; fresh bindings | Pass; saved-seed-report.json |
| Cold restore | 1 | 1 | 42 / 21; defaults, async metadata and captures | Pass; saved-cold-report.json |
| Missing effect | 1 | 1 | named PY_SAVED_FUNCTION_UNUSABLE; only first echo | Pass; saved-missing-report.json |
| Earlier helper | 1 | 1 | saved evens listed; new cell returns 2 / 4 | Pass; saved-helper-report.json |

Total: **executed=4, passed=4, failed=0**. Full selectors and target labels are
in the runbook and `.kiln/FIG-5987/proof-results.json`. Each selector executed
once on the declared SQLite-backed target. The earlier-helper target initially
had executed=0 due to the migration law's stale store-reader method; its
compile-red receipt is retained. It passed after that caller repair without
changing its assertions. Python laws passed on their first invocation.

This is facade/executor and retained-helper evidence; it makes no sealed
release or arbitrary older-format compatibility claim. No helper seal was run.

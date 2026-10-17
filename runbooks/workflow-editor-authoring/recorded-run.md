# Recorded corpus browser pass — 2026-10-17

The final FIG-6385 pass used a fresh SQLite database, the in-process durable
engine and the shared `corpus.echo` mock. It made no provider calls.
Evidence is archived under `.kiln/FIG-6385/browser-final/`; the executable
browser driver is `.kiln/FIG-6385/browser.cjs` and its final log is
`.kiln/FIG-6385/browser-final.log`.

| Gate | Observed |
| --- | --- |
| Catalog inventory | 18/18 cases, manifest order preserved |
| Module views | 22/22 across TypeScript and Python |
| Mapped source line selects a canvas node | 22/22, including both non-ASCII views |
| Expected module refusals, unchanged revision | 10/10 |
| Cell source and recorded crossing/finish witnesses | 4/4; cells were not executed |
| Authored import sends selected language and retains map id | 2/2 |
| Unchanged printed read preserves document and identity | 2/2 |
| Preview and applied edit agree, old map is dropped | 2/2 |
| Terminal result, retained display and rendered trace show `edited λ` | 2/2 |
| Screenshots | 45, including every case/language and mobile source controls |
| Browser page errors | 0 |
| Teardown | This pass's server stopped |

`display-typescript.png` and `display-python.png` pair the rendered successful
trace with the corresponding `run-*.jsonl`. `non-ascii-*.png` capture intact
Japanese and astral text with mapped selection. `mobile-source.png` shows the
Python printed-document editor at 390 × 844; controls remain visible and the
document has no horizontal overflow. Module replies, refusal replies and edit
preview/application pairs are retained beside the screenshots.

The server ran under `ulimit -v 8388608`; `/usr/bin/time -v` reported peak RSS
146340 KiB. Chromium and its driver ran without a virtual-memory cap; their
recorded maximum RSS was 207680 KiB. These are observed process maxima, not
summed host memory.

Two earlier attempts are preserved separately. The first inspected 18 TypeScript
cases and stopped because the automation response-body API decoded UTF-8 SSE as
Latin-1; the page rendered the correct text. The driver now captures a cloned
Fetch response inside the page. The second confirmed the TypeScript display
flow and stopped at Python's unmapped `import asyncio` line, which had been
marked clickable. The final implementation marks only supplied mapped lines
as clickable, and the final fresh pass completed all gates above.

The pass does not certify session execution of cell witnesses, crash recovery,
historical-run browsing or external services.

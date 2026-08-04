# E2E Scenario: Workbench Attachments — Upload, Reference, Restart, Retrieve

> **Read [../RULES.md](../RULES.md) first** — especially the browser-surface,
> screenshot, polling, real-token, Abort/RCA, and teardown rules. This runbook adds only
> the attachment scenario.

**Purpose.** Prove that the Agent Workbench can upload a PNG, visibly attach it to a user
turn, deliver that exact content-addressed attachment to the model, and retrieve identical
bytes after replacing the web process.

**Contract cited.** Lash's host turn contract accepts MIME-tagged attachment sources.
The workbench deliberately remains PNG-only: it creates an inline `image/png` source,
runtime [`normalize_input_items`](../../crates/lash-core/src/runtime/io.rs) persists those
bytes, and the provider adapter materializes the resulting stored source. Generic document
support is available in Lash but is not enabled by this workbench surface.

**Real tokens.** The referenced turn uses OpenRouter. Judge attachment plumbing and
cross-surface identity, not the quality of the model's image description.

## Scenario-specific golden rules

1. **Use the rendered upload control.** Select the PNG through **attach png** and require
   the control to render `attached · <filename>` before sending. Direct API upload alone
   does not prove the affordance.
2. **One id across host surfaces.** The upload response's `attachment.id`, the next
   `/api/turn.attachment_id`, the retrieval response's `x-lash-attachment-id`, and the
   retrieval URL must agree. The trace deliberately records bytes SHA-256/length/MIME
   rather than the host attachment id; reconcile those content facts separately.
3. **Compare bytes, not availability.** Save the source and both retrievals; SHA-256 and
   byte length must match exactly before and after restart.
4. **Replace the web process.** Use `just agent-workbench-restart <port>` and require the
   PID to change while the data directory and session id remain unchanged.
5. **The attachment facet is the same in both session modes.** The workbench always wires
   `FileAttachmentStore`; SQLite/Postgres changes the session ledger, not attachment blob
   storage. The deterministic companion gate reopens that file store and separately runs
   the usage restart assertion against both session-store backends.

## Working material

- First run `just agent-workbench-attachment-usage-gate <port>`. It is model-free and
  asserts upload → reference → persist → retrieve, non-zero internally consistent usage,
  JSONL `llm_call_completed` agreement, and exact usage after reconstruction. Its managed
  Postgres stays inside the worktree block at offset `+0..+9`, selected by the last decimal
  digit of `<port>` (`3042` selects `+2`); its container name also derives from `<port>`.
- Boot the browser scenario with a fresh directory:
  `AGENT_WORKBENCH_DATA_DIR=<fresh-tmp> AGENT_WORKBENCH_OPEN=0 just agent-workbench <port>`.
  Require `OPENROUTER_API_KEY`; missing credentials are a harness gap → Abort. Teardown is
  `just agent-workbench-down <port>`.
- Prepare a valid PNG no larger than 1 MiB and record its filename, byte length, and
  SHA-256 in the artifact directory.
- UI truth: **attach png**, its attached filename state, transcript, running/idle pill.
- API truth: `POST /api/attachments`, `POST /api/turn`,
  `GET /api/attachments/{attachment_id}`, and `GET /api/state`.
- Disk truth: `<data-dir>/attachments/` and `<data-dir>/trace.jsonl`.

## Phase 0 — Boot and identify the session

Poll `/healthz`, open the browser, and require the rendered session id to equal
`/api/state.settings.session_id` and `<data-dir>/session-id`. Record the PID and screenshot
`00-ready.png`.

## Phase 1 — Upload through the composer

Choose **attach png** and select the prepared file while capturing the
`POST /api/attachments` response. Poll until the control renders
`attached · <filename>`. Require HTTP 200, MIME `image/png`, exact source byte length, a
non-empty content-addressed `attachment.id`, and a `retrieve_url` containing that id.

Save the response as `01-upload.json` and screenshot `01-attached.png`. GET the returned
URL, require `content-type: image/png` and the matching `x-lash-attachment-id`, save the
body as `01-before-restart.png`, and compare its SHA-256 with the source.

## Phase 2 — Reference the attachment in a turn

Enter a short prompt with a unique marker such as `FIG425-ATTACH-<run-id>` asking for a
brief description, then press **send** while capturing `/api/turn`. Require its request
JSON body to contain the upload id as `attachment_id`. Poll until the UI is idle,
`/api/state.active_turns` is empty, and the committed user/assistant pair is rendered.

From the matching trace turn, save the `llm_call_started` record as
`02-provider-request.json`. Require one request attachment with MIME `image/png`, source
byte length, and `bytes_sha256` equal to the source hash. A plausible visual answer without
this provider-request evidence is not a pass. Screenshot the scrolled transcript as
`02-referenced-turn.png`.

## Phase 3 — Replace the process and retrieve again

Run `just agent-workbench-restart <port>` and poll `/healthz`. Require a changed PID and
unchanged rendered/API/disk session id. Reload the page, GET the original `retrieve_url`,
and save the body as `03-after-restart.png`. Require its id header, byte length, and
SHA-256 to match both the source and `01-before-restart.png`. Screenshot the reconstructed
transcript and usage rail as `03-restarted.png`.

Any missing blob, changed hash, changed id, or UI/API/trace disagreement is a contract
violation → Abort/RCA.

## Phase 4 — Teardown and score

Run `just agent-workbench-down <port>` and confirm the workbench and its managed services
are gone.

| Item | Objective gate | Verdict | Evidence |
|------|----------------|---------|----------|
| Deterministic companion | SQLite + Postgres gate command exits zero | | command log |
| Rendered upload | attached filename is visible before send | | `01-attached.png`, upload JSON |
| Byte fidelity | source and pre-restart retrieval hashes/lengths agree | | source, `01-before-restart.png` |
| Turn reference | `/api/turn` carries the upload id; trace carries matching content facts | | request capture, `02-provider-request.json` |
| Committed turn | UI and `/api/state` contain the settled pair | | `02-referenced-turn.png`, state JSON |
| Restart persistence | PID changed; post-restart retrieval is byte-identical | | `03-after-restart.png`, command log |
| Cross-surface identity | upload/turn/retrieval ids all agree | | saved JSON + headers |

**Aggregate:** did the rendered workbench flow carry one exact PNG through Lash's image
turn contract and preserve its retrievability across a cold web-process restart?

---

_Stop triggers and the Abort/RCA + reporting protocol are in [../RULES.md](../RULES.md)._

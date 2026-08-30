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

This PNG boundary belongs to the **WORKBENCH upload surface**, not to Lash's provider
contract. The transport capability source of truth is
[`lash-core/src/llm/transport.rs`](../../crates/lash-core/src/llm/transport.rs): OpenAI
Responses and Chat, Anthropic Messages, and Google Gemini each enforce their own image/file
allowlists (with Google's additional audio/text/video family support). A syntactically valid
MIME outside the selected transport's allowlist returns the typed
`unsupported_attachment_capability` refusal before wire serialization; it is not silently
dropped or left for a provider HTTP error.

| Surface | Attachment boundary |
|---------|---------------------|
| Agent Workbench upload | `image/png` only |
| OpenAI Responses | `OPENAI_IMAGE_MIMES` + `OPENAI_FILE_MIMES` |
| OpenAI-compatible Chat Completions | `OPENAI_IMAGE_MIMES` |
| Anthropic Messages | `ANTHROPIC_IMAGE_MIMES` + `ANTHROPIC_FILE_MIMES` |
| Google Gemini | `GOOGLE_IMAGE_MIMES` + `GOOGLE_FILE_MIMES` + `GOOGLE_MEDIA_FAMILIES` |

The fixture laws for the transport allowlists live with the provider crates. The runbook's
`image/png` assertions therefore judge the Workbench boundary and one supported transport
intersection; they are not a claim that Lash accepts only PNG.

**Real tokens.** The referenced turn uses OpenRouter. Judge attachment plumbing and
cross-surface identity, not the quality of the model's image description.

## Scenario-specific golden rules

1. **Use the rendered upload control.** Select the PNG through **attach png** and require
   the control to render `attached · <filename>` before sending. Direct API upload alone
   does not prove the affordance.
2. **One id across host surfaces.** The upload response's `attachment.id`, the next
   `/api/turn.attachment_id`, the retrieval response's `x-lash-attachment-id`, and the
   retrieval URL must agree. Durable effects normalize inline bytes to a stored source
   before `llm_call_started`, so correlate three trace records: the upload event carries
   id/byte length/MIME, `llm_call_started` carries the stored source/MIME and the exact
   reference in the rendered prompt, and `provider_request` carries the serialized wire
   body's length and SHA-256. `bytes_sha256`/`bytes_len` on a trace attachment are
   inline-source fields and are intentionally absent after this normalization.
3. **Compare bytes, not availability.** Save the source and both retrievals; SHA-256 and
   byte length must match exactly before and after restart.
4. **Replace the web process.** Use `just agent-workbench-restart <port>` and require the
   PID to change while the data directory and session id remain unchanged.
5. **The attachment facet is the same in both session modes.** The workbench always wires
   `FileAttachmentStore`; SQLite/Postgres changes the session ledger, not attachment blob
   storage. The deterministic companion gate reopens that file store and separately runs
   the usage restart assertion against both session-store backends.
6. **The transcript image is the attachment contract.** The matching user row must contain
   exactly one linked image whose `data-attachment-id`, `src`, and link target agree with
   the upload response. A filename pill or successful provider call does not substitute for
   a rendered image.

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
- Disk truth: the active session backend's committed attachment-manifest row,
  `<data-dir>/attachments/blake3/<first-two-id-characters>/<attachment-id>`, and
  `<data-dir>/trace.jsonl`. SQLite calls the per-session table `attachment_manifest`;
  Postgres calls the shared table `lash_attachment_manifest`.

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

Enter a short prompt with a unique marker such as `FIG994-ATTACH-<run-id>` asking for a
brief description, then press **send** while capturing `/api/turn`. Require its request
JSON body to contain the upload id as `attachment_id`. Before the turn settles, poll the
matching optimistic user row until it contains exactly one linked `<img>` with the upload
id in `data-attachment-id` and the upload `retrieve_url` as both `src` and link target.
Require `complete === true`, positive `naturalWidth` and `naturalHeight`, and a rendered
box no larger than 640 × 420 CSS pixels. GET that image URL independently and require HTTP
200. Screenshot the scrolled row as `02-rendered-image.png`.

Then poll until the UI is idle, `/api/state.active_turns` is empty, and the committed
user/assistant pair is rendered. Require the matching `/api/state.messages` user message
to carry exactly one `attachments` entry with the same `attachment_id` and `retrieve_url`.
Re-check that the settled row still contains exactly one loaded image; this is the FIG-972
handoff from the UI-owned row to committed backfill, not a second row.

Complete the three-layer attachment cross-check before continuing:

1. **DOM:** one matching user row and one loaded linked image with sane natural and rendered
   dimensions.
2. **API state:** one matching user message and one attachment reference with the upload id
   and retrieval URL.
3. **Durable state:** one committed manifest row for the session/id plus the content blob at
   the expected `attachments/blake3/` path; hash and length must match the source.

From the matching trace turn, save the `llm_call_started` record as
`02-provider-request.json`. Require one request attachment with `source: stored` and MIME
`image/png`, and require the request's rendered attachment descriptor to contain the upload
id as its `reference`. Save the matching `agent_workbench.api.attachment.uploaded` record as
`02-upload-trace.json` and require its id, MIME, and byte length to match the source. Save
the `provider_request` record with the same `llm_call_id` as `02-provider-wire.json`; require
a non-empty serialized body with a SHA-256 digest. These correlated records prove the exact
stored source reached a real provider request while the upload/retrieval/blob checks prove
its content facts. A plausible visual answer without this trace chain is not a pass. Save
`/api/state` as `02-state.json` and screenshot the settled scrolled transcript as
`02-referenced-turn.png`.

## Phase 3 — Replace the process and retrieve again

Run `just agent-workbench-restart <port>` and poll `/healthz`. Require a changed PID and
unchanged rendered/API/disk session id. Reload the page, GET the original `retrieve_url`,
and save the body as `03-after-restart.png`. Require its id header, byte length, and
SHA-256 to match both the source and `01-before-restart.png`; the content-addressed image
route must return 200 and must not inherit the 409 semantics of session-scoped admission.

Poll the reconstructed committed user row until its linked image is complete with positive
natural dimensions. Re-run the same DOM/API/durable three-layer cross-check: the reloaded
DOM has one image, `/api/state.messages` has the same committed attachment reference, the
manifest row remains committed, and the blob still has the source hash and length. Save the
state as `03-state.json` and screenshot the reconstructed transcript and usage rail as
`03-restarted.png`.

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
| Turn reference | `/api/turn` carries the upload id; correlated upload/request/wire traces carry matching reference and content facts | | request capture, `02-upload-trace.json`, `02-provider-request.json`, `02-provider-wire.json` |
| Live image render | optimistic user row has one loaded, bounded, linked image; retrieval is 200 | | `02-rendered-image.png`, DOM capture, headers |
| Committed turn | one settled UI row and `/api/state` attachment ref agree with committed manifest/blob | | `02-referenced-turn.png`, `02-state.json`, manifest query |
| Restart persistence | PID changed; image reloads from the committed ref and retrieval is byte-identical | | `03-restarted.png`, `03-state.json`, `03-after-restart.png`, command log |
| Cross-surface identity | upload/turn/retrieval ids all agree | | saved JSON + headers |

**Aggregate:** did the rendered workbench flow carry one exact PNG through Lash's image
turn contract and preserve its retrievability across a cold web-process restart?

---

_Stop triggers and the Abort/RCA + reporting protocol are in [../RULES.md](../RULES.md)._

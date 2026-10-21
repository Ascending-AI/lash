# Provider bodies are persistent segments (FIG-6443)

Status: implemented with FIG-6443. Binding: ADR 0133 §5–§6 (admission, the
fragment index, WIRE-SLOTS) and ADR 0135 (slots). This note replaces the
coalescing body contract those ADRs described.

## Problem

Every model call lowered the whole history again on the host. FIG-6441
measured the per-call allocation slope per prior turn on the coding-heavy
replay (DHAT, turns 2 and 8): canonical lowering 1.17 MB, admission
fragments 459 KB, canonical decoding 420 KB, prompt rendering 229 KB (fixed
by FIG-6441) and wire materialization 178 KB. FIG-6450 found the same shape
on the analysis turns: protocol events cloned, cell records and retained
results decoded, admission fragments encoded and provider JSON built again
for every prior turn.

The cause was the body contract. A `RecordedRequestTemplate` was one
coalesced sequence: literal JSON text alternating with attachment slots, no
two literals adjacent. A provider built a `serde_json` tree of the whole
request and serialized it into that sequence, so nothing a previous call
built could be kept. Admission scanned the whole text again to cut it into
content-addressed fragments. The in-process model concatenated the whole
text and parsed it back. `LiveRequestBody::wire()` copied it all into one
`String`.

## How others do it

- Provider SDKs and agent frameworks convert the full history on every call.
  The Vercel AI SDK runs `convertToModelMessages` over the whole message array
  in each route handler; LangChain's `convert_to_openai_messages` and memory
  classes rebuild the message list each call. Neither caches conversions:
  the cost they care about is tokens, and their histories are short.
- The stateless provider APIs all reward an exact, byte-stable prefix.
  Anthropic caches by exact prefix in the order tools, system, messages up to
  a `cache_control` breakpoint. OpenAI routes on a hash of the first tokens
  and hits only on exact prefix matches of at least 1024 tokens. Gemini's
  implicit caching hits when a request shares a common prefix with an
  earlier one. Any change to an early byte, such as a timestamp in the
  system prompt, misses every turn.
- The only incremental client we found is Codex CLI's Responses WebSocket
  mode. It keeps the previous request's input items and sends a
  `response.append` with only the new tail when the non-input fields are
  unchanged and the new input is a strict extension of the old. Otherwise it
  sends a fresh `response.create`. Server-side state
  (`previous_response_id`, the Conversations API) avoids resending history
  at all, but it is provider state, which ADR 0133 rules out: the body is
  the only statement of what a call asks.
- Compilers and build systems solve the same problem with immutable,
  content-identified units and memoized derivations keyed by identity
  (rustc's interned queries, Merkle trees). The admission fragment index is
  already such a tree.

Sources: Anthropic prompt caching
(https://docs.anthropic.com/en/docs/build-with-claude/prompt-caching),
OpenAI prompt caching
(https://developers.openai.com/api/docs/guides/prompt-caching), Gemini
context caching (https://ai.google.dev/gemini-api/docs/caching), Vercel AI
SDK `convertToModelMessages`
(https://upstash.com/blog/ai-sdk-chat-history), LangChain message
conversion
(https://reference.langchain.com/python/langchain-core/messages/utils/convert_to_openai_messages),
Codex CLI Responses WebSocket incrementality
(https://forge.lthn.ai/core/core-agent-ide/commit/0639c3389263c4fea045ecbcf8b95f84e8197fa9).

## Decision

Lash keeps sending stateless, whole-history bodies, byte for byte as before.
Only the host's rebuild work becomes incremental: what an unchanged prefix
lowered to is kept and reused, and nothing derived from it is recomputed.

### Segments

A body is a sequence of immutable `RequestSegment`s. A segment is shared
(`Arc`) and holds parts: literal JSON text and attachment slots. It has one
of two kinds:

- **Text**: any run of JSON text, read in context with its neighbours. The
  envelope (model, tools, instructions, generation fields) is text.
- **Item**: exactly one complete JSON value that stands as an array element.
  The body writes `,` between two adjacent items, so a plugin can insert,
  drop or reorder items without touching separators. A history message (or
  each wire item a message lowers to) is an item.

The body's bytes are the parts in order, slots filled, with the implicit
commas. Template validity is checked once per construction without
re-reading cached items: an item is validated when it is built, and the
template validates a skeleton where each run of items stands as one value.
A run of two or more items must sit in an array, and no run may sit at the
top level, so the envelope alone states the response mode.

Template equality is wire equality: two templates are equal when their
metadata, bytes and slots are equal, however they are segmented.

### Reuse by retained identity

`SegmentCache` maps a lowered unit to its segments. The key is a digest of
the provider's namespace, a context digest (the request-level inputs the
lowering reads: model, acceptance, dialect flags, codec), the identity of
each source message the unit lowers (the address of its shared, immutable
`blocks` allocation, its role and its user-segment marker) and the unit's
modifiers (its position where a provider numbers by position, a cache
breakpoint, mapped tool ids). The entry holds the source `blocks` it was
lowered from, so the address cannot be reused while the entry lives, and a
shared allocation cannot change: `Arc::make_mut` on it copies. A hit
therefore always carries the content its key names.

Invalidation needs no hook. Compaction, a history edit, a reasoning or
replay drop, and a frame switch all produce new message allocations, or new
positions, so their units miss and are lowered again. A provider, model or
configuration switch changes the context digest. A plugin that rewrites a
message rewrites its allocation. Nothing stale can be returned; at worst an
entry is never hit again and ages out. The cache is bounded by the bytes of
the segments it holds (least recently used out) and lives with the provider
components, shared by their clones like the rate limiter, so it spans turns
and sessions.

### Default providers

Each default provider lowers per *wire unit*: the smallest run of source
messages whose wire form reads nothing outside it. A structural pass over
the history decides units and modifiers without building JSON; only a unit
that misses is built. The shared `UnitWriter` (`lash_core::provider::
attachment_wire`) writes each unit through the cache, pinning its slots
once, and the provider's whole-JSON builder (used by its unit tests and by
any caller that wants the plain body) writes the same units with
`UnitWriter::Plain`, so both paths run the same code.

- Canonical (`RecordedRequestTemplate::of_request`): one item per message.
  The cache carries the lowered history under the first message's retained
  identity and the lowering context. Each call compares message identities
  (blocks, role and user-segment marker), keeps the matching prefix's items
  and lowers only the changed suffix. Immutable chunks own each source with
  its derived item; appends share complete chunks and copy at most the final
  32 records. Each chunk carries its cumulative literal-byte charge, so cache
  accounting does not recount the prefix. Branches retain only their matching
  prefix, and an oversized suffix cannot stay reachable through an earlier
  cached history. Replacing the history updates its cache byte charge; an
  oversized history is not retained.
  Cache loss rebuilds from the request before admission. Recorded bodies
  own their segments independently, so later branches cannot change an
  admitted request and a resend needs no cache.
- Anthropic Messages: one item per wire message, the run of same-role
  source messages it merges. Its key holds each member's native-feedback
  form (decided with look-ahead), the role, the cache-control target after
  the results-first sort, and the wire ids of its tool calls when the
  request maps an illegal id (the map is request-wide).
- OpenAI Responses and Codex: the items from one assistant message that
  writes an item up to the next. Within such a group user content merges
  and tool outputs move ahead of feedback; across groups nothing does. The
  key holds the group's first message index, which default item ids name.
- OpenAI Chat Completions: the messages from one assistant message to the
  next (the boundary that ends the feedback a tool message moves before).
  The cache-control target (the last marked text part, else the last text
  part, else the instruction message) is planned first and keyed into the
  group that holds it.
- Google: one item per merged `contents` entry, with the
  functionResponse-first order.

Envelope fields are rebuilt each call; they do not grow with history. A
message a transform rebuilds on every call is a new allocation every call
and is lowered every call: a reasoning or replay block dropped for a
foreign route (`drop_foreign_replay`, `reasoning_retention_safe_for`) is the
one such transform, and it only touches messages whose replay names
another route.

### Derived facts live with the segment

`RequestSegment::derived` memoizes a value computed from a segment for the
segment's lifetime. Admission stores each item's fragment encodings and
content addresses there. The in-process canonical decode stores each
message item's decoded `LlmMessage` there. A segment a plugin replaced is a
new segment with no derived facts, so it is encoded and decoded afresh.

### Admission

The fragment index keeps its shape (ADR 0133 §5): literal and slot leaves
under a canonical append tree. Fragment boundaries now follow items: each
item, with the comma before it, is its own leaf (still split at 32 KiB and
at line groups of long strings). Text segments chunk as before. The reusable
session prefix ends after the last history item. Fragment texts are shared
(`Arc<str>`) from the segment through `PromptText` to the store, which binds
only the bodies it does not hold. A new call encodes its new items and
logarithmic index paths; the prefix costs pointer copies.

### Decoding

`canonical_request` decodes the envelope with the history run removed, and
takes each message from its item's memo. It does so only where the item run
is exactly the top-level `messages` array; any other body (a resend read
back from the store, a custom plugin's whole-body rewrite) is decoded whole,
as before.

### Wire

`LiveRequestBody::wire() -> String` is gone. A body is read as chunks
(`wire_chunks`), written (`write_wire`) or measured (`wire_len`); none of
them copies a literal. The in-process canonical model reads the request
from the segments and never materializes the body. An HTTP provider hands
its transport one contiguous body (`to_wire_string`, allocated once at the
exact `wire_len`): the transport contract takes `Bytes` and resends them on
a token retry, and reqwest computes `Content-Length` from them. A chunked
request body would save that one copy per attempt; it needs a streaming
body in every transport and its test doubles, and is left as a follow-up.
The Responses resume fingerprint hashes the serialized template as it is
written, and the template serializes its coalesced literals piece by piece.

### Plugin control is unchanged

A provider plugin still sees and controls every byte:

- `template.segments()` and `template.parts()` expose every segment, part
  and slot in wire order; `RequestSegment::item`, `item_json`, `text` and
  `from_parts` build new segments.
- `template.edit_segments(|segments| ...)` maps, replaces, inserts or drops
  segments and validates the result; `template.map_slots(...)` re-pins
  slots and keeps every slot-free segment shared.
- `template.to_json()` returns the whole body as one `TemplateJson` tree,
  slots as attachment nodes, and `RecordedRequestTemplate::from_json`
  rebuilds a template from a tree: a plugin that wants the body as one
  value still gets it. That template is one text segment, so the plugin
  opts out of reuse for the call it rewrote, and only for that call.
- A decorator receives the same `SegmentCache` in `Provider::lower` and may
  key its own lowered units into it under its own context namespace.

The custom-plugin law pins both: a whole-body rewrite and a segment
mapper each send byte for byte what lowering the request they mean sends,
and the body reads back as that request.

### Prompt caching

The bytes are unchanged, so provider prompt caching is unchanged. Reuse makes
the stability visible: an item reused from the cache is byte-identical to
the one sent before, so the prefix up to the first changed item is the same
bytes every call.

### Analysis turns (FIG-6450 C)

A `ProtocolEvent`'s payload is a `ProtocolPayload`: a shared, immutable
JSON value that serializes as the value itself, so the stored shape is
unchanged. Cloning history copies pointers; the restore view shares the
active events instead of copying them. A writer changes a payload
copy-on-write (`make_mut`), which gives it a new identity.

Decoded facts live with the payload, as with segments:
`ProtocolPayload::derived` memoizes a value computed from it for the
payload's lifetime. The RLM decoder stores each event's validated decode
there, so the transcript fold, the history index and a restore read one
decode of each retained event; the per-dialect event cache and its clears
are gone. A decoded trajectory entry is a shared `Arc<CellRecord>`, and the
transcript shares it rather than cloning it. Admission fragments and
provider JSON follow from the segment rules above.

## Consequences

- Per-call host allocation for an unchanged prefix is O(new suffix) plus
  pointer copies per unit; the measured receipts are in FIG-6443.
- Hosts that write provider plugins: `LiveRequestBody::wire() -> String`
  and the coalesced segment sequence (`RequestSegment::Literal` /
  `RequestSegment::Attachment`, `from_segments` over coalesced literals)
  are replaced by `wire_chunks`/`write_wire`/`wire_len`/`to_wire_string`,
  `parts()`, shared `RequestSegment`s, `edit_segments`, `map_slots` and
  `to_json`/`from_json`. `Provider::lower` takes the `SegmentCache`.
- The serialized template keeps its shape (coalesced literals and slots);
  a template read back from the store is one text segment and is sent and
  decoded whole, as before.
- Fragment boundaries follow items for new admissions; stored records keep
  their shape and old records still assemble.
- Not incremental: a body a plugin rewrites whole, messages a foreign-route
  transform rebuilds, the envelope, the one contiguous HTTP body per
  attempt, and the Codex WebSocket path, which parses the admitted body to
  plan its continuation. Each is O(what it rewrites or sends).
- RLM sessions render their history messages from events on every call,
  and the plugin session that renders them is materialized per call, so
  those messages are new each time and their items are lowered and
  encoded again (about 15 KB per prior turn on the FIG-6450 analysis
  replays). Keeping them needs a
  session-lived holder of the last rendering; it is a follow-up.

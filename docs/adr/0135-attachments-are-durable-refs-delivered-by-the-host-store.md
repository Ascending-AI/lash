# 0135: Attachments are durable refs delivered by the host store

## Status

Accepted. Implementing it is open work that this decision depends on: the
ref and the store's delivery (FIG-5443), provider acceptance and slot codecs
(FIG-5444), request templates and live slot filling (FIG-5445), producers and
history (FIG-5446), and the remote shapes (FIG-5447), integrated as one series
by FIG-5448. Until FIG-5448 lands, `AttachmentSource`
(`crates/lash-sansio/src/llm/types.rs`) and the exact-body admission of
ADR 0133 §6 are on main.

## Context

An attachment reached a model call in four shapes: inline bytes, a stored
ref, an external URL and a provider file id. Each shape flowed through
history, the remote wire, commit identity, request budgets, RLM projections,
traces and every provider adapter. Lash fetched none of the borrowed shapes,
so a URL or provider file id could be durable history with no content
identity, no retention and no expiry handling: commit identity hashed the
URL's spelling, not the content behind it.

The delivery form is a property of one request to one provider, not of the
attachment. The same image may travel as base64 to one route, as a signed
URL to another, and as an uploaded file id to a third. A signed URL or file
id is also short-lived and often secret, so it cannot be recorded. Yet
ADR 0133 §6 records the exact provider body before the first byte is sent,
and resends it unchanged after a crash: a delivered URL placed in that body
would be durable secret material that could expire before the resend.

## Decision

### 1. One durable ref

`AttachmentRef { id, media_type, byte_len, type_metadata, label }` is the only
attachment concept in input, messages, tool values, tool results, history,
checkpoints and the remote protocol. `AttachmentId` is the content digest:
exactly 64 lowercase hex characters of the domain-separated BLAKE3 hash of
the bytes (`content_id`). Nothing else parses as an id, so a ref names
content, never a locator.

Two refs name the same content when their ids are equal; blobs, referrer
edges and delivery caches key on the id. Identity is the whole ref: commit
identity and request identity hash all five fields in occurrence order and
no delivery form, so the same request delivered by bytes or by URL has one
identity, and changing the MIME, length, type metadata or label changes it.

A ref arriving from a host, a tool, a guest or a remote peer is a claim. It
becomes trusted when the referrer authority adopts it on upload evidence
(ADR 0124), and its length is checked against the content at the first read
or delivery.

### 2. Acceptance is host data, narrowed by the route

The session records the host's attachment catalogue
(`AttachmentCapabilitySnapshot`, ADR 0026): for each provider, the media
types and families a model accepts at each position (a message block or a
tool result) and the delivery forms the host permits (`bytes`, `url`,
`provider_file`). The serving provider declares the forms its route can
encode (`Provider::attachment_accepts`) and its provider-file scope
(`ProviderFileScope { provider, endpoint, credential_scope }`, never a
credential). The effective acceptance of one attachment is the
intersection: a `ProviderAccepts { bytes, url, provider_file }` for that
MIME at that position.

An attachment no provider in the catalogue accepts is replaced by its
deterministic notice before dispatch. One the catalogue accepts but the
serving route cannot encode is refused at lowering as
`unsupported_attachment_capability`. Scopes compare exactly; a provider file
id is never used outside the scope it was created in.

### 3. The host store delivers

The host's `AttachmentStore` turns a ref into one accepted form when a
request is sent: `deliver(ref, &ProviderAccepts, &DeliveryLimits) ->
Delivery::{Bytes, Url, ProviderFile}`. SQLite delivers bounded bytes. S3
delivers a presigned GET URL when a URL is accepted and the horizon fits a
signature, else bounded bytes. `invalidate_delivery` forgets one cached
delivery a provider rejected, comparing it so a newer entry survives, and
`get` remains for explicit reads by tools and archives.

A URL or provider file must serve exactly the content the ref names,
unchanged, through the attempt's validity horizon. A backend that cannot
guarantee that delivers bytes or refuses. Lash never fetches a mutable URL
on a host's behalf, and a host registers URL-served content only through the
guarded put lifecycle, never by minting a ref from a URL.

Provider-file reuse belongs to the store. An optional component wraps any
backend with per-provider uploaders and a bounded cache keyed by content id,
MIME and exact scope. The cache is a derivative: eviction or expiry never
ends a referrer or deletes original bytes, a miss re-uploads from the
original, and duplicate uploads after a crash are tolerated.

### 4. Delivery is transient

A delivery happens inside one attempt of an admitted call, under that call's
pinned deadline and cancellation, and outside every recorded effect. A
`Delivery`, its URL or file id, an encoded slot value and the filled live
body have no serialized form and print redacted. None of them reaches an
admission record, a journal, a trace, an error, failure evidence, an
observation or a log; provider text that may echo them is scrubbed before it
leaves the attempt. Traces describe an attachment by id, MIME, length,
position and delivery form only.

A delivery fault is an unsent attempt failure with a typed cause: a
transient store fault is retryable within the call's deadline and settles as
`attachment_delivery_unavailable` when it outlasts the retries; a missing
blob, a content mismatch or an exceeded bound settles as
`attachment_resolution_failed`; no producible accepted form settles as
`unsupported_attachment_capability`. A provider's definite rejection of a
delivered file id or URL invalidates that delivery and retries the attempt.
None of these changes history or degrades an attachment to a notice.

### 5. Budgets stay an engine bound

`AttachmentReadPolicy` keeps its 32 MiB per-blob and 128 MiB per-request
bounds (ADR 0058). Each attempt reserves one request budget across its slots
before any backend call: a per-occurrence envelope for MIME and label, each
unique blob's retained buffer once, its base64 expansion for every
occurrence, and the escaped length of every URL or file id. A backend
receives only the actual-byte bound it must enforce. Exceeding a bound is an
unsent refusal.

### 6. A call records a template and fills its slots live

A provider lowers a call to a `RecordedRequestTemplate`: its exact literal
JSON text with one typed `AttachmentSlot { reference, position, accepts,
codec }` wherever an attachment value goes. The admission records the
template, and every attempt fills each slot from a fresh delivery through
the slot's pinned codec (ADR 0133 §6). A completed call replays its recorded
result with no delivery.

### 7. Referrers hold refs, never deliveries

Liveness follows ADR 0124's holders with no new kind. A host puts through the
session's guarded store before it sends, under an expiring upload holder,
and the enqueue of input naming the ref acquires the session's edge. A tool
puts under its execution. Before a call is admitted, every slot's ref is
acquired under the holder its owner puts under, so a ref named only by an
admitted call survives a takeover and is reclaimable once that owner
settles.

### 8. The remote protocol carries refs

Remote input, LLM blocks, tool-result blocks and tagged tool values carry
`{"type": "attachment", "reference": {id, media_type, byte_len,
type_metadata?, label?}}`. Inline bytes travel only through the host's
remote put operation, which answers a ref before a turn names it. A remote
worker serving refs shares the attachment backend's namespace or resolves
through the host; a ref carries no bytes.

## Consequences

- Core has one attachment concept, and its identity is content.
- Hosts choose how bytes reach providers without changing history, identity
  or replay.
- Signed URLs and provider file ids never become durable or traced.
- Hosts put before they send, and a remote host exposes a put operation.
- Providers declare their encodable forms and keep a codec revision per
  slot shape.
- S3 gains presigning; provider-file uploads move from the Google adapter
  into a store component.

## Rejected alternatives

- **Keep an external-URL variant in core.** It records locator identity, not
  content, escapes retention, and carries secret and expiring values into
  history.
- **Record the delivered body for byte-identical resends.** It persists
  secrets and freezes expiring URLs.
- **Rebuild the provider body on every resend.** A changed renderer or
  builder would send different bytes after a takeover (ADR 0133 §6).
- **A ref of only id and digest.** Provider encoding, read bounds, RLM image
  handling and labels need the MIME, length and metadata.
- **Provider-owned global file caches.** They split credential scope and
  retention policy across adapters and keep a delivery concern in provider
  code.
- **A Lash-side file-id cache.** It duplicates the host store's credential
  and lifecycle authority and makes cross-process coordination core's
  problem.
- **Counting a cache entry's lifetime as a referrer.** It would either leak
  originals or let live content disappear.

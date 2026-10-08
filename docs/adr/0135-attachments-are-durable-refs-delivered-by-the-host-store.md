# 0135: Attachments are durable refs delivered by the host store

## Status

Accepted. The durable `AttachmentRef`, host-store delivery and
`RecordedRequestTemplate` cutover is on main. Admission records literals and
typed slots; each attempt fills those slots live (WIRE-SLOTS, ADR 0133 §6).

## Context

An attachment reached a model call in four shapes: inline bytes, a stored
ref, an external URL and a provider file id. Each shape flowed through
history, host projections, commit identity, request budgets, RLM projections,
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
checkpoints and host projections. `AttachmentId` is the content digest:
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
delivers bounded bytes too, unless the host opts into URL delivery
(`S3AttachmentStoreBuilder::presigned_url_delivery`, off by default): then
it delivers a presigned GET URL when a URL is accepted and the horizon fits
a signature, else bounded bytes. URL delivery is the host's statement that
the provider's servers can reach its store; nothing narrows a URL the
provider could not fetch back to bytes. `invalidate_delivery` forgets one
cached delivery a provider rejected, comparing it so a newer entry
survives, and `get` remains for explicit reads by tools and archives.

A URL or provider file must serve exactly the content the ref names,
unchanged, through the attempt's validity horizon. A backend that cannot
guarantee that delivers bytes or refuses. Lash never fetches a mutable URL
on a host's behalf, and a host registers URL-served content only through the
guarded put lifecycle, never by minting a ref from a URL.

Provider-file reuse belongs to the store. An optional component wraps any
backend with per-provider uploaders and a bounded cache keyed by content id,
MIME and exact scope, with host-set limits
(`LashCoreBuilder::provider_file_cache`). The cache is a derivative:
eviction or expiry never ends a referrer or deletes original bytes, a miss
re-uploads from the original, and duplicate uploads after a crash are
tolerated. The uploader is optional and never removes a working form: when
reading for upload or the upload itself fails, the slot is delivered by
another form it accepts, and the failure is logged by ref id and class. A
missing or mismatching original and a refused credential stay the call's
failure, and so does an uploaded file that expires before the attempt's
horizon (a terminal `upload` backend failure).

### 4. Delivery is transient

A delivery happens inside one attempt of an admitted call, under that call's
pinned deadline and cancellation, and outside every recorded effect. A
`Delivery`, its URL or file id, an encoded slot value and the filled live
body have no serialized form and print redacted. No delivered value is
journaled or recorded in an admission record: the recorded request is a
template, and its slots are filled live from fresh deliveries per attempt.
Request-body evidence and request traces use the template's ref summary.

Provider text in errors, failure evidence, stream deltas and trace events
is handled as for every other call, without scrubbing. Attachment calls
stream and resume under the same rules as calls without slots. Hosts should
sign URLs with short lifetimes, since a provider may echo one into an error.

A delivery fault is an unsent attempt failure with a typed cause: a
transient store fault is retryable within the call's deadline and settles as
`attachment_delivery_unavailable` when it outlasts the retries; a missing
blob, a content mismatch or an exceeded bound settles as
`attachment_resolution_failed`; no producible accepted form settles as
`unsupported_attachment_capability`. A provider's definite rejection of a
delivered provider file (a missing, deleted or expired file id) marks the
slots that attempt delivered as files and no others: those cached files are
forgotten and the attempt is retried, uploading afresh, unless the failure
is an authentication one. The live body tells the adapter each slot's
delivered form by name, so a slot sent as bytes or a URL is never marked. A
URL the provider cannot fetch has no recovery: it fails the call as the
provider classified it, which is the opted-in host's configuration to fix.
None of these changes history or degrades an attachment to a notice.

### 5. Budgets stay an engine bound

The host states `AttachmentReadPolicy` through `DataRetention::attachments`.
`AttachmentPolicy::standard()` selects 32 MiB per blob and 128 MiB per request
(ADR 0058); the selected bounds may differ. Each attempt reserves one request
budget across its slots before any backend call: a per-occurrence envelope
for MIME and label, and then what each delivered form costs. Bytes cost the blob's retained buffer
once per delivery group and four base64-sized encoding allowances for every
occurrence. A URL costs its escaped length for every occurrence. A provider file costs its escaped id for every
occurrence, plus the bytes it read as upload scratch when this delivery
uploaded it; a file reused from the cache reads nothing. A provider file is
never charged the base64 expansion: provider files exist to carry what does
not fit inline. Slots share one delivery when they name the same content as
the same media type under the same acceptance, since a provider file is
uploaded and typed by its media type. A backend receives only the actual-byte
bounds it must enforce, one for bytes and one for an upload. Exceeding a
bound is an unsent refusal.

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

### 8. Hosts transport attachment refs

Hosts define DTOs for attachment refs and put operations. They upload bytes
through their attachment backend before submitting a turn that names the ref.
A ref carries no bytes; another worker shares the backend namespace or resolves
through the host ([ADR 0136](0136-hosts-own-their-wire-contracts.md)).

## Consequences

- Core has one attachment concept, and its identity is content.
- Hosts choose how bytes reach providers without changing history, identity
  or replay.
- Delivered values stay out of admissions and journals by construction;
  provider text can echo them.
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

# Commit-identity families mint frozen unframed preimages

Amended 2026-09-23 (FIG-3540), **not yet implemented**: [ADR 0101](0101-one-session-ingress-carries-every-admitted-item.md) changes the
`lash.intent` typed payload at its reject-and-recreate cutover. The two
completed-claim lists become one, `enqueued_queue_batches` is removed, and the
pending follow-on (a session-head field) takes its place; the config carries
`config_revision`. The serializer, the unframed grammar and the domain label are
unchanged, as in the append-message generation 5 cutover below.

Lash has two coordinated durable-identity registries. `BLAKE3_DOMAINS`
(`lash-sansio/core_support.rs`) reserves the `lash-*/vN` hash labels that
`Blake3DomainHasher` mixes into every durable digest; it is append-only and
debug-asserted at every mint site. `FAMILY_DOMAINS`
(`lash-core-ids/stable_identity.rs`) reserves the semantic domains of families
whose preimages carry the `magic || salt || family-version || domain` framing
header emitted by `IdentityEncoder::new`.

Three families minted by `lash-core-store`'s `commit_identity.rs` —
`lash.append-request`, `lash.intent`, and `lash.history-node`, hashed under
the registered labels `lash-append-request/v2`, `lash-intent/v2`, and
`lash-history-node/v3` — predate the framed kit. Their digests are persisted
opaque evidence, not decodable data: append-receipt request hashes and
turn-commit hashes live in durable receipts and are compared by exact
equality to decide replay versus conflict, and history node ids are stored
graph rows. A preimage is only ever computed at write time; nothing can
re-derive and rewrite the stored digests afterward.

Adding the framing header to these preimages would therefore change every
minted digest and orphan all existing evidence: a retried append would stop
matching its stored receipt, and a retried commit would conflict instead of
replaying. The store's recreation-only cutover doctrine makes that legal at
a schema boundary but buys nothing — the salt lever exists to repudiate
families after a framing defect, and bytes that are equality-compared
evidence cannot be repudiated by framing them differently; recovery from a
defect in these grammars would still require a bespoke versioned encoding,
exactly the mechanism the append family already carries
(`APPEND_REQUEST_IDENTITY_ENCODING_VERSION` beside the digest in the receipt).

These families are therefore minted through `IdentityEncoder::new_unframed`,
which emits the family's frozen grammar without the header and only for the
grandfathered `FROZEN_UNFRAMED_DOMAINS` set. The three domains are also
registered in `FAMILY_DOMAINS` so a later family cannot silently claim the
same semantic space, and the registry is append-only: a frozen grammar stays
frozen. New durable identity families must use `IdentityEncoder::new`; the
unframed path must never grow a fourth member.

The append family's JSON projection is likewise frozen: it is a type-tagged
binary grammar that keeps `i64`/`u64`/`f64` numbers distinct, whereas
`identity_json`'s leaf is normalized `serde_json` bytes for caller-supplied
`Value` trees. The two cannot be unified without moving the minted digests.

The intent and history-node projections serialize typed structs through
`stable_json_string`; their canonicality comes from fixed struct field order,
not from `identity_json` normalization, which does not apply.

## Append-message generation 5 cutover

FIG-1952 and FIG-1955 intentionally replace the append-message projection in
one recreation-only store cutover. Every append request uses encoding 5.
Plugin messages encode id, role, origin and one ordered parts body. Parts
have no lifecycle discriminant; attachment sources remain inside attachment
parts. Provider replay routes and the separately retained response metadata
leaves are encoded explicitly. The obsolete payload-dependent encoders and
their corpora are removed; `append_request_identity_v5.hex` owns the new
node, whole-request, route and effect-cause vectors.

Amended 2026-09-24: the append encoding has since advanced to 7 (FIG-1961,
FIG-3515), and `append_request_identity_v7.hex` owns the current vectors. The
current value lives in `lash::formats`.

The append family retains its unframed envelope and `lash-append-request/v2`
hash label. Encoding 5 is the receipt's grammar discriminator, not a new hash
domain. The `lash.history-node` projection and the separately owned effect
address projections, versions and domain labels are unchanged. The intent
family's serializer and domain are also unchanged; its typed payload reflects
the new message shape at the same database cutover.

Pre-cutover catalogs are refused and recreated: SQLite durable-core 72 and
effects 32, PostgreSQL 112. Node bodies use generation 19, turn checkpoints 6,
tool settlements 5, and attempt captures 4. Remote protocol 86 and trace
schema 25 mark their wire changes. Durable read fixture generation 98 records
both backends. No removed message field or part lifecycle field is accepted
by a current decoder.

The renamed plugin mints compaction request IDs under
`lash-standard-compaction/v1`. Its former hash labels stay reserved in the
append-only domain registry as retired historical labels; they are not aliases
or active encoders. This plugin-owned domain change does not alter the generic
effect address or history-node families.

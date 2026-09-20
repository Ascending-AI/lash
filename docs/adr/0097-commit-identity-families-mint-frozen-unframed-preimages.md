# Commit-identity families mint frozen unframed preimages

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
(`LEGACY_APPEND_REQUEST_IDENTITY_ENCODING_VERSION` /
`APPEND_REQUEST_IDENTITY_ENCODING_VERSION` beside the digest in the receipt).

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

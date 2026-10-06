# Commit-identity families mint frozen unframed preimages

## Status

Accepted.

## Context

Append receipts, commit receipts, and graph rows persist opaque digests and
compare them by exact equality. Changing preimage framing changes idempotency and
conflict decisions even when the requested operation is equal.

## Decision

`BLAKE3_DOMAINS` reserves the hash labels mixed into durable BLAKE3 digests.
`FAMILY_DOMAINS` reserves the semantic domains of `IdentityEncoder` families.
Domain reservations prevent another family from reusing their names.

Exactly three families use `IdentityEncoder::new_unframed`:

| Semantic domain | Hash label | Persisted evidence |
| --- | --- | --- |
| `lash.append-request` | `lash-append-request/v2` | Append request receipt hash |
| `lash.intent` | `lash-intent/v2` | Runtime-commit intent hash |
| `lash.history-node` | `lash-history-node/v3` | Graph node identity |

Their frozen grammar contains no `magic || salt || family-version || domain`
header. `FROZEN_UNFRAMED_DOMAINS` admits only these three, which also occur in
`FAMILY_DOMAINS`. New families use framed `IdentityEncoder::new`.

The append projection is a type-tagged binary grammar that distinguishes
`i64`, `u64`, and `f64`. It cannot share `identity_json`'s normalized JSON leaf
without changing minted digests. Intent and history-node projections serialize
typed projections through `stable_json_string` and declared field order.
Golden corpora freeze the bytes.

Evidence: `crates/lash-sansio/src/core_support.rs:23`,
`crates/lash-core-ids/src/stable_identity.rs:21`, `:70`, `:237`, and
`crates/lash-core-store/src/store/commit_identity.rs:28`, `:643`, `:1218`.

## Append-message encoding

`APPEND_REQUEST_IDENTITY_ENCODING_VERSION` is 7. The discriminator sits beside
the digest in the receipt, separately from the unframed envelope and hash label.
`append_request_identity_v7.hex` pins the grammar. Plugin messages encode id,
role, origin, and ordered parts; attachment sources stay inside attachment
parts. Replay routes and response metadata have explicit projections.

The commit intent has distinct completed input and completed queue-batch lists.
It includes persisted config, including `config_revision`, and includes
`pending_follow_on` only when the commit leaves that obligation on the head.
These are typed payload facts, not additional identity families.

Evidence: `crates/lash-core-store/src/store/commit_identity.rs:28`, `:1218`,
`:1233`, `:1246`, and `crates/lash-core-store/src/store/identity_projection.rs`.

## Alternatives considered

Adding framing changes every digest and breaks equality evidence without
providing a decoder or recovery mechanism. Frozen families keep their grammar;
new families use framing from their first publication. One normalized JSON
encoder also changes append number identity and typed projection spelling.

## Consequences

- Idempotent retries compare exact persisted evidence with a pinned grammar.
- The unframed allowlist has exactly three members.
- Other identity owners retain their own domains and preimages.
- Format evolution follows the pre-1.0 freeze and
  [ADR 0115](0115-the-1-0-binary-carries-its-half-of-every-upgrade.md).

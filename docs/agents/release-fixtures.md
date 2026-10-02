# Release fixtures

ADR 0106 and ADR 0115 define the boundary: the durable fixtures of a release
tag are captured once, at the tag, and never regenerated.

At the exact tagged checkout, capture into a new destination:

```sh
. ./env.sh
kiln gate lash <fork> -- just release-fixtures-capture v1.0.0 fixtures/release/v1.0.0
python3 scripts/verify_release_fixtures.py fixtures/release/v1.0.0
kiln gate lash <fork> -- just release-fixtures-read-back fixtures/release/v1.0.0
```

The capture recipe sources `env.sh`, stages the VM worker through the shared
build pool, and runs the owning Kiln fixture generators under an owned PostgreSQL
16 service. Every leg holds what the tagged build wrote: the stores, the
parked-segment golden, the tool-intent journals from actual endpoint
interruptions, and the replay journals. Goldens that record a generation
record the tagged build's. Capture refuses a different tag commit and an
occupied destination.

The verifier checks tag provenance, six exact nonempty inventories, every
digest and byte count, and service identities from the gate environment. The
manifest's `source_commit` is the corpus's only provenance; the replay
journals carry their build generation themselves, and capture refuses journals
of more than one generation.

Read-back requires the corpus argument, parses every retained artifact, checks
SQLite integrity without writing, and runs typed store assertions against
copied SQLite catalogs and a restored PostgreSQL dump. No reader falls back to
local fixture sources.

Journal replay is a separate, non-required workflow during the freeze. It
reads the `replay-corpus` leg through `LASH_REPLAY_CORPUS_ROOT` and compares
every journal whose generation is this build's (`crates/lash-restate/README.md`).

At the cut, `docs/release/cut-1.0.md` describes regeneration, tagged capture
and required-gate activation. Existing rehearsal corpora are historical evidence.

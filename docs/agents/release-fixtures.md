# Release fixture cut preparation

FIG-4532 supplies opt-in tooling for FIG-4495. The release corpus and required
CI read-back remain gated on FIG-4495 and the 1.0 cut. No pre-cut tag is a
release compatibility baseline. ADR 0106 and ADR 0115 define the boundary.

At the exact tagged checkout, capture into a new destination:

```sh
. ./env.sh
kiln gate lash <fork> -- just release-fixtures-capture v1.0.0 fixtures/release/v1.0.0
python3 scripts/verify_release_fixtures.py fixtures/release/v1.0.0
kiln gate lash <fork> -- just release-fixtures-read-back fixtures/release/v1.0.0
```

The capture recipe sources `env.sh`, stages the VM worker through the shared
build pool, and runs the retained Cargo generators under an owned PostgreSQL
16 service. Historical segment captures stay intact;
their generators require their predecessor writers. Tool-intent journals come
from actual endpoint interruptions. Capture refuses a different tag commit
and an occupied destination.

The verifier checks tag provenance, six exact nonempty inventories, every
digest and byte count, and service identities from the gate environment.
Read-back requires the corpus argument, parses every retained artifact, checks
SQLite integrity without writing, and runs typed store assertions against
copied SQLite catalogs and a restored PostgreSQL dump. Both store checks are
ignored until invoked explicitly for FIG-4495. No reader falls back to local
fixture sources. `--runs-per-test 20` repeats the store checks uncached.

FIG-4097 and FIG-4533 own journal replay enforcement. Their input is the
`replay-corpus` leg, with per-scenario `journal.json` and capture provenance
in `manifest.json`. This read-back does not substitute for that replay job.

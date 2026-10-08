# Release fixtures

ADR 0106 and ADR 0115 specify the release boundary: a release corpus must
contain what the exact tagged build wrote and remain immutable afterward.
That is a release requirement, not evidence that tagged capture runs today.

## Current tooling and evidence gap

The current `justfile` retains `release-fixtures-read-back`, backed by
`scripts/read_release_fixtures.py`. Its verifier,
`scripts/verify_release_fixtures.py`, imports a capture module that is absent
from the tracked tree. The tagged-capture recipe is also absent. Consequently
neither capture nor verifier/read-back is an executable release proof in this
checkout. Do not regenerate a corpus under another build or claim a successful
tagged read-back from the presence of the recipe alone.

The retained reader describes explicit corpus admission: provenance and exact
inventories, artifact digests and byte counts, read-only SQLite integrity,
then typed store assertions against copied SQLite catalogs and a restored
PostgreSQL dump. These are intended checks, not a record of their execution.
Restoring capture and its imported definitions is separate implementation work
required before advertising that workflow.

Current encoded-format fixtures and their decode/resume laws live in
`crates/lash-durable-test/tests/format_fixtures.rs`. They exercise the build's
actor formats; they do not replace tagged-release capture or a two-build
rolling-upgrade proof. See [ADR 0115](../adr/0115-the-1-0-binary-carries-its-half-of-every-upgrade.md#6-current-format-evidence-and-release-proof-gaps)
for that distinction.

# Release fixtures

ADR 0106 and ADR 0115 specify the release boundary: a release corpus must
contain what the exact tagged build wrote and remain immutable afterward.
That is a release requirement, not evidence that tagged capture runs today.

## Current tooling and evidence gap

Tagged capture, verification and read-back are unavailable in this checkout.
The read-back recipe was removed because its reader depends on an absent
capture module. Do not regenerate a corpus under another build or claim a
successful tagged read-back. A tagged writer, provenance verification and
store read-back need implementation before the release workflow can be used.

Current encoded-format fixtures and their decode/resume laws live in
`crates/lash-durable-test/tests/format_fixtures.rs`. They exercise the build's
actor formats; they do not replace tagged-release capture or a two-build
rolling-upgrade proof. See [ADR 0115](../adr/0115-the-1-0-binary-carries-its-half-of-every-upgrade.md#6-current-format-evidence-and-release-proof-gaps)
for that distinction.

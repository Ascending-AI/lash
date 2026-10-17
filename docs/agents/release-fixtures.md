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

Before 1.0, durable laws record and resume within one run; the first release images are recorded fresh at the cut. Readable expected outputs use ignored `regenerates <repository-relative path>` tests gated on `LASH_REGENERATE=1`, discovered by `scripts/fixture_regenerators.py`.

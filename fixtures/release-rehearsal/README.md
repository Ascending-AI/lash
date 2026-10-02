# Release fixture rehearsal

`cut-1.0-dry-run` is the six-leg corpus `just release-fixtures-capture` wrote
against the 1.0 cut branch, under `lash.release-fixtures-manifest.v1`. Its
capture tag `cut-1.0-dry-run` was a local tag at the manifest's
`source_commit` and was never pushed, so `verify_release_fixtures.py` and the
read-back law pass only where that tag is recreated at that commit.

It is not a release baseline. CI's release-journal replay reads its
`replay-corpus` leg until the corpus of the real `v1.0.0` tag replaces it;
`docs/release/cut-1.0.md` has the commands.

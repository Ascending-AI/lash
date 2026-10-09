# Cutting 1.0

Main retains version-independent preparation from FIG-4802, including
fixture generators, refusal witnesses, strict-gate tooling and production
migration catalogs. Tagged capture and verification are currently unavailable
as described below. The version
freeze remains in force. The cut resets counters and artifacts, changes the
release channel, activates required gates and captures the tagged corpus.

Use a fresh Kiln fork and source `env.sh`. The orchestrator owns rebasing and
landing; implementation workers never rebase main themselves.

## Regenerate the candidate

FIG-4803 creates a fresh reset tree from current main on every rehearsal.
Regenerate version-dependent values through their owners; the old rehearsal
branch is disposable. The reset discovers fixture writers by their
`#[ignore = "regenerates <path>"]` declarations and runs them with
`LASH_REGENERATE=1`. Replace each newly found literal pin with its owner's
constant or generated table, and keep the copied dry-run corpus disposable.

```sh
kiln gate lash <fork> -- python3 scripts/release_reset.py --dry-run
kiln gate lash <fork> -- scripts/ci/with-service.sh pg -- \
  python3 scripts/release_reset.py --apply
kiln gate lash <fork> -- python3 scripts/release_baseline.py check
kiln fmt
```

The reset regenerates schemas, PostgreSQL artifacts, durable stores, replay
journals, tool-intent journals and the parked-loop segment. Synthetic adjacent
migration steps remain. Review the output against the dry run. Refresh literal
byte and identity pins through their owning generators or laws before committing.

The candidate must descend from the checked main tip. Its remaining paths
must belong to the reset plan, the release channel or gate activation:

```sh
kiln gate lash <fork> -- python3 scripts/check_cut_residual.py \
  --base origin/main --head HEAD
```

An unexpected path means preparation remains on the branch. Move that work to
main or give it an actual owning generator before proceeding. The checker
never grants a blanket exception for workflow changes.

## Activate the gates

After the reset, make version-bumps required in the CI conclusion and release
publication dependencies. Compare with the reset
commit. Comparing the reset with pre-cut main intentionally refuses counters
moving backwards.

```sh
kiln gate lash <fork> -- scripts/ci/version-bump-gate.sh HEAD <reset-sha>
kiln gate lash <fork> -- scripts/ci/version-bump-gate.sh HEAD
```

With no v1 tag, the second command checks the initial release baseline. After
a tag exists, it selects the newest preceding v1-or-later tag. Run selected
laws and required gates through Kiln under the current pre-land proof policy,
and report actual executed counts. The orchestrator lands the candidate and
changes the release channel in `Cargo.toml` at the cut.

## Tag and capture

Run the local Confidence runner on the release SHA before cutting, and record
its summary. Use `just confidence-local` at that checkout and attach the
`.kiln/confidence-local/<timestamp>/summary.txt` evidence to the release record.
The release workflow validates the release SHA's full-profile CI; Confidence
has no GitHub workflow or release precondition.

The release workflow owns the tag. The release owner certifies the exact main
SHA before publication; preparation workers do not dispatch CI or create the
release tag.

Tagged capture is a release requirement that cannot currently run: the capture
recipe and module are absent. The broken verifier and read-back reader were
removed. Do not claim that a tagged corpus has been certified. The
[release-fixture guide](../agents/release-fixtures.md)
describes the current gap, and [ADR 0115 §6](../adr/0115-the-1-0-binary-carries-its-half-of-every-upgrade.md#6-current-format-evidence-and-release-proof-gaps)
separates actor-format laws from release-upgrade proof.

Before the cut can capture a corpus, separate implementation work must restore
a tagged writer, verification and read-back. The resulting manifest must name
the exact source commit. Commit only that tagged corpus, remove rehearsal
inputs from required job selection, and keep tagged bytes immutable afterward.

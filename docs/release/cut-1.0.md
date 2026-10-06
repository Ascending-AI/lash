# Cutting 1.0

Main owns the version-independent preparation from FIG-4802: fixture
generators, refusal witnesses, generation-keyed replay, capture checks,
strict-gate tooling and empty production migration catalogs. The version
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
kiln gate lash <fork> -- scripts/ci/with-service.sh pg16 -- \
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

After the reset, make version-bumps and release-journal-replay required in the
CI conclusion and release publication dependencies. Compare with the reset
commit. Comparing the reset with pre-cut main intentionally refuses counters
moving backwards.

```sh
kiln gate lash <fork> -- bash scripts/ci/version-bump-gate.sh HEAD <reset-sha>
kiln gate lash <fork> -- bash scripts/ci/version-bump-gate.sh HEAD
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
release tag. At the tagged checkout, capture into an empty destination:

```sh
kiln gate lash <fork> -- just release-fixtures-capture v1.0.0 fixtures/release/v1.0.0
kiln gate lash <fork> -- python3 scripts/verify_release_fixtures.py fixtures/release/v1.0.0
kiln gate lash <fork> -- just release-fixtures-read-back fixtures/release/v1.0.0
```

Capture refuses a different tag, changed committed generator outputs and mixed
generations. Use the workspace Kiln feature graph for capture and replay. The
manifest names the exact source commit; each journal records its generation
and ordered entries.

Commit the tagged corpus and point `LASH_REPLAY_CORPUS_ROOT` in required replay
jobs at `fixtures/release/v1.0.0/replay-corpus`. Remove rehearsal inputs from
required job selection. The tagged corpus is immutable. Later builds compare
only journals with their generation; other generations retain their drain
routes. The added-step law checks the exact JOURNAL_LOGIC_EPOCH bump message.

# Cutting 1.0

The cut lives on the branch `lanes/cut-1-0`, one commit per cut ticket, in this
order:

| Commit | Ticket | Holds |
| --- | --- | --- |
| 1 | FIG-4485 | the baseline reset: every guarded constant at 1, the regenerated artifacts and goldens, the refreshed test pins, and the reset tooling |
| 2 | FIG-4493 | the empty PostgreSQL expand catalog and its laws |
| 3 | FIG-4494 | the strict version-bump gate as a required check |
| 4 | FIG-4495 | the rehearsal release corpus and the capture tooling |
| 5 | FIG-4097 | the release-journal replay as a required check |
| 6 | FIG-3846 | this document |

Run every step below in a fresh Kiln fork, from its root, after `. ./env.sh`.
`<fork>` is the fork's name.

## 1. Rebase

```sh
fork="$(kiln fork lash cut-1-0-final)" && cd "$fork" && . ./env.sh
git fetch origin main lanes/cut-1-0
git switch --detach origin/lanes/cut-1-0
git rebase origin/main
```

Main moves under the branch, so the FIG-4485 commit conflicts wherever main
bumped a guarded constant, changed a golden or touched a file the reset
renamed. Do not merge those by hand. In each conflicted file keep main's side
of every generated value (constants, hashes, fixture bytes, schema files,
`BUCK`), keep the branch's side of hand-written code, and continue; step 2
regenerates the generated values from the result.

Conflicts in the other commits are ordinary code conflicts.

Once the rebase is through, apply the changes main gained after the branch was
built:

- FIG-4631 (87b4e8abe1) left the recording double journaling a success as
  `{"Ok": value}` because the committed replay corpus pinned that shape. Make
  the double journal the plain value (`install_recorded_runtime_effects` and
  `decode_recorded_runtime_effect` in
  `crates/lash-restate/src/tests/recording_context.rs`) before step 2
  regenerates the corpus.
- FIG-4704 moves each store's version into one `compat.rs` constant that the
  descriptors and the on-disk stamps read (PostgreSQL 141, SQLite 99, 44 and
  12). Once it is on main, take main's side for those constants and for every
  store file it touched, and let step 2 reset the new constants to 1 and
  regenerate. Do not carry the branch's reset of the old backend literals
  across; `release_baseline.py check` names any stamp left behind.
- Any surface main registered since then is reset by step 2 without further
  work; any test main added that pins a pre-cut value shows up red in step 3.

## 2. Regenerate

```sh
kiln gate lash <fork> -- scripts/ci/with-service.sh pg16 -- \
  python3 scripts/release_reset.py --dry-run
kiln gate lash <fork> -- scripts/ci/with-service.sh pg16 -- \
  python3 scripts/release_reset.py --apply
python3 scripts/release_baseline.py check
kiln fmt
```

`--apply` is idempotent. It rewrites the constants, renames the versioned
schema files, runs every generator and ends with `kiln sync`. `check` must
print zero mismatches.

Then refresh the test pins the reset cannot derive. Run the affected tests and
fix each red by its kind:

```sh
kiln gate lash <fork> -- python3 scripts/dev-test.py --dependents
```

| Red | Refresh |
| --- | --- |
| a literal version in an assertion or fixture | the baseline value, 1; a refusal test uses a version outside the baseline range |
| an identity hash (`<family>:v1:blake3:…`, `frame-key/…`, a preimage table) | the value the failure prints as `left` |
| `semantic_boundary_request_v1.golden` | rerun its law with `--test_env=UPDATE_SEMANTIC_BOUNDARY_REQUEST_V1_GOLDEN=1 --test_env=BUILD_WORKSPACE_DIRECTORY=$PWD` |
| `usage_fact_payload.hex`, the Lashlang snapshot digest, the RLM golden root | the bytes the failure prints as `left` |
| `crates/lash-sim/replays/*/trace.json` | the frame ids the divergence prints as `actual` |
| `examples/agent-workbench` identity prefixes | the family version the Rust side renders |

A golden with a generator is never edited by hand: rerun `--apply`.

Fold the result into the FIG-4485 commit:

```sh
git add -A
git commit --fixup "$(git log --format=%H --grep='FIG-4485' -1)"
GIT_SEQUENCE_EDITOR=true git rebase -i --autosquash origin/main
```

Recapture the rehearsal corpus the same way step 6 captures the real one. The
capture needs an empty destination and its tag at `HEAD`, so it runs between
two fixups of the FIG-4495 commit, under a local tag that is never pushed:

```sh
rehearsal="$(git log --format=%H --grep='FIG-4495' -1)"
git rm -r -q fixtures/release-rehearsal/cut-1.0-dry-run
git commit --fixup "$rehearsal"
git tag -f cut-1.0-dry-run HEAD
kiln gate lash <fork> -- just release-fixtures-capture cut-1.0-dry-run \
  fixtures/release-rehearsal/cut-1.0-dry-run
kiln gate lash <fork> -- just release-fixtures-read-back \
  fixtures/release-rehearsal/cut-1.0-dry-run
git add -A fixtures/release-rehearsal/cut-1.0-dry-run
git commit --fixup "$rehearsal"
git tag -d cut-1.0-dry-run
GIT_SEQUENCE_EDITOR=true git rebase -i --autosquash origin/main
```

The rehearsal manifest's `source_commit` then names a commit the squash
replaced. That is acceptable for a rehearsal and for nothing else.

The capture regenerates every generator-owned fixture through Cargo and
refuses to continue if that changes a fixture the tagged commit carries. A
refusal on the replay journals means the capturing build's generation differs
from the workspace build's that wrote them; the corpus would then never be
compared in CI, so fix the feature selection in `REPLAY_CORPUS_REGENERATE`
(`scripts/capture_release_fixtures.py`) rather than committing the journals.

## 3. Verify

```sh
kiln build
python3 scripts/release_baseline.py check
kiln gate lash <fork> -- python3 scripts/dev-test.py --dependents
kiln gate lash <fork> -- scripts/ci/store-tests.sh pg-store
kiln gate lash <fork> -- scripts/ci/store-tests.sh pg-store-synthetic-next
bash scripts/ci/version-bump-gate.sh HEAD "$(git log --format=%H --grep='FIG-4485' -1)"
bash scripts/ci/version-bump-gate.sh HEAD
kiln test //crates/lash-restate:lash-restate__unit_test \
  --local-test-execution --no-test-cache \
  --test_arg=tests::replay_corpus:: --test_arg=--nocapture \
  --test_env LASH_REPLAY_CORPUS_ROOT=fixtures/release-rehearsal/cut-1.0-dry-run/replay-corpus
kiln clippy
```

The affected-tests gate is compared against the reds main already carries.
The first version-bump run proves no guarded shape moved after the reset. The
second runs the gate as the release does: with no `v1` tag yet it requires the
tree to be exactly the baseline.

One red is by design and is not a failure of the cut:

```sh
bash scripts/ci/version-bump-gate.sh HEAD origin/main
```

Against pre-cut main every constant "moved backwards". The same red appears
once, on the cut's own push to main, in the standalone `Version bumps` and
required `version-bumps` checks whose base is pre-cut main. Land the cut
through the path the orchestrator uses for a known-red required check; every
change after it is compared against a post-cut base.

## 4. Land

Set the release channel and land the branch:

```sh
sed -i 's/^channel = "0.1.0-alpha"$/channel = "1.0.0"/' Cargo.toml
python3 scripts/release_version.py print-next   # prints 1.0.0
git commit -m 'The release channel is 1.0.0 (FIG-3846)' Cargo.toml
```

Push the branch and land it on main as six commits, not squashed.

## 5. Tag

The tag is the release workflow's, never a hand-made one.

1. Dispatch `ci.yml` on main's head (`gh workflow run ci.yml --ref main`) and
   wait for a green full-profile run.
2. Dispatch `release.yml` (`gh workflow run release.yml --ref main`). Its
   `version-bumps` job runs the gate with no base, finds no `v1` tag, checks
   the baseline, and the workflow tags `v1.0.0` at the certified commit.

## 6. Capture

At the tagged commit, in a fork:

```sh
git fetch origin --tags && git switch --detach v1.0.0
kiln gate lash <fork> -- just release-fixtures-capture v1.0.0 fixtures/release/v1.0.0
python3 scripts/verify_release_fixtures.py fixtures/release/v1.0.0
kiln gate lash <fork> -- just release-fixtures-read-back fixtures/release/v1.0.0
```

Commit the corpus on a branch from main and point CI at it in the same change:

- `LASH_REPLAY_CORPUS_ROOT` in `.github/workflows/ci.yml` and
  `.github/workflows/release-journal-replay.yml` becomes
  `fixtures/release/v1.0.0/replay-corpus`;
- `RELEASE_JOURNAL_CORPUS_DIRS` in `scripts/ci_plan.py` loses its
  `fixtures/release-rehearsal/` entry;
- `git rm -r fixtures/release-rehearsal`;
- `python3 scripts/test_ci_plan.py` holds the three in step.

From that change on, the corpus is frozen: nothing regenerates it, and the
version-bump gate's baseline is the `v1.0.0` tag.

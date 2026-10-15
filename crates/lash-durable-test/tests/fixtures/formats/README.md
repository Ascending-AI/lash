# Retained parked-run upgrade witnesses

`lashvm/` and `session/` are the original pre-cut 1.0 SQLite images. Their
original recording build is `95bc17cfa86757844c2aea97bfbc8243206f39a1`, kernel semantics
1, helper release 1.0. Re-recordings authorized before the cut are
listed below; otherwise keep their database and WAL bytes unchanged.

`corpus.json` is an archive of realistic images, grouped by the
full recording build commit. Each set names its kernel semantics version and
its exact helper release identity catalogue (including both dialects). Each
image names its shape, dialect, source and recorded result. Its database and
WAL are separately gzipped, exactly as in the existing `support/images.rs`
recorder, then base64 encoded to keep bytes and oracle in one compile-time
input. SQLite's committed checkpoint graph holds the pinned helper manifest
and native fingerprints; the catalogue records all held function identities.

The first set was recorded on main
`4b1b3a0d684e448108de3ac8f13d0a153ffe0ef1`. It includes:

- A process mid-loop, retaining a live closure over a mutable counter and two
  aliases of a mutable list; it resumes to `[6, 3, 6]`.
- A process waiting in a task join, whose child sleeps; it returns
  `"joined child"`.
- A process suspended inside `finally`, carrying a pending return and its
  mutable result; cleanup changes the result to `["cleanup completed"]`.
- TypeScript and Python session cells parked on a pending `Once` tool call,
  with its committed outcome awaiting consumption.
- A TypeScript cell retaining a declared-read answer across its pending write.
  The reader ran once before the save and never runs during resume.

The session oracles retain the complete committed history, including tool
answers, cell output and the model's final answer. Resume uses a fresh node
and deployment on the production durable engine over a SQLite file. It must
load saved state, without entering the cell or process from scratch or repeating
an operation. All current witnesses require successful completion; an
intentional retirement needs an explicit typed refusal oracle and its documented
reason, such as `StateNotCarried`. A missing-node, corrupt-artifact or decode
failure is never an accepted upgrade outcome. Do not change an old expected
result to accept a regression.

## Recording policy

Before the 1.0 cut, a lane that changes the unsealed helper release re-records,
in place, exactly the images pinned to it that the change breaks, keeping their
programs and expected results. Witnesses become append-only once their release
is sealed at the cut.

A lane changing the kernel semantics version, a sealed helper release or a native
version must append a new image set **beside all previous sets**. Never overwrite
or delete an image pinned to a sealed release: they are the upgrade witnesses.
This extends the existing `lash.worker` header rule: describing the recording
build in a header alone does not prove that retained parked state resumes.
Keep the exact helper identities as well as the build name and the images.
The pre-1.0 version freeze still applies; do not bump a guarded version or seal
a helper release to record these witnesses.

Record only on a tree rebased onto current `origin/main`. Re-record a baseline
image when your own change intentionally alters a stored or parked shape, or
breaks its unsealed helper release under the rule above. Other semantic/helper/native
changes append witnesses instead. FIG-6183's explicit
baseline repair is an exception: FIG-6153 (`4b1b3a0d68`) changed the parked
shape and recorded these two images before FIG-6180 (`82af350c06`) repaired
the fingerprint probe, then rebased. The orchestrator authorized re-recording
exactly `lashvm` and `session` on the fixed build; their old fingerprints were
a bad recording of the intended baseline.

FIG-6253 changed the SQLite baseline schema in place: `turn_phases` gained
`answer_ref`, the model-call receipt's root, so no earlier image could read
its turn rows. The orchestrator authorized re-recording `lashvm`, `session`
and all six images of set `4b1b3a0d68` in place on one build rebased onto
`origin/main`. The set keeps its id, programs and expected results; every
re-recorded image resumed to the result it recorded before.

FIG-6357 replaced each executed call's bare outcome with a tagged verdict
carrying the settle failure's code and bounded message. The orchestrator
authorized re-recording `lashvm`, `session` and set `4b1b3a0d68`'s
`pending-tool-python` image in place on one build rebased onto `origin/main`.
The three kernel images and the `pending-tool-typescript` and
`saved-declared-read` images remain byte-identical. The set keeps its id and
programs; only the call outcome members of its session oracles change,
derived from their own committed records.

The fresh FIG-6357 recording (recorder build tag
`05d23c0b22fa93f086c4d63227c63ef9797abed5`, including this change's working
tree) produced `{"ok":true,"wrote":{"x":7}}` for the two TypeScript result
summaries; the retained set recorded on
`4b1b3a0d684e448108de3ac8f13d0a153ffe0ef1` expects
`{"ok":true,"wrote":{"x":7.0}}`. The orchestrator ruled to preserve those
original byte pairs and summary strings, subject to the retained-image resume
law. This numeric encoding observation is routed separately; no oracle
accepts that drift. Every retained image resumes to its previously recorded
result, with only the new call verdict spelling.

FIG-6293 added `result_summary`, a presentation projection of a call's
committed ledger record, to each executed call that a cell's committed record
lists. No image bytes changed. A resumed cell commits its record on the
resuming build, so the three session images of set `4b1b3a0d68` that finish
their cell (`pending-tool-typescript`, `pending-tool-python`,
`saved-declared-read`) now carry that member. The orchestrator authorized
adding exactly that member to those three `expected` oracles, with the value
computed from each image's own committed ledger record, and changing nothing
else. A recorded cell without the member renders `result_summary: null`.

FIG-6368 changed `ts.math.random` in the unsealed TypeScript helper release
to return the kernel's unit-interval draw directly. The orchestrator
authorized re-recording `lashvm` and `session` in place with their existing
recorders on a build rebased onto `origin/main`: both images pinned nodes
replaced by the changed unsealed release. The same authorization covers set
`4b1b3a0d684e448108de3ac8f13d0a153ffe0ef1`'s `loop-closures-aliases`, `task-join`,
`suspending-cleanup`, `pending-tool-typescript` and `saved-declared-read` images,
which failed for that exact missing-node reason. `pending-tool-python` passes
and remains byte-identical. Every re-recorded image resumes to its recorded
result, with one authorized oracle update: `calls[0].result_summary.text` in
the two TypeScript images is derived from each fresh committed record as
`{"ok":true,"wrote":{"x":7}}`. FIG-6290 intentionally makes whole floats reach
tools as JSON integers, as confirmed by FIG-6366; the earlier images committed
`7.0`. Every other oracle member, each set id and program, and every version
remain unchanged. No release was sealed.

FIG-6372 deletes 132 obsolete `ts.method.*` dispatchers, the unused
`ts.call_computed` dispatcher and `ts.url.commit` from the unsealed TypeScript
release. Under the pre-cut standing rule, the orchestrator authorized refreshing
only the broken `lashvm` and `session` baselines and the existing corpus
images `loop-closures-aliases`, `task-join`, `suspending-cleanup`,
`pending-tool-typescript` and `saved-declared-read` in place, with the existing recorders on this tree rebased onto `origin/main`.
Programs, version constants, set metadata and every expected result stay unchanged.
The passing `pending-tool-python` image remains byte-identical.
The helper-result Text inference fix affects newly lowered source only; it
intends no change to an existing parked program's result.

Source `env.sh`, then run the existing recorder locally on the build being
retained, naming its full commit (use a new build, never an existing set):

```sh
kiln test //crates/lash-durable-test:vertical_crash_proof__test \
  --local-test-execution --no-test-cache \
  --test_arg=--ignored --test_arg=--exact \
  --test_arg=corpus::record_current_parked_image_set \
  --test_env=LASH_REGENERATE=1 \
  --test_env=BUILD_WORKSPACE_DIRECTORY="$PWD" \
  --test_env=LASH_IMAGE_BUILD="$(git rev-parse HEAD)"
```

The recorder verifies every fresh image on a cold deployment before appending
the complete set. It refuses a duplicate build instead of replacing it. Commit
the appended archive with the semantic/helper/native change and run:

```sh
kiln test //crates/lash-durable-test:vertical_crash_proof__test \
  --test_arg=--exact \
  --test_arg=corpus::every_retained_parked_image_resumes_to_its_recorded_result
```

This law reads every archive entry and both original 1.0 images. The owning
`vertical_crash_proof__test` target is already in `//:dev_tests` and
`//:main_tests`, so the scheduled main run checks the entire retained corpus.
FIG-6183 proved the law fails with the native fingerprint regression from
before `82af350c06` restored: the pinned release node is missing.

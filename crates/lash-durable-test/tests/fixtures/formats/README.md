# Retained parked-run upgrade witnesses

`lashvm/` and `session/` are the original 1.0 SQLite images. Their last
recording build is `95bc17cfa86757844c2aea97bfbc8243206f39a1`, kernel semantics
1, helper release 1.0. Keep their database and WAL bytes unchanged.

`corpus.json` is an append-only archive of realistic images, grouped by the
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

A lane changing the kernel semantics version, a helper release or a native
version must append a new image set **beside all previous sets**. Never overwrite
or delete an old set or original 1.0 image: they are the upgrade witnesses.
This extends the existing `lash.worker` header rule: describing the recording
build in a header alone does not prove that retained parked state resumes.
Keep the exact helper identities as well as the build name and the images.
The pre-1.0 version freeze still applies; do not bump a guarded version or seal
a helper release to record these witnesses.

Record only on a tree rebased onto current `origin/main`. Re-record a baseline
image only when your own change intentionally alters a stored or parked shape;
semantic/helper/native changes append witnesses instead. FIG-6183's explicit
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

FIG-6193 replaced the whole-state parked run (a KernelWhole graph root,
or a kernel process's state carrying its sealed graph inline) with the
persistent heap's CheckpointRoot pages, in place under the 1.0 freeze. It is
the sanctioned parked-shape change that FIG-6183's (`b6cf0eeb40`) retained
images name. The orchestrator authorized re-recording `lashvm`, `session` and
all six images of set `4b1b3a0d68` in place, on one build of the integrated
heap tree. The set keeps its id, programs, helper catalogue and expected
results; each fresh image recorded exactly its old result before it replaced
the old bytes.

`pre-heap-lashvm/` and `pre-heap-session/` keep the last whole-state images
of `lashvm` and `session` byte for byte. They are refusal witnesses, not
upgrade witnesses: no build reads them.
`format_fixtures::pre_heap_images_are_refused_naming_their_format` holds that
a pre-heap cell's graph read is refused with `GraphRefusal::RetiredFormat`
naming `KernelWhole`, and that the kernel engine refuses a pre-heap process's
state by naming the same format. Never re-record them.

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

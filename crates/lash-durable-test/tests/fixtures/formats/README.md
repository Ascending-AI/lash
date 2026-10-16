# Retained parked-run upgrade witnesses

`lashvm/` and `session/` are the original pre-cut 1.0 SQLite images. Their
original recording build is `95bc17cfa86757844c2aea97bfbc8243206f39a1`, kernel semantics
1, helper release 1.0. Keep their database and WAL bytes unchanged except
for an authorized pre-cut refresh under the recording policy below.

`corpus/<full-build-commit>/<image-name>.json` holds exactly one retained
image, with its build's kernel semantics and exact helper release identity
catalogue (including both dialects). Its single corpus entry names the shape,
dialect, source and recorded result. Its own database and WAL are separately
gzipped, exactly as in `support/images.rs`, then base64 encoded alongside the
oracle. No database is shared between corpus images. SQLite's committed
checkpoint graph holds the pinned helper manifest and native fingerprints;
the catalogue records all held function identities.

The decode-and-resume laws discover every file in `corpus/` and group entries
by build, checking that their metadata agrees. The files are the index: do not
maintain an image list or add README rows when recording or refreshing images.

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

Before the 1.0 cut, refresh the current unsealed set in place when a stored
shape or helper release change requires new image bytes. Keep its set id,
programs, helper catalogue, version constants and every oracle member.
Record only on a tree rebased onto current `origin/main`. Re-recording adds
no README text; put provenance in the commit body.

Witnesses become append-only once their release is sealed at the cut.
A lane changing the kernel semantics version, a sealed helper release or a native
version must append a new image set **beside all previous sets**. Never overwrite
or delete an image pinned to a sealed release: they are the upgrade witnesses.
Keep the exact helper identities as well as the build name and the images.
The pre-1.0 version freeze still applies; do not bump a guarded version or seal
a helper release to record these witnesses.

`pre-heap-lashvm/` and `pre-heap-session/` keep the last whole-state images
of `lashvm` and `session` byte for byte. They are refusal witnesses, not
upgrade witnesses: no build reads them.
`format_fixtures::pre_heap_images_are_refused_naming_their_format` holds that
a pre-heap cell's graph read is refused with `GraphRefusal::RetiredFormat`
naming `Annotations` (kind 11, retired by FIG-6376), and that the kernel
engine refuses a pre-heap process's state by naming `KernelWhole` (kind 16).
A retired kind is refused as soon as its envelope is read, before its body;
the cell reaches its retired annotation layer before its whole-state run.
Never re-record these refusal witnesses.

## Refreshing the current set

Source `env.sh`, then run the discoverable generator on the current build:

```sh
kiln test //crates/lash-durable-test:vertical_crash_proof__test \
  --local-test-execution --no-test-cache \
  --test_arg=--ignored --test_arg=--exact \
  --test_arg=corpus::regenerate_current_parked_image_set \
  --test_env=LASH_REGENERATE=1 \
  --test_env=BUILD_WORKSPACE_DIRECTORY="$PWD"
```

The generator records every current image from its archived source and kind,
then cold-resumes each fresh image. It refuses before writing any image if a
result differs from its old oracle. Only database and WAL payloads are
replaced; metadata and oracles are preserved. Its `#[ignore = "regenerates
<path>"]` attribute lets `scripts/fixture_regenerators.py` discover the output
for automatic conflict resolution. The guard law
`corpus::generator_output_names_the_current_set` keeps that literal path in
agreement with the current set constant.

The baseline recorders write only their own `lashvm/` or `session/` database
and WAL under the same pre-cut policy. Never re-record refusal witnesses.

## Appending at a release cut

Run the append recorder locally on the build being retained, naming its full
commit (use a new build, never an existing set):

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
its own file. A new set refuses an existing build instead of replacing it.
When a new unsealed set becomes current, update the generator's output
attribute and current set constant together.

After recording, run:

```sh
kiln test //crates/lash-durable-test:vertical_crash_proof__test \
  --test_arg=--exact \
  --test_arg=corpus::every_retained_parked_image_resumes_to_its_recorded_result
```

This law reads every archive entry and both original 1.0 images. The owning
`vertical_crash_proof__test` target is already in `//:dev_tests` and
`//:main_tests`, so the scheduled main run checks the entire retained corpus.

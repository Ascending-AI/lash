# Fuzz targets

`Cargo.toml` owns target membership. `scripts/check_fuzz_corpus.py` requires a
nonempty committed corpus for every target; the existing `fuzz-smoke` workflow
reads that list and runs each target with the nightly toolchain and a time,
timeout and RSS budget. New targets need no workflow list to maintain.

`checkpoint_tokens` sends at most 4096 bytes directly to the `Datum`,
`CheckpointRoot` and opaque `Checkpoint` token decoders. Successful decoding must
re-encode to exactly the input bytes. Recursion is narrowed to 64 for this
campaign; the existing primitive codec laws own the standard depth ceiling,
signed zero/NaN and duplicate-key rules.

`checkpoint_envelopes` accepts at most 1024 mutation bytes. Its first five bytes
are `[seed, operation, offset_low, offset_high, mutation_byte]`; remaining bytes
are insertion/replacement material. Seeds select parked or ended roots. Operations preserve the valid control, truncate a physical body,
insert tokens, XOR a byte, replace the body, remove/add a declared child, mutate
framing, or remove a required adapter grant. The root seed populates every tree
and the ended result so each grant site can be selected by offset.

Bodies are framed through the production encoder. After mutation the target
recomputes the production envelope digest and authenticates with
`check_envelope` before expanding physical slots and decoding inner records.
A deliberately wrong digest must independently yield `HashMismatch`.
Declared-child mismatches must yield a typed canonical refusal; truncated
records must fail decoding. Unchanged controls must succeed. Removing a required
child grant from the authenticated adapter answer must yield `HeapFault::Corrupt`
from both checkpoint readers. Production authentication is unchanged.

For a bounded local run in a Kiln fork, source `env.sh` first and use the existing
Cargo-owned fuzz recipe through `kiln gate`, for example:

```sh
kiln gate lash <fork> -- bash -c '
  ulimit -v 8388608
  /usr/bin/time -v cargo +nightly fuzz run --fuzz-dir fuzz --sanitizer none \
    checkpoint_tokens -- -max_total_time=30 -runs=100000 -timeout=5 \
    -rss_limit_mb=2048 -malloc_limit_mb=64 -max_len=4096
'
```

Use the same recipe for `checkpoint_envelopes`. Sanitizer coverage remains on;
local AddressSanitizer is disabled because its virtual address reservation
exceeds the hard memory cap. The existing CI run retains its sanitizer setup.
Keep discovered corpora and crash artifacts under the fork's ticket evidence
directory when investigating; retain a minimized regression only for a named
rule or a found bug.

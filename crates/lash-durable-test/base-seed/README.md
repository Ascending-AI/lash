# Base-state seed (FIG-7694)

`base_state_seed.rs` is the test that wrote the frozen base store the
cutover's deployment receipt opens. It is written against the base build's
API (main at `fc9f8f4f3c`), not this branch's. It lives outside `tests/`, so
no build target compiles it here. It is kept byte for byte as it ran (sha256
`b83ee79aa56357f55b6ecc69137851017ec6fbe7abdb0b8e5bf41a26e9e0dcd2`). Its
header still says "never committed", which was true when it ran.

The receipt names the base artifact as `fc9f8f4f3c` plus this file at its
committed hash.

To reproduce the store on a fork at `fc9f8f4f3c`:

1. Copy the file to `crates/lash-durable-test/tests/base_state_seed.rs`.
2. Run `kiln sync`. It adds the `base_state_seed__test` target.
3. Run the target with `LASH_BASE_SEED=<dir>`.

The store it writes holds a parked actor, a session root with bindings, a
saved function and a started `Once` tool row with no outcome. The `Once`
body ends the OS process, so its row stays started.

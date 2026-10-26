# Guest call depth

FIG-7430, measured on 2026-10-26 using the confined production worker loop
through the `pool_laws` worker fixture. The worker used its inherited standard
8 MiB native stack, with charge and memory metering on. No charge prices changed.

`Bounds::DEFAULT_CALL_DEPTH` is 10,000. `PoolConfig::standard` uses it, RLM
copies its pool's depth into cells and processes, and runtime hosts record it
through `KernelRecordedSettings`. Standalone `lash-kernel-engine::Limits` also
uses this default. The worker admits each run's requested depth
against its pool ceiling. The register executor's 200-frame reservation is a
batch: its explicit frames materialize on exit and the general machine carries
on. Both executors keep frames in heap-backed collections and dispatch through
loops, so a guest call adds no recursive Rust invocation.

The worker law lowered and ran TypeScript and Python `descend(n)`, returning
`1 + descend(n - 1)` until zero. The oracle is `n`. Values below are the largest
memory meter reported at 1,000-unit slices, in bytes. `VmStk` is Linux's retained
main-stack mapping high-water mark, read from `/proc/<pid>/status` after each
run; `/proc/<pid>/limits` reported 8,388,608 bytes for the native stack limit.

| Dialect | Source recursion n | Configured depth | Reported heap bytes | Native stack KiB | Result |
| --- | ---: | ---: | ---: | ---: | --- |
| TypeScript | 8 | 10,016 | 6,768 | 396 | 8 |
| TypeScript | 5,000 | 10,000 | 564,608 | 396 | 5,000 |
| TypeScript | 10,000 | 10,000 | 1,126,896 | 396 | typed CallDepth |
| TypeScript | 10,000 | 10,016 | 1,125,728 | 396 | 10,000 |
| TypeScript | 10,016 | 10,016 | 1,128,688 | 396 | typed CallDepth |
| TypeScript | 64 | 32 | 10,480 | 396 | typed CallDepth |
| Python | 8 | 10,016 | 6,144 | 396 | 8 |
| Python | 5,000 | 10,000 | 564,576 | 396 | 5,000 |
| Python | 10,000 | 10,000 | 1,125,152 | 396 | typed CallDepth |
| Python | 10,000 | 10,016 | 1,125,024 | 396 | 10,000 |
| Python | 10,016 | 10,016 | 1,126,944 | 396 | typed CallDepth |
| Python | 64 | 32 | 8,736 | 396 | typed CallDepth |

The measured stack growth between shallow and deep recursion was 0 KiB in
both dialects, at Linux mapping granularity. The 396 KiB high-water mark leaves
over 20 times the native stack headroom, exceeding the required factor of four.
Heap use increases with live guest frames and remains subject to the memory
bound. There is no wall-time or instruction-performance claim here.

The source recursion argument is not a frame count: `n = 10,000` enters the
base-case function too, and helper calls also take depth units. Its success
case therefore explicitly raises the pool ceiling and run bound to 10,016.
The same source under the standard 10,000 bound ends with typed `CallDepth`,
as it should. Every refusal preserves its configured limit and a site mapped
to the guest `descend` function, and the worker resets and runs the next case.

TypeScript hoisting now lowers closures under their declaration span. Python's
private direct-recursion body now retains its declaration origin. The kernel
attaches the nearest authored statement to depth refusals, and the wire and
recorded end retain that site. RLM uses a source map whose source digest matches
the cell to report the function's declaration and the failing line and column.

## Reproduction

All commands run from a sourced Kiln fork. The source regression tests failed
on unfixed main `2ec8e492978f`: the kernel and Python reported `CallDepth` at
200, and TypeScript reported the same bound with `ts.strict_equals` as its
function. The fixed bounds law also checks call 33 against a configured depth
of 32 and resolves the refusal site to the authored `descend` definition.

```sh
kiln test //crates/lash-kernel-vm:lash-kernel-vm__unit_test --test_arg=laws::bounds::
kiln test //crates/lash-kernel-vm:lash-kernel-vm__unit_test --test_arg=laws::limiters::
kiln test //crates/lash-dialect-typescript:lash-dialect-typescript__unit_test //crates/lash-dialect-python:lash-dialect-python__unit_test --test_arg=--exact --test_arg=tests::machine::standard_depth_admits_five_thousand_guest_calls
kiln test //crates/lash-vm-worker:pool_laws__test --test_arg=--exact --test_arg=guest_recursion_uses_the_configured_depth_and_metered_heap --test_arg=--nocapture
kiln test //crates/lash-protocol-rlm:lash-protocol-rlm__unit_test --test_arg=feedback::tests::
kiln test //crates/lash-vm-broker:lash-vm-broker__unit_test --test_arg=--exact --test_arg=kernel::state::tests::a_recorded_end_keeps_the_thrown_value_or_guest_bound_coordinate
kiln build //:schema_checks
```

`scripts/land` runs the required workspace `kiln clippy` before pushing.

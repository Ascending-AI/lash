# Exact generated-source costs

Measured 2026-10-14 through the public facade example
[`generated_source.rs`](../../crates/lash/examples/generated_source.rs).
The example independently prints, compiles with the selected dialect's public
generated compiler, and compares complete canonical document bytes. The same
composition is available through `WorkerService`; printing alone never compiles.

Repeat from a Kiln fork with one command:

```sh
. ./env.sh && (ulimit -v 8388608; /usr/bin/time -v kiln run //crates/lash:generated_source__example -- 20)
```

The default Kiln development build was used (these first-party crates at
opt-level 0, Cargo-derived development optimization for dependencies), on Linux
x86_64, AMD Ryzen 9 5950X, 16 physical / 32 logical CPUs. The shared host was
not pinned to a core. The example uses a current-thread host runtime and a
prewarmed, reused worker. It performs no admission or execution.
The entire invocation ran under an 8 GiB virtual-memory cap. `/usr/bin/time -v`
reported peak RSS **67,588 KiB**, no swaps and exit status 0; that process-tree
measurement includes the Kiln launcher, rather than isolating codec allocations.

Each row is the arithmetic mean of 20 interleaved samples. Print-only timing
ends before compilation and comparison; those samples are then verified outside
the timed region. Composed timing starts before printing and ends after both
canonical encodings and the byte comparison. Every successful print in the
measurement is checked. Worker timings include transport, JSON document
encoding/decoding and MessagePack source/map transport. Pool bootstrap and
local `WorkflowDocument::derive` are outside both timings. Local map JSON-size
measurement is outside the timing; worker map serialization remains inside it.
There is no wall-clock CI threshold or latency guarantee.

## Documents and bytes

Fixtures are deterministic host-built Documents: one list binding (`small`),
40 bindings inside two nested controls (`typical`), 500 bindings (`wide`), and
one binding inside 20 nested controls (`deep`). Each ends with `finish null`.
Node counts use an independent `Node::children` walk. Document bytes are the
complete canonical encoding. Source sizes count final UTF-8 bytes; maps do not
change them. Map bytes below count JSON encoding for a reproducible size
comparison; the worker wire uses MessagePack.

| Case | Nodes | Document bytes | TS source bytes | Python source bytes | TS map bytes | Python map bytes |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| small | 7 | 205 | 208 | 287 | 854 | 841 |
| typical | 171 | 4796 | 2829 | 6748 | 17225 | 17272 |
| wide | 2003 | 58372 | 33209 | 67220 | 191071 | 191063 |
| deep | 87 | 1445 | 1368 | 3847 | 10868 | 11398 |

## Timings

All values are microseconds per operation. “Composed” means print + generated
compile + both canonical encodings + byte comparison, with the same map setting.

| Case | Dialect | Maps | Local print | Local composed | Worker print | Worker composed |
| --- | --- | --- | ---: | ---: | ---: | ---: |
| small | typescript | off | 66.0 | 303.9 | 351.0 | 850.7 |
| small | typescript | on | 80.0 | 345.7 | 509.6 | 1324.4 |
| small | python | off | 76.2 | 414.7 | 326.6 | 1013.7 |
| small | python | on | 95.1 | 352.4 | 440.6 | 899.4 |
| typical | typescript | off | 561.8 | 2173.2 | 1553.1 | 4099.5 |
| typical | typescript | on | 792.4 | 2394.5 | 2973.4 | 5289.1 |
| typical | python | off | 720.5 | 2597.3 | 1657.6 | 4377.8 |
| typical | python | on | 1355.0 | 3357.2 | 3560.5 | 6639.4 |
| wide | typescript | off | 5174.4 | 20520.0 | 13034.4 | 35574.8 |
| wide | typescript | on | 8988.3 | 25738.4 | 29847.4 | 51718.0 |
| wide | python | off | 6473.7 | 22669.7 | 15294.1 | 38353.4 |
| wide | python | on | 9954.6 | 25716.3 | 30207.3 | 53314.0 |
| deep | typescript | off | 134.0 | 954.9 | 541.0 | 1710.9 |
| deep | typescript | on | 234.7 | 1036.5 | 1464.0 | 2712.0 |
| deep | python | off | 207.9 | 1390.4 | 678.0 | 2272.9 |
| deep | python | on | 339.1 | 1698.0 | 1806.1 | 3669.8 |

The wide fixture's requested maps add about 191 KiB of JSON data. Its worker
print cost rises from about 13–15 ms without maps to about 30 ms with them;
local print rises from about 5–6 ms to 9–10 ms. Host-composed verification costs
more than print alone, so a host can choose when it pays that independent cost.
These observations describe this development build and these inputs only.

Coverage excludes release builds, cold worker startup, concurrent requests,
real application-size distributions, maximal manifests or literals, all kernel
variants, admission, execution, resume and persistence. `typical` is a synthetic
fixture size, not a measured production median. Small timing inversions between
map settings reflect sampling noise on a shared host. The executable emits CSV
rows, accepts a positive sample count, and reproduces all four fixture shapes;
there is no cache or automatically stored source/map involved.

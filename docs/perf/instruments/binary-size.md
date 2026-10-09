# Binary size from an existing Kiln artifact

`scripts/binary_size.py` reports section sizes, the largest demangled symbols,
crate attribution and changes against a saved JSON receipt. It reads an existing
linked ELF; it never invokes a compiler or rebuilds dependencies.

Build an optimized binary with retained symbols and final materialization:

```sh
. ./env.sh
mkdir -p .kiln/binary-size
kiln build --target-platforms //tools/buck2:profiling \
  -c kiln.rust_profile=optimized //crates/lash-perf:lash-perf__bin \
  --materializations final --build-report .kiln/binary-size/build.json
python3 scripts/binary_size.py --build-report .kiln/binary-size/build.json \
  --label //crates/lash-perf:lash-perf__bin --top 20 \
  --out .kiln/binary-size/baseline.json
```

The profiling platform uses optimization level 3, line tables and retained
symbols. The ordinary optimized configuration can also be inspected, but
stripping or LTO can reduce attribution. Resolve reports through the existing
`tools/buck2/outputs.py` contract; exactly one materialized ELF must match the
label. A report can contain other non-ELF outputs.

For an explicit artifact, or a subsequent build:

```sh
python3 scripts/binary_size.py --elf path/to/binary \
  --out .kiln/binary-size/current.json \
  --baseline .kiln/binary-size/baseline.json --top 10
```

Both inputs print a report and write a receipt. `--out` defaults to
`binary-size.receipt.json` in the current directory. It must differ from every
input. `--baseline` prints separate top growth and shrinkage lists for symbols
and crates, including added and removed names, and total byte deltas. Save the
baseline receipt before rebuilding: build output paths can be reused. Receipts
contain every sized symbol, regardless of the display limit.

GNU `nm --defined-only --print-size --demangle --radix=x` supplies symbol extents
and Rust demangling. GNU `readelf -W -S` supplies section sizes, addresses and
allocation flags; `-W -h` and `-W -n` supply ELF identity and the optional GNU
build ID. These installed GNU tools provide section flags and identity directly,
so a second LLVM size framework is unnecessary. The script validates GNU tool
names and records their resolved executable paths and exact version strings.
The receipt also includes ELF SHA-256, file size, ELF header, and, for report
input, the report SHA-256, label, selected result and build metadata. It records
artifact identity, rather than guessing which Git revision produced an ELF.

## What the numbers mean

- `.text`, `.rodata`, `.data` and `.bss` appear explicitly. Debug sections are
  listed separately, followed by other sections. `.bss` and other `NOBITS`
  sections contribute allocated bytes without contributing section file bytes.
  File headers and padding reconcile the remaining file bytes.
- Symbol rankings and symbol deltas use declared extents. Aliases can overlap,
  so adding these sizes does **not** give binary size. Deltas aggregate equal
  demangled names after removing a trailing Rust `::h` plus 16 hexadecimal
  digits; full names remain in the receipt. Compiler naming changes can still
  appear as removals and additions.
- Crate attribution uses the leading Rust path identifier, including qualified
  implementing types such as `<lash_core::Runner as core::fmt::Debug>::fmt`.
  This is a demangled namespace estimate, not Cargo package/dependency identity
  or attribution of inlined instructions to their original crate.
- Each allocated section is partitioned into symbol extents and gaps. Only a
  single covering symbol with a recognized crate prefix owns a range. Multiple
  covering symbols go to `unattributed.shared`, even when their prefixes agree.
  Recognizable `.llvm.`, `.lto`, `.constprop.` and `.isra.` merged names go to
  `unattributed.lto_merged`. C names, unknown paths, missing/zero-size symbols,
  alignment gaps and linker-generated metadata go to `unattributed.unknown`.
  TLS sections remain unknown because `nm` reports TLS offsets rather than
  ordinary virtual addresses. Debug/nonallocated bytes have no crate owner.
- The `unattributed` row is always present. Crate bytes plus that row equal all
  allocated section bytes; its three reason counters equal the row. No absent
  symbols or aliases silently disappear from those totals. Unmarked LTO merging
  cannot be recovered from `nm`; it may look like ordinary symbol ownership.
  Inspect comparable symbolized configurations when interpreting a diff.

The report measures bytes, not runtime cost, optimization advice or a size gate.
No CI configuration or size reduction is performed.

The named ownership law has a tiny demangled `nm` fixture covering a Rust prefix,
a qualified type, unknown and merged names, aliases and an uncovered range:

```sh
kiln gate lash <fork> -- python3 -m unittest \
  scripts.test_binary_size.CrateAttribution.test_demangled_prefix_owns_only_unambiguous_allocated_bytes
```

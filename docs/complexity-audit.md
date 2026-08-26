# Complexity hotspot audit

Run the repeatable audit from the repository root:

```sh
python3 scripts/complexity_hotspots.py
python3 scripts/complexity_hotspots.py --top 10
python3 scripts/complexity_hotspots.py --json > complexity-hotspots.json
```

The default scope is `crates/`; positional arguments replace that scope. The
tool requires Mozilla `rust-code-analysis-cli` 0.0.25 and never installs it:

```sh
cargo install rust-code-analysis-cli --version 0.0.25 --locked
```

The Markdown output gives the total function count and strict CCN threshold
counts (`CCN > 10`, `> 15`, `> 20`, `> 30`), followed by the top-N rows. Each
row contains cyclomatic complexity (CCN), cognitive complexity, source lines,
the inclusive source span, and function name. Results are deterministic:
CCN descending, then path and starting line. `--json` emits the same
normalized raw rows for tooling.

The audit excludes `target/`, the known generated surfaces, and any Rust file
whose first five lines contain `@generated` or `DO NOT EDIT`. `lizard` was
evaluated and rejected: its Rust parser mis-attributed function spans, so its
numbers are not suitable for this audit.

## Reading the signal

Complexity is a candidate finder, not a refactor verdict. Flat exhaustive
matches over closed worlds—VM dispatch, error taxonomies, standard-library
dispatch, oracles, and conformance `apply_operation`—are house style. High CCN
with cognitive complexity around 1 is a leave-alone signal: the metric counts
the closed-world cases, while the low cognitive score says there is little
nesting or interleaved decision logic to simplify.

Accidental complexity is different: deep nesting, repeated branch clusters,
interleaved concerns, and boolean-parameter forks are refactor signals. Keep
the exhaustive domain match visible while extracting genuinely independent
policy, validation, orchestration, or normalization seams.

The workflow is: run the audit, cross-check candidates against open FIG
tickets, report first, and create a ticket only after a ruling. The baseline
audit on 2026-08-26 at `feb1286ca` produced [FIG-2195](https://github.com/Ascending-AI/lash/issues/2195),
[FIG-2196](https://github.com/Ascending-AI/lash/issues/2196),
[FIG-2197](https://github.com/Ascending-AI/lash/issues/2197),
[FIG-2198](https://github.com/Ascending-AI/lash/issues/2198),
[FIG-2199](https://github.com/Ascending-AI/lash/issues/2199), and
[FIG-2200](https://github.com/Ascending-AI/lash/issues/2200).

Top essential leave-alones from that baseline, which should not be re-flagged
merely because they remain high, are `step_instruction_fast`; the runtime
error enum conversions and exact-display table; `rlm_protocol_execution_fact`;
the JavaScript standard-library dispatches; conformance state-machine
`apply_operation`; `execute_intrinsic`; and the explicit object/path matrix in
`assign_path_reference`. Persistence commits and `stream_queued_work` were
already covered by open tickets and likewise need no duplicate ticket.

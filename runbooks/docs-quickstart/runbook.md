# E2E Scenario: Docs Quickstart — Compiled, Correctly Named, CLI-Free

> **Read [../RULES.md](../RULES.md) first** — especially the browser-surface,
> screenshot, objective-gate, Abort/RCA, and teardown rules. This runbook adds only the
> docs quickstart scenario.

**Purpose.** Prove that the rendered quickstart describes Lash's in-repo embedded Rust
surface, that its Rust example is copied from a compiling source, and that every displayed
Lash dependency is pinned to the latest released version. This scenario never invokes or
drives a CLI host.

**No real tokens.** The quickstart checker and compiled snippets use only repository
sources. Do not configure a provider or run the example program.

## Scenario-specific golden rules

1. **The source is the example.** The Rust block rendered in `docs/quickstart.html` must
   match `examples/docs-snippets/src/quickstart.rs` through its
   `quickstart#hello-lash` region. Hand-copying code into the page is a failure.
2. **Published name and Rust import are different on purpose.** The page must show the
   package `lash-runtime`; the compiled source must import `lash`; and
   `crates/lash/Cargo.toml` must declare `[package].name = "lash-runtime"` with
   `[lib].name = "lash"`.
3. **Keep the scope embedded and minimal.** The rendered page must frame the program as a
   minimal embedded agent using in-memory facets, identify persistence as an explicit
   next step, and contain no CLI-host install or invocation workflow.
4. **The release pin is mechanical.** All exact Lash pins in the README and docs entry
   pages must equal `docs/released-version.txt`; when newer local release tags exist,
   docs lint must reject the fallback as stale.
5. **Repository gates are authoritative.** `cargo check -p docs-snippets --locked` and
   `python3 scripts/lint_docs.py` must both exit zero. Browser rendering cannot excuse a
   checker failure.

## Working material

- Checker commands, from the repository root:
  `cargo check -p docs-snippets --locked` and `python3 scripts/lint_docs.py`.
- Browser surface: serve the checked-in `docs/` directory on an unused loopback port,
  open `/quickstart.html`, and stop the server during teardown. The server only exposes
  static in-repo files; it is not a Lash or CLI process.
- Source truth: `examples/docs-snippets/src/quickstart.rs`,
  `examples/docs-snippets/Cargo.toml`, `crates/lash/Cargo.toml`,
  `docs/released-version.txt`, and local `v*` git tags.
- Save command output and source extracts in the run artifact directory. Do not edit or
  regenerate sources during a judged run.

## Phase 0 — Run the repository gates

Record `git status --short` and the latest local release tag. Run both checker commands
without `--fix-snippets`; save their complete output as `00-docs-gates.txt`. Require both
to exit zero. A failure is an objective gate failure → Abort/RCA.

Extract the quickstart snippet region, the facade package/library names, and the checked-in
released version to `00-source-contract.txt`. Require the latest visible tag to equal the
checked-in value when that tag is available. In a shallow checkout where it is absent,
record that the documented offline fallback was used.

## Phase 1 — Render the crate-name and scope contract

Serve `docs/` on loopback, open `/quickstart.html`, and poll until the **What This Builds**
and **Before You Start** sections render. Gate all of the following text or elements:

- `lash-runtime` is the app-facing facade and the Rust import is `use lash::...`;
- the result is a minimal embedded-agent program using in-memory runtime facets;
- persistence is linked as the step for surviving process restart;
- streaming, RLM, tools, background work, and traces are framed as later additions.

Search the rendered document text for each CLI-host install and invocation marker
(`install_lash`, `cargo install`, `curl -fsSL`, `~/.lash`, and a `$ lash ` shell prompt) and
require zero matches for every one of them. These are the same markers `lint_docs.py`
enforces, so a disagreement between page and checker is itself a failure. Capture
`01-embedded-scope.png` with the crate-name contract and scope framing visible.

## Phase 2 — Render and reconcile the install and Rust examples

Gate that **Add The Crates** renders every Lash dependency at the value from
`docs/released-version.txt`. Save the displayed dependency block as
`02-rendered-dependencies.txt`.

Gate that **Minimal Example** identifies `quickstart#hello-lash`, imports from `lash`,
builds a `LashCore`, opens a session, and runs a turn. Compare its decoded code text with
the extracted source region byte-for-byte; this should be the same invariant already
passed by `lint_docs.py`, now reconciled with what the browser renders. Capture
`02-compiled-example.png`.

Any page/source/checker disagreement is a contract violation → Abort/RCA.

## Phase 3 — Teardown and score

Stop the static docs server and confirm the loopback port is closed.

| Item | Objective gate | Verdict | Evidence |
|------|----------------|---------|----------|
| Repository checks | snippets compile; docs lint exits zero | | `00-docs-gates.txt` |
| Crate identity | rendered `lash-runtime` / `use lash` agrees with facade manifest | | `00-source-contract.txt`, `01-embedded-scope.png` |
| Embedded scope | minimal in-memory host, persistence next, no CLI-host install or invocation markers | | `01-embedded-scope.png` |
| Release pin | rendered pins, fallback file, and latest available tag agree | | `00-source-contract.txt`, `02-rendered-dependencies.txt` |
| Snippet fidelity | rendered code equals compiled source region byte-for-byte | | `02-compiled-example.png`, checker output |
| CLI-free execution | no CLI host command or process used | | command log |

**Aggregate:** did the browser page, compiled in-repo snippet, crate manifests, and
release-pin checker all describe the same embedded Lash quickstart without relying on a
CLI host?

---

_Stop triggers and the Abort/RCA + reporting protocol are in [../RULES.md](../RULES.md)._

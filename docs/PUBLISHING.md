# Publishing to crates.io

The workspace publishes as one lockstep release: every publishable crate shares
the workspace version (the `0.0.0-dev` placeholder in-tree; the real release
version once stamped at packaging time) and pins its internal dependencies to
that exact version (centralized in the root `[workspace.dependencies]`). A
release publishes them all together, in dependency order.

## What gets published

- **Published:** every workspace member without `publish = false`. The public
  entry point is `lash-runtime` (imported as `lash`); embedders also pull in
  provider crates (`lash-provider-*`), stores (`lash-sqlite-store`,
  `lash-postgres-store`, `lash-s3-store`,
  `lash-restate`), the remote protocol DTOs (`lash-remote-protocol`), and
  a-la-carte capability crates (`lash-tools`, `lash-plugin-mcp`,
  `lash-subagents`, `lash-plugin-plan-mode`,
  `lash-plugin-tool-output-budget`, `lash-llm-tools`).
- **Not published:** anything marked `publish = false` — examples, E2E
  harnesses, and dev/internal tooling (`lash-perf`, `lash-trace-viewer`). The
  CLI product and its private crates live in
  [`lash-cli`](https://github.com/SamGalanakis/lash-cli). Harness evolution lives in the separate
  [`lash-evolve`](https://github.com/SamGalanakis/lash-evolve) repository.

Because of the exact `=` version pins, a published crate's internal deps must
already be on crates.io at the same version — so it is **all-or-nothing**:
`scripts/publish_workspace.py` publishes the whole set one crate at a time in
dependency order and waits for crates.io visibility between crates.

## How a release runs

There is no version-bump commit and no second CI pass. `main` always carries the
`0.0.0-dev` placeholder in every workspace manifest; the release version is
computed at cut time and stamped into an ephemeral tag checkout at packaging
time (`scripts/release_version.py stamp`, `scripts/publish_workspace.py
--version`), so `main` never records a released version.

1. Pull requests and pushes to `main` run CI (`.github/workflows/ci.yml`). A
   green merge updates the releasable trunk but never tags or publishes it.
2. When a release is wanted, a maintainer manually dispatches the GitHub
   `Release` workflow. `release_sha` may name any commit on `main`; leaving it
   blank selects the current `main` head.
3. The workflow proves that the selected commit is on `main` and has a
   successful main-push CI run, then requires curated notes for the unreleased
   range. `scripts/release_version.py print-next` computes the next version from
   `[workspace.metadata.release].channel` and the existing `v*` tags. The
   workflow tags that exact commit and is idempotent when retried.
4. `release.yml` validates `cargo metadata --locked`, then:
   - `publish-crates` runs
     `python3 .release-tools/scripts/publish_workspace.py --version <version>`,
     which stamps the manifests + lockfile and publishes every crate in
     topological dependency layers (crates in a layer publish concurrently, one
     crates.io visibility wait per layer). Already-published versions are
     skipped, so a failed run can be re-run to resume.
   - the `publish` job builds the SDK GitHub release with the curated notes.

The independent `lash-cli` Release workflow owns binary artifacts, checksums,
the installer, and `lash --version` stamping.

The main CI workflow also runs:

```bash
python3 scripts/test_release_version.py
python3 scripts/test_publish_workspace.py
python3 scripts/test_release_notes.py
```

Those tests pin the lockstep/private-crate version behavior, the publisher's
transient retry classification, and the release-notes extraction rules.

## Docs release pin

Install snippets use the latest published version, recorded in
`docs/released-version.txt`. `scripts/lint_docs.py` requires every exact Lash
pin in the README and docs entry pages to match that file. It also compares the
value with local `v*` tags and fails when a newer release tag exists. If the
matching tag is unavailable in an offline or shallow checkout, the checked-in
value is the fallback, so docs lint never needs network access.

After a release is published, the release workflow runs the same mechanical
docs-pin update against current `main`:

```bash
python3 scripts/release_version.py stamp-docs X.Y.Z
python3 scripts/lint_docs.py
```

`stamp-docs` is deliberately separate from the ephemeral manifest stamp used
by the release workflow: it changes checked-in documentation, not release
artifacts. The workflow commits the result directly to `main` after publishing.
An already-current pin creates no commit. A concurrent main update triggers a
bounded rebase-and-push retry; exhaustion is reported loudly but cannot make an
otherwise successful release fail. The direct commit triggers ordinary CI,
including docs lint. Tagless local and offline checkouts continue to use
`docs/released-version.txt` as their fallback authority.

## Release notes (required)

Every release ships curated notes. Any commit that should contribute
user-facing notes carries a `Release-Notes:` section in its body — everything
after the marker line, written as Markdown:

```text
Add durable suspension to processes

Implementation details for reviewers...

Release-Notes:
- Processes now suspend durably while waiting on signals or timers.
- Signals are named and typed; the unnamed `wait_signal()` is removed.
```

The manual release workflow runs `scripts/release_notes.py collect --require`
before it creates a new tag. If no commit in `previous-tag..release_sha` carries
a section, the release stops without publishing. The publish job collects the
same range's sections (oldest first) into the GitHub release body; the
auto-generated commit list is appended below. The previous tag is resolved by
graph ancestry (`git describe`), not version sorting, so tags from unrelated
history lines are ignored. The flow authors no synthetic commits, so every
commit in range is a real change eligible to contribute notes.

## Docs code snippets

Every Rust code block on a published docs page is compiled. The sources live in
`examples/docs-snippets/` (one module per page) inside
`// docs:start:<id>` / `// docs:end:<id>` regions, and each page block carries
`<pre data-snippet="<module>#<id>">`. CI runs
`cargo check -p docs-snippets --locked` (snippets must build against the
current API) and `python3 scripts/lint_docs.py` (the HTML must match the
regions byte-for-byte). To change a snippet, edit the `.rs` source and run
`python3 scripts/lint_docs.py --fix-snippets` to re-inject the HTML (and the
README hero block). Display-only blocks (shell transcripts, Lashlang, API-shape
excerpts) are marked `data-lang="..."` instead.

## Docs search index

The static site checks in its Pagefind bundle and generated index under
`docs/pagefind/`. When adding, removing, or renaming docs pages, regenerate the
index with the pinned Pagefind version used by the checked-in bundle:

```bash
rm -f docs/pagefind/fragment/*.pf_fragment docs/pagefind/index/*.pf_index docs/pagefind/pagefind.*.pf_meta
npx -y pagefind@1.5.2 --site docs --output-path docs/pagefind
```

Run `python3 scripts/lint_docs.py` after regeneration; the linter verifies the
hand-authored registry, links, snippets, and static pagers that Pagefind indexes.

## Auth

The `publish-crates` job uses a `CARGO_REGISTRY_TOKEN` repository secret (a
crates.io API token).

A token (not Trusted Publishing) is required for the **first** publish of a
brand-new crate, because crates.io Trusted Publishing can only be configured on
a crate that already exists.

### First publish of a new crate

New publishable crates inherit the lockstep version automatically
(`version.workspace = true`) and reference internal deps through
`{ workspace = true }`. Add the crate's own pin to root
`[workspace.dependencies]` so dependents can use it, and keep internal-only
crates `publish = false`.

To publish the workspace manually (the working tree carries `0.0.0-dev`, so pass
the real version to stamp):

```bash
# from a clean checkout at the release tag, with a crates.io token:
cargo login          # or export CARGO_REGISTRY_TOKEN=...
python3 scripts/publish_workspace.py --version X.Y.Z
```

`--version` stamps the manifests + lockfile, then the helper asks crates.io
whether each `(crate, version)` is already visible, skips published versions,
publishes each dependency layer concurrently with `cargo publish -p <crate>
--no-verify --locked`, retries transient registry/network failures, waits for
API visibility once per layer, then continues to the next layer.
`python3 scripts/publish_workspace.py --plan --version X.Y.Z` prints the computed
publish layers without touching crates.io.

### Upgrade to Trusted Publishing (recommended, after bootstrap)

Once every crate exists on crates.io, configure a trusted publisher for each
(crate settings → Trusted Publishing → repo `Ascending-AI/lash`, workflow
`release.yml`), then in `publish-crates`:

- add `id-token: write` to `permissions`,
- add a `rust-lang/crates-io-auth-action@v1` step (`id: auth`) before publish,
- set `CARGO_REGISTRY_TOKEN: ${{ steps.auth.outputs.token }}`,
- delete the long-lived `CARGO_REGISTRY_TOKEN` secret.

# Helper release archive

This is release data, independent of runtime installation. `release-NNNN.json`
files are immutable deltas in the order listed by `src/archive_releases.rs`.
Each stores new FunctionId-keyed definitions, package closed member sets and
HelperLibraryIds, public roots, HelperId bindings, versioned Rust native bindings, labels and reviewed branded
representation contracts. Definitions use FunctionDefinition's canonical Serde
shape. Unlike ReleasedFunctions, this encoding contains no build-local census,
index or native-constructor positions. Exact callee edges and lifted anonymous
bodies are part of the definitions. Unchanged definitions live only in an
earlier delta. `ledger.json` is the permanent HelperId map of contract revisions
and tombstone release sequence numbers.

`Archive::validate` first recomputes every added FunctionId, then verifies
library identity, exact named callee closure, public bindings and the entire
ledger. `check_append` compares with a previous archive to reject historical
rewrites. `check_natives` checks package, name and native_version against the
engine's actual versioned Rust constructors, independently of FunctionId.

Contract facts come from these owners:

| Fact | Owner |
| --- | --- |
| Signature, errors, guard unit and full limit, kernel version | FunctionDefinition, lash-kernel-doc/src/function.rs |
| Expression or action call form | Definition's native availability, lash-kernel-doc/src/ast.rs and validate.rs |
| Static operation requirements | FunctionBody.imports across exact callee closure, including signatures |
| Callback effects | Dynamic call or spawn in reached bodies, including lifted and escaping closures; conservatively unrestricted caller effects, otherwise none |
| Task and mutation effects | Reached spawn, join, await, yield, cancel, assignment and removal forms |
| Branded output representations | representations.json, reviewed against the source owners named in each guarantee |

Representation guarantees currently attach conservatively to every public
helper. This can refuse an upgrade that would in fact be safe; it cannot claim
compatibility by omitting an `Any` result's hidden layout. Body and charge
identity are deliberately excluded from the contract. Definitions retain both.
A contract digest is a content digest, not a proof of behavioral equivalence.

Builds authenticate checked-in history and emit `archive.candidate.json` in
OUT_DIR without modifying source data. Feature subsets preserve disabled
packages' previous entries. Synthetic builds are never release cuts. The
full-feature release check requires that the checked-in archive equals the
build candidate. Record a reviewed release explicitly, then commit its new
files and ledger append:

```sh
kiln run //crates/lash-kernel-releases:record-helper-data__bin -- record_archive
kiln sync
```

The recorder uses create-new for release files, verifies the previous history
is a prefix, and updates the ledger and explicit include list. Never edit an
old release file to make a check pass. Reset the pre-1.0 archive at the 1.0 cut,
as specified by ADR 0149. A future removed package must appear as a release
without its bindings so that the ledger records its tombstones.

Runtime metadata includes the authenticated union of current and historical
members for the packages enabled by the build. Public resolver roots remain
current. The build emits this union as a lazy binary image, not as a JSON
archive a worker must parse. The engine generates exact native constructor
rows from current declarations and all retained native bindings. Worker plans
are filtered to present exact bodies; other archived bodies interpret.
See `docs/perf/helper-archive-loading.md` for the cache bounds and measurements.

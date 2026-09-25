#!/usr/bin/env python3
"""Freeze the durable fixtures of a release tag into ``fixtures/release/<tag>/``.

ADR 0106 section 4: at the clean-slate release, capture the durable fixtures
every upgrade law reads and never regenerate them. This tool assembles that
corpus out of the *existing* fixture trees -- the ones the committed
regeneration utilities produce -- rather than serializing anything new:

- ``sqlite-stores``: the durable-read fixture's four SQLite catalogs
  (``fixtures/durable-read/v1/sqlite/``), whose write-shape law keeps them
  byte-identical to what the tagged build writes;
- ``postgres-store``: the durable-read fixture's PostgreSQL dump, version
  manifest, and expectations;
- ``session-at-rest``: the seeded ``durable-read-fixture`` session's own
  catalog file, captured on its own so the session-state upgrade law has a
  named artifact (its bytes are the same file ``sqlite-stores`` carries);
- ``segment-state``: the parked ``LashlangSegmentState`` captures and their
  provenance sidecars under ``crates/lash-lashlang-runtime/src/fixtures/``;
- ``tool-intent-journals``: the checked-in Restate tool-intent journal corpus;
- ``replay-corpus``: the deterministic ``RecordedRuntimeEffect`` journals.

``--regenerate`` first re-runs the committed generators (the ignored Rust
capture tests) so the sources are fresh; without it the tool snapshots the
checked-in trees, which the write-shape laws already hold current. A real
capture requires ``HEAD`` to be exactly ``--tag``; ``--dry-run`` writes the
same layout into a temporary directory (or ``--dest``) with no tag check and
no regeneration, to prove the plumbing before the tag exists.

Only the Python standard library is used.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile


ROOT = Path(__file__).resolve().parents[1]

REGENERATE_DURABLE_READ = {"LASH_REGENERATE_DURABLE_READ_FIXTURES": "1"}
SQLITE_REGENERATE = (
    REGENERATE_DURABLE_READ,
    [
        "kiln", "run", "//crates/lash-sqlite-store:durable_read_fixture__test", "--",
        "regenerate_sqlite_durable_fixture", "--ignored", "--exact",
    ],
)
POSTGRES_REGENERATE = (
    REGENERATE_DURABLE_READ,
    [
        "kiln", "run", "//crates/lash-postgres-store:durable_read_fixture__test", "--",
        "regenerate_postgres_durable_fixture", "--ignored", "--exact",
    ],
)
REPLAY_CORPUS_REGENERATE = (
    {"LASH_REGENERATE_REPLAY_CORPUS": "1"},
    [
        "kiln", "run", "//crates/lash-restate:lash-restate__unit_test", "--",
        "tests::replay_corpus::regenerate_replay_corpus_fixtures", "--ignored", "--exact",
    ],
)
TOOL_INTENT_CAPTURE = (
    {},
    [
        "kiln", "run", "//crates/lash-restate:lash-restate__unit_test", "--",
        "tests::recording_context::capture_tool_intent_journal_corpus_from_real_endpoint_interruptions",
        "--ignored", "--exact",
    ],
)


@dataclass(frozen=True)
class Leg:
    """One fixture tree the release corpus carries.

    ``source`` is a repo-relative file or directory the checked-in generators
    own; ``regenerate`` is the committed capture utility that rewrites it, as
    ``(environment overlay, argv)`` pairs run only under ``--regenerate``.
    """

    name: str
    source: str
    regenerate: tuple = ()
    requires_env: tuple[str, ...] = ()
    note: str = ""


LEGS = (
    Leg(
        name="sqlite-stores",
        source="fixtures/durable-read/v1/sqlite",
        regenerate=(SQLITE_REGENERATE,),
        note=(
            "the four SQLite catalogs the write-shape law holds byte-identical to the "
            "tagged build's output; expected.json and versions.json come along"
        ),
    ),
    Leg(
        name="postgres-store",
        source="fixtures/durable-read/v1/postgres",
        regenerate=(POSTGRES_REGENERATE,),
        requires_env=("LASH_POSTGRES_DATABASE_URL",),
        note=(
            "the durable-read pg_dump, version manifest, and expectations; "
            "regeneration needs an owned throwaway database"
        ),
    ),
    Leg(
        name="session-at-rest",
        source="fixtures/durable-read/v1/sqlite/durable-core.db",
        regenerate=(SQLITE_REGENERATE,),
        note=(
            "the seeded `durable-read-fixture` session catalog at rest: session-state "
            "marker, session head, graph nodes, and checkpoints. The same file the "
            "sqlite-stores leg carries, named separately for the session-state law"
        ),
    ),
    Leg(
        name="segment-state",
        source="crates/lash-lashlang-runtime/src/fixtures",
        note=(
            "parked LashlangSegmentState captures plus their provenance sidecars; "
            "new captures are written by the version-pinned ignored tests in "
            "process::segment_trace_tests"
        ),
    ),
    Leg(
        name="tool-intent-journals",
        source="crates/lash-restate/tests/fixtures/tool_intent_journals",
        regenerate=(TOOL_INTENT_CAPTURE,),
        note="Restate endpoint-interruption journal captures at each generation gate",
    ),
    Leg(
        name="replay-corpus",
        source="crates/lash-restate/testdata/replay-corpus",
        regenerate=(REPLAY_CORPUS_REGENERATE,),
        note="deterministic RecordedRuntimeEffect journals per scenario",
    ),
)

TAG_COMPONENT = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._-]*$")
MANIFEST_SCHEMA = "lash.release-fixtures-manifest.v1"


class CaptureError(RuntimeError):
    """The capture cannot proceed: bad arguments, sources, or environment."""


def git(repo: Path, *args: str) -> str:
    result = subprocess.run(
        ["git", *args], cwd=repo, capture_output=True, text=True, check=False
    )
    if result.returncode != 0:
        raise CaptureError(
            f"git {' '.join(args)} failed ({result.returncode}): {result.stderr.strip()}"
        )
    return result.stdout.strip()


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def copy_leg(repo: Path, leg: Leg, dest_root: Path) -> list[dict]:
    """Copy one leg's source tree under ``dest_root/<leg.name>/`` verbatim."""
    source = repo / leg.source
    if not source.exists():
        raise CaptureError(
            f"{leg.name}: source {leg.source} does not exist; run its generator "
            "first (--regenerate) or fix the leg table"
        )
    target = dest_root / leg.name
    if source.is_dir():
        shutil.copytree(source, target)
    else:
        target.mkdir(parents=True)
        shutil.copy2(source, target / source.name)
    files: list[dict] = []
    for path in sorted(target.rglob("*")):
        if path.is_file():
            files.append(
                {
                    "path": path.relative_to(target).as_posix(),
                    "sha256": sha256_file(path),
                    "bytes": path.stat().st_size,
                }
            )
    if not files:
        raise CaptureError(f"{leg.name}: source {leg.source} produced no files")
    return files


def run_generators(repo: Path, legs: tuple[Leg, ...]) -> None:
    """Re-run each leg's committed generator once, before copying."""
    done: set[tuple] = set()
    for leg in legs:
        for missing in leg.requires_env:
            if not os.environ.get(missing):
                raise CaptureError(
                    f"{leg.name}: --regenerate needs {missing} set "
                    f"({leg.note})"
                )
        for env_overlay, argv in leg.regenerate:
            command_key = (tuple(sorted(env_overlay.items())), tuple(argv))
            if command_key in done:
                continue
            done.add(command_key)
            print("+ " + " ".join(argv), flush=True)
            result = subprocess.run(
                argv, cwd=repo, env={**os.environ, **env_overlay}, check=False
            )
            if result.returncode != 0:
                raise CaptureError(
                    f"{leg.name}: generator exited {result.returncode}: "
                    + " ".join(argv)
                )


def verify_tag(repo: Path, tag: str) -> str:
    """Require HEAD to be the tagged commit; return that commit."""
    resolved = subprocess.run(
        ["git", "rev-parse", "--verify", "--quiet", f"refs/tags/{tag}^{{commit}}"],
        cwd=repo, capture_output=True, text=True, check=False,
    )
    if resolved.returncode != 0 or not resolved.stdout.strip():
        raise CaptureError(f"tag {tag} does not resolve to a commit")
    tagged = resolved.stdout.strip()
    head = git(repo, "rev-parse", "HEAD")
    if tagged != head:
        raise CaptureError(
            f"HEAD {head[:12]} is not tag {tag} ({tagged[:12]}); capture fixtures "
            "at the tagged commit, or pass --dry-run to rehearse"
        )
    return head


def write_manifest(
    dest: Path, tag: str, commit: str | None, dry_run: bool, legs: list[dict]
) -> None:
    manifest = {
        "schema": MANIFEST_SCHEMA,
        "tag": tag,
        "source_commit": commit,
        "dry_run": dry_run,
        "captured_at": datetime.now(timezone.utc).isoformat(timespec="seconds"),
        "legs": legs,
    }
    (dest / "manifest.json").write_text(
        json.dumps(manifest, indent=2) + "\n", encoding="utf-8"
    )


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--tag", required=True, help="release tag, e.g. v1.0")
    parser.add_argument(
        "--dest",
        type=Path,
        help="output directory (default: fixtures/release/<tag>; a temp dir under --dry-run)",
    )
    parser.add_argument(
        "--regenerate",
        action="store_true",
        help="re-run each leg's committed generator before copying",
    )
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="copy into a temporary directory without tag verification or regeneration",
    )
    parser.add_argument("--repo", type=Path, default=ROOT, help=argparse.SUPPRESS)
    args = parser.parse_args(argv)
    if not TAG_COMPONENT.fullmatch(args.tag):
        parser.error("--tag must be a single safe path component")
    if args.dry_run and args.regenerate:
        parser.error("--dry-run never regenerates; drop one of the flags")
    return args


def main(argv: list[str] | None = None) -> int:
    args = parse_args(sys.argv[1:] if argv is None else argv)
    repo = args.repo
    dry_run = args.dry_run
    if args.dest is not None:
        dest = args.dest
    elif dry_run:
        dest = Path(tempfile.mkdtemp(prefix="lash-release-fixtures-"))
    else:
        dest = repo / "fixtures" / "release" / args.tag

    try:
        commit = None if dry_run else verify_tag(repo, args.tag)
        if dest.exists() and any(dest.iterdir()):
            raise CaptureError(
                f"{dest} already has content; release fixtures are frozen once "
                "written. Remove the directory by hand to recapture"
            )
        if args.regenerate:
            run_generators(repo, LEGS)
        dest.mkdir(parents=True, exist_ok=True)
        captured = []
        for leg in LEGS:
            files = copy_leg(repo, leg, dest)
            captured.append(
                {
                    "name": leg.name,
                    "source": leg.source,
                    "note": leg.note,
                    "files": files,
                }
            )
            print(f"{leg.name}: {len(files)} files from {leg.source}", flush=True)
        write_manifest(dest, args.tag, commit, dry_run, captured)
    except CaptureError as error:
        print(f"capture-release-fixtures error: {error}", file=sys.stderr)
        return 2
    print(
        f"captured {sum(len(leg['files']) for leg in captured)} files "
        f"into {dest}" + (" (dry run)" if dry_run else "")
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

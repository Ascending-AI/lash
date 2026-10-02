#!/usr/bin/env python3
"""Freeze the durable fixtures of a release tag into ``fixtures/release/<tag>/``.

ADR 0106 section 4: at the clean-slate release, capture the durable fixtures
every upgrade law reads and never regenerate them. This tool assembles that
corpus out of the fixture trees the committed regeneration utilities produce
rather than serializing anything new:

- ``sqlite-stores``: the durable-read fixture's SQLite catalogs
  (``fixtures/durable-read/v1/sqlite/``), as the tagged build writes them;
- ``postgres-store``: the durable-read fixture's PostgreSQL dump, version
  manifest, and expectations;
- ``session-at-rest``: the seeded ``durable-read-fixture`` session's own
  catalog file, captured on its own so the session-state upgrade law has a
  named artifact (its bytes are the same file ``sqlite-stores`` carries);
- ``segment-state``: the parked ``LashlangSegmentState`` golden under
  ``crates/lash-lashlang-runtime/src/fixtures/``;
- ``tool-intent-journals``: the checked-in Restate tool-intent journal corpus;
- ``replay-corpus``: the deterministic release replay journals.

A fixture that records the generation that wrote it belongs to exactly that
build, and its own law refuses it under any other. The legs therefore hold
only what the tagged build wrote: predecessor goldens are not part of a
release corpus.

``--regenerate`` first re-runs the committed generators through Kiln after
the caller sources ``./env.sh``. The ignored Rust capture tests then refresh
the sources. The durable-read trees are not checked
in -- each backend's round-trip law proves the tagged build reads what it
writes -- so a real capture needs ``--regenerate`` to produce them; without it
the tool snapshots whatever trees are present and refuses a missing one. A real
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

REGENERATE_DURABLE_READ = {"LASH_REGENERATE": "1"}


def capture_command(target: str, law: str) -> list[str]:
    return ["kiln", "test", target, "--local-test-execution", "--no-test-cache",
            "--test_arg=--ignored", "--test_arg=--exact", f"--test_arg={law}",
            "--test_env=BUILD_WORKSPACE_DIRECTORY"]


SQLITE_REGENERATE = (
    REGENERATE_DURABLE_READ,
    capture_command("//crates/lash-sqlite-store:durable_read_fixture__test",
                    "regenerate_sqlite_durable_fixture"),
)
POSTGRES_REGENERATE = (
    REGENERATE_DURABLE_READ,
    [*capture_command("//crates/lash-postgres-store:durable_read_fixture__test",
                      "regenerate_postgres_durable_fixture"),
     "--test_env=LASH_POSTGRES_DATABASE_URL", "--test_env=LASH_REQUIRE_POSTGRES"],
)
REPLAY_CORPUS_REGENERATE = (
    {"LASH_REGENERATE": "1"},
    capture_command("//crates/lash-restate:lash-restate__unit_test",
                    "tests::replay_corpus::regenerate_replay_corpus_fixtures"),
)
SEGMENT_STATE_CAPTURE = (
    {"LASH_REGENERATE": "1"},
    capture_command("//crates/lash-lashlang-runtime:lash-lashlang-runtime__unit_test",
                    "process::segment_trace_tests::capture_parked_loop_segment"),
)
TOOL_INTENT_CAPTURE = (
    {"LASH_REGENERATE": "1"},
    capture_command("//crates/lash-restate:lash-restate__unit_test",
                    "tests::recording_context::capture_tool_intent_journal_corpus_from_real_endpoint_interruptions"),
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
            "the SQLite catalogs the tagged build's generator writes; "
            "expected.json and versions.json come along"
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
        regenerate=(SEGMENT_STATE_CAPTURE,),
        note=(
            "the LashlangSegmentState the tagged build parks inside a loop, with "
            "the generation that wrote it"
        ),
    ),
    Leg(
        name="tool-intent-journals",
        source="crates/lash-restate/tests/fixtures/tool_intent_journals",
        regenerate=(TOOL_INTENT_CAPTURE,),
        note=(
            "Restate endpoint-interruption journal captures, each with the "
            "generation that wrote it"
        ),
    ),
    Leg(
        name="replay-corpus",
        source="crates/lash-restate/testdata/replay-corpus",
        regenerate=(REPLAY_CORPUS_REGENERATE,),
        note="one ordered journal per scenario, stamped with the capturing build's generation",
    ),
)

TAG_COMPONENT = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._-]*$")
MANIFEST_SCHEMA = "lash.release-fixtures-manifest.v1"

# `git` variables that attach an invocation to a different repository, index,
# object store or namespace than the directory it runs in, or that inject the
# configuration a parent `git` process passes to its children. They leak out
# of hooks, wrappers and CI environments; a capture or verification must run
# against the repository named by its argument, never the ambient one.
GIT_AMBIENT = (
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_COMMON_DIR",
    "GIT_NAMESPACE",
    "GIT_PREFIX",
    "GIT_QUARANTINE_PATH",
    "GIT_CEILING_DIRECTORIES",
    "GIT_DISCOVERY_ACROSS_FILESYSTEM",
    "GIT_CONFIG_PARAMETERS",
    "GIT_CONFIG_COUNT",
)


def git_env() -> dict[str, str]:
    """The ambient environment minus the variables that steer git elsewhere."""
    return {
        name: value
        for name, value in os.environ.items()
        if name not in GIT_AMBIENT
        and not name.startswith(("GIT_CONFIG_KEY_", "GIT_CONFIG_VALUE_"))
    }


class CaptureError(RuntimeError):
    """The capture cannot proceed: bad arguments, sources, or environment."""


def git(repo: Path, *args: str) -> str:
    result = subprocess.run(
        ["git", *args], cwd=repo, capture_output=True, text=True, check=False,
        env=git_env(),
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
            argv = [*argv, *[f"--test_env={key}={value}" for key, value in env_overlay.items()]]
            command_key = (tuple(sorted(env_overlay.items())), tuple(argv))
            if command_key in done:
                continue
            done.add(command_key)
            print("+ " + " ".join(argv), flush=True)
            result = subprocess.run(
                argv, cwd=repo, env={**git_env(), "BUILD_WORKSPACE_DIRECTORY": str(repo), **env_overlay}, check=False
            )
            if result.returncode != 0:
                raise CaptureError(
                    f"{leg.name}: generator exited {result.returncode}: "
                    + " ".join(argv)
                )


def require_committed_fixtures_unchanged(repo: Path, legs: tuple[Leg, ...]) -> None:
    """Refuse a regeneration that rewrote a fixture the tagged commit carries.

    The committed goldens were written by the build the tag names. A generator
    that produces other bytes for them ran as a different build (another
    feature graph gives another build generation), so what it captured is not
    what the tagged build's laws read.
    """
    sources = sorted({leg.source for leg in legs})
    result = subprocess.run(
        ["git", "status", "--porcelain", "--", *sources],
        cwd=repo, capture_output=True, text=True, check=False, env=git_env(),
    )
    if result.returncode != 0:
        raise CaptureError(f"git status failed: {result.stderr.strip()}")
    changed = [line[3:] for line in result.stdout.splitlines()]
    if changed:
        raise CaptureError(
            "regeneration changed fixtures the tagged commit carries, so the "
            "capturing build is not the build that wrote them: " + ", ".join(changed)
        )


def verify_tag(repo: Path, tag: str) -> str:
    """Require HEAD to be the tagged commit; return that commit."""
    resolved = subprocess.run(
        ["git", "rev-parse", "--verify", "--quiet", f"refs/tags/{tag}^{{commit}}"],
        cwd=repo, capture_output=True, text=True, check=False, env=git_env(),
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


GENERATION = re.compile(r"^[0-9a-f]{12}$")


def check_replay_corpus_generation(root: Path) -> None:
    """Require one build generation across the copied journals.

    The journals carry their own generation; the manifest does not repeat it.
    """
    journals = sorted((root / "replay-corpus").glob("*/journal.json"))
    if not journals:
        raise CaptureError("replay-corpus: no scenario journals")
    generations = set()
    for journal in journals:
        try:
            generation = json.loads(journal.read_text(encoding="utf-8"))["generation"]
        except (OSError, ValueError, KeyError, TypeError) as error:
            raise CaptureError(
                f"replay-corpus: {journal.parent.name}: missing or invalid generation"
            ) from error
        if not isinstance(generation, str) or not GENERATION.fullmatch(generation):
            raise CaptureError("replay-corpus: generation must be 12 lowercase hex characters")
        generations.add(generation)
    if len(generations) != 1:
        raise CaptureError("replay-corpus: journals were written by different generations")


def write_manifest(
    dest: Path, tag: str, commit: str | None, dry_run: bool, legs: list[dict]
) -> None:
    check_replay_corpus_generation(dest)
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
            require_committed_fixtures_unchanged(repo, LEGS)
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

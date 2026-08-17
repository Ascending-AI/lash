#!/usr/bin/env python3
"""Check durable-semantics changes in inline insta snapshots."""

from __future__ import annotations

import json
import os
from pathlib import Path
import re
import subprocess
import sys
import urllib.error
import urllib.parse
import urllib.request
from itertools import zip_longest
from typing import NamedTuple


DEFAULT_RANGE = "origin/main...HEAD"
SNAPSHOT_OPEN = re.compile(r'@r(?P<hashes>#+)?"')
REV_TRANSITION = re.compile(r"\brev=\S+->\S+")
USAGE_COMPONENT = re.compile(r"\busage\s+entries=")
# The behavior-transcript vocabulary's durable-write event names plus its
# fixed component renderings, and the two legacy class tokens the pre-vocabulary
# renderer emitted. USAGE_COMPONENT intentionally accepts any renderer padding.
# Keep these in sync with
# `lash_core::testing::behavior_transcript::DURABLE_WRITE_EVENTS`.
DURABLE_MARKERS = (
    "checkpoint.commit",
    "checkpoint.request",
    "durable.effect",
    "stored logical=",
    "ref (unchanged)",
    "Checkpoint",
    "DurableEffect",
)
HUNK_HEADER = re.compile(r"^@@ -(?P<old>\d+)(?:,\d+)? \+(?P<new>\d+)(?:,\d+)? @@")
# All Rust files: the `@r` snapshot-body requirement already suppresses noise,
# and any path allowlist is a silent-decay surface — a snapshot added in an
# excluded file (inline `#[cfg(test)]` modules live throughout src/) would be
# invisible forever and indistinguishable from "nothing to report".
RUST_TEST_PATHS = ("*.rs", ":(glob)**/*.rs")


class ChangedLine(NamedTuple):
    line_number: int
    text: str
    body: str


class Finding(NamedTuple):
    path: str
    old: ChangedLine | None
    new: ChangedLine | None


class Options(NamedTuple):
    enforce: bool
    revision_range: str


def snapshot_body(line: str, delimiter: str | None) -> tuple[str | None, str | None]:
    """Return the snapshot-body portion of a source line and the next delimiter."""
    if delimiter is None:
        opener = SNAPSHOT_OPEN.search(line)
        if opener is None:
            return None, None
        hashes = opener.group("hashes") or ""
        delimiter = '"' + hashes
        remainder = line[opener.end() :]
    else:
        remainder = line

    closing = remainder.find(delimiter)
    if closing >= 0:
        return remainder[:closing], None
    return remainder, delimiter


def is_durable(body: str) -> bool:
    return (
        any(marker in body for marker in DURABLE_MARKERS)
        or bool(USAGE_COMPONENT.search(body))
        or bool(REV_TRANSITION.search(body))
    )


def classify_patch(patch: str) -> list[Finding]:
    """Classify a full-context unified diff."""
    findings: list[Finding] = []
    path = ""
    old_line = 0
    new_line = 0
    old_delimiter: str | None = None
    new_delimiter: str | None = None
    removed: list[ChangedLine] = []
    added: list[ChangedLine] = []

    def flush_changes() -> None:
        for old, new in zip_longest(removed, added):
            if (old and is_durable(old.body)) or (new and is_durable(new.body)):
                findings.append(Finding(path, old, new))
        removed.clear()
        added.clear()

    for diff_line in patch.splitlines():
        if diff_line.startswith("diff --git "):
            flush_changes()
            path = ""
            old_delimiter = None
            new_delimiter = None
            continue
        if diff_line.startswith("+++ "):
            candidate = diff_line[4:]
            path = candidate[2:] if candidate.startswith("b/") else candidate
            continue

        hunk = HUNK_HEADER.match(diff_line)
        if hunk:
            flush_changes()
            old_line = int(hunk.group("old"))
            new_line = int(hunk.group("new"))
            old_delimiter = None
            new_delimiter = None
            continue
        if not path or diff_line.startswith("--- "):
            continue

        prefix = diff_line[:1]
        text = diff_line[1:]
        if prefix == " ":
            flush_changes()
            _, old_delimiter = snapshot_body(text, old_delimiter)
            _, new_delimiter = snapshot_body(text, new_delimiter)
            old_line += 1
            new_line += 1
        elif prefix == "-":
            body, old_delimiter = snapshot_body(text, old_delimiter)
            if body is not None:
                removed.append(ChangedLine(old_line, text, body))
            old_line += 1
        elif prefix == "+":
            body, new_delimiter = snapshot_body(text, new_delimiter)
            if body is not None:
                added.append(ChangedLine(new_line, text, body))
            new_line += 1

    flush_changes()
    return findings


def render_findings(findings: list[Finding]) -> str:
    if not findings:
        return "No durable transcript snapshot lines changed."

    lines = [
        "Durable transcript snapshot lines changed; add a `Transcript:` "
        "justification sentence to the PR body (docs/agents/pr-style.md):"
    ]
    for finding in findings:
        old_location = (
            str(finding.old.line_number) if finding.old is not None else "none"
        )
        new_location = (
            str(finding.new.line_number) if finding.new is not None else "none"
        )
        lines.append(f"{finding.path}:{old_location}->{new_location}")
        lines.append(f"  - {finding.old.text if finding.old else '(none)'}")
        lines.append(f"  + {finding.new.text if finding.new else '(none)'}")
    return "\n".join(lines)


def append_summary(summary_path: str, findings: list[Finding]) -> None:
    if not findings:
        return
    with Path(summary_path).open("a", encoding="utf-8") as summary:
        summary.write(
            "## Durable transcript lines changed — needs a justification "
            "sentence in the PR body (pr-style.md)\n\n"
        )
        summary.write("```diff\n")
        for finding in findings:
            summary.write(f"# {finding.path}\n")
            summary.write(f"- {finding.old.text if finding.old else '(none)'}\n")
            summary.write(f"+ {finding.new.text if finding.new else '(none)'}\n")
        summary.write("```\n")


def requested_options(argv: list[str]) -> Options:
    enforce = True
    args = list(argv)
    if args and args[0] in {"--enforce", "--advisory"}:
        enforce = args.pop(0) == "--enforce"
    if not args:
        return Options(enforce, DEFAULT_RANGE)
    if len(args) == 1:
        return Options(enforce, args[0])
    if len(args) == 2 and args[0] == "--range":
        return Options(enforce, args[1])
    raise ValueError(
        "usage: check-transcript-diff.py [--enforce|--advisory] "
        "[--range] [REV_RANGE]"
    )


JUSTIFICATION = re.compile(r"(?m)^\s*Transcript:\s+\S")
GITHUB_API = "https://api.github.com"


def body_is_justified(body: str | None) -> bool:
    return bool(body) and bool(JUSTIFICATION.search(body))


def event_pull_request_body() -> str | None:
    """The PR body carried in the triggering event payload, if it carries one.

    Only `pull_request` events do. A `workflow_dispatch` payload has no
    `pull_request` key at all, which is why this returns `None` rather than an
    empty string: "no body here" and "a body that says nothing" are different
    answers, and only the first one should send us looking elsewhere.
    """
    event_path = os.environ.get("GITHUB_EVENT_PATH")
    if not event_path:
        return None
    try:
        event = json.loads(Path(event_path).read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return None
    pull_request = event.get("pull_request")
    if not isinstance(pull_request, dict):
        return None
    return pull_request.get("body") or ""


def current_branch() -> str | None:
    branch = os.environ.get("GITHUB_HEAD_REF") or os.environ.get("GITHUB_REF_NAME")
    if branch:
        return branch
    try:
        result = subprocess.run(
            ["git", "rev-parse", "--abbrev-ref", "HEAD"],
            check=True,
            capture_output=True,
            text=True,
        )
    except (OSError, subprocess.CalledProcessError):
        return None
    branch = result.stdout.strip()
    return branch or None


def queued_pull_request_number() -> str | None:
    """The PR number a merge-queue ref is queueing, if this is one.

    A `merge_group` run checks out `gh-readonly-queue/<base>/pr-<n>-<sha>`, a ref
    that is no branch any pull request has as its head — so the by-head query
    below finds nothing and the gate would fail a change whose PR justifies it.
    The queue writes the PR number into the ref, so ask for that PR by number.

    Only the queue's own event may name a PR this way. A ref name is something a
    developer can choose, and reading one as a queue ref outside the queue would
    let a branch named after the pattern fetch an unrelated PR's body and answer
    this gate with someone else's justification.
    """
    if os.environ.get("GITHUB_EVENT_NAME") != "merge_group":
        return None
    branch = current_branch()
    if not branch:
        return None
    match = re.fullmatch(r"gh-readonly-queue/[^/]+/pr-(?P<number>\d+)-[0-9a-f]+", branch)
    return match.group("number") if match else None


def queried_pull_request_body() -> str | None:
    """The open PR for this branch, asked for directly.

    A manually dispatched run is the sanctioned recovery when GitHub stops
    delivering a branch's events, and it arrives with no PR in its payload. A
    merge-queue run has no PR in its payload either, by design. The
    justification still exists — it is in the PR body — so the gate asks for it
    rather than failing a run for the shape of its trigger. Returns `None` when
    the question cannot be asked at all, which is distinct from asking and
    finding nothing.
    """
    token = os.environ.get("GITHUB_TOKEN")
    repository = os.environ.get("GITHUB_REPOSITORY")
    branch = current_branch()
    if not (token and repository and branch):
        return None
    if number := queued_pull_request_number():
        return queried_pull_request_body_by_number(repository, token, number)
    owner = repository.split("/")[0]
    head = urllib.parse.quote(f"{owner}:{branch}", safe="")
    request = urllib.request.Request(
        f"{GITHUB_API}/repos/{repository}/pulls?state=open&head={head}",
        headers={
            "Authorization": f"Bearer {token}",
            "Accept": "application/vnd.github+json",
            "X-GitHub-Api-Version": "2022-11-28",
            "User-Agent": "lash-transcript-gate",
        },
    )
    try:
        with urllib.request.urlopen(request, timeout=15) as response:
            payload = json.loads(response.read().decode("utf-8"))
    except (OSError, ValueError) as error:
        print(
            f"note: could not query the pull request for `{branch}`: {error}",
            file=sys.stderr,
        )
        return None
    if not isinstance(payload, list):
        return None
    for pull_request in payload:
        if isinstance(pull_request, dict):
            return pull_request.get("body") or ""
    return None


def queried_pull_request_body_by_number(
    repository: str, token: str, number: str
) -> str | None:
    """One PR's body, by number. Same failure semantics as the by-head query."""
    request = urllib.request.Request(
        f"{GITHUB_API}/repos/{repository}/pulls/{number}",
        headers={
            "Authorization": f"Bearer {token}",
            "Accept": "application/vnd.github+json",
            "X-GitHub-Api-Version": "2022-11-28",
            "User-Agent": "lash-transcript-gate",
        },
    )
    try:
        with urllib.request.urlopen(request, timeout=15) as response:
            payload = json.loads(response.read().decode("utf-8"))
    except (OSError, ValueError) as error:
        print(
            f"note: could not query queued pull request #{number}: {error}",
            file=sys.stderr,
        )
        return None
    if not isinstance(payload, dict):
        return None
    return payload.get("body") or ""


def has_transcript_justification() -> bool:
    """Whether the PR for this change carries a `Transcript:` sentence.

    The event payload is consulted first because it needs no network. When it
    does not settle the question — no payload body, or a body written before
    the sentence was added — the live PR is asked. Both sources are the same
    PR body, so this never accepts a justification the PR does not actually
    make; it only stops a stale or absent payload from standing in for one.
    Failing closed is the answer when neither source produces a justification.
    """
    if body_is_justified(event_pull_request_body()):
        return True
    return body_is_justified(queried_pull_request_body())


def main(argv: list[str]) -> int:
    try:
        options = requested_options(argv)
        result = subprocess.run(
            [
                "git",
                "diff",
                "--unified=100000",
                "--no-color",
                "--no-ext-diff",
                options.revision_range,
                "--",
                *RUST_TEST_PATHS,
            ],
            check=True,
            capture_output=True,
            text=True,
        )
        findings = classify_patch(result.stdout)
        print(render_findings(findings))
        if summary_path := os.environ.get("GITHUB_STEP_SUMMARY"):
            append_summary(summary_path, findings)
        if findings and options.enforce and not has_transcript_justification():
            return 1
    except Exception as error:
        print(
            f"error: durable transcript classifier could not run: {error}",
            file=sys.stderr,
        )
        return 2
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))

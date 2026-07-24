#!/usr/bin/env python3
"""Validate temporary test retry and quarantine metadata."""

from __future__ import annotations

import datetime as dt
import json
import sys
from pathlib import Path
from urllib.parse import urlparse


ROOT = Path(__file__).resolve().parents[1]
MANIFEST = ROOT / ".github" / "test-quarantines.json"
SCHEMA = "lash.test-quarantines.v1"
REQUIRED_FIELDS = {
    "id",
    "test_selector",
    "mode",
    "owner",
    "issue_url",
    "rca_status",
    "expires_on",
}
MODES = {"quarantine", "retry"}
RCA_STATUSES = {
    "investigating",
    "root_cause_identified",
    "fix_in_progress",
    "fix_verified",
}


def fail(message: str) -> None:
    raise ValueError(message)


def nonempty_string(entry_id: str, field: str, value: object) -> str:
    if not isinstance(value, str) or not value.strip():
        fail(f"{entry_id}: {field} must be a non-empty string")
    return value


def validate_manifest(path: Path, today: dt.date | None = None) -> None:
    today = today or dt.date.today()
    payload = json.loads(path.read_text(encoding="utf-8"))
    if payload.get("schema") != SCHEMA:
        fail(f"{path}: schema must be {SCHEMA}")
    entries = payload.get("quarantines")
    if not isinstance(entries, list):
        fail(f"{path}: quarantines must be an array")

    seen: set[str] = set()
    for index, entry in enumerate(entries):
        if not isinstance(entry, dict):
            fail(f"entry {index}: quarantine metadata must be an object")
        missing = REQUIRED_FIELDS - entry.keys()
        unknown = entry.keys() - REQUIRED_FIELDS
        if missing:
            fail(f"entry {index}: missing fields: {', '.join(sorted(missing))}")
        if unknown:
            fail(f"entry {index}: unknown fields: {', '.join(sorted(unknown))}")

        entry_id = nonempty_string(f"entry {index}", "id", entry["id"])
        if entry_id in seen:
            fail(f"{entry_id}: duplicate quarantine id")
        seen.add(entry_id)
        nonempty_string(entry_id, "test_selector", entry["test_selector"])
        nonempty_string(entry_id, "owner", entry["owner"])

        mode = nonempty_string(entry_id, "mode", entry["mode"])
        if mode not in MODES:
            fail(f"{entry_id}: mode must be one of {', '.join(sorted(MODES))}")

        issue_url = nonempty_string(entry_id, "issue_url", entry["issue_url"])
        parsed_issue = urlparse(issue_url)
        if parsed_issue.scheme != "https" or not parsed_issue.netloc:
            fail(f"{entry_id}: issue_url must be an https URL")

        rca_status = nonempty_string(entry_id, "rca_status", entry["rca_status"])
        if rca_status not in RCA_STATUSES:
            fail(
                f"{entry_id}: rca_status must be one of "
                f"{', '.join(sorted(RCA_STATUSES))}"
            )

        expires_on = nonempty_string(entry_id, "expires_on", entry["expires_on"])
        try:
            expiry = dt.date.fromisoformat(expires_on)
        except ValueError as error:
            fail(f"{entry_id}: expires_on must be an ISO date: {error}")
        if expiry < today:
            fail(f"{entry_id}: quarantine expired on {expiry.isoformat()}")


def main() -> int:
    try:
        validate_manifest(MANIFEST)
    except (OSError, json.JSONDecodeError, ValueError) as error:
        print(f"test quarantine metadata invalid: {error}", file=sys.stderr)
        return 1
    print("test quarantine metadata: valid")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

#!/usr/bin/env python3
"""Create and verify same-run CI artifact provenance manifests."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import stat
import sys
import tarfile
from dataclasses import dataclass
from pathlib import Path, PurePosixPath
from typing import Mapping, NoReturn, Sequence


MANIFEST_SCHEMA = "lash.ci-artifact-provenance.v1"
RECEIPT_SCHEMA = "lash.ci-artifact-consumption.v1"
SHA256 = re.compile(r"[0-9a-f]{64}")
POSITIVE_INTEGER = re.compile(r"[1-9][0-9]*")


class ProvenanceError(ValueError):
    """The artifact cannot be trusted under the requested provenance contract."""


@dataclass(frozen=True)
class SourceContext:
    repository: str
    run_id: str
    head_sha: str
    ref: str
    event_name: str
    workflow_ref: str
    actor_id: str

    @classmethod
    def from_environment(cls, environment: Mapping[str, str]) -> "SourceContext":
        values = {
            "repository": environment.get("GITHUB_REPOSITORY", ""),
            "run_id": environment.get("GITHUB_RUN_ID", ""),
            "head_sha": environment.get("GITHUB_SHA", ""),
            "ref": environment.get("GITHUB_REF", ""),
            "event_name": environment.get("GITHUB_EVENT_NAME", ""),
            "workflow_ref": environment.get("GITHUB_WORKFLOW_REF", ""),
            "actor_id": environment.get("GITHUB_ACTOR_ID", ""),
        }
        missing = [key for key, value in values.items() if not value]
        if missing:
            raise ProvenanceError(
                "missing GitHub source context: " + ", ".join(sorted(missing))
            )
        if not POSITIVE_INTEGER.fullmatch(values["run_id"]):
            raise ProvenanceError("GITHUB_RUN_ID must be a positive integer")
        if not POSITIVE_INTEGER.fullmatch(values["actor_id"]):
            raise ProvenanceError("GITHUB_ACTOR_ID must be a positive integer")
        if not re.fullmatch(r"[0-9a-fA-F]{40}", values["head_sha"]):
            raise ProvenanceError("GITHUB_SHA must be a 40-character hexadecimal commit")
        return cls(**values)

    def to_json(self) -> dict[str, str]:
        return {
            "repository": self.repository,
            "run_id": self.run_id,
            "head_sha": self.head_sha.lower(),
            "ref": self.ref,
            "event_name": self.event_name,
            "workflow_ref": self.workflow_ref,
            "actor_id": self.actor_id,
        }


def _fail(message: str) -> NoReturn:
    raise ProvenanceError(message)


def _positive_integer(value: str, label: str) -> str:
    if not POSITIVE_INTEGER.fullmatch(value):
        _fail(f"{label} must be a positive integer")
    return value


def _require_exact_keys(value: object, expected: set[str], label: str) -> dict[str, object]:
    if not isinstance(value, dict):
        _fail(f"{label} must be an object")
    actual = set(value)
    if actual != expected:
        missing = sorted(expected - actual)
        extra = sorted(actual - expected)
        _fail(f"{label} keys mismatch: missing={missing}, extra={extra}")
    return value


def _safe_payload_path(root: Path, relative: str, *, must_exist: bool = True) -> Path:
    pure = PurePosixPath(relative)
    if pure.is_absolute() or not pure.parts or any(part in ("", ".", "..") for part in pure.parts):
        _fail(f"unsafe payload path: {relative!r}")
    candidate = root.joinpath(*pure.parts)
    try:
        resolved_root = root.resolve(strict=True)
        resolved = candidate.resolve(strict=must_exist)
    except FileNotFoundError as error:
        _fail(f"payload is missing: {relative}")
    if not resolved.is_relative_to(resolved_root):
        _fail(f"payload escapes its root: {relative}")
    if must_exist and (candidate.is_symlink() or not candidate.is_file()):
        _fail(f"payload is not a regular file: {relative}")
    return candidate


def _digest(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def create_manifest(
    *,
    manifest_path: Path,
    payload_root: Path,
    payloads: Sequence[str],
    artifact_name: str,
    producer_job: str,
    producer_instance: str,
    producer_attempt: str,
    source: SourceContext,
) -> dict[str, object]:
    if not artifact_name or not producer_job or not producer_instance:
        _fail("artifact name, producer job, and producer instance must be non-empty")
    attempt = _positive_integer(producer_attempt, "producer attempt")
    if not payloads:
        _fail("at least one payload is required")
    if len(set(payloads)) != len(payloads):
        _fail("payload paths must be unique")

    payload_entries = []
    for relative in sorted(payloads):
        path = _safe_payload_path(payload_root, relative)
        payload_entries.append(
            {"path": relative, "sha256": _digest(path), "size": path.stat().st_size}
        )

    manifest: dict[str, object] = {
        "schema": MANIFEST_SCHEMA,
        "artifact": {
            "name": artifact_name,
            "producer_job": producer_job,
            "producer_instance": producer_instance,
            "producer_attempt": attempt,
        },
        "source": source.to_json(),
        "payloads": payload_entries,
    }
    manifest_path.parent.mkdir(parents=True, exist_ok=True)
    manifest_path.write_text(
        json.dumps(manifest, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )
    return manifest


def _load_manifest(path: Path) -> dict[str, object]:
    try:
        parsed = json.loads(path.read_text(encoding="utf-8"))
    except FileNotFoundError:
        _fail(f"provenance manifest is missing: {path}")
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        _fail(f"provenance manifest is invalid JSON: {error}")
    return _require_exact_keys(parsed, {"schema", "artifact", "source", "payloads"}, "manifest")


def validate_manifest(
    *,
    manifest_path: Path,
    payload_root: Path,
    expected_artifact_name: str,
    expected_producer_job: str,
    expected_producer_instance: str,
    expected_producer_attempt: str,
    selected_artifact_id: str,
    current_attempt: str,
    required_payloads: Sequence[str],
    source: SourceContext,
) -> dict[str, object]:
    selected_id = _positive_integer(selected_artifact_id, "selected artifact ID")
    producer_attempt = _positive_integer(expected_producer_attempt, "expected producer attempt")
    consumer_attempt = _positive_integer(current_attempt, "current attempt")
    if int(producer_attempt) > int(consumer_attempt):
        _fail("producer attempt cannot be newer than the consumer attempt")
    manifest = _load_manifest(manifest_path)
    if manifest["schema"] != MANIFEST_SCHEMA:
        _fail(f"unsupported provenance schema: {manifest['schema']!r}")

    artifact = _require_exact_keys(
        manifest["artifact"],
        {"name", "producer_job", "producer_instance", "producer_attempt"},
        "artifact",
    )
    expected_artifact = {
        "name": expected_artifact_name,
        "producer_job": expected_producer_job,
        "producer_instance": expected_producer_instance,
        "producer_attempt": producer_attempt,
    }
    if artifact != expected_artifact:
        _fail(f"artifact provenance mismatch: got {artifact!r}, expected {expected_artifact!r}")

    recorded_source = _require_exact_keys(
        manifest["source"],
        {"repository", "run_id", "head_sha", "ref", "event_name", "workflow_ref", "actor_id"},
        "source",
    )
    expected_source = source.to_json()
    if recorded_source != expected_source:
        mismatches = [
            key
            for key in expected_source
            if recorded_source.get(key) != expected_source[key]
        ]
        _fail("artifact source/trust mismatch: " + ", ".join(mismatches))

    entries = manifest["payloads"]
    if not isinstance(entries, list) or not entries:
        _fail("payloads must be a non-empty array")
    seen: set[str] = set()
    verified_payloads: list[dict[str, object]] = []
    for index, raw in enumerate(entries):
        entry = _require_exact_keys(raw, {"path", "sha256", "size"}, f"payloads[{index}]")
        relative = entry["path"]
        digest = entry["sha256"]
        size = entry["size"]
        if not isinstance(relative, str) or relative in seen:
            _fail(f"payloads[{index}].path must be a unique string")
        if not isinstance(digest, str) or not SHA256.fullmatch(digest):
            _fail(f"payloads[{index}].sha256 is invalid")
        if not isinstance(size, int) or isinstance(size, bool) or size < 0:
            _fail(f"payloads[{index}].size is invalid")
        seen.add(relative)
        path = _safe_payload_path(payload_root, relative)
        if path.stat().st_size != size:
            _fail(f"payload size mismatch: {relative}")
        actual_digest = _digest(path)
        if actual_digest != digest:
            _fail(f"payload digest mismatch: {relative}")
        verified_payloads.append(dict(entry))

    required = set(required_payloads)
    if not required or seen != required:
        _fail(
            "manifest payload set mismatch: "
            f"got={sorted(seen)}, required={sorted(required)}"
        )

    return {
        "schema": RECEIPT_SCHEMA,
        "selected_artifact_id": selected_id,
        "artifact": dict(artifact),
        "source": expected_source,
        "consumer_attempt": consumer_attempt,
        "payloads": verified_payloads,
    }


def validate_selection(
    *,
    artifact_name: str,
    expected_artifact_name: str,
    selected_artifact_id: str,
    producer_attempt: str,
    current_attempt: str,
) -> None:
    if artifact_name != expected_artifact_name:
        _fail(
            f"selected artifact name mismatch: got {artifact_name!r}, "
            f"expected {expected_artifact_name!r}"
        )
    _positive_integer(selected_artifact_id, "selected artifact ID")
    producer = _positive_integer(producer_attempt, "producer attempt")
    consumer = _positive_integer(current_attempt, "current attempt")
    if int(producer) > int(consumer):
        _fail("producer attempt cannot be newer than the consumer attempt")


def safe_extract_tar(archive: Path, destination: Path) -> None:
    """Extract regular files and directories after rejecting unsafe members."""

    destination.mkdir(parents=True, exist_ok=True)
    destination_root = destination.resolve(strict=True)
    seen: set[PurePosixPath] = set()
    with tarfile.open(archive, mode="r:") as source:
        members = source.getmembers()
        safe_members: list[tuple[tarfile.TarInfo, PurePosixPath]] = []
        for member in members:
            relative = PurePosixPath(member.name)
            normalized_parts = tuple(part for part in relative.parts if part not in ("", "."))
            normalized = PurePosixPath(*normalized_parts)
            if not normalized.parts and member.isdir():
                # GNU tar emits this harmless root marker for `tar -C dir -cf out .`.
                continue
            if (
                relative.is_absolute()
                or not normalized.parts
                or any(part == ".." for part in normalized.parts)
                or normalized in seen
            ):
                _fail(f"unsafe or duplicate tar member: {member.name!r}")
            seen.add(normalized)
            if not (member.isdir() or member.isreg()):
                _fail(f"tar member is not a regular file or directory: {member.name!r}")
            safe_members.append((member, normalized))

        for member, relative in safe_members:
            target = destination_root.joinpath(*relative.parts)
            if not target.resolve(strict=False).is_relative_to(destination_root):
                _fail(f"tar member escapes extraction root: {member.name!r}")
            if member.isdir():
                target.mkdir(parents=True, exist_ok=True)
                continue
            target.parent.mkdir(parents=True, exist_ok=True)
            extracted = source.extractfile(member)
            if extracted is None:
                _fail(f"tar member cannot be read: {member.name!r}")
            with target.open("xb") as output:
                for chunk in iter(lambda: extracted.read(1024 * 1024), b""):
                    output.write(chunk)
            target.chmod(stat.S_IMODE(member.mode) & 0o755)


def write_receipt(receipt: dict[str, object], path: Path, step_summary: Path | None) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(receipt, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    if step_summary is None:
        return
    artifact = receipt["artifact"]
    source = receipt["source"]
    assert isinstance(artifact, dict) and isinstance(source, dict)
    with step_summary.open("a", encoding="utf-8") as summary:
        summary.write(
            "\n### Verified CI artifact\n\n"
            "| Field | Value |\n| --- | --- |\n"
            f"| Artifact ID | `{receipt['selected_artifact_id']}` |\n"
            f"| Artifact name | `{artifact['name']}` |\n"
            f"| Producer | `{artifact['producer_job']} / {artifact['producer_instance']}` |\n"
            f"| Producer attempt | `{artifact['producer_attempt']}` |\n"
            f"| Consumer attempt | `{receipt['consumer_attempt']}` |\n"
            f"| Run ID | `{source['run_id']}` |\n"
            f"| Head SHA | `{source['head_sha']}` |\n"
        )


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)

    create = subparsers.add_parser("create")
    create.add_argument("--manifest", type=Path, required=True)
    create.add_argument("--payload-root", type=Path, required=True)
    create.add_argument("--payload", action="append", required=True)
    create.add_argument("--artifact-name", required=True)
    create.add_argument("--producer-job", required=True)
    create.add_argument("--producer-instance", required=True)
    create.add_argument("--producer-attempt", required=True)

    select = subparsers.add_parser("select")
    select.add_argument("--artifact-name", required=True)
    select.add_argument("--expected-artifact-name", required=True)
    select.add_argument("--selected-artifact-id", required=True)
    select.add_argument("--producer-attempt", required=True)
    select.add_argument("--current-attempt", required=True)

    consume = subparsers.add_parser("consume")
    consume.add_argument("--manifest", type=Path, required=True)
    consume.add_argument("--payload-root", type=Path, required=True)
    consume.add_argument("--require-payload", action="append", required=True)
    consume.add_argument("--artifact-name", required=True)
    consume.add_argument("--producer-job", required=True)
    consume.add_argument("--producer-instance", required=True)
    consume.add_argument("--producer-attempt", required=True)
    consume.add_argument("--selected-artifact-id", required=True)
    consume.add_argument("--current-attempt", required=True)
    consume.add_argument("--receipt", type=Path, required=True)
    consume.add_argument("--step-summary", type=Path)
    consume.add_argument("--extract-tar")
    consume.add_argument("--extract-to", type=Path)
    return parser


def main(argv: Sequence[str] | None = None) -> int:
    args = _parser().parse_args(argv)
    try:
        if args.command == "select":
            validate_selection(
                artifact_name=args.artifact_name,
                expected_artifact_name=args.expected_artifact_name,
                selected_artifact_id=args.selected_artifact_id,
                producer_attempt=args.producer_attempt,
                current_attempt=args.current_attempt,
            )
            return 0

        source = SourceContext.from_environment(os.environ)
        if args.command == "create":
            create_manifest(
                manifest_path=args.manifest,
                payload_root=args.payload_root,
                payloads=args.payload,
                artifact_name=args.artifact_name,
                producer_job=args.producer_job,
                producer_instance=args.producer_instance,
                producer_attempt=args.producer_attempt,
                source=source,
            )
            return 0

        receipt = validate_manifest(
            manifest_path=args.manifest,
            payload_root=args.payload_root,
            expected_artifact_name=args.artifact_name,
            expected_producer_job=args.producer_job,
            expected_producer_instance=args.producer_instance,
            expected_producer_attempt=args.producer_attempt,
            selected_artifact_id=args.selected_artifact_id,
            current_attempt=args.current_attempt,
            required_payloads=args.require_payload,
            source=source,
        )
        if bool(args.extract_tar) != bool(args.extract_to):
            _fail("--extract-tar and --extract-to must be supplied together")
        if args.extract_tar:
            verified_paths = {
                entry["path"]
                for entry in receipt["payloads"]
                if isinstance(entry, dict) and isinstance(entry.get("path"), str)
            }
            if args.extract_tar not in verified_paths:
                _fail("tar selected for extraction is not a verified manifest payload")
            archive = _safe_payload_path(args.payload_root, args.extract_tar)
            safe_extract_tar(archive, args.extract_to)
        write_receipt(receipt, args.receipt, args.step_summary)
        return 0
    except (OSError, ProvenanceError, tarfile.TarError) as error:
        print(f"artifact provenance refusal: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
